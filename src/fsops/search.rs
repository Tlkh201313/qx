//! The `glob` and `grep` operations, the walkers under them, and the caches they share.

use super::*;

/// OneDrive-style files that exist only in the cloud: reading one downloads it.
fn cloud_placeholder(md: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // OFFLINE, RECALL_ON_OPEN, RECALL_ON_DATA_ACCESS
        md.file_attributes() & (0x1000 | 0x40000 | 0x400000) != 0
    }
    #[cfg(not(windows))]
    {
        let _ = md;
        false
    }
}

/// Pull a workspace into the OS file cache in the background the first time it is named, so the first
/// glob or grep over it is not a cold disk walk. Bounded in entries and bytes; never blocks a call.
#[cfg_attr(test, allow(dead_code))] // only the warm-up thread calls it, and tests do not start that
pub(crate) fn prime(dir: &Path) {
    // A drive root or the home folder is not a workspace: reading it would walk the whole disk.
    if crate::warmup::off() || dir.parent().is_none() || crate::resolve::home_dir().is_some_and(|h| h == dir) {
        return;
    }
    static SEEN: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        if seen.len() >= 8 || seen.iter().any(|d| d == dir) {
            return;
        }
        seen.push(dir.to_path_buf());
    }
    let dir = dir.to_path_buf();
    let _ = std::thread::Builder::new().name("qx-prime".into()).spawn(move || {
        crate::index::ensure(&dir);
        const MAX_ENTRIES: usize = 60_000;
        const MAX_BYTES: u64 = 64 << 20;
        const MAX_FILE: u64 = 256 << 10;
        let (mut entries, mut bytes) = (0usize, 0u64);
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            let Ok(rd) = fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                entries += 1;
                if entries > MAX_ENTRIES || bytes > MAX_BYTES {
                    return;
                }
                let Ok(ft) = e.file_type() else { continue };
                let name = e.file_name().to_string_lossy().into_owned();
                if ft.is_dir() {
                    if !is_vcs_dir(&name) && !skip_dir(&name, false) {
                        stack.push(e.path());
                    }
                } else if ft.is_file() {
                    if let Ok(md) = e.metadata() {
                        if md.len() <= MAX_FILE && !cloud_placeholder(&md) {
                            bytes += md.len();
                            let _ = fs::read(e.path());
                        }
                    }
                }
            }
        }
    });
}

// --------------------------------------------------------------------- glob

pub(super) fn glob(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let pattern = req_str(args, "pattern")?;
    let reply = glob_once(root, args, pattern)?;
    // `app/[id]/*.tsx` and `pages/[id].tsx` name a folder or file called `[id]`, which as a pattern is a
    // one-letter class. When that finds nothing, every bracket is tried as a plain character.
    if reply.structured["count"] == 0 && pattern.contains('[') {
        let retry = glob_once(root, args, &pattern.replace('[', "[[]"))?;
        if retry.structured["count"] != 0 {
            return Ok(retry);
        }
    }
    Ok(reply)
}

fn glob_once(root: &Root, args: &Value, pattern: &str) -> Result<Reply, FsError> {
    if pattern.starts_with('!') {
        return Err(FsError::new(
            "FS_UNSUPPORTED",
            format!(
                "negated glob patterns are not supported by qx; use the native {} tool",
                native(Native::Glob)
            ),
        )
        .fb("glob"));
    }
    let raw_path = path_arg(args).unwrap_or(".");
    let root_path = root.resolve_read(raw_path).map_err(|e| e.fb("glob"))?;
    let patterns = expand_braces(&relative_pattern(pattern, &root_path));
    let max_results = usize_arg(
        args,
        "max_results",
        DEFAULT_GLOB_RESULTS,
        1,
        MAX_GLOB_RESULTS,
    )?;
    let ignore_case = bool_arg(args, "ignore_case", false);
    // Asking for a generated directory by name (`node_modules/**`) is asking to look inside it.
    let all = bool_arg(args, "all", false)
        || patterns.iter().any(|p| names_skipped_dir(p) || names_ignored_dir(&root_path, p))
        || names_skipped_dir(raw_path);
    let globs = compile_globs(&patterns, ignore_case);
    let mut skipped: Vec<String> = Vec::new();

    let mut hits = Newest::new();
    let mut count = 0usize;
    let scanned: usize;
    let mut scan_capped = false;

    let md = fs::metadata(&root_path)
        .map_err(|e| FsError::io(format!("cannot stat \"{raw_path}\""), e).fb("glob"))?;
    if md.is_file() {
        scanned = 1;
        let rel = root_path
            .file_name()
            .map(|s| s.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if matches_any(&globs, &rel, &rel, &mut GlobScratch::default()) {
            count = 1;
            keep_newest(&mut hits, max_results, md.modified().unwrap_or(UNIX_EPOCH), &root_path);
        }
    } else {
        // Match inside the walk: only hits are stored, and a hit's mtime comes from the directory entry
        // (free on Windows) instead of a second stat. Each walker thread keeps its own newest hits;
        // the newest `max_results` of all of them are the newest overall, whatever order they arrive in.
        // Patterns with a directory part (`src/**/*.rs`) can only match below it: read just that range.
        let mut hints: Vec<String> = Vec::new();
        if !ignore_case && patterns.iter().all(|p| p.contains('/') && !literal_dir(p).is_empty()) {
            let mut dirs: Vec<String> = patterns.iter().map(|p| literal_dir(p)).collect();
            // By path component, not by string: `src-x` sorts before `src/a`, which would hide that `src` covers it.
            dirs.sort_by(|a, b| a.split('/').cmp(b.split('/')));
            // Exact case on every platform: the matcher is case-sensitive, so `Src` and `src/deep` are different ranges.
            dirs.dedup_by(|b, a| b == a || b.strip_prefix(a.as_str()).is_some_and(|rest| rest.starts_with('/')));
            hints = dirs;
        }
        // No `**` and a `/` in every pattern: a match lies as many directories down as the pattern has.
        let max_depth = if patterns.iter().all(|p| p.contains('/') && !p.contains("**")) {
            patterns.iter().map(|p| p.split('/').count() - 1).max()
        } else {
            None
        };
        let (parts, skipped_dirs) = walk_bounded(&root_path, all, &hints, max_depth, GlobPart::default, |part, p, meta| {
            part.walked += 1;
            let rel = p
                .strip_prefix(&root_path)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/");
            let base = p
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            if matches_any(&globs, &rel, &base, &mut part.scratch) {
                part.count += 1;
                keep_newest(&mut part.hits, max_results, meta.modified(), p);
            }
        });
        skipped = skipped_dirs;
        let mut walked = 0usize;
        for part in parts {
            walked += part.walked;
            count += part.count;
            for Reverse((modified, Reverse(p))) in part.hits {
                keep_newest(&mut hits, max_results, modified, &p);
            }
        }
        scan_capped = walked >= MAX_WALK_FILES;
        scanned = walked;
    }

    let mut hits: Vec<(SystemTime, PathBuf)> = hits.into_iter().map(|Reverse((t, Reverse(p)))| (t, p)).collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let shown = hits.len().min(max_results);
    let mut out = String::new();
    for (_, p) in hits.iter().take(shown) {
        out.push_str(&root.display(p));
        out.push('\n');
    }
    if count == 0 {
        out.push_str("No files found.\n");
        out.push_str(&outside_note(&patterns, &root_path));
    } else if count > shown {
        out.push_str(&format!(
            "[qx] showing {shown} of {}{} paths; narrow the pattern or use the native {} tool.\n",
            if scan_capped { "at least " } else { "" },
            count,
            native(Native::Glob)
        ));
    }
    if scan_capped {
        out.push_str("[qx] scan stopped at the file budget; results may be incomplete.\n");
    }
    // Said only when nothing was found: with hits the line is ~45 tokens on every call, and the
    // `all` option is in the tool's description.
    if count == 0 && !skipped.is_empty() {
        out.push_str(&skip_note(&skipped));
    }
    Ok(Reply {
        text: out,
        structured: json!({
            "op": "glob",
            "path": root.display(&root_path),
            "count": count,
            "shown": shown,
            "truncated": count > shown || scan_capped,
            "scan_capped": scan_capped,
            "scanned": scanned,
        }),
        is_error: false,
    })
}

/// The newest hits of a glob, as a min-heap whose top is the worst one kept: memory stays
/// bounded and a late, newer file still displaces an older one.
type Newest = BinaryHeap<Reverse<(SystemTime, Reverse<PathBuf>)>>;

/// What one walker thread found.
#[derive(Default)]
struct GlobPart {
    hits: Newest,
    count: usize,
    walked: usize,
    scratch: GlobScratch,
}

fn keep_newest(hits: &mut Newest, max_results: usize, modified: SystemTime, p: &Path) {
    if hits.len() < max_results || hits.peek().is_some_and(|Reverse((t, _))| modified >= *t) {
        hits.push(Reverse((modified, Reverse(p.to_path_buf()))));
        if hits.len() > max_results {
            hits.pop();
        }
    }
}

fn compile_globs(patterns: &[String], ignore_case: bool) -> Vec<Glob> {
    patterns.iter().map(|p| Glob::new(p, ignore_case)).collect()
}

fn matches_any(globs: &[Glob], rel: &str, base: &str, scratch: &mut GlobScratch) -> bool {
    globs.iter().any(|g| g.matches(if g.has_slash { rel } else { base }, scratch))
}

// --------------------------------------------------------------------- grep

/// How one `grep` call matches a line. Plain text is the escaped pattern, so one engine and one
/// case-folding rule serve both.
pub(super) type Matcher = regex::Regex;

pub(super) fn grep(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let needle = req_str(args, "pattern")?;
    let raw_path = path_arg(args).unwrap_or(".");
    let root_path = root.resolve_read(raw_path).map_err(|e| e.fb("grep"))?;
    if needle.contains(['\n', '\r']) {
        return Err(FsError::arg(
            "pattern is matched line by line: remove the line break (match one line, or one line at a time)",
        )
        .fb("grep"));
    }
    let include = opt_str(args, "include");
    if let Some(inc) = include {
        // A comma inside braces (`*.{ts,tsx}`) is one glob; only a top-level comma is a list.
        if split_top_level(inc).len() > 1 {
            return Err(FsError::arg(
                "include takes one glob; comma-separated lists are not supported (use *.{a,b})",
            )
            .fb("grep"));
        }
        if inc.starts_with('!') {
            return Err(FsError::new(
                "FS_UNSUPPORTED",
                "negated include globs are not supported; search with a narrower include",
            )
            .fb("grep"));
        }
    }
    let include_patterns = include.map(|s| expand_braces(&relative_pattern(s, &root_path)));
    let ignore_case = bool_arg(args, "ignore_case", false);
    let all = bool_arg(args, "all", false)
        || names_skipped_dir(raw_path)
        || include_patterns.as_ref().is_some_and(|ps| ps.iter().any(|p| names_skipped_dir(p) || names_ignored_dir(&root_path, p)));
    let max_matches = usize_arg(
        args,
        "max_matches",
        DEFAULT_GREP_MATCHES,
        1,
        MAX_GREP_MATCHES,
    )?;
    let max_line_chars = usize_arg(
        args,
        "max_line_chars",
        DEFAULT_GREP_LINE_CHARS,
        20,
        MAX_READ_LINE_CHARS,
    )?;
    // `context`, `-A`, `-B` and `-C` are grep and Claude Code's own names: models send them unprompted,
    // so they are honoured though the schema does not advertise them.
    let ctx = |k: &str| {
        let n = args.get(k).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())));
        n.map_or(0, |v| (v as usize).min(MAX_GREP_CONTEXT))
    };
    let c = ctx("context").max(ctx("-C"));
    let (before, after) = (c.max(ctx("-B")), c.max(ctx("-A")));

    // Regex by default, like the dsh grep tool. A pattern that is not a valid regex (`foo(`, a lone `[`)
    // is searched as plain text instead of failing, and the reply says so.
    let build = |p: &str| regex::RegexBuilder::new(p).case_insensitive(ignore_case).size_limit(32 * 1024 * 1024).build();
    let literal = bool_arg(args, "literal", false);
    let mut literal_fallback = false;
    let built = if literal {
        build(&regex::escape(needle))
    } else {
        build(needle).or_else(|_| {
            literal_fallback = true;
            build(&regex::escape(needle))
        })
    };
    let matcher = built.map_err(|e| FsError::arg(format!("pattern too large to search: {e}")).fb("grep"))?;

    // With the file's mtime when the index supplied the list: that is what makes its cached text usable.
    let mut files: Vec<(PathBuf, Option<SystemTime>)> = Vec::new();
    let md = fs::metadata(&root_path)
        .map_err(|e| FsError::io(format!("cannot stat \"{raw_path}\""), e).fb("grep"))?;
    let mut scan_capped = false;
    let mut skipped_dirs: Vec<String> = Vec::new();
    if md.is_file() {
        files.push((root_path.clone(), None));
    } else {
        let (parts, skipped) = walk_files(&root_path, all, Vec::new, |found: &mut Vec<(PathBuf, Option<SystemTime>)>, p, meta| {
            found.push((p.to_path_buf(), meta.indexed_mtime()))
        });
        skipped_dirs = skipped;
        files = parts.into_iter().flatten().collect();
        scan_capped = files.len() >= MAX_WALK_FILES;
    }
    // No sort here: matching files are sorted by path below, which fixes the output order.
    if let Some(patterns) = &include_patterns {
        let globs = compile_globs(patterns, ignore_case);
        let mut scratch = GlobScratch::default();
        // Below a folder, `include` is relative to it. A single file has nothing below it: its path from
        // the workspace is what `include` is then matched against.
        let rel_to = if md.is_file() { root.cwd.as_path() } else { root_path.as_path() };
        files.retain(|(p, _)| {
            let rel = p
                .strip_prefix(rel_to)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/");
            let base = p
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            matches_any(&globs, &rel, &base, &mut scratch)
        });
    }

    let trace = std::env::var_os("QX_FS_TRACE").is_some();
    let t_scan = Instant::now();
    let panicked = AtomicBool::new(false);
    let fast = fast_regex(needle, literal || literal_fallback, ignore_case);
    let fast = fast.as_ref();
    let matcher = &matcher;
    // Threads cost ~60 us each to start: only as many as the work is worth. A file whose text is
    // cached takes ~10 us; one that must be opened and read, ~150 us.
    let cold = files.iter().filter(|(p, m)| m.is_none_or(|m| content_get(p, m).is_none())).count();
    let work_us = cold * 150 + (files.len() - cold) * 10;
    let threads = fs_threads().min(files.len().max(1)).min(work_us / 400 + 1);
    let mut scans: Vec<ScanFile> = if threads <= 1 {
        files
            .iter()
            .map(|(p, m)| scan_file(p, *m, matcher, fast, max_line_chars, max_matches))
            .collect()
    } else {
        // Each thread takes the next unread file, so one big file does not leave the others idle.
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| {
                        let mut mine = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some((p, m)) = files.get(i) else { break };
                            mine.push(scan_file(p, *m, matcher, fast, max_line_chars, max_matches));
                        }
                        mine
                    })
                })
                .collect();
            let mut out = Vec::with_capacity(files.len());
            for h in handles {
                match h.join() {
                    Ok(v) => out.extend(v),
                    Err(_) => panicked.store(true, Ordering::Relaxed),
                }
            }
            out
        })
    };
    if trace {
        let big = scans.iter().filter(|s| s.large).count();
        eprintln!("[grep] files {} cold {} threads {} scan {:?} large {} fast {}", files.len(), cold, threads, t_scan.elapsed(), big, fast.is_some());
    }
    let total: usize = scans.iter().map(|s| s.total).sum();
    let skipped_binary: usize = scans.iter().filter(|s| s.binary).count();
    let skipped_large: usize = scans.iter().filter(|s| s.large).count();
    let read_errors: usize = scans.iter().filter(|s| s.error).count();
    scans.retain(|s| s.total > 0); // only files with matches are listed: sort just those
    scans.sort_by(|a, b| a.path.cmp(&b.path));
    let with_matches = scans.len();

    // Grouped by file, like the dsh grep tool: the path is said once per file, not once per match.
    let mut out = String::new();
    let mut shown = 0usize;
    if total > 0 {
        out.push_str(&format!(
            "Found {total} match{} in {with_matches} file{}\n",
            if total == 1 { "" } else { "es" },
            if with_matches == 1 { "" } else { "s" }
        ));
    }
    'files: for sf in &scans {
        if shown >= max_matches {
            break;
        }
        out.push('\n');
        out.push_str(&root.display(&sf.path));
        let lines = if before + after > 0 { context_lines(&sf.path, &sf.matches, before, after, max_line_chars) } else { HashMap::new() };
        let mut next = 1usize; // the first line of this file not yet printed
        for (i, m) in sf.matches.iter().enumerate() {
            if shown >= max_matches {
                break 'files;
            }
            let from = m.line.saturating_sub(before).max(next);
            if !lines.is_empty() && from > next && next > 1 {
                out.push_str("\n--");
            }
            for n in from..m.line {
                if let Some(l) = lines.get(&n) {
                    out.push('\n');
                    push_usize(&mut out, n);
                    out.push_str("- ");
                    push_clipped(&mut out, l, max_line_chars);
                }
            }
            out.push('\n');
            push_usize(&mut out, m.line);
            out.push_str(": ");
            out.push_str(&m.text);
            shown += 1;
            next = m.line + 1;
            let stop = sf.matches.get(i + 1).map_or(usize::MAX, |n| n.line);
            let last = (m.line + after).min(stop.saturating_sub(1));
            let mut printed_to = next;
            for n in next..=last {
                // Past the end of the file there is no line: the window simply has none.
                let Some(l) = lines.get(&n) else { break };
                out.push('\n');
                push_usize(&mut out, n);
                out.push_str("- ");
                push_clipped(&mut out, l, max_line_chars);
                printed_to = n + 1;
            }
            next = printed_to;
        }
    }
    if total == 0 {
        out.push_str("No matches found.\n");
        out.push_str(&outside_note(include_patterns.as_deref().unwrap_or_default(), &root_path));
    } else if total > shown {
        out.push_str(&format!(
            "\n[qx] showing {shown} of {total} matches; narrow the pattern, path or include.\n"
        ));
    } else {
        out.push('\n');
    }
    if literal_fallback {
        out.push_str("[qx] the pattern is not a valid regex; it was searched as plain text.\n");
    }
    let left_out: Vec<String> = [(skipped_binary, "binary"), (skipped_large, "large"), (read_errors, "unreadable")]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
    if !left_out.is_empty() {
        out.push_str(&format!("[qx] skipped {} file(s).\n", left_out.join(", ")));
    }
    if total == 0 && !skipped_dirs.is_empty() {
        out.push_str(&skip_note(&skipped_dirs));
    }
    if scan_capped {
        out.push_str("[qx] scan stopped at the file budget; results may be incomplete.\n");
    }
    if panicked.load(Ordering::Relaxed) {
        out.push_str("[qx] a scan worker failed; results may be incomplete.\n");
    }
    Ok(Reply {
        text: out,
        structured: json!({
            "op": "grep",
            "count": total,
            "files": with_matches,
            "shown": shown,
            "truncated": total > shown || scan_capped || panicked.load(Ordering::Relaxed),
            "skipped_binary": skipped_binary,
            "skipped_large": skipped_large,
            "read_errors": read_errors,
            "scan_capped": scan_capped,
            "threads": threads,
            "scanned": files.len(),
        }),
        is_error: false,
    })
}

struct Match {
    line: usize,
    text: String,
}

struct ScanFile {
    path: PathBuf,
    matches: Vec<Match>,
    total: usize,
    binary: bool,
    large: bool,
    error: bool,
}

/// What a search found in a file the last time it read it, valid while the index still reports the
/// same mtime. Opening and reading a file costs 100-200 us on Windows even when it is cached by the OS;
/// a search over a settled workspace is then pure memory.
enum Cached {
    Text(std::sync::Arc<str>),
    Binary,
}

static CONTENT: std::sync::RwLock<Option<HashMap<PathBuf, (SystemTime, Cached)>>> = std::sync::RwLock::new(None);
static CONTENT_BYTES: AtomicUsize = AtomicUsize::new(0);
const MAX_CONTENT_BYTES: usize = 32 << 20;
const MAX_CACHED_FILE: usize = 256 << 10;
/// A file written less than this long ago is read fresh: its writer may still be going.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(3);

fn content_get(path: &Path, mtime: SystemTime) -> Option<Cached> {
    let guard = CONTENT.read().ok()?;
    let (m, c) = guard.as_ref()?.get(path)?;
    if *m != mtime {
        return None;
    }
    Some(match c {
        Cached::Text(t) => Cached::Text(t.clone()),
        Cached::Binary => Cached::Binary,
    })
}

fn content_put(path: &Path, mtime: SystemTime, what: Cached) {
    if SystemTime::now().duration_since(mtime).map_or(true, |age| age < SETTLE) {
        return;
    }
    let size = match &what {
        Cached::Text(t) => t.len() + 128,
        Cached::Binary => 128,
    };
    if let Ok(mut guard) = CONTENT.write() {
        let map = guard.get_or_insert_with(HashMap::new);
        // Full: start over. Refusing new files would keep the oldest ones (and deleted ones) for the whole
        // session and stop the cache helping; a cleared one refills with what is searched now.
        if CONTENT_BYTES.load(Ordering::Relaxed) + size > MAX_CONTENT_BYTES {
            map.clear();
            map.shrink_to_fit();
            CONTENT_BYTES.store(0, Ordering::Relaxed);
        }
        if let Some((_, old)) = map.insert(path.to_path_buf(), (mtime, what)) {
            let old_size = match old {
                Cached::Text(t) => t.len() + 128,
                Cached::Binary => 128,
            };
            CONTENT_BYTES.fetch_sub(old_size.min(CONTENT_BYTES.load(Ordering::Relaxed)), Ordering::Relaxed);
        }
        CONTENT_BYTES.fetch_add(size, Ordering::Relaxed);
    }
}

/// The index saw `path` change: what is cached for it no longer counts, even when the rewrite kept
/// its mtime. With `tree`, everything cached below it goes too (a folder was removed or renamed away).
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows index reports changes
pub(crate) fn content_forget(path: &Path, tree: bool) {
    let Ok(mut guard) = CONTENT.write() else { return };
    let Some(map) = guard.as_mut() else { return };
    let size = |c: &Cached| match c {
        Cached::Text(t) => t.len() + 128,
        Cached::Binary => 128,
    };
    let mut freed = map.remove(path).map_or(0, |(_, c)| size(&c));
    if tree {
        map.retain(|k, (_, c)| {
            let keep = !k.starts_with(path);
            if !keep {
                freed += size(c);
            }
            keep
        });
    }
    CONTENT_BYTES.fetch_sub(freed.min(CONTENT_BYTES.load(Ordering::Relaxed)), Ordering::Relaxed);
}

fn scan_file(path: &Path, indexed: Option<SystemTime>, matcher: &Matcher, fast: Option<&regex::Regex>, max_line_chars: usize, max_store: usize) -> ScanFile {
    let mut sf = ScanFile {
        path: path.to_path_buf(),
        matches: Vec::new(),
        total: 0,
        binary: false,
        large: false,
        error: false,
    };
    if let Some(m) = indexed {
        match content_get(path, m) {
            Some(Cached::Binary) => {
                sf.binary = true;
                return sf;
            }
            Some(Cached::Text(text)) => {
                scan_text(&text, matcher, fast, max_line_chars, max_store, &mut sf);
                return sf;
            }
            None => {}
        }
    }
    let md = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => {
            sf.error = true;
            return sf;
        }
    };
    if !md.is_file() {
        return sf;
    }
    if md.len() > MAX_GREP_FILE_BYTES {
        if md.len() > MAX_GREP_STREAM_BYTES {
            sf.large = true;
            return sf;
        }
        scan_stream(path, matcher, fast, max_line_chars, max_store, &mut sf);
        return sf;
    }
    let bytes = match read_unless_binary(path, md.len()) {
        Ok(Some(b)) => b,
        Ok(None) => {
            sf.binary = true;
            if let Some(m) = indexed {
                content_put(path, m, Cached::Binary);
            }
            return sf;
        }
        Err(_) => {
            sf.error = true;
            return sf;
        }
    };
    let cacheable = indexed.filter(|_| bytes.len() <= MAX_CACHED_FILE);
    let text = crate::exec::decode_owned(bytes);
    if let Some(m) = cacheable {
        content_put(path, m, Cached::Text(std::sync::Arc::from(text.as_str())));
    }
    scan_text(&text, matcher, fast, max_line_chars, max_store, &mut sf);
    sf
}

/// The file's bytes, or `None` when its first 8 KB hold a NUL (a binary: the rest is never read).
fn read_unless_binary(path: &Path, len: u64) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let mut f = fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(len as usize + 1);
    Read::by_ref(&mut f).take(8192).read_to_end(&mut bytes)?;
    let utf16 = bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]);
    if !utf16 && bytes.contains(&0) {
        return Ok(None);
    }
    f.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

fn scan_text(text: &str, matcher: &Matcher, fast: Option<&regex::Regex>, max_line_chars: usize, max_store: usize, sf: &mut ScanFile) {
    // A byte-order mark is not part of the first line: `^fn` must match it.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match fast {
        Some(re) => scan_buffer(text, 1, re, matcher, max_line_chars, max_store, sf),
        None => {
            if text.is_empty() {
                return;
            }
            let text = text.strip_suffix('\n').unwrap_or(text);
            for (idx, line) in text.split('\n').enumerate() {
                let line = line.strip_suffix('\r').unwrap_or(line);
                if !matcher.is_match(line) {
                    continue;
                }
                sf.total += 1;
                if sf.matches.len() < max_store {
                    sf.matches.push(Match { line: idx + 1, text: clip_match(line, max_line_chars, matcher) });
                }
            }
        }
    }
}

/// Search the whole text at once and look only at the lines the search lands on. `re` is a
/// multi-line version of the pattern that finds a superset of the matching lines; each line it
/// names is then judged by the exact per-line `matcher`, so the answer is the same as testing
/// every line, without testing the lines that cannot match.
fn scan_buffer(text: &str, first_line: usize, re: &regex::Regex, matcher: &Matcher, max_line_chars: usize, max_store: usize, sf: &mut ScanFile) {
    let bytes = text.as_bytes();
    let (mut cursor, mut counted_to, mut line_no) = (0usize, 0usize, first_line);
    while cursor <= text.len() {
        let Some(m) = re.find_at(text, cursor) else { break };
        let s = m.start();
        // After the last line break there is no line: `^$` or `x*` would report an empty one.
        if s == text.len() && (s == 0 || bytes[s - 1] == b'\n') {
            break;
        }
        let ls = bytes[..s].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let le = bytes[s..].iter().position(|&b| b == b'\n').map_or(text.len(), |i| s + i);
        let line = text[ls..le].strip_suffix('\r').unwrap_or(&text[ls..le]);
        cursor = le + 1;
        if !matcher.is_match(line) {
            continue;
        }
        line_no += bytes[counted_to..ls].iter().filter(|&&b| b == b'\n').count();
        counted_to = ls;
        sf.total += 1;
        if sf.matches.len() < max_store {
            sf.matches.push(Match { line: line_no, text: clip_match(line, max_line_chars, matcher) });
        }
    }
}

/// The multi-line search expression for `scan_buffer`, when it is certain to find every line the
/// per-line matcher would accept: a pattern with no anchors or flags that mean something different
/// on a whole text than on one line.
fn fast_regex(pattern: &str, text: bool, ignore_case: bool) -> Option<regex::Regex> {
    let ci = if ignore_case { "(?i)" } else { "" };
    let source = if text {
        if pattern.is_empty() {
            return None;
        }
        format!("{ci}{}", regex::escape(pattern))
    } else {
        let p = pattern;
        if p.is_empty() || p.contains("\\A") || p.contains("\\z") || p.contains("\\Z") || p.contains("\\n") || p.contains("\\r") {
            return None;
        }
        // Only plain groups and `(?i)`: any other flag could change what `^`, `$` or `.` mean.
        let mut rest = p;
        while let Some(i) = rest.find("(?") {
            let after = &rest[i + 2..];
            if !(after.starts_with(':') || after.starts_with("i)") || after.starts_with("P<") || after.starts_with('<')) {
                return None;
            }
            rest = after;
        }
        format!("(?mR){ci}{p}")
    };
    regex::RegexBuilder::new(&source).size_limit(32 * 1024 * 1024).build().ok()
}

/// Reads past the rest of the current line. False on a read error.
fn skip_line(r: &mut impl std::io::BufRead) -> bool {
    loop {
        let (done, used) = match r.fill_buf() {
            Ok([]) => return true,
            Ok(b) => match b.iter().position(|&c| c == b'\n') {
                Some(i) => (true, i + 1),
                None => (false, b.len()),
            },
            Err(_) => return false,
        };
        r.consume(used);
        if done {
            return true;
        }
    }
}

/// A file too big to load whole is searched line by line.
fn scan_stream(path: &Path, matcher: &Matcher, fast: Option<&regex::Regex>, max_line_chars: usize, max_store: usize, sf: &mut ScanFile) {
    use std::io::{BufRead, BufReader, Read};
    let Ok(f) = fs::File::open(path) else {
        sf.error = true;
        return;
    };
    const LONG_LINE: usize = 16 << 20;
    let mut r = BufReader::with_capacity(1 << 20, f);
    match r.fill_buf() {
        Ok(head) if head.iter().take(8192).any(|&b| b == 0) => {
            sf.binary = true;
            return;
        }
        Err(_) => {
            sf.error = true;
            return;
        }
        _ => {}
    }
    // As in `scan_text`: the mark is not part of the first line.
    if r.fill_buf().is_ok_and(|h| h.starts_with(b"\xEF\xBB\xBF")) {
        r.consume(3);
    }
    if let Some(re) = fast {
        // 4 MB at a time, cut at the last line break: the whole-buffer search at memory speed.
        const CHUNK: usize = 4 << 20;
        let mut skipping = false;
        let mut carry: Vec<u8> = Vec::new();
        let mut first_line = 1usize;
        let mut block = vec![0u8; CHUNK];
        loop {
            let n = match r.read(&mut block) {
                Ok(n) => n,
                Err(_) => {
                    sf.error = true;
                    return;
                }
            };
            let mut fresh = &block[..n];
            if skipping {
                // The rest of a line already searched as a prefix: dropped up to its line break.
                match fresh.iter().position(|&b| b == b'\n') {
                    Some(i) => {
                        skipping = false;
                        fresh = &fresh[i + 1..];
                    }
                    None if n == 0 => return,
                    None => continue,
                }
            }
            carry.extend_from_slice(fresh);
            let cut = if n == 0 { carry.len() } else { carry.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1) };
            if cut > 0 {
                let text = String::from_utf8_lossy(&carry[..cut]);
                scan_buffer(&text, first_line, re, matcher, max_line_chars, max_store, sf);
                first_line += carry[..cut].iter().filter(|&&b| b == b'\n').count();
                carry.drain(..cut);
            } else if carry.len() > LONG_LINE {
                // One line this long (a minified bundle, a data blob) would be held whole: only its first
                // LONG_LINE bytes are searched, as one line, and the rest is skipped.
                let text = String::from_utf8_lossy(&carry);
                scan_buffer(&text, first_line, re, matcher, max_line_chars, max_store, sf);
                first_line += 1;
                carry.clear();
                skipping = true;
            }
            if n == 0 {
                return;
            }
        }
    }
    let mut buf = Vec::new();
    let mut idx = 0usize;
    loop {
        buf.clear();
        // At most LONG_LINE bytes of a line are held; the rest of a longer one is read past and dropped.
        match (&mut r).take(LONG_LINE as u64).read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if n == LONG_LINE && buf.last() != Some(&b'\n') && !skip_line(&mut r) {
                    sf.error = true;
                    break;
                }
            }
            Err(_) => {
                sf.error = true;
                break;
            }
        }
        idx += 1;
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);
        if !matcher.is_match(line) {
            continue;
        }
        sf.total += 1;
        if sf.matches.len() < max_store {
            sf.matches.push(Match { line: idx, text: clip_match(line, max_line_chars, matcher) });
        }
    }
}

/// The lines around a file's matches, by line number: only those, never the whole file. A file over
/// `MAX_GREP_FILE_BYTES` is streamed (as `scan_stream` read it) and the read stops at the last wanted line.
fn context_lines(path: &Path, matches: &[Match], before: usize, after: usize, max_chars: usize) -> HashMap<usize, String> {
    use std::io::{BufReader, Cursor, Read};
    let ranges: Vec<(usize, usize)> = matches.iter().map(|m| (m.line.saturating_sub(before).max(1), m.line + after)).collect();
    let Ok(mut f) = fs::File::open(path) else { return HashMap::new() };
    if f.metadata().is_ok_and(|m| m.len() > MAX_GREP_FILE_BYTES) {
        return collect_lines(BufReader::with_capacity(1 << 20, f), &ranges, max_chars);
    }
    let mut bytes = Vec::new();
    if f.read_to_end(&mut bytes).is_err() {
        return HashMap::new();
    }
    let text = decode_text(&bytes);
    collect_lines(Cursor::new(text.as_bytes()), &ranges, max_chars)
}

/// Lines `(first, last)`, 1-based and both included, read from `r` and kept by number; reading stops after
/// the last wanted line. `ranges` ascend in both ends. A kept line is cut to one char past `max_chars`
/// (`push_clipped` then says it was cut), so a huge line is never held whole.
fn collect_lines(mut r: impl std::io::BufRead, ranges: &[(usize, usize)], max_chars: usize) -> HashMap<usize, String> {
    let mut out = HashMap::new();
    let Some(last) = ranges.iter().map(|r| r.1).max() else { return out };
    let keep = (max_chars + 1) * 4;
    let (mut ri, mut buf) = (0usize, Vec::new());
    for n in 1..=last {
        buf.clear();
        let mut any = false;
        loop {
            let (done, used) = match r.fill_buf() {
                Ok([]) | Err(_) => break,
                Ok(b) => {
                    any = true;
                    let (upto, done) = b.iter().position(|&c| c == b'\n').map_or((b.len(), false), |i| (i + 1, true));
                    buf.extend_from_slice(&b[..upto.min(keep.saturating_sub(buf.len()))]);
                    (done, upto)
                }
            };
            r.consume(used);
            if done {
                break;
            }
        }
        if !any {
            break;
        }
        while ri < ranges.len() && ranges[ri].1 < n {
            ri += 1;
        }
        if ri < ranges.len() && ranges[ri].0 <= n {
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim_end_matches(['\n', '\r']);
            out.insert(n, line.chars().take(max_chars + 1).collect());
        }
    }
    out
}

/// What a search left out on purpose, so "no matches" is never mistaken for "searched everywhere".
fn skip_note(skipped: &[String]) -> String {
    format!("[qx] not searched: {} (all=true includes them)\n", skipped.join(", "))
}

// ------------------------------------------------------------------- walking

/// Visit every file below `root` - the built-in skip list (`node_modules`, `target`, `.git`)
/// does not apply - until `visit` returns false or the walk cap is hit. Symlinks are not
/// followed: neither is `grep -r`. Returns whether every file was visited: false when `visit`
/// stopped it, a cap or the depth limit cut it, or a directory or entry could not be read.
pub(crate) fn walk_every_file<F: FnMut(&Path) -> bool>(root: &Path, mut visit: F) -> bool {
    // (keep going, nothing skipped so far)
    fn go<F: FnMut(&Path) -> bool>(dir: &Path, depth: usize, count: &mut usize, visit: &mut F, whole: &mut bool) -> bool {
        if depth > MAX_WALK_DEPTH {
            *whole = false;
            return true;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            *whole = false;
            return true;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                *whole = false;
                continue;
            };
            let Ok(ft) = entry.file_type() else {
                *whole = false;
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if !go(&entry.path(), depth + 1, count, visit, whole) {
                    return false;
                }
            } else if ft.is_file() {
                *count += 1;
                if *count > MAX_WALK_FILES || !visit(&entry.path()) {
                    *whole = false;
                    return false;
                }
            }
        }
        true
    }
    let (mut count, mut whole) = (0usize, true);
    go(root, 0, &mut count, &mut visit, &mut whole);
    whole
}

/// What a walk knows about a file it visits: the directory entry of a live walk, or the mtime the
/// index holds.
pub(crate) enum FileMeta<'a> {
    Live(&'a fs::DirEntry),
    Cached(SystemTime),
}

impl FileMeta<'_> {
    /// The mtime when it came from the index (the one a cached copy of the content is keyed by).
    pub(crate) fn indexed_mtime(&self) -> Option<SystemTime> {
        match self {
            FileMeta::Live(_) => None,
            FileMeta::Cached(t) => Some(*t),
        }
    }

    pub(crate) fn modified(&self) -> SystemTime {
        match self {
            FileMeta::Live(e) => e.metadata().and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH),
            FileMeta::Cached(t) => *t,
        }
    }
}

pub(crate) fn skip_dir_name(name: &str) -> bool {
    skip_dir(name, false)
}

pub(crate) fn vcs_dir_name(name: &str) -> bool {
    is_vcs_dir(name)
}

pub(crate) fn walk_files<S, I, F>(root: &Path, all: bool, init: I, visit: F) -> (Vec<S>, Vec<String>)
where
    S: Send,
    I: Fn() -> S + Sync,
    F: Fn(&mut S, &Path, &FileMeta) + Sync,
{
    walk_hinted(root, all, &[], init, visit)
}

/// `walk_files` that may start from the workspace index. `hints` are directories below `root`
/// (`/`-separated) that hold every file the caller can match; an empty list means all of `root`.
pub(crate) fn walk_hinted<S, I, F>(root: &Path, all: bool, hints: &[String], init: I, visit: F) -> (Vec<S>, Vec<String>)
where
    S: Send,
    I: Fn() -> S + Sync,
    F: Fn(&mut S, &Path, &FileMeta) + Sync,
{
    walk_bounded(root, all, hints, None, init, visit)
}

/// `walk_hinted` that also knows how deep a match can lie: `max_depth` is the number of directory
/// levels below `root` whose files can match (`src/*.rs` -> 1). Without an index the live walk then
/// starts in the hinted directories and never descends past that depth, instead of reading the tree.
pub(crate) fn walk_bounded<S, I, F>(
    root: &Path,
    all: bool,
    hints: &[String],
    max_depth: Option<usize>,
    init: I,
    visit: F,
) -> (Vec<S>, Vec<String>)
where
    S: Send,
    I: Fn() -> S + Sync,
    F: Fn(&mut S, &Path, &FileMeta) + Sync,
{
    if !all {
        if let Some((snap, prefix)) = crate::index::snapshot(root) {
            return walk_snapshot(&snap, &prefix, hints, init, visit);
        }
    }
    // A hint that steps out of the root (`..`) or is not a plain directory name falls back to the root.
    let usable = !hints.is_empty()
        && hints.iter().all(|h| !h.is_empty() && h.split('/').all(|s| !s.is_empty() && s != "." && s != ".." && !s.contains(':')));
    let starts: Vec<(PathBuf, usize)> = if usable {
        hints.iter().map(|h| (root.join(h), h.split('/').count())).collect()
    } else {
        vec![(root.to_path_buf(), 0)]
    };
    let (parts, skipped, _) = walk_from(root, all, starts, max_depth.unwrap_or(MAX_WALK_DEPTH).min(MAX_WALK_DEPTH), init, visit);
    (parts, skipped)
}

fn walk_snapshot<S, I, F>(snap: &crate::index::Snapshot, prefix: &str, hints: &[String], init: I, visit: F) -> (Vec<S>, Vec<String>)
where
    S: Send,
    I: Fn() -> S + Sync,
    F: Fn(&mut S, &Path, &FileMeta) + Sync,
{
    let join = |h: &str| if prefix.is_empty() { h.to_string() } else if h.is_empty() { prefix.to_string() } else { format!("{prefix}/{h}") };
    let mut ranges: Vec<&[crate::index::Entry]> = Vec::new();
    if hints.is_empty() {
        ranges.push(snap.range(prefix));
    } else {
        for h in hints {
            ranges.push(snap.range(&join(h)));
        }
    }
    let total: usize = ranges.iter().map(|r| r.len()).sum();
    let all: Vec<&crate::index::Entry> = ranges.into_iter().flatten().collect();
    let skipped: Vec<String> = snap.skipped.iter().cloned().collect();
    const CHUNK: usize = 1024;
    let threads = fs_threads().min(total / CHUNK + 1);
    let next = AtomicUsize::new(0);
    let run = || {
        let mut part = init();
        loop {
            let start = next.fetch_add(CHUNK, Ordering::Relaxed);
            if start >= all.len() {
                break;
            }
            for e in &all[start..(start + CHUNK).min(all.len())] {
                visit(&mut part, e.path.as_path(), &FileMeta::Cached(e.modified));
            }
        }
        part
    };
    let parts = if threads <= 1 {
        vec![run()]
    } else {
        std::thread::scope(|scope| {
            let handles: Vec<_> = (1..threads).map(|_| scope.spawn(run)).collect();
            let mut parts = vec![run()];
            for h in handles {
                match h.join() {
                    Ok(part) => parts.push(part),
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            }
            parts
        })
    };
    (parts, skipped)
}

/// The directory every file matching `pattern` lies under, without a wildcard in it.
fn literal_dir(pattern: &str) -> String {
    let segs: Vec<&str> = pattern.split('/').collect();
    let mut keep = Vec::new();
    for (i, s) in segs.iter().enumerate() {
        if i + 1 == segs.len() || s.contains(['*', '?', '[', '{', '\\']) {
            break;
        }
        keep.push(*s);
    }
    keep.join("/")
}

/// Visit every file below `root`, skipping the generated directories of `skip_dir`, on up to
/// `fs_threads()` threads. Each thread gets its own `init()` state and `visit` sees only that, so
/// no locking sits on the hot path; the states come back in no particular order. Symlinks are not
/// followed, and the walk stops visiting at `MAX_WALK_FILES` files (which ones is then up to
/// timing). The flag is false when a folder or entry could not be read for a reason other than
/// being gone or forbidden (`walk_lost`), so some files may be missing.
pub(crate) fn walk_live<S, I, F>(root: &Path, all: bool, init: I, visit: F) -> (Vec<S>, Vec<String>, bool)
where
    S: Send,
    I: Fn() -> S + Sync,
    F: Fn(&mut S, &Path, &FileMeta) + Sync,
{
    walk_from(root, all, vec![(root.to_path_buf(), 0)], MAX_WALK_DEPTH, init, visit)
}

/// A read error that may have hidden files a later read would see. A folder that is gone or that
/// this user may not list is left out by every walk alike, so it costs nothing to skip.
pub(crate) fn walk_lost(e: &std::io::Error) -> bool {
    !matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied)
}

fn walk_from<S, I, F>(root: &Path, all: bool, starts: Vec<(PathBuf, usize)>, limit: usize, init: I, visit: F) -> (Vec<S>, Vec<String>, bool)
where
    S: Send,
    I: Fn() -> S + Sync,
    F: Fn(&mut S, &Path, &FileMeta) + Sync,
{
    let starts: Vec<(PathBuf, usize)> = starts.into_iter().filter(|(p, d)| *d <= limit && p.is_dir()).collect();
    if starts.is_empty() {
        return (vec![init()], Vec::new(), true);
    }
    let walk = Walk {
        root,
        all,
        limit,
        ignored: if all { IgnoredDirs::default() } else { git_ignored_dirs(root) },
        skipped: Mutex::new(std::collections::BTreeSet::new()),
        state: Mutex::new(WalkState {
            pending: starts.len(),
            dirs: starts,
        }),
        wake: Condvar::new(),
        files: AtomicUsize::new(0),
        lost: AtomicBool::new(false),
    };
    let run = || {
        let mut part = init();
        walk.work(&mut part, &visit);
        part
    };
    let helpers = fs_threads() - 1;
    let parts = if helpers == 0 {
        vec![run()]
    } else {
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..helpers).map(|_| scope.spawn(run)).collect();
            let mut parts = vec![run()];
            for h in handles {
                match h.join() {
                    Ok(part) => parts.push(part),
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            }
            parts
        })
    };
    let skipped = walk.skipped.into_inner().unwrap_or_else(|e| e.into_inner()).into_iter().collect();
    (parts, skipped, !walk.lost.into_inner())
}

struct Walk<'a> {
    root: &'a Path,
    /// Also enter the generated directories of `skip_dir` (never VCS metadata).
    all: bool,
    /// Deepest directory level whose files are visited (the root is level 0).
    limit: usize,
    /// Folders below `root` that git ignores (`git_ignored_dirs`); left out like the generated ones.
    ignored: IgnoredDirs,
    /// Generated directories left out, so a reply can say what it did not search.
    skipped: Mutex<std::collections::BTreeSet<String>>,
    state: Mutex<WalkState>,
    wake: Condvar,
    files: AtomicUsize,
    /// Set when a read failed in a way `walk_lost` says may have hidden files.
    lost: AtomicBool,
}

struct WalkState {
    dirs: Vec<(PathBuf, usize)>,
    /// Directories queued or being read; the walk is over when it reaches zero.
    pending: usize,
}

/// Marks one directory as read, even when `visit` panics, so no thread waits on it forever.
struct DirDone<'a>(&'a Walk<'a>);

impl Drop for DirDone<'_> {
    fn drop(&mut self) {
        let mut st = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        st.pending -= 1;
        if st.pending == 0 {
            self.0.wake.notify_all();
        }
    }
}

impl Walk<'_> {
    fn work<S, F: Fn(&mut S, &Path, &FileMeta)>(&self, part: &mut S, visit: &F) {
        loop {
            let (dir, depth) = {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(next) = st.dirs.pop() {
                        break next;
                    }
                    if st.pending == 0 {
                        return;
                    }
                    st = self.wake.wait(st).unwrap_or_else(|e| e.into_inner());
                }
            };
            let _done = DirDone(self);
            self.read(&dir, depth, part, visit);
        }
    }

    fn read<S, F: Fn(&mut S, &Path, &FileMeta)>(&self, dir: &Path, depth: usize, part: &mut S, visit: &F) {
        if depth > self.limit || self.files.load(Ordering::Relaxed) >= MAX_WALK_FILES {
            return;
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                if walk_lost(&e) {
                    self.lost.store(true, Ordering::Relaxed);
                }
                return;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                self.lost.store(true, Ordering::Relaxed);
                continue;
            };
            let Ok(ft) = entry.file_type() else {
                self.lost.store(true, Ordering::Relaxed);
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if skip_dir(&name, self.all) {
                    if !is_vcs_dir(&name) {
                        self.skipped.lock().unwrap_or_else(|e| e.into_inner()).insert(name);
                    }
                    continue;
                }
                if !self.ignored.is_empty() {
                    let path = entry.path();
                    let rel = path.strip_prefix(self.root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                    if self.ignored.contains(&rel) {
                        self.skipped.lock().unwrap_or_else(|e| e.into_inner()).insert(rel);
                        continue;
                    }
                }
                if depth >= self.limit {
                    continue;
                }
                if let Some(child) = child_dir(&entry, self.root) {
                    let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    st.dirs.push((child, depth + 1));
                    st.pending += 1;
                    self.wake.notify_one();
                }
            } else if ft.is_file() {
                if self.files.fetch_add(1, Ordering::Relaxed) >= MAX_WALK_FILES {
                    return;
                }
                visit(part, &entry.path(), &FileMeta::Live(&entry));
            }
        }
    }
}

/// Resolve a traversed child directory only when it can leave the root. On
/// Windows a normal directory is already canonical because its parent was;
/// junctions/reparse points are the only case that needs `canonicalize`.
fn child_dir(entry: &fs::DirEntry, root: &Path) -> Option<PathBuf> {
    let path = entry.path();
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        let attrs = entry.metadata().ok()?.file_attributes();
        if attrs & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            return Some(path);
        }
    }
    let canonical = fs::canonicalize(&path).ok()?;
    canonical.starts_with(root).then_some(canonical)
}

fn is_vcs_dir(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), ".git" | ".hg" | ".svn")
}

/// Whether a path or glob has a segment that is one of the skipped generated directories.
fn names_skipped_dir(s: &str) -> bool {
    s.split(['/', '\\']).any(|seg| !is_vcs_dir(seg) && skip_dir(seg, false))
}

fn skip_dir(name: &str, all: bool) -> bool {
    if is_vcs_dir(name) {
        return true;
    }
    if all {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | ".nuxt"
            | ".output"
            | "vendor"
            | "__pycache__"
            | ".venv"
            | "venv"
            | "coverage"
            | ".cache"
            | ".turbo"
    )
}

type IgnoredDirs = std::sync::Arc<std::collections::HashSet<String>>;

static IGNORED_CACHE: Mutex<Vec<(PathBuf, Instant, IgnoredDirs)>> = Mutex::new(Vec::new());

/// Forget what git said about ignored folders: an ignore rule changed, so the next walk asks again.
pub(crate) fn drop_ignored_cache() {
    if let Ok(mut cache) = IGNORED_CACHE.lock() {
        cache.clear();
    }
}

/// Whether git ignores the existing folder `rel` below `root`. `None` when git is missing, `root` is no
/// repository, or git takes over three seconds: the caller then has no answer to rely on.
pub(crate) fn git_ignores_dir(root: &Path, rel: &str) -> Option<bool> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.fsmonitor=false", "-C"])
        .arg(root)
        .args(["check-ignore", "-q", "--"])
        .arg(rel)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().ok()?;
    let end = Instant::now() + std::time::Duration::from_secs(3);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < end => std::thread::sleep(std::time::Duration::from_millis(2)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    match status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// Folders below `root` that git ignores (`scratch`, `.worktrees`, `a/out`: `/`-separated, relative to
/// `root`), so a search does not read old copies and build output that `skip_dir` has no name for.
/// Git is asked once per root per half minute. Empty when git is missing, `root` is no repository,
/// `root` is itself ignored (it was asked for by name), or git takes over three seconds.
pub(crate) fn git_ignored_dirs(root: &Path) -> IgnoredDirs {
    const FRESH: std::time::Duration = std::time::Duration::from_secs(30);
    if let Ok(cache) = IGNORED_CACHE.lock() {
        if let Some((_, _, hit)) = cache.iter().find(|(p, at, _)| p == root && at.elapsed() < FRESH) {
            return hit.clone();
        }
    }
    let found: IgnoredDirs = std::sync::Arc::new(ask_git_for_ignored_dirs(root));
    if let Ok(mut cache) = IGNORED_CACHE.lock() {
        cache.retain(|(p, at, _)| p != root && at.elapsed() < FRESH);
        if cache.len() >= 8 {
            cache.remove(0);
        }
        cache.push((root.to_path_buf(), Instant::now(), found.clone()));
    }
    found
}

fn ask_git_for_ignored_dirs(root: &Path) -> std::collections::HashSet<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let none = std::collections::HashSet::new();
    let mut cmd = Command::new("git");
    // `core.fsmonitor` is a command the searched repo names in its own config: not one to run for a search.
    cmd.args(["-c", "core.fsmonitor=false", "-C"])
        .arg(root)
        .args(["ls-files", "-z", "-o", "-i", "--exclude-standard", "--directory"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let Ok(mut child) = cmd.spawn() else {
        return none;
    };
    let Some(mut stdout) = child.stdout.take() else {
        return none;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        let _ = tx.send(bytes);
    });
    let Ok(bytes) = rx.recv_timeout(std::time::Duration::from_secs(3)) else {
        let _ = child.kill();
        let _ = child.wait();
        return none;
    };
    if !child.wait().is_ok_and(|s| s.success()) {
        return none;
    }
    let mut dirs = std::collections::HashSet::new();
    for entry in bytes.split(|b| *b == 0) {
        let entry = String::from_utf8_lossy(entry);
        // Only a folder git ignores whole is listed with a trailing slash; `./` is `root` itself.
        match entry.strip_suffix('/') {
            Some(".") => return none,
            Some(dir) if !dir.is_empty() => {
                dirs.insert(dir.to_string());
            }
            _ => {}
        }
    }
    dirs
}

/// Whether the folder part of a pattern lies in or under a folder git ignores: it is asked for by name.
fn names_ignored_dir(root: &Path, pattern: &str) -> bool {
    let dir = literal_dir(pattern);
    if dir.is_empty() {
        return false;
    }
    let ignored = git_ignored_dirs(root);
    if ignored.is_empty() {
        return false;
    }
    let mut prefix = String::new();
    for seg in dir.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(seg);
        if ignored.contains(&prefix) {
            return true;
        }
    }
    false
}

fn fs_threads() -> usize {
    static THREADS: OnceLock<usize> = OnceLock::new();
    *THREADS.get_or_init(|| {
        std::env::var("QX_FS_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or_else(|| {
                let cpus = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4);
                (cpus / 2).clamp(1, 8)
            })
            .clamp(1, 32)
    })
}

// --------------------------------------------------------------------- glob

/// A glob is matched against paths relative to the searched folder: `./src/**` and `<folder>/src/**`
/// mean `src/**`, and would otherwise match nothing.
fn relative_pattern(pattern: &str, base: &Path) -> String {
    let mut p = pattern.replace('\\', "/");
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.to_string();
    }
    let b = base.to_string_lossy().replace('\\', "/");
    let b = b.strip_prefix("//?/").unwrap_or(&b).trim_end_matches('/');
    if p.len() > b.len() && p.is_char_boundary(b.len()) && p[..b.len()].eq_ignore_ascii_case(b) && p.as_bytes()[b.len()] == b'/' {
        return p[b.len() + 1..].to_string();
    }
    p
}

/// A pattern that is still absolute after `relative_pattern` points outside the searched folder and can never match.
fn outside_note(patterns: &[String], root: &Path) -> String {
    let absolute = |p: &str| p.starts_with('/') || matches!(p.as_bytes(), [d, b':', b'/', ..] if d.is_ascii_alphabetic());
    if patterns.iter().any(|p| absolute(p)) {
        format!("[qx] an absolute pattern lies outside the searched folder {}; give that folder as `path` and keep the pattern relative.\n", root.display())
    } else {
        String::new()
    }
}

fn expand_braces(pattern: &str) -> Vec<String> {
    let mut out = Vec::new();
    expand_braces_into(pattern, 0, &mut out);
    if out.is_empty() {
        out.push(pattern.to_string());
    }
    out
}

fn expand_braces_into(pattern: &str, depth: usize, out: &mut Vec<String>) {
    if depth > 8 || out.len() >= 32 {
        out.push(pattern.to_string());
        return;
    }
    let bytes = pattern.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        let mut level = 0i32;
        let mut j = i;
        while j < bytes.len() {
            match bytes[j] {
                b'{' => level += 1,
                b'}' => {
                    level -= 1;
                    if level == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let inner = &pattern[i + 1..j];
        let parts = split_top_level(inner);
        if parts.len() > 1 {
            for part in parts {
                let mut next = String::with_capacity(pattern.len());
                next.push_str(&pattern[..i]);
                next.push_str(part);
                next.push_str(&pattern[j + 1..]);
                expand_braces_into(&next, depth + 1, out);
                if out.len() >= 32 {
                    return;
                }
            }
            return;
        }
        i = j + 1;
    }
    out.push(pattern.to_string());
}

fn split_top_level(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut level = 0i32;
    let mut start = 0usize;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'{' => level += 1,
            b'}' => level -= 1,
            b',' if level == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// One glob pattern, analysed once per call: matching a path then reuses the caller's scratch and
/// allocates nothing.
struct Glob {
    /// A pattern with a `/` is matched against the relative path, one without against the name.
    has_slash: bool,
    ignore_case: bool,
    fast: Option<FastGlob>,
    /// The pattern's chars, folded the way the text will be.
    chars: Vec<char>,
}

/// The shapes that cover nearly every real pattern: `LIT`, `*LIT`, `**/LIT` and `**/*LIT`, where LIT
/// has no glob character. `**/LIT` needs LIT to start a path segment, while the `*` of `**/*LIT` may
/// start inside a name; `*` cannot cross '/'. Matched by a byte compare.
struct FastGlob {
    lit: String,
    anywhere: bool,
    star: bool,
    segment: bool,
}

/// Per-thread buffers for the general matcher: the folded text and its memo table.
#[derive(Default)]
struct GlobScratch {
    text: Vec<char>,
    memo: Vec<u8>,
}

/// Char by char, so `Ä` and `ä` meet the way Windows names do; each char folds alike wherever it sits.
fn fold_into(s: &str, ignore_case: bool, out: &mut Vec<char>) {
    out.clear();
    if ignore_case && s.is_ascii() {
        out.extend(s.bytes().map(|b| b.to_ascii_lowercase() as char));
    } else if ignore_case {
        out.extend(s.chars().flat_map(char::to_lowercase));
    } else {
        out.extend(s.chars());
    }
}

impl Glob {
    fn new(pattern: &str, ignore_case: bool) -> Self {
        let mut chars = Vec::new();
        fold_into(pattern, ignore_case, &mut chars);
        Self { has_slash: pattern.contains('/'), ignore_case, fast: FastGlob::new(pattern, ignore_case), chars }
    }

    fn matches(&self, text: &str, scratch: &mut GlobScratch) -> bool {
        match &self.fast {
            Some(f) => f.matches(text, self.ignore_case),
            None => self.general(text, scratch),
        }
    }

    fn general(&self, text: &str, scratch: &mut GlobScratch) -> bool {
        fold_into(text, self.ignore_case, &mut scratch.text);
        scratch.memo.clear();
        scratch.memo.resize((self.chars.len() + 1) * (scratch.text.len() + 1), 0);
        glob_go(&self.chars, 0, &scratch.text, 0, &mut scratch.memo)
    }
}

impl FastGlob {
    fn new(pattern: &str, ignore_case: bool) -> Option<Self> {
        let plain = |s: &str| !s.contains(['*', '?', '[', '\\']);
        // (literal, `**/` prefix, `*` prefix, literal must start a path segment)
        let (lit, anywhere, star, segment) = if let Some(rest) = pattern.strip_prefix("**/") {
            match rest.strip_prefix('*') {
                Some(lit) => (lit, true, false, false),
                None => (rest, true, false, true),
            }
        } else if let Some(lit) = pattern.strip_prefix('*') {
            (lit, false, true, false)
        } else {
            (pattern, false, false, false)
        };
        // The byte compare folds ASCII only.
        if !plain(lit) || (ignore_case && !lit.is_ascii()) {
            return None;
        }
        Some(Self { lit: lit.to_string(), anywhere, star, segment })
    }

    fn matches(&self, text: &str, ignore_case: bool) -> bool {
        let (t, l) = (text.as_bytes(), self.lit.as_bytes());
        let eq = |a: &[u8]| if ignore_case { a.eq_ignore_ascii_case(l) } else { a == l };
        if self.anywhere {
            t.len() >= l.len()
                && eq(&t[t.len() - l.len()..])
                && (!self.segment || t.len() == l.len() || t[t.len() - l.len() - 1] == b'/')
        } else if self.star {
            t.len() >= l.len() && eq(&t[t.len() - l.len()..]) && !t[..t.len() - l.len()].contains(&b'/')
        } else {
            t.len() == l.len() && eq(t)
        }
    }
}

fn glob_go(
    p: &[char],
    i: usize,
    t: &[char],
    k: usize,
    memo: &mut [u8],
) -> bool {
    let slot = i * (t.len() + 1) + k; // 0 = unknown, 1 = no match, 2 = match
    if memo[slot] != 0 {
        return memo[slot] == 2;
    }
    let result = if i == p.len() {
        k == t.len()
    } else {
        match p[i] {
            '*' => {
                if i + 1 < p.len() && p[i + 1] == '*' {
                    let j = i + 2;
                    if j < p.len() && p[j] == '/' {
                        // `**/` swallows whole directories: what follows starts a path segment.
                        let at_segment = k == 0 || t[k - 1] == '/';
                        (at_segment && glob_go(p, j + 1, t, k, memo))
                            || (k < t.len() && glob_go(p, i, t, k + 1, memo))
                    } else {
                        glob_go(p, j, t, k, memo) || (k < t.len() && glob_go(p, i, t, k + 1, memo))
                    }
                } else {
                    glob_go(p, i + 1, t, k, memo)
                        || (k < t.len() && t[k] != '/' && glob_go(p, i, t, k + 1, memo))
                }
            }
            '?' => k < t.len() && t[k] != '/' && glob_go(p, i + 1, t, k + 1, memo),
            '[' => {
                if let Some((matched, next)) = class_match(p, i, t.get(k).copied()) {
                    // `[!a]` "matches" the end of the text; stepping past it would index outside the memo.
                    matched && k < t.len() && glob_go(p, next, t, k + 1, memo)
                } else {
                    k < t.len() && t[k] == '[' && glob_go(p, i + 1, t, k + 1, memo)
                }
            }
            '\\' if i + 1 < p.len() => {
                k < t.len() && t[k] == p[i + 1] && glob_go(p, i + 2, t, k + 1, memo)
            }
            c => k < t.len() && t[k] == c && glob_go(p, i + 1, t, k + 1, memo),
        }
    };
    memo[slot] = if result { 2 } else { 1 };
    result
}

fn class_match(p: &[char], start: usize, c: Option<char>) -> Option<(bool, usize)> {
    let mut j = start + 1;
    let negate = j < p.len() && (p[j] == '!' || p[j] == '^');
    if negate {
        j += 1;
    }
    let mut matched = false;
    let mut first = true;
    while j < p.len() && (p[j] != ']' || first) {
        first = false;
        let (lo, next) = if p[j] == '\\' && j + 1 < p.len() {
            (p[j + 1], j + 2)
        } else {
            (p[j], j + 1)
        };
        if next < p.len() && p[next] == '-' && next + 1 < p.len() && p[next + 1] != ']' {
            let hi = p[next + 1];
            if let Some(ch) = c {
                if ch != '/' && ch >= lo && ch <= hi {
                    matched = true;
                }
            }
            j = next + 2;
        } else {
            if c == Some(lo) && lo != '/' {
                matched = true;
            }
            j = next;
        }
    }
    if j >= p.len() {
        return None;
    }
    Some((if negate { !matched } else { matched }, j + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glob_match(pattern: &str, text: &str, ignore_case: bool) -> bool {
        Glob::new(pattern, ignore_case).matches(text, &mut GlobScratch::default())
    }

    fn glob_general(pattern: &str, text: &str, ignore_case: bool) -> bool {
        Glob::new(pattern, ignore_case).general(text, &mut GlobScratch::default())
    }

    fn glob_fast(pattern: &str, text: &str, ignore_case: bool) -> Option<bool> {
        FastGlob::new(pattern, ignore_case).map(|f| f.matches(text, ignore_case))
    }

    #[cfg(windows)]
    #[test]
    fn grep_sees_a_rewrite_that_keeps_the_size_and_the_mtime() {
        let dir = tmp("grep-same-mtime");
        let f = dir.join("a.rs");
        fs::write(&f, "fn alpha() {}\n").unwrap();
        // Settled (older than SETTLE), so the first search caches the content under this mtime.
        let old = SystemTime::now() - std::time::Duration::from_secs(3600);
        let set = || fs::OpenOptions::new().write(true).open(&f).unwrap().set_modified(old).unwrap();
        set();
        crate::index::ensure(&dir);
        assert!(crate::index::snapshot(&fs::canonicalize(&dir).unwrap()).is_some(), "an index was built");
        let g = |pat: &str| invoke(&dir, "grep", json!({"pattern": pat})).text;
        assert!(g("alpha").contains("a.rs"), "{}", g("alpha"));
        fs::write(&f, "fn omega() {}\n").unwrap();
        set();
        assert!(g("alpha").contains("No matches"), "{}", g("alpha"));
        assert!(g("omega").contains("a.rs"), "{}", g("omega"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_walk_that_cannot_read_a_folder_says_files_may_be_missing() {
        let dir = tmp("walk-lost");
        fs::write(dir.join("f.txt"), "x").unwrap();
        let (_, _, complete) = walk_live(&dir, false, || (), |_, _, _| {});
        assert!(complete);
        fn nothing(_: &mut (), _: &Path, _: &FileMeta) {}
        let walk = Walk {
            root: &dir,
            all: true,
            limit: MAX_WALK_DEPTH,
            ignored: IgnoredDirs::default(),
            skipped: Mutex::new(Default::default()),
            state: Mutex::new(WalkState { dirs: Vec::new(), pending: 0 }),
            wake: Condvar::new(),
            files: AtomicUsize::new(0),
            lost: AtomicBool::new(false),
        };
        // A folder that is gone is left out by every walk alike.
        walk.read(&dir.join("gone"), 0, &mut (), &nothing);
        assert!(!walk.lost.load(Ordering::Relaxed));
        // Any other failure (here a file where a folder was expected) may hide files.
        walk.read(&dir.join("f.txt"), 0, &mut (), &nothing);
        assert!(walk.lost.load(Ordering::Relaxed));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_anchors_match_the_first_line_after_a_byte_order_mark() {
        let dir = tmp("grep-bom");
        fs::write(dir.join("a.rs"), "\u{feff}fn main() {}\nlet x = 1;\n").unwrap();
        let r = invoke(&dir, "grep", json!({"pattern": "^fn"}));
        assert!(r.text.contains("a.rs"), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_ignore_case_folds_non_ascii_letters() {
        assert!(glob_match("\u{e4}.txt", "\u{c4}.TXT", true));
        assert!(glob_match("**/*\u{e4}.txt", "dir/X\u{c4}.TXT", true));
        assert!(glob_match("*.txt", "\u{c4}.TXT", true));
        assert!(!glob_match("\u{e4}.txt", "\u{c4}.TXT", false));
    }

    #[test]
    fn grep_honours_context_arguments_and_caps_them() {
        let dir = tmp("grep-ctx");
        let body: String = (1..=20).map(|i| if i == 5 || i == 7 || i == 18 { format!("hit {i}\n") } else { format!("l{i}\n") }).collect();
        fs::write(dir.join("a.txt"), body).unwrap();
        let g = |extra: Value| {
            let mut a = json!({"pattern": "hit"});
            a.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            invoke(&dir, "grep", a).text
        };
        // Overlapping context merges; a gap gets `--`; matches keep `:`, context takes `-`.
        let t = g(json!({"-C": 1}));
        assert!(t.contains("4- l4\n5: hit 5\n6- l6\n7: hit 7\n8- l8\n--\n17- l17\n18: hit 18\n19- l19"), "{t}");
        // After only, and `context` spelled out; never more than the cap.
        let t = g(json!({"-A": 1}));
        assert!(t.contains("5: hit 5\n6- l6\n7: hit 7\n8- l8\n--\n18: hit 18\n19- l19") && !t.contains("4- l4"), "{t}");
        assert_eq!(g(json!({"context": 99})), g(json!({"context": MAX_GREP_CONTEXT})));
        assert!(g(json!({"context": 99})).contains("12- l12\n13- l13"));
        // No context asked: exactly the old shape.
        assert!(!g(json!({})).contains("- l"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_ignore_case_reaches_the_fast_path_for_regex_and_literal() {
        let dir = tmp("grep-ci");
        fs::write(dir.join("a.txt"), "Hello world\nhello world\nHELLO WORLD\nnothing\n").unwrap();
        for args in [
            json!({"pattern":"hello","ignore_case":true}),
            json!({"pattern":"hel+o","ignore_case":true}),
            json!({"pattern":"hello","ignore_case":true,"literal":true}),
        ] {
            let r = invoke(&dir, "grep", args.clone());
            assert!(r.text.contains("Found 3 matches"), "{args}: {}", r.text);
        }
        let r = invoke(&dir, "grep", json!({"pattern":"hello"}));
        assert!(r.text.contains("Found 1 match"), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_ignore_case_folds_non_ascii_letters_like_a_regex_does() {
        let dir = tmp("grep-ci-unicode");
        fs::write(dir.join("a.txt"), "caf\u{e9}\nCAF\u{c9}\nplain\n").unwrap();
        for args in [
            json!({"pattern":"caf\u{e9}","ignore_case":true,"literal":true}),
            json!({"pattern":"caf\u{e9}","ignore_case":true}),
        ] {
            let r = invoke(&dir, "grep", args.clone());
            assert!(r.text.contains("Found 2 matches"), "{args}: {}", r.text);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_reports_no_phantom_empty_line_at_the_end_of_a_file() {
        let dir = tmp("grep-phantom");
        fs::write(dir.join("two.txt"), "a\nb\n").unwrap();
        fs::write(dir.join("gap.txt"), "a\n\nb\n").unwrap();
        // Fast path (anchors only) and the per-line path (`\r` keeps it off the fast path).
        for pat in ["^$", "^$|\\r"] {
            let r = invoke(&dir, "grep", json!({"pattern": pat}));
            assert!(r.text.contains("Found 1 match in 1 file") && r.text.contains("2: "), "{pat}: {}", r.text);
            assert!(!r.text.contains("two.txt"), "{pat}: {}", r.text);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_shows_the_part_of_a_long_line_that_matched() {
        let dir = tmp("grep-window");
        let line = format!("{}NEEDLE{}\n", "x".repeat(3000), "y".repeat(3000));
        fs::write(dir.join("min.js"), line).unwrap();
        for ci in [false, true] {
            let r = invoke(&dir, "grep", json!({"pattern": "needle", "ignore_case": true, "literal": ci}));
            assert!(r.text.contains("NEEDLE") && r.text.contains("truncated"), "{}", r.text);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn streamed_grep_does_not_hold_a_huge_single_line() {
        let dir = tmp("grep-longline");
        let path = dir.join("blob.txt");
        // One 20 MB line (more than the 16 MB held), then two normal lines.
        let mut text = format!("NEEDLE first{}\n", "z".repeat(20 << 20));
        text.push_str("plain\nNEEDLE last\n");
        fs::write(&path, text).unwrap();
        let matcher = regex::Regex::new("NEEDLE").unwrap();
        let fast = fast_regex("NEEDLE", true, false).unwrap();
        let mut sf = ScanFile { path: path.clone(), matches: Vec::new(), total: 0, binary: false, large: false, error: false };
        scan_stream(&path, &matcher, Some(&fast), 200, 10, &mut sf);
        let lines: Vec<usize> = sf.matches.iter().map(|m| m.line).collect();
        assert_eq!(lines, [1, 3], "{:?}", sf.matches.iter().map(|m| (m.line, m.text.len())).collect::<Vec<_>>());
        assert!(sf.matches[0].text.len() < 400);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_line_by_line_path_does_not_hold_a_huge_single_line_either() {
        let dir = tmp("grep-longline-slow");
        let path = dir.join("blob.txt");
        let mut text = format!("NEEDLE first{}\n", "z".repeat(20 << 20));
        text.push_str("plain\nNEEDLE last\n");
        fs::write(&path, text).unwrap();
        let matcher = regex::Regex::new("NEEDLE").unwrap();
        let mut sf = ScanFile { path: path.clone(), matches: Vec::new(), total: 0, binary: false, large: false, error: false };
        scan_stream(&path, &matcher, None, 200, 10, &mut sf);
        let lines: Vec<usize> = sf.matches.iter().map(|m| m.line).collect();
        assert_eq!(lines, [1, 3], "{:?}", sf.matches.iter().map(|m| (m.line, m.text.len())).collect::<Vec<_>>());
        assert!(!sf.error && sf.matches[0].text.len() < 400);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absolute_pattern_outside_the_folder_says_why_it_found_nothing() {
        let dir = tmp("glob-outside");
        let other = tmp("glob-outside-other");
        fs::write(other.join("x.rs"), "fn x() {}\n").unwrap();
        let abs = format!("{}/*.rs", other.display().to_string().replace('\\', "/"));
        let r = invoke(&dir, "glob", json!({"pattern": abs}));
        assert!(r.text.contains("No files found") && r.text.contains("absolute pattern"), "{}", r.text);
        let r = invoke(&dir, "grep", json!({"pattern": "fn", "include": abs}));
        assert!(r.text.contains("No matches found") && r.text.contains("absolute pattern"), "{}", r.text);
        let r = invoke(&dir, "glob", json!({"pattern": "*.nothing"}));
        assert!(r.text.contains("No files found") && !r.text.contains("absolute"), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&other);
    }

    #[test]
    fn glob_and_include_accept_a_dot_slash_or_absolute_prefix() {
        let dir = tmp("glob-prefix");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src").join("a.rs"), "fn main() {}\n").unwrap();
        let r = invoke(&dir, "glob", json!({"pattern": "./src/*.rs"}));
        assert!(r.text.contains("a.rs"), "{}", r.text);
        let r = invoke(&dir, "grep", json!({"pattern": "main", "include": "./src/*.rs"}));
        assert!(r.text.contains("Found 1 match"), "{}", r.text);
        let base = Path::new("C:/proj");
        assert_eq!(relative_pattern("C:\\proj\\src\\**\\*.rs", base), "src/**/*.rs");
        assert_eq!(relative_pattern("c:/proj/src/*.rs", base), "src/*.rs");
        assert_eq!(relative_pattern("C:/projx/src/*.rs", base), "C:/projx/src/*.rs");
        assert_eq!(relative_pattern("././a/*.rs", base), "a/*.rs");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_explains_patterns_that_cannot_match_literally() {
        let dir = tmp("grep-literal");
        fs::write(dir.join("a.txt"), "TODO: x\na\nb\n").unwrap();
        let r = invoke(&dir, "grep", json!({"pattern":"TODO|FIXME"}));
        assert!(!r.is_error && r.text.contains("Found 1 match") && r.text.contains("1: TODO: x"), "{}", r.text);
        // Not a valid regex: searched as text, and said so.
        let r = invoke(&dir, "grep", json!({"pattern":"TODO: x("}));
        assert!(r.text.contains("No matches found") && r.text.contains("not a valid regex"), "{}", r.text);
        let r = invoke(&dir, "grep", json!({"pattern":"a\nb"}));
        assert!(r.is_error, "{}", r.text);
        assert_eq!(r.structured["code"], "FS_BAD_ARG");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_include_takes_a_brace_glob() {
        let dir = tmp("grep-brace");
        for f in ["a.ts", "b.tsx", "c.rs"] {
            fs::write(dir.join(f), "needle\n").unwrap();
        }
        let r = invoke(&dir, "grep", json!({"pattern":"needle","include":"*.{ts,tsx}"}));
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("a.ts") && r.text.contains("b.tsx") && !r.text.contains("c.rs"), "{}", r.text);
        let r = invoke(&dir, "grep", json!({"pattern":"needle","include":"*.ts,*.tsx"}));
        assert_eq!(r.structured["code"], "FS_BAD_ARG", "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_is_recursive_skips_generated_dirs_and_supports_braces() {
        let dir = tmp("glob");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::create_dir_all(dir.join("node_modules")).unwrap();
        fs::write(dir.join("src/a.rs"), "").unwrap();
        fs::write(dir.join("src/b.txt"), "").unwrap();
        fs::write(dir.join("README.md"), "").unwrap();
        fs::write(dir.join("node_modules/x.rs"), "").unwrap();

        let r = invoke(&dir, "glob", json!({"pattern":"**/*.{rs,txt}"}));
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("src/a.rs"));
        assert!(r.text.contains("src/b.txt"));
        assert!(!r.text.contains("node_modules/x.rs"));
        assert!(!r.text.contains("not searched"), "a search with hits does not repeat the note: {}", r.text);
        let r = invoke(&dir, "glob", json!({"pattern":"**/*.nothing"}));
        assert!(r.text.contains("not searched: node_modules"), "an empty result says what was left out: {}", r.text);
        // Naming the directory, or asking for all, looks inside it.
        let r = invoke(&dir, "glob", json!({"pattern":"node_modules/**/*.rs"}));
        assert!(r.text.contains("node_modules/x.rs"), "{}", r.text);
        let r = invoke(&dir, "glob", json!({"pattern":"**/*.rs", "all": true}));
        assert!(r.text.contains("node_modules/x.rs"), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
    }

    fn git_init(dir: &Path) -> bool {
        std::process::Command::new("git").arg("init").arg("-q").arg(dir).output().is_ok_and(|o| o.status.success())
    }

    #[test]
    fn search_skips_folders_git_ignores_and_says_so() {
        let dir = tmp("git-ignored");
        if !git_init(&dir) {
            let _ = fs::remove_dir_all(&dir);
            return; // no git on this machine: nothing to check
        }
        fs::create_dir_all(dir.join("out/deep")).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join(".gitignore"), "out/\n").unwrap();
        fs::write(dir.join("out/deep/x.rs"), "needle\n").unwrap();
        fs::write(dir.join("src/a.rs"), "needle\n").unwrap();
        assert!(git_ignored_dirs(&dir).contains("out"), "{:?}", git_ignored_dirs(&dir));

        let r = invoke(&dir, "grep", json!({"pattern":"needle"}));
        assert!(r.text.contains("src/a.rs") && !r.text.contains("out/deep/x.rs"), "{}", r.text);
        assert!(!r.text.contains("not searched:"), "{}", r.text);
        let r = invoke(&dir, "grep", json!({"pattern":"absent-word"}));
        assert!(r.text.contains("not searched:") && r.text.contains("out"), "{}", r.text);
        let r = invoke(&dir, "glob", json!({"pattern":"**/*.rs"}));
        assert!(r.text.contains("src/a.rs") && !r.text.contains("out/deep/x.rs"), "{}", r.text);
        // Asking for everything, or naming the folder, still looks inside it.
        let r = invoke(&dir, "grep", json!({"pattern":"needle","all":true}));
        assert!(r.text.contains("out/deep/x.rs"), "{}", r.text);
        let r = invoke(&dir, "grep", json!({"pattern":"needle","include":"out/**/*.rs"}));
        assert!(r.text.contains("out/deep/x.rs"), "{}", r.text);
        let r = invoke(&dir, "glob", json!({"pattern":"out/**/*.rs"}));
        assert!(r.text.contains("out/deep/x.rs"), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_skip_note_counts_only_what_was_skipped() {
        let dir = tmp("skip-note");
        fs::write(dir.join("a.txt"), "needle\n").unwrap();
        fs::write(dir.join("b.bin"), b"needle\0\0\0").unwrap();
        let r = invoke(&dir, "grep", json!({"pattern":"needle"}));
        assert!(r.text.contains("[qx] skipped 1 binary file(s)."), "{}", r.text);
        assert!(!r.text.contains("0 large") && !r.text.contains("0 unreadable"), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_outside_a_git_repo_skips_nothing_extra() {
        let dir = tmp("no-git");
        fs::create_dir_all(dir.join("out")).unwrap();
        fs::write(dir.join(".gitignore"), "out/\n").unwrap();
        fs::write(dir.join("out/x.rs"), "needle\n").unwrap();
        let r = invoke(&dir, "grep", json!({"pattern":"needle"}));
        assert!(r.text.contains("out/x.rs"), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_and_grep_visit_every_file_of_a_wide_tree_once() {
        let dir = tmp("wide-tree");
        for a in 0..6 {
            for b in 0..6 {
                let leaf = dir.join(format!("a{a}/b{b}/c"));
                fs::create_dir_all(&leaf).unwrap();
                for f in 0..3 {
                    fs::write(leaf.join(format!("f{f}.txt")), "needle\n").unwrap();
                }
            }
        }
        let r = invoke(&dir, "glob", json!({"pattern":"**/*.txt","max_results":1000}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(r.structured["count"], 108);
        assert_eq!(r.structured["scanned"], 108);
        let listed: std::collections::HashSet<&str> = r.text.lines().collect();
        assert_eq!(listed.len(), 108, "{}", r.text);

        let r = invoke(&dir, "grep", json!({"pattern":"needle","max_matches":1000}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(r.structured["count"], 108);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_lists_the_truly_newest_when_matches_exceed_the_cap() {
        let dir = tmp("glob-newest");
        let base = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        for i in 0..40u64 {
            let p = dir.join(format!("f{i:02}.txt"));
            fs::write(&p, "").unwrap();
            // f33 is the newest and f21 the second newest, both far past any "first N in walk order" prefix
            let age = match i {
                33 => 1000,
                21 => 900,
                _ => i,
            };
            let f = OpenOptions::new().write(true).open(&p).unwrap();
            f.set_modified(base + std::time::Duration::from_secs(age)).unwrap();
        }
        let r = invoke(&dir, "glob", json!({"pattern":"*.txt","max_results":2}));
        assert!(!r.is_error, "{}", r.text);
        let lines: Vec<&str> = r.text.lines().collect();
        assert!(lines[0].ends_with("f33.txt") && lines[1].ends_with("f21.txt"), "{}", r.text);
        assert_eq!(r.structured["count"], 40);
        assert_eq!(r.structured["shown"], 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_is_literal_parallel_and_honors_include() {
        let dir = tmp("grep");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.rs"), "first\nneedle here\nlast\n").unwrap();
        fs::write(dir.join("src/b.txt"), "needle here too\n").unwrap();
        let r = invoke(&dir, "grep", json!({"pattern":"needle"}));
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("src/a.rs\n2: needle here"), "{}", r.text);
        assert!(r.text.contains("src/b.txt\n1: needle here too"), "{}", r.text);
        assert_eq!(r.structured["count"], 2);

        let r = invoke(&dir, "grep", json!({"pattern":"needle","include":"*.rs"}));
        assert!(r.text.contains("src/a.rs"));
        assert!(!r.text.contains("src/b.txt"));

        // The pattern is a regex; `literal` searches it as text.
        let r = invoke(&dir, "grep", json!({"pattern":"n.*e\\b"}));
        assert!(r.text.contains("src/a.rs"), "{}", r.text);
        let r = invoke(&dir, "grep", json!({"pattern":"n.*e", "literal": true}));
        assert!(r.text.contains("No matches found"), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_skips_binary_files() {
        let dir = tmp("binary");
        fs::write(dir.join("a.bin"), [0u8, 1, 2, 3]).unwrap();
        fs::write(dir.join("a.txt"), "needle\n").unwrap();
        let r = invoke(&dir, "grep", json!({"pattern":"needle"}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(r.structured["skipped_binary"], 1);
        assert!(r.text.contains("a.txt\n1: needle"), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn double_star_slash_starts_at_a_path_segment() {
        // `**/name` is "name in any directory", not "anything that ends in name".
        assert!(!glob_match("**/name", "xname", false));
        assert!(!glob_match("**/index.js", "a/reindex.js", false));
        assert!(!glob_match("src/**/mod.rs", "src/a/submod.rs", false));
        assert!(glob_match("**/name", "name", false) && glob_match("**/name", "a/b/name", false));
        assert!(glob_match("src/**/mod.rs", "src/mod.rs", false) && glob_match("src/**/mod.rs", "src/a/b/mod.rs", false));
        // a star after the slash may still start inside a name
        assert!(glob_match("**/*.rs", "xb.rs", false) && glob_match("**/*.rs", "a/xb.rs", false));
    }

    #[test]
    fn glob_fast_path_agrees_with_the_general_matcher() {
        let patterns = ["*.rs", "**/*.rs", "**/name", "**/*", "*", "Cargo.toml", "**/a/b", "*a/b", "x.RS", "**/"];
        let texts = [
            "a.rs", "src/deep/a.rs", "a/.rs", "x.rs/y.rs", "a/b.rs/c", "dir.rs/", "xname", "d/name", "Cargo.toml",
            "a/b", "xa/b", "x/a/b", "x.rs", "X.RS", "src/Z.Rs", "", "caf\u{e9}.rs", "\u{e9}", "n/\u{e9}.rs",
        ];
        for p in patterns {
            for t in texts {
                for ic in [false, true] {
                    assert_eq!(glob_match(p, t, ic), glob_general(p, t, ic), "{p} vs {t} ic={ic}");
                }
            }
        }
        assert!(glob_fast("**/*.rs", "a/b.rs", false) == Some(true) && glob_fast("src/[ab].rs", "x", false).is_none());
        // the general matcher's memo must not alias a trailing negated class onto the next row
        assert!(!glob_match("a[!b]", "a", false) && glob_match("a[!b]*", "ac", false));
    }

    #[test]
    fn glob_matcher_handles_star_star_and_classes() {
        assert!(glob_match("**/*.rs", "src/deep/a.rs", false));
        assert!(!glob_match("*.rs", "src/a.rs", false));
        assert!(glob_match("*.rs", "a.rs", false));
        assert!(glob_match("src/[ab].rs", "src/a.rs", false));
        assert!(!glob_match("src/[!a].rs", "src/a.rs", false));
        assert!(glob_match("src/**/x?.rs", "src/a/b/x1.rs", false));
    }

    #[cfg(windows)]
    #[test]
    fn a_folder_inside_an_indexed_root_reuses_that_index() {
        let dir = tmp("index-sub");
        fs::create_dir_all(dir.join("src/deep")).unwrap();
        fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        fs::write(dir.join("src/deep/a.rs"), "fn a() {}\n").unwrap();
        fs::write(dir.join("node_modules/pkg/x.rs"), "fn x() {}\n").unwrap();
        crate::index::ensure(&dir);
        let canon = fs::canonicalize(&dir).unwrap();
        // Naming a sub-folder builds nothing new: its queries are answered from the root's index.
        crate::index::ensure(&canon.join("src"));
        let (_, prefix) = crate::index::snapshot(&canon.join("src")).expect("served by the root index");
        assert_eq!(prefix, "src");
        // A folder the root walk skipped is not in that index: it gets its own.
        crate::index::ensure(&canon.join("node_modules"));
        let (snap, prefix) = crate::index::snapshot(&canon.join("node_modules")).expect("own index");
        assert_eq!(prefix, "");
        assert!(snap.files.iter().any(|e| e.rel.ends_with("x.rs")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn the_workspace_index_follows_every_change_at_once() {
        let dir = tmp("index");
        fs::create_dir_all(dir.join("src/deep")).unwrap();
        fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        fs::write(dir.join("src/a.rs"), "fn alpha() {}\n").unwrap();
        fs::write(dir.join("src/deep/b.rs"), "fn beta() {}\n").unwrap();
        fs::write(dir.join("node_modules/pkg/x.rs"), "fn hidden() {}\n").unwrap();
        crate::index::ensure(&dir);
        let canon = fs::canonicalize(&dir).unwrap();
        assert!(crate::index::snapshot(&canon).is_some(), "an index was built");
        let glob = |p: &str| invoke(&dir, "glob", json!({"pattern": p})).text;
        let names = |p: &str| {
            let mut v: Vec<String> = glob(p).lines().filter(|l| !l.starts_with("[qx]")).map(String::from).collect();
            v.sort();
            v
        };
        assert_eq!(names("**/*.rs"), ["src/a.rs", "src/deep/b.rs"]);
        // New file, new directory with files, a copy, a rename, a delete: each visible on the next call.
        fs::write(dir.join("src/c.rs"), "fn gamma() {}\n").unwrap();
        assert_eq!(names("src/*.rs"), ["src/a.rs", "src/c.rs"]);
        fs::create_dir_all(dir.join("lib/inner")).unwrap();
        fs::write(dir.join("lib/inner/d.rs"), "fn delta() {}\n").unwrap();
        assert_eq!(names("lib/**/*.rs"), ["lib/inner/d.rs"]);
        fs::rename(dir.join("src/deep"), dir.join("moved")).unwrap();
        assert_eq!(names("**/*.rs"), ["lib/inner/d.rs", "moved/b.rs", "src/a.rs", "src/c.rs"]);
        fs::remove_file(dir.join("src/a.rs")).unwrap();
        fs::remove_dir_all(dir.join("lib")).unwrap();
        assert_eq!(names("**/*.rs"), ["moved/b.rs", "src/c.rs"]);
        // A generated directory stays out, and an empty reply says so.
        assert!(glob("**/*.nothing").contains("node_modules"), "{}", glob("**/*.nothing"));
        assert!(!glob("**/*.rs").contains("node_modules/pkg"));
        // Grep reads what the files hold now, including a rewrite of a file it has already searched.
        let g = |pat: &str| invoke(&dir, "grep", json!({"pattern": pat})).text;
        assert!(g("gamma").contains("src/c.rs"), "{}", g("gamma"));
        fs::write(dir.join("src/c.rs"), "fn omega() {}\n").unwrap();
        assert!(g("gamma").contains("No matches"), "{}", g("gamma"));
        assert!(g("omega").contains("src/c.rs"), "{}", g("omega"));
        // A search that names the generated directory goes to the disk, not the index.
        assert!(invoke(&dir, "glob", json!({"pattern": "node_modules/**/*.rs"})).text.contains("node_modules/pkg/x.rs"));
        let _ = fs::remove_dir_all(&dir);
    }
    #[cfg(windows)]
    #[test]
    fn the_index_equals_a_fresh_walk_after_random_changes() {
        let dir = tmp("index-stress");
        crate::index::ensure(&dir);
        let canon = fs::canonicalize(&dir).unwrap();
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move |n: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let mut dirs: Vec<PathBuf> = vec![dir.clone()];
        for step in 0..400 {
            let d = dirs[next(dirs.len() as u64) as usize].clone();
            match next(6) {
                0 | 1 => {
                    let _ = fs::write(d.join(format!("f{}.rs", next(40))), format!("x{step}"));
                }
                2 => {
                    let n = d.join(format!("d{}", next(12)));
                    if fs::create_dir_all(&n).is_ok() {
                        dirs.push(n);
                    }
                }
                3 => {
                    let _ = fs::remove_file(d.join(format!("f{}.rs", next(40))));
                }
                4 => {
                    if dirs.len() > 1 {
                        let i = 1 + next(dirs.len() as u64 - 1) as usize;
                        let gone = dirs.remove(i);
                        let _ = fs::remove_dir_all(&gone);
                        dirs.retain(|p| !p.starts_with(&gone));
                    }
                }
                _ => {
                    if dirs.len() > 2 {
                        let i = 1 + next(dirs.len() as u64 - 1) as usize;
                        let from = dirs[i].clone();
                        let to = dir.join(format!("m{step}"));
                        if fs::rename(&from, &to).is_ok() {
                            dirs.retain(|p| !p.starts_with(&from));
                            dirs.push(to);
                        }
                    }
                }
            }
            if step % 40 == 39 {
                let mut want: Vec<String> = Vec::new();
                let (parts, _, _) = walk_live(&canon, false, Vec::<String>::new, |v, p, _| {
                    v.push(p.strip_prefix(&canon).unwrap().to_string_lossy().replace('\\', "/"));
                });
                for p in parts {
                    want.extend(p);
                }
                want.sort();
                // None is a correct answer (the walk serves): parallel tests can evict this root
                // (MAX_ROOTS) or a watcher overflow can mark it broken. Rebuild and compare that.
                let (snap, _) = crate::index::snapshot(&canon)
                    .or_else(|| {
                        crate::index::ensure(&canon);
                        crate::index::snapshot(&canon)
                    })
                    .expect("index answers after a rebuild");
                let got: Vec<String> = snap.files.iter().map(|e| e.rel.clone()).collect();
                assert_eq!(got, want, "after step {step}");
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }
    #[cfg(windows)]
    #[test]
    fn a_file_named_like_a_generated_folder_is_indexed_and_its_deletion_follows() {
        let dir = tmp("index-vendor-file");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.rs"), "").unwrap();
        crate::index::ensure(&dir);
        let canon = fs::canonicalize(&dir).unwrap();
        let rels = || -> Vec<String> {
            let (snap, _) = crate::index::snapshot(&canon).expect("index stays valid");
            snap.files.iter().map(|e| e.rel.clone()).collect()
        };
        fs::write(dir.join("vendor"), "x").unwrap();
        fs::write(dir.join("src/dist"), "x").unwrap();
        assert_eq!(rels(), ["src/a.rs", "src/dist", "vendor"]);
        fs::remove_file(dir.join("vendor")).unwrap();
        assert_eq!(rels(), ["src/a.rs", "src/dist"]);
        // A folder of that name is still left out, with what it holds.
        fs::create_dir_all(dir.join("vendor")).unwrap();
        fs::write(dir.join("vendor/x.rs"), "").unwrap();
        assert_eq!(rels(), ["src/a.rs", "src/dist"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn deleting_by_a_short_8_3_name_does_not_leave_the_file_in_the_index() {
        let dir = tmp("index-short");
        fs::write(dir.join("a.rs"), "").unwrap();
        fs::write(dir.join("longfilename.txt"), "").unwrap();
        let short = dir.join("LONGFI~1.TXT");
        if !short.exists() {
            let _ = fs::remove_dir_all(&dir);
            return; // 8.3 names are off on this volume: nothing to check
        }
        crate::index::ensure(&dir);
        let canon = fs::canonicalize(&dir).unwrap();
        assert!(crate::index::snapshot(&canon).is_some(), "an index was built");
        fs::remove_file(&short).unwrap();
        // The kernel reports the short name, which names no entry: the index is dropped, not left wrong.
        let r = invoke(&dir, "glob", json!({"pattern": "**/*.txt"}));
        assert!(!r.text.contains("longfilename"), "{}", r.text);
        crate::index::ensure(&dir);
        let (snap, _) = crate::index::snapshot(&canon).expect("rebuilt");
        let rels: Vec<&str> = snap.files.iter().map(|e| e.rel.as_str()).collect();
        assert_eq!(rels, ["a.rs"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn the_index_leaves_out_what_git_ignores_also_a_folder_made_or_ignored_after_it_started() {
        let dir = tmp("index-gitignore");
        if !git_init(&dir) {
            let _ = fs::remove_dir_all(&dir);
            return; // no git on this machine: nothing to check
        }
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join(".gitignore"), "gen/\n").unwrap();
        fs::write(dir.join("src/a.rs"), "needle\n").unwrap();
        crate::index::ensure(&dir);
        let canon = fs::canonicalize(&dir).unwrap();
        assert!(crate::index::snapshot(&canon).is_some(), "an index was built");
        let grep = || invoke(&dir, "grep", json!({"pattern": "needle"})).text;
        // A folder git ignores, made after the index started.
        fs::create_dir_all(dir.join("gen/deep")).unwrap();
        fs::write(dir.join("gen/deep/x.rs"), "needle\n").unwrap();
        let r = grep();
        assert!(r.contains("src/a.rs") && !r.contains("gen/deep/x.rs"), "{r}");
        // A folder that is searched until the ignore file is edited.
        fs::create_dir_all(dir.join("late")).unwrap();
        fs::write(dir.join("late/y.rs"), "needle\n").unwrap();
        assert!(grep().contains("late/y.rs"));
        fs::write(dir.join(".gitignore"), "gen/\nlate/\n").unwrap();
        let r = grep();
        assert!(r.contains("src/a.rs") && !r.contains("late/y.rs"), "{r}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The indexed `glob` and `grep` answer what a walk of the disk finds, after the changes the index
    /// has to read from names alone: a file called `vendor`, a short 8.3 name, a folder git ignores.
    #[cfg(windows)]
    #[test]
    fn indexed_glob_and_grep_equal_a_walk_after_changes_to_names_and_ignore_rules() {
        let dir = tmp("index-vs-walk");
        if !git_init(&dir) {
            let _ = fs::remove_dir_all(&dir);
            return; // no git on this machine: nothing to check
        }
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join(".gitignore"), "gen/\n").unwrap();
        fs::write(dir.join("src/a.rs"), "needle\n").unwrap();
        crate::index::ensure(&dir);
        let canon = fs::canonicalize(&dir).unwrap();
        assert!(crate::index::snapshot(&canon).is_some(), "an index was built");
        let all = [
            "src/a.rs", "vendor", "src/dist", "gen/x.rs", "late/y.rs", "longfilename.txt", "lib/z.rs",
        ];
        fs::write(dir.join("vendor"), "needle\n").unwrap();
        fs::write(dir.join("src/dist"), "needle\n").unwrap();
        fs::create_dir_all(dir.join("gen")).unwrap();
        fs::write(dir.join("gen/x.rs"), "needle\n").unwrap();
        fs::create_dir_all(dir.join("late")).unwrap();
        fs::write(dir.join("late/y.rs"), "needle\n").unwrap();
        fs::write(dir.join("longfilename.txt"), "needle\n").unwrap();
        fs::create_dir_all(dir.join("lib")).unwrap();
        fs::write(dir.join("lib/z.rs"), "needle\n").unwrap();
        fs::write(dir.join(".gitignore"), "gen/\nlate/\n").unwrap();
        fs::remove_file(dir.join("src/a.rs")).unwrap();
        let short = dir.join("LONGFI~1.TXT");
        fs::remove_file(if short.exists() { short } else { dir.join("longfilename.txt") }).unwrap();

        let globbed = invoke(&dir, "glob", json!({"pattern": "**/*"})).text;
        let mut got: Vec<&str> = globbed.lines().filter(|l| all.contains(l)).collect();
        got.sort();
        let (parts, _, _) = walk_live(&canon, false, Vec::<String>::new, |v, p, _| {
            v.push(p.strip_prefix(&canon).unwrap().to_string_lossy().replace('\\', "/"));
        });
        let mut want: Vec<String> = parts.into_iter().flatten().filter(|r| !r.starts_with('.')).collect();
        want.sort();
        assert_eq!(got, want, "glob: {globbed}");
        let grepped = invoke(&dir, "grep", json!({"pattern": "needle"})).text;
        for f in all {
            assert_eq!(grepped.contains(&format!("{f}:")) || grepped.contains(&format!("{f}\n")), want.iter().any(|w| w == f), "{f} in grep: {grepped}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "micro-benchmark: cargo test --release bench_fs -- --ignored --nocapture"]
    fn bench_fs() {
        let cwd = std::env::current_dir().unwrap();
        let time = |name: &str, args: Value| {
            for _ in 0..200 {
                let _ = call(&args);
            }
            let n = 2000;
            let t = Instant::now();
            for _ in 0..n {
                let _ = call(&args);
            }
            println!("{name:<14} {:>8.1} us/call", t.elapsed().as_secs_f64() * 1e6 / n as f64);
        };
        let c = cwd.to_string_lossy().to_string();
        time("stat", json!({"op":"stat","cwd":c,"path":"Cargo.toml"}));
        time("read", json!({"op":"read","cwd":c,"path":"Cargo.toml"}));
        time("list src", json!({"op":"list","cwd":c,"path":"src"}));
        time("glob src/*.rs", json!({"op":"glob","cwd":c,"pattern":"src/*.rs"}));
        time("grep src", json!({"op":"grep","cwd":c,"pattern":"fn \\w+_schema","path":"src"}));
        let r = Root::new(&json!({"cwd": c})).unwrap();
        let t = Instant::now();
        for _ in 0..2000 {
            let _ = Root::new(&json!({"cwd": c}));
        }
        println!("Root::new      {:>8.1} us", t.elapsed().as_secs_f64() * 1e6 / 2000.0);
        let t = Instant::now();
        for _ in 0..2000 {
            let _ = r.resolve_read("Cargo.toml");
        }
        println!("resolve_read   {:>8.1} us", t.elapsed().as_secs_f64() * 1e6 / 2000.0);
        let t = Instant::now();
        for _ in 0..2000 {
            let _ = fs::metadata("Cargo.toml");
        }
        println!("fs::metadata   {:>8.1} us", t.elapsed().as_secs_f64() * 1e6 / 2000.0);
        let t = Instant::now();
        for _ in 0..2000 {
            let _ = fs::canonicalize("Cargo.toml");
        }
        println!("canonicalize   {:>8.1} us", t.elapsed().as_secs_f64() * 1e6 / 2000.0);
    }
    #[test]
    fn a_bounded_live_glob_finds_what_a_full_walk_finds() {
        let dir = tmp("bounded");
        for p in ["a.rs", "src/a.rs", "src/deep/b.rs", "src/deep/er/c.rs", "lib/x/d.rs", "docs/e.md", "src-x/y.rs"] {
            let f = dir.join(p);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, "x").unwrap();
        }
        let canon = fs::canonicalize(&dir).unwrap();
        let names = |pat: &str| {
            let mut v: Vec<String> = invoke(&dir, "glob", json!({"pattern": pat}))
                .text.lines().filter(|l| !l.starts_with("[qx]") && *l != "No files found.").map(String::from).collect();
            v.sort();
            v
        };
        assert_eq!(names("src/*.rs"), ["src/a.rs"]);
        assert_eq!(names("src/*/*.rs"), ["src/deep/b.rs"]);
        assert_eq!(names("src/**/*.rs"), ["src/a.rs", "src/deep/b.rs", "src/deep/er/c.rs"]);
        assert_eq!(names("{src,lib}/*/*.rs"), ["lib/x/d.rs", "src/deep/b.rs"]);
        assert_eq!(names("*.rs"), ["a.rs", "lib/x/d.rs", "src-x/y.rs", "src/a.rs", "src/deep/b.rs", "src/deep/er/c.rs"]);
        assert_eq!(names("nope/*.rs"), Vec::<String>::new());
        // `src-x` sorts between `src` and `src/deep`: the nested hint must still be dropped, not walked twice.
        assert_eq!(names("{src,src-x,src/deep}/*.rs"), ["src-x/y.rs", "src/a.rs", "src/deep/b.rs"]);
        // The walker itself: starts in the hinted directory and stops at the depth asked for.
        let seen = std::sync::Mutex::new(Vec::new());
        let _ = walk_bounded(&canon, false, &["src".to_string()], Some(1), || (), |_, p, _| {
            seen.lock().unwrap().push(p.strip_prefix(&canon).unwrap().to_string_lossy().replace('\\', "/"));
        });
        assert_eq!(seen.into_inner().unwrap(), ["src/a.rs"]);
        let _ = fs::remove_dir_all(&dir);
    }
    /// The server builds the index on one worker thread and re-arms and reads it from others, each gone
    /// a moment later. A watch tied to its issuing thread dies with it (error 995) and the index is
    /// then rebuilt on every call: this is what made globs and greps slow for real sessions.
    #[cfg(windows)]
    #[test]
    fn the_index_outlives_the_threads_that_built_and_read_it() {
        let dir = tmp("index-threads");
        fs::write(dir.join("a.rs"), "x").unwrap();
        let canon = fs::canonicalize(&dir).unwrap();
        let d = dir.clone();
        std::thread::spawn(move || crate::index::ensure(&d)).join().unwrap();
        for round in 0..6 {
            fs::write(dir.join(format!("n{round}.rs")), "y").unwrap();
            let c = canon.clone();
            let got = std::thread::spawn(move || {
                crate::index::snapshot(&c).map(|(s, _)| s.files.iter().map(|e| e.rel.clone()).collect::<Vec<_>>())
            })
            .join()
            .unwrap()
            .expect("index still valid after the threads that used it exited");
            assert!(got.contains(&format!("n{round}.rs")) && got.contains(&"a.rs".to_string()), "round {round}: {got:?}");
            assert_eq!(got.len(), round + 2, "round {round}: {got:?}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- batch 4 / FS7 ----

    #[test]
    fn git_ignored_dirs_does_not_run_the_repos_fsmonitor_command() {
        let dir = tmp("fsmonitor");
        if !git_init(&dir) {
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap();
        let marker = dir.join("MARKER.txt");
        let hook = format!("echo ran > \"{}\"", marker.to_string_lossy().replace('\\', "/"));
        git(&["config", "core.fsmonitor", &hook]);
        fs::write(dir.join("b.txt"), "x\n").unwrap();
        let _ = git_ignored_dirs(&dir);
        assert!(!marker.exists(), "the searched repo's core.fsmonitor command ran");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_of_one_file_matches_include_against_its_path() {
        let dir = tmp("grep-file-include");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/one.rs"), "needle\n").unwrap();
        for inc in ["**/*.rs", "src/*.rs", "*.rs", "src/one.rs"] {
            let r = invoke(&dir, "grep", json!({"pattern":"needle","path":"src/one.rs","include":inc}));
            assert!(r.text.contains("Found 1 match"), "include {inc}: {}", r.text);
        }
        let r = invoke(&dir, "grep", json!({"pattern":"needle","path":"src/one.rs","include":"**/*.txt"}));
        assert!(r.text.contains("No matches"), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_context_given_as_a_string_is_honoured() {
        let dir = tmp("grep-ctx-string");
        fs::write(dir.join("a.txt"), "one\nneedle\nthree\n").unwrap();
        let num = invoke(&dir, "grep", json!({"pattern":"needle","context":1}));
        let txt = invoke(&dir, "grep", json!({"pattern":"needle","context":"1"}));
        assert!(num.text.contains("1- one") && num.text.contains("3- three"), "{}", num.text);
        assert_eq!(num.text, txt.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_brackets_in_a_folder_name_are_literal_when_nothing_else_matches() {
        let dir = tmp("glob-brackets");
        fs::create_dir_all(dir.join("app/[id]")).unwrap();
        fs::write(dir.join("app/[id]/page.tsx"), "").unwrap();
        let r = invoke(&dir, "glob", json!({"pattern":"app/[id]/*.tsx"}));
        assert!(r.text.contains("app/[id]/page.tsx"), "{}", r.text);
        assert_eq!(r.structured["count"], 1, "{}", r.text);
        // A real class still works and is not reinterpreted.
        fs::create_dir_all(dir.join("app/i")).unwrap();
        fs::write(dir.join("app/i/x.tsx"), "").unwrap();
        let r = invoke(&dir, "glob", json!({"pattern":"app/[id]/*.tsx"}));
        assert!(r.text.contains("app/i/x.tsx") && !r.text.contains("page.tsx"), "{}", r.text);
        // A file called `[id].tsx`, with and without a folder part.
        fs::create_dir_all(dir.join("pages")).unwrap();
        fs::write(dir.join("pages/[id].tsx"), "").unwrap();
        for pat in ["pages/[id].tsx", "**/[id].tsx", "[id].tsx"] {
            let r = invoke(&dir.join(if pat == "[id].tsx" { "pages" } else { "." }), "glob", json!({"pattern": pat}));
            assert_eq!(r.structured["count"], 1, "{pat}: {}", r.text);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    struct CountingReader<R> {
        inner: R,
        read: std::rc::Rc<std::cell::Cell<usize>>,
    }
    impl<R: std::io::Read> std::io::Read for CountingReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read.set(self.read.get() + n);
            Ok(n)
        }
    }

    #[test]
    fn context_windows_come_from_a_stream_that_stops_at_the_last_wanted_line() {
        let text: String = (1..=200_000).map(|i| format!("line {i} padding padding padding\n")).collect();
        let total = text.len();
        let read = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let r = std::io::BufReader::with_capacity(8192, CountingReader { inner: text.as_bytes(), read: read.clone() });
        let got = collect_lines(r, &[(99, 103), (101, 105)], 2000);
        let mut keys: Vec<_> = got.keys().copied().collect();
        keys.sort();
        assert_eq!(keys, (99..=105).collect::<Vec<_>>());
        assert_eq!(got[&100], "line 100 padding padding padding");
        assert!(read.get() < total / 100, "read {} of {total} bytes", read.get());
    }

    #[test]
    fn grep_context_on_a_streamed_file_gives_the_same_lines() {
        let dir = tmp("grep-ctx-stream");
        let mut text = String::new();
        for i in 1..=200_000 {
            if i == 150_000 {
                text.push_str("UNIQUE_NEEDLE_LINE\n");
            } else {
                text.push_str(&format!("filler line number {i} with some padding text\n"));
            }
        }
        assert!(text.len() as u64 > MAX_GREP_FILE_BYTES);
        fs::write(dir.join("big.log"), &text).unwrap();
        let r = invoke(&dir, "grep", json!({"pattern":"UNIQUE_NEEDLE_LINE","path":"big.log","context":2}));
        assert!(r.text.contains("149998- filler line number 149998 with some padding text\n"), "{}", r.text);
        assert!(r.text.contains("149999- filler line number 149999"), "{}", r.text);
        assert!(r.text.contains("150000: UNIQUE_NEEDLE_LINE"), "{}", r.text);
        assert!(r.text.contains("150001- filler line number 150001"), "{}", r.text);
        assert!(r.text.contains("150002- filler line number 150002"), "{}", r.text);
        assert!(!r.text.contains("150003-") && !r.text.contains("149997-"), "{}", r.text);
        // A match on the last line has no after-context and does not fail.
        fs::write(dir.join("tail.log"), format!("{text}UNIQUE_NEEDLE_TAIL\n")).unwrap();
        let r = invoke(&dir, "grep", json!({"pattern":"UNIQUE_NEEDLE_TAIL","path":"tail.log","context":2}));
        assert!(r.text.contains("200001: UNIQUE_NEEDLE_TAIL") && r.text.contains("200000- filler"), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A full content cache starts over instead of refusing every new file for the rest of the session.
    #[test]
    fn a_full_content_cache_clears_and_caches_new_files_again() {
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let chunk: std::sync::Arc<str> = std::sync::Arc::from("x".repeat(200 << 10).as_str());
        let puts = MAX_CONTENT_BYTES / (200 << 10) + 8;
        for i in 0..puts {
            content_put(Path::new(&format!("full-cache-test-{i}")), old, Cached::Text(chunk.clone()));
        }
        let last = Path::new("full-cache-test-last");
        content_put(last, old, Cached::Text(chunk.clone()));
        assert!(content_get(last, old).is_some(), "a new file is cached after the cache filled");
        assert!(CONTENT_BYTES.load(Ordering::Relaxed) <= MAX_CONTENT_BYTES, "the cache stays within its cap");
    }
}
