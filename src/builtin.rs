//! Commands implemented natively: no process creation at all.
//!
//! Every handler is conservative: if it sees a flag or shape it does not fully
//! understand it returns `None` and qx falls back to the real executable.

use crate::resolve;
use std::fmt::Write as _;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct BuiltinOut {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

/// A `NAME=value` prefix on the command line (`QX_LS_FULL=1 ls`) reaches built-ins like it reaches a child.
fn env_of(envs: &[(String, String)], key: &str) -> Option<String> {
    let mine = envs.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(key));
    mine.map(|(_, v)| v.clone()).or_else(|| std::env::var(key).ok())
}

pub fn try_exec(program: &str, args: &[String], cwd: Option<&Path>, envs: &[(String, String)]) -> Option<BuiltinOut> {
    if env_of(envs, "QX_NO_BUILTINS").as_deref() == Some("1") {
        return None;
    }
    if program.is_empty() || program.contains('/') || program.contains('\\') {
        return None;
    }
    if program.contains('*') || program.contains('?') {
        return None;
    }

    let start = Instant::now();
    let name = program.to_ascii_lowercase();
    if PATHY.contains(&name.as_str()) && args.iter().any(|a| a.starts_with('/')) {
        return None;
    }
    let mut out = match name.as_str() {
        "pwd" => cmd_pwd(args, cwd),
        "echo" => cmd_echo(args),
        "true" => Some(ok(String::new())),
        "false" => Some(BuiltinOut {
            code: 1,
            stdout: String::new(),
            stderr: String::new(),
            elapsed: Duration::ZERO,
        }),
        "ls" | "dir" => cmd_ls(args, cwd, env_of(envs, "QX_LS_FULL").is_some()),
        "cat" | "type" => cmd_cat(args, cwd),
        "head" => cmd_head(args, cwd, false),
        "tail" => cmd_head(args, cwd, true),
        "wc" => cmd_wc(args, cwd),
        "grep" => cmd_grep(args, cwd),
        "mkdir" => cmd_mkdir(args, cwd),
        "rm" | "del" | "erase" => cmd_rm(args, cwd),
        "cp" | "copy" => cmd_cp(args, cwd),
        "mv" | "move" => cmd_mv(args, cwd),
        "touch" => cmd_touch(args, cwd),
        "which" => cmd_which(args),
        "basename" => cmd_basename(args),
        "dirname" => cmd_dirname(args),
        "realpath" => cmd_realpath(args, cwd),
        "whoami" => cmd_whoami(args),
        "hostname" => cmd_hostname(args),
        "seq" => cmd_seq(args),
        "sleep" => cmd_sleep(args),
        "clear" | "cls" => Some(ok("\x1b[2J\x1b[H".to_string())),
        _ => None,
    }?;
    out.elapsed = start.elapsed();
    Some(out)
}

/// Every name `try_exec` may answer in-process.
pub const NAMES: &[&str] = &[
    "pwd", "echo", "true", "false", "ls", "dir", "cat", "type", "head", "tail", "wc", "mkdir", "rm",
    "del", "erase", "cp", "copy", "mv", "move", "touch", "which", "basename", "dirname", "realpath",
    "whoami", "hostname", "seq", "sleep", "clear", "cls", "grep",
];

/// Names that take file paths. A `/x` argument is a cmd.exe switch (`dir /b`) or an MSYS path
/// (`/tmp/x`); neither is a Windows path, so the real tool gets it.
const PATHY: &[&str] = &[
    "ls", "dir", "cat", "type", "head", "tail", "wc", "mkdir", "rm", "del", "erase", "cp", "copy", "mv", "move",
    "touch", "realpath",
];

pub fn is_builtin(name: &str) -> bool {
    NAMES.contains(&name.to_ascii_lowercase().as_str())
}

fn ok(stdout: String) -> BuiltinOut {
    BuiltinOut {
        code: 0,
        stdout,
        stderr: String::new(),
        elapsed: Duration::ZERO,
    }
}

fn fail(msg: String) -> BuiltinOut {
    BuiltinOut {
        code: 1,
        stdout: String::new(),
        stderr: format!("{msg}\n"),
        elapsed: Duration::ZERO,
    }
}

fn path_of(cwd: Option<&Path>, s: &str) -> PathBuf {
    let p = PathBuf::from(s);
    if p.is_absolute() {
        p
    } else if let Some(c) = cwd {
        c.join(p)
    } else {
        p
    }
}

fn wildcard(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

// ---------------------------------------------------------------- basics

fn cmd_pwd(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    if !args.is_empty() {
        return None;
    }
    match cwd {
        Some(c) => Some(ok(format!("{}\n", c.display()))),
        None => {
            let d = std::env::current_dir().ok()?;
            Some(ok(format!("{}\n", d.display())))
        }
    }
}

fn cmd_echo(args: &[String]) -> Option<BuiltinOut> {
    let mut newline = true;
    let mut start = 0;
    while start < args.len() && args[start].starts_with('-') && args[start].len() > 1 {
        match args[start].as_str() {
            "-n" => newline = false,
            _ => return None,
        }
        start += 1;
    }
    let mut s = args[start..].join(" ");
    if newline {
        s.push('\n');
    }
    Some(ok(s))
}

fn cmd_sleep(args: &[String]) -> Option<BuiltinOut> {
    if args.len() != 1 || args[0].starts_with('-') {
        return None;
    }
    let secs: f64 = args[0].parse().ok()?;
    if !(0.0..=10.0).contains(&secs) {
        return None;
    }
    std::thread::sleep(Duration::from_secs_f64(secs));
    Some(ok(String::new()))
}

// ---------------------------------------------------------------- listing

const LS_HARD_CAP: usize = 1_000_000;
const LS_HEADER_THRESHOLD: usize = 200;
const LS_SAMPLE_HEAD: usize = 100;
const LS_SAMPLE_TAIL: usize = 20;
const LS_GAP_DIRS: usize = 50;
const LS_R_LINE_CAP: usize = 2_000;
const LS_MAX_DEPTH: usize = 32;

#[derive(Clone, Copy, PartialEq)]
enum LsSort {
    Name,
    Time,
    Size,
}

struct LsOpts {
    full: bool,
    all: bool,
    long: bool,
    recursive: bool,
    reverse: bool,
    dirs_as_files: bool,
    sort: LsSort,
}

struct LsEntry {
    name: String,
    lower: String,
    is_dir: bool,
    len: u64,
    modified: Option<SystemTime>,
}

struct LsWalk {
    shown: usize,
    limit: usize,
    capped: bool,
    depth_capped: bool,
}

fn cmd_ls(args: &[String], cwd: Option<&Path>, full: bool) -> Option<BuiltinOut> {
    let mut opts = LsOpts {
        full,
        all: false,
        long: false,
        recursive: false,
        reverse: false,
        dirs_as_files: false,
        sort: LsSort::Name,
    };
    let mut paths: Vec<&str> = Vec::new();
    let mut literal = false;

    for a in args {
        if !literal && a == "--" {
            literal = true;
            continue;
        }
        if !literal && a.starts_with('-') && a.len() > 1 {
            if a.starts_with("--") {
                match a.as_str() {
                    "--color"
                    | "--color=auto"
                    | "--color=always"
                    | "--color=never"
                    | "--group-directories-first" => continue,
                    _ => return None,
                }
            }
            for ch in a[1..].chars() {
                match ch {
                    'a' | 'A' => opts.all = true,
                    'l' => opts.long = true,
                    'R' => opts.recursive = true,
                    'r' => opts.reverse = true,
                    't' => opts.sort = LsSort::Time,
                    'S' => opts.sort = LsSort::Size,
                    'd' => opts.dirs_as_files = true,
                    '1' => {}
                    _ => return None,
                }
            }
        } else {
            if wildcard(a) {
                return None;
            }
            paths.push(a);
        }
    }
    if paths.is_empty() {
        paths.push(".");
    }

    let mut out = String::new();
    if opts.dirs_as_files {
        for p in paths {
            let path = path_of(cwd, p);
            match fs::metadata(&path) {
                Ok(md) => write_entry(
                    &mut out,
                    p,
                    md.is_dir(),
                    md.len(),
                    md.modified().ok(),
                    opts.long,
                ),
                Err(e) => return Some(fail(format!("ls: {}: {e}", path.display()))),
            }
        }
        return Some(ok(out));
    }

    if opts.recursive {
        let mut walk = LsWalk {
            shown: 0,
            limit: LS_R_LINE_CAP,
            capped: false,
            depth_capped: false,
        };
        for p in &paths {
            let path = path_of(cwd, p);
            if let Err(e) = fs::metadata(&path) {
                return Some(fail(format!("ls: {}: {e}", path.display())));
            }
            list_recursive(&path, p, &opts, &mut out, &mut walk, 0);
        }
        if walk.capped {
            let _ = writeln!(out, "[qx] -R capped at {LS_R_LINE_CAP} displayed entries");
        }
        if walk.depth_capped {
            let _ = writeln!(out, "[qx] -R depth capped at {LS_MAX_DEPTH}");
        }
        return Some(ok(out));
    }

    let multiple = paths.len() > 1;
    let mut first = true;
    for p in paths {
        let path = path_of(cwd, p);
        if multiple {
            if !first {
                out.push('\n');
            }
            let _ = writeln!(out, "{p}:");
        }
        first = false;
        match list_one(&path, &opts) {
            Ok(s) => out.push_str(&s),
            Err(e) => return Some(fail(format!("ls: {}: {e}", path.display()))),
        }
    }
    Some(ok(out))
}

fn list_one(path: &Path, opts: &LsOpts) -> std::io::Result<String> {
    let md = fs::metadata(path)?;
    if !md.is_dir() {
        let mut out = String::new();
        write_entry(
            &mut out,
            &path.file_name().unwrap_or_default().to_string_lossy(),
            false,
            md.len(),
            md.modified().ok(),
            opts.long,
        );
        return Ok(out);
    }

    let (mut entries, total, dirs, files, capped) = collect_entries(path, opts)?;
    sort_entries(&mut entries, opts);
    let mut walk = LsWalk {
        shown: 0,
        limit: usize::MAX,
        capped: false,
        depth_capped: false,
    };
    let mut out = String::new();
    write_listing(&mut out, &entries, total, dirs, files, capped, opts, &mut walk);
    Ok(out)
}

fn list_recursive(
    path: &Path,
    display: &str,
    opts: &LsOpts,
    out: &mut String,
    walk: &mut LsWalk,
    depth: usize,
) {
    if walk.capped {
        return;
    }
    if depth > LS_MAX_DEPTH {
        walk.depth_capped = true;
        return;
    }

    let (mut entries, total, dirs, files, capped) = match collect_entries(path, opts) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(out, "ls: {}: {e}", path.display());
            return;
        }
    };
    sort_entries(&mut entries, opts);
    let _ = writeln!(out, "{display}:");
    write_listing(out, &entries, total, dirs, files, capped, opts, walk);
    if walk.capped {
        return;
    }

    let children: Vec<String> = entries
        .iter()
        .filter(|e| e.is_dir)
        .map(|e| e.name.clone())
        .collect();
    for name in children {
        if walk.capped {
            return;
        }
        let child = path.join(&name);
        let child_display = Path::new(display).join(&name).display().to_string();
        list_recursive(&child, &child_display, opts, out, walk, depth + 1);
    }
}

fn collect_entries(
    path: &Path,
    opts: &LsOpts,
) -> std::io::Result<(Vec<LsEntry>, usize, usize, usize, bool)> {
    let need_meta = opts.long || opts.sort != LsSort::Name;
    let mut entries: Vec<LsEntry> = Vec::new();
    let (mut total, mut dirs, mut files) = (0usize, 0usize, 0usize);
    let mut capped = false;

    for e in fs::read_dir(path)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if !opts.all && name.starts_with('.') {
            continue;
        }
        total += 1;
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            dirs += 1;
        } else {
            files += 1;
        }
        if entries.len() >= LS_HARD_CAP {
            capped = true;
            continue;
        }
        let md = if need_meta { e.metadata().ok() } else { None };
        let len = md.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = md.as_ref().and_then(|m| m.modified().ok());
        entries.push(LsEntry {
            lower: name.to_ascii_lowercase(),
            name,
            is_dir,
            len,
            modified,
        });
    }
    Ok((entries, total, dirs, files, capped))
}

fn sort_entries(entries: &mut [LsEntry], opts: &LsOpts) {
    entries.sort_by(|a, b| {
        let order = match opts.sort {
            LsSort::Name => a.lower.cmp(&b.lower),
            LsSort::Time => b.modified.cmp(&a.modified),
            LsSort::Size => b.len.cmp(&a.len),
        };
        order.then_with(|| a.lower.cmp(&b.lower))
    });
    if opts.reverse {
        entries.reverse();
    }
}

#[allow(clippy::too_many_arguments)]
fn write_listing(
    out: &mut String,
    entries: &[LsEntry],
    total: usize,
    dirs: usize,
    files: usize,
    stored_capped: bool,
    opts: &LsOpts,
    walk: &mut LsWalk,
) {
    let full = opts.full;
    if !full && total > LS_HEADER_THRESHOLD {
        write_header(out, total, dirs, files, entries, stored_capped);
    }

    let n = entries.len();
    if full || n <= LS_SAMPLE_HEAD + LS_SAMPLE_TAIL {
        for e in entries {
            if !take_line(walk) {
                break;
            }
            write_entry(out, &e.name, e.is_dir, e.len, e.modified, opts.long);
        }
    } else {
        for e in &entries[..LS_SAMPLE_HEAD] {
            if !take_line(walk) {
                break;
            }
            write_entry(out, &e.name, e.is_dir, e.len, e.modified, opts.long);
        }
        if !walk.capped {
            // Directories are navigation anchors: never drop one silently.
            let gap = &entries[LS_SAMPLE_HEAD..n - LS_SAMPLE_TAIL];
            let _ = write!(
                out,
                "... {} more entries (showing {LS_SAMPLE_HEAD}+{LS_SAMPLE_TAIL})",
                gap.len()
            );
            let mut hidden = gap.iter().filter(|e| e.is_dir);
            let names: Vec<&str> = hidden.by_ref().take(LS_GAP_DIRS).map(|e| e.name.as_str()).collect();
            if !names.is_empty() {
                let _ = write!(out, "; dirs inside: {}", names.join(", "));
                let more = hidden.count();
                if more > 0 {
                    let _ = write!(out, " +{more}");
                }
            }
            out.push_str(" ...\n");
            for e in &entries[n - LS_SAMPLE_TAIL..] {
                if !take_line(walk) {
                    break;
                }
                write_entry(out, &e.name, e.is_dir, e.len, e.modified, opts.long);
            }
        }
    }
    if stored_capped {
        let _ = writeln!(
            out,
            "[qx] listing capped at {LS_HARD_CAP} stored entries; total {total}"
        );
    }
}

fn take_line(walk: &mut LsWalk) -> bool {
    if walk.shown >= walk.limit {
        walk.capped = true;
        return false;
    }
    walk.shown += 1;
    true
}

fn write_header(
    out: &mut String,
    total: usize,
    dirs: usize,
    files: usize,
    entries: &[LsEntry],
    capped: bool,
) {
    let mut ext: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for e in entries {
        if e.is_dir {
            continue;
        }
        if let Some(x) = Path::new(&e.name).extension().and_then(|x| x.to_str()) {
            *ext.entry(x).or_insert(0) += 1;
        }
    }
    let mut top: Vec<(&str, usize)> = ext.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    top.truncate(6);

    let _ = write!(out, "[qx] {total} entries ({dirs} dirs, {files} files)");
    if capped {
        let _ = write!(out, ", stored {}", entries.len());
    }
    if !top.is_empty() {
        out.push_str(" | ext:");
        for (x, count) in top {
            let _ = write!(out, " .{x} {count}");
        }
    }
    out.push('\n');
}

fn write_entry(
    out: &mut String,
    name: &str,
    is_dir: bool,
    len: u64,
    modified: Option<SystemTime>,
    long: bool,
) {
    if !long {
        out.push_str(name);
        out.push('\n');
        return;
    }
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mode = if is_dir {
        "drwxr-xr-x"
    } else if matches!(ext.as_str(), "exe" | "cmd" | "bat" | "ps1" | "com") {
        "-rwxr-xr-x"
    } else {
        "-rw-r--r--"
    };
    let _ = write!(out, "{mode} {len:>10} ");
    write_time(out, modified.unwrap_or(UNIX_EPOCH));
    out.push(' ');
    out.push_str(name);
    out.push('\n');
}

fn write_time(out: &mut String, t: SystemTime) {
    out.push_str(&format_time(t));
}

pub(crate) fn format_time(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    ((y + if m <= 2 { 1 } else { 0 }), m as u32, d as u32)
}

// ---------------------------------------------------------------- reading

const READ_CAP: u64 = 32 * 1024 * 1024;

fn read_text_capped(path: &Path) -> std::io::Result<(String, bool)> {
    let f = File::open(path)?;
    let mut buf = Vec::new();
    f.take(READ_CAP + 1).read_to_end(&mut buf)?;
    let cut = buf.len() as u64 > READ_CAP;
    if cut {
        buf.truncate(READ_CAP as usize);
    }
    Ok((crate::exec::decode(&buf), cut))
}

fn cmd_cat(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    let mut number = false;
    let mut files: Vec<&str> = Vec::new();
    for a in args {
        if a == "-n" {
            number = true;
        } else if (a.starts_with('-') && a.len() > 1) || wildcard(a) {
            return None;
        } else {
            files.push(a);
        }
    }
    if files.is_empty() {
        return None;
    }

    let mut out = String::new();
    let mut err = String::new();
    let mut code = 0;
    let mut n = 0usize;
    for f in files {
        let path = path_of(cwd, f);
        match read_text_capped(&path) {
            Ok((text, cut)) => {
                if number {
                    for line in text.lines() {
                        n += 1;
                        out.push_str(&format!("{n:>6}\t{line}\n"));
                    }
                } else {
                    out.push_str(&text);
                    if !text.ends_with('\n') && !text.is_empty() {
                        out.push('\n');
                    }
                }
                if cut || out.len() > READ_CAP as usize {
                    let mut end = READ_CAP as usize;
                    while end > 0 && !out.is_char_boundary(end) {
                        end -= 1;
                    }
                    out.truncate(end);
                    out.push_str("\n[qx] cat truncated at 32 MiB\n");
                    break;
                }
            }
            Err(e) => {
                err.push_str(&format!("cat: {}: {e}\n", path.display()));
                code = 1;
            }
        }
    }
    Some(BuiltinOut {
        code,
        stdout: out,
        stderr: err,
        elapsed: Duration::ZERO,
    })
}

/// `usize::parse` accepts `+2`, which `tail -n +2` means as "from line 2".
fn count_arg(s: &str) -> Option<usize> {
    if s.starts_with('+') {
        None
    } else {
        s.parse().ok()
    }
}

fn cmd_head(args: &[String], cwd: Option<&Path>, tail: bool) -> Option<BuiltinOut> {
    let mut count = 10usize;
    let mut files: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-n" || a == "--lines" {
            i += 1;
            count = count_arg(args.get(i)?)?;
            i += 1;
        } else if let Some(rest) = a.strip_prefix("-n") {
            count = count_arg(rest)?;
            i += 1;
        } else if let Some(rest) = a.strip_prefix("--lines=") {
            count = count_arg(rest)?;
            i += 1;
        } else if (a.starts_with('-') && a.len() > 1) || wildcard(a) {
            return None;
        } else {
            files.push(a);
            i += 1;
        }
    }
    if files.len() != 1 {
        return None;
    }
    let path = path_of(cwd, files[0]);
    let text = match read_head_tail(&path, count, tail) {
        Ok(t) => t,
        Err(e) => {
            return Some(fail(format!(
                "{}: {}: {e}",
                if tail { "tail" } else { "head" },
                path.display()
            )))
        }
    };
    Some(ok(text))
}

/// Read only what `head`/`tail` needs. Large files are bounded to the first or
/// last 32 MiB instead of falling back to a binary that may not exist on
/// Windows.
fn read_head_tail(path: &Path, count: usize, tail: bool) -> std::io::Result<String> {
    if count == 0 {
        return Ok(String::new());
    }
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    if !tail {
        // Stream until `count` lines instead of reading the whole 32 MiB window.
        let mut reader = BufReader::new(f.take(READ_CAP));
        let mut out = String::new();
        let mut buf = Vec::new();
        let mut seen = 0usize;
        while seen < count {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf)?;
            if n == 0 {
                break;
            }
            seen += 1;
            let mut line = crate::exec::decode(&buf);
            if line.ends_with('\n') {
                line.pop();
                if line.ends_with('\r') {
                    line.pop();
                }
            }
            out.push_str(&line);
            out.push('\n');
        }
        return Ok(out);
    }

    // Walk backwards in small chunks until enough line breaks are found. The
    // 32 MiB bound keeps a newline-free multi-GB file from being loaded whole.
    let mut pos = len;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut total = 0usize;
    let mut newlines = 0usize;
    while pos > 0 && total < READ_CAP as usize {
        let take = (pos.min(64 * 1024) as usize).min(READ_CAP as usize - total);
        pos -= take as u64;
        let mut chunk = vec![0u8; take];
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut chunk)?;
        newlines += chunk.iter().filter(|b| **b == b'\n').count();
        total += take;
        chunks.push(chunk);
        if pos == 0 || newlines > count {
            break;
        }
    }
    let mut buf = Vec::with_capacity(total);
    for chunk in chunks.iter().rev() {
        buf.extend_from_slice(chunk);
    }
    let text = crate::exec::decode(&buf);
    let text = if pos > 0 {
        match text.find('\n') {
            Some(i) => text[i + 1..].to_string(),
            None => String::new(),
        }
    } else {
        text
    };

    let mut out = String::new();
    let mut ring: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
    for line in text.lines() {
        if ring.len() == count {
            ring.pop_front();
        }
        ring.push_back(line);
    }
    for line in ring {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

fn cmd_wc(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    let mut lines_f = false;
    let mut words_f = false;
    let mut bytes_f = false;
    let mut files: Vec<&str> = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 {
            for ch in a[1..].chars() {
                match ch {
                    'l' => lines_f = true,
                    'w' => words_f = true,
                    'c' => bytes_f = true,
                    _ => return None,
                }
            }
        } else if wildcard(a) {
            return None;
        } else {
            files.push(a);
        }
    }
    if files.len() != 1 {
        return None;
    }
    let path = path_of(cwd, files[0]);
    let (lines, words, bytes) = match count_file(&path) {
        Ok(v) => v,
        Err(e) => return Some(fail(format!("wc: {}: {e}", path.display()))),
    };
    let any = lines_f || words_f || bytes_f;
    let mut parts: Vec<String> = Vec::new();
    if lines_f || !any {
        parts.push(lines.to_string());
    }
    if words_f || !any {
        parts.push(words.to_string());
    }
    if bytes_f || !any {
        parts.push(bytes.to_string());
    }
    parts.push(files[0].to_string());
    Some(ok(format!("{}\n", parts.join(" "))))
}

/// More files than this and the real grep is the honest answer.
const MAX_GREP_FILES: usize = 20_000;
/// Stop before compaction would have to throw away the tail anyway.
const GREP_OUT_CAP: usize = 8 * 1024 * 1024;

/// `grep` for the shapes agents type: literal patterns with `-r`, `-n`, `-i`, `-l` and
/// `--include=*.ext`. Everything else - a real regex, `-c`/`-v`/`-e`, stdin, context flags,
/// an unreadable path, a binary or >32 MiB file - returns `None` so the real grep answers:
/// a wrong line list costs more than the 50 ms process it saves.
fn cmd_grep(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    let mut insensitive = false;
    let mut number = false;
    let mut list_only = false;
    let mut recursive = false;
    let mut fixed = false;
    let mut include: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            i += 1;
            break;
        }
        if let Some(rest) = a.strip_prefix("--") {
            let (key, val) = match rest.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (rest, None),
            };
            match key {
                "include" => {
                    let v = val?;
                    if !simple_include(v) {
                        return None;
                    }
                    include = Some(v.to_string());
                }
                // Colour is never emitted; buffering is not observable in captured output.
                "color" | "colour" | "line-buffered" => {}
                _ => return None,
            }
        } else if a.len() > 1 && a.starts_with('-') {
            for ch in a[1..].chars() {
                match ch {
                    'i' => insensitive = true,
                    'n' => number = true,
                    'l' => list_only = true,
                    'r' | 'R' => recursive = true,
                    'F' => fixed = true,
                    // qx reports no per-file errors, so silence changes nothing.
                    's' => {}
                    _ => return None,
                }
            }
        } else {
            break;
        }
        i += 1;
    }
    let pattern = args.get(i)?.as_str();
    i += 1;
    if pattern.is_empty() || pattern == "-" || (!fixed && needs_regex(pattern)) {
        return None;
    }
    let mut specs: Vec<(&str, PathBuf)> = Vec::new();
    if i == args.len() {
        if !recursive {
            // No path means stdin, which qx never gives a child.
            return None;
        }
        specs.push((".", path_of(cwd, ".")));
    } else {
        for raw in &args[i..] {
            if raw == "-" {
                return None;
            }
            specs.push((raw.as_str(), path_of(cwd, raw)));
        }
    }

    // Expand to files in the spelling grep would print: `grep -r x src` says `src/a.rs`.
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for (raw, path) in &specs {
        let md = fs::metadata(path).ok()?;
        if md.is_file() {
            files.push((raw.to_string(), path.clone()));
            continue;
        }
        if !md.is_dir() || !recursive {
            // `grep x dir` is an error grep words better than qx can.
            return None;
        }
        let base = raw.trim_end_matches(['/', '\\']);
        let mut full = false;
        crate::fsops::walk_every_file(path, |file| {
            if files.len() >= MAX_GREP_FILES {
                full = true;
                return false;
            }
            let rel = file
                .strip_prefix(path)
                .unwrap_or(file)
                .to_string_lossy()
                .replace('\\', "/");
            if rel.is_empty() {
                return true;
            }
            if let Some(g) = &include {
                let name = file
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !name.ends_with(&g[1..]) {
                    return true;
                }
            }
            let disp = if base.is_empty() {
                rel
            } else {
                format!("{base}/{rel}")
            };
            files.push((disp, file.to_path_buf()));
            true
        });
        if full {
            return None;
        }
    }
    if files.len() > MAX_GREP_FILES {
        return None;
    }

    // grep prefixes the filename whenever it recurses, even for a single hit file.
    let multiple = recursive || files.len() > 1;
    let needle = if insensitive {
        pattern.to_lowercase()
    } else {
        pattern.to_string()
    };
    let mut out = String::new();
    let mut hits = 0usize;
    for (disp, path) in &files {
        let bytes = fs::read(path).ok()?;
        if bytes.len() as u64 > READ_CAP {
            return None;
        }
        if bytes[..bytes.len().min(8192)].contains(&0) {
            // GNU prints "Binary file ... matches"; never guess at that.
            return None;
        }
        let text = crate::exec::decode(&bytes);
        for (idx, raw_line) in text.split('\n').enumerate() {
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            let hit = if insensitive {
                line.to_lowercase().contains(&needle)
            } else {
                line.contains(&needle)
            };
            if !hit {
                continue;
            }
            hits += 1;
            if list_only {
                out.push_str(disp);
                out.push('\n');
                break;
            }
            if multiple {
                out.push_str(disp);
                out.push(':');
            }
            if number {
                let _ = write!(out, "{}:", idx + 1);
            }
            out.push_str(line);
            out.push('\n');
            if out.len() >= GREP_OUT_CAP {
                out.push_str("[qx] grep output truncated at 8 MiB\n");
                return Some(ok(out));
            }
        }
    }
    Some(BuiltinOut {
        code: if hits > 0 { 0 } else { 1 },
        stdout: out,
        stderr: String::new(),
        elapsed: Duration::ZERO,
    })
}

/// Characters that make the pattern a regex. A literal scan must never answer these.
fn needs_regex(p: &str) -> bool {
    p.chars().any(|c| ".[]()*+?^${}|\\".contains(c))
}

/// The one `--include` shape qx can honour exactly: `*.ext`.
fn simple_include(g: &str) -> bool {
    match g.strip_prefix("*.") {
        Some(ext) => !ext.is_empty() && !ext.chars().any(|c| "*?[]\\/".contains(c)),
        None => false,
    }
}

/// Stream the file so `wc` on a multi-gigabyte log stays bounded.
fn count_file(path: &Path) -> std::io::Result<(usize, usize, usize)> {
    let mut f = File::open(path)?;
    let mut buf = [0u8; 64 * 1024];
    let (mut lines, mut words, mut bytes) = (0usize, 0usize, 0usize);
    let mut in_word = false;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes += n;
        for &b in &buf[..n] {
            if b == b'\n' {
                lines += 1;
            }
            if b.is_ascii_whitespace() {
                in_word = false;
            } else if !in_word {
                in_word = true;
                words += 1;
            }
        }
    }
    Ok((lines, words, bytes))
}

// ---------------------------------------------------------------- mutation

fn cmd_mkdir(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    let mut parents = false;
    let mut paths: Vec<&str> = Vec::new();
    for a in args {
        if a == "-p" || a == "--parents" {
            parents = true;
        } else if (a.starts_with('-') && a.len() > 1) || wildcard(a) {
            return None;
        } else {
            paths.push(a);
        }
    }
    if paths.is_empty() {
        return None;
    }
    let mut err = String::new();
    let mut code = 0;
    for p in paths {
        let path = path_of(cwd, p);
        let res = if parents {
            fs::create_dir_all(&path)
        } else {
            fs::create_dir(&path)
        };
        if let Err(e) = res {
            err.push_str(&format!("mkdir: {}: {e}\n", path.display()));
            code = 1;
        }
    }
    Some(BuiltinOut {
        code,
        stdout: String::new(),
        stderr: err,
        elapsed: Duration::ZERO,
    })
}

fn norm_path(path: &Path) -> String {
    let canon = fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_default()
                .join(path)
        }
    });
    let mut s = canon.to_string_lossy().to_ascii_lowercase();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    s.trim_end_matches(['\\', '/']).to_string()
}

fn dangerous(path: &Path) -> bool {
    let s = norm_path(path);
    if s.is_empty() || (s.len() == 2 && s.ends_with(':')) {
        return true;
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        if s == norm_path(Path::new(&home)) {
            return true;
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        if s == norm_path(&cwd) {
            return true;
        }
    }
    false
}

fn cmd_rm(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    let mut force = false;
    let mut recursive = false;
    let mut paths: Vec<&str> = Vec::new();
    for a in args {
        if a == "--" {
            continue;
        }
        if a.starts_with('-') && a.len() > 1 {
            if a.starts_with("--") {
                return None;
            }
            for ch in a[1..].chars() {
                match ch {
                    'f' => force = true,
                    'r' | 'R' => recursive = true,
                    _ => return None,
                }
            }
        } else if wildcard(a) {
            return None;
        } else {
            paths.push(a);
        }
    }
    if paths.is_empty() {
        return None;
    }

    let mut err = String::new();
    let mut code = 0;
    for p in paths {
        let path = path_of(cwd, p);
        if dangerous(&path) {
            err.push_str(&format!("rm: refusing to remove {}\n", path.display()));
            code = 1;
            continue;
        }
        if fs::symlink_metadata(&path).is_err() {
            if !force {
                err.push_str(&format!("rm: {}: no such file or directory\n", path.display()));
                code = 1;
            }
            continue;
        }
        let res = if path.is_dir() {
            if recursive {
                fs::remove_dir_all(&path)
            } else {
                Err(std::io::Error::other(
                    "is a directory (use -r)",
                ))
            }
        } else {
            fs::remove_file(&path)
        };
        if let Err(e) = res {
            err.push_str(&format!("rm: {}: {e}\n", path.display()));
            code = 1;
        }
    }
    Some(BuiltinOut {
        code,
        stdout: String::new(),
        stderr: err,
        elapsed: Duration::ZERO,
    })
}

fn copy_rec(src: &Path, dst: &Path, depth: usize) -> std::io::Result<()> {
    if depth > 64 {
        return Err(std::io::Error::other(
            "directory nesting is too deep (possible junction cycle)",
        ));
    }
    let md = fs::symlink_metadata(src)?;
    if md.file_type().is_symlink() {
        if fs::metadata(src).map(|m| m.is_dir()).unwrap_or(false) {
            return Err(std::io::Error::other(
                "refusing to follow a directory symlink",
            ));
        }
        fs::copy(src, dst)?;
        return Ok(());
    }
    if md.is_dir() {
        // A missing parent is the caller's error (`cp -r d nodir/x`); only the tree itself is made.
        match fs::create_dir(dst) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_rec(&e.path(), &dst.join(e.file_name()), depth + 1)?;
        }
        Ok(())
    } else {
        fs::copy(src, dst)?;
        Ok(())
    }
}

/// A symlink or junction to a directory anywhere under `p`: copying it is a choice (follow or
/// link) this builtin does not make, so `cp -r` leaves it to the real tool.
fn has_dir_link(p: &Path, depth: usize) -> bool {
    let Ok(md) = fs::symlink_metadata(p) else { return false };
    if md.file_type().is_symlink() {
        return fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false);
    }
    md.is_dir() && (depth > 64 || fs::read_dir(p).into_iter().flatten().flatten().any(|e| has_dir_link(&e.path(), depth + 1)))
}

/// True when `target` is `src` itself or lies inside it, judged on real paths (the nearest existing
/// ancestor of a not-yet-created target). `allow_same` lets an exact match through (mv case-rename).
fn into_self(src: &Path, target: &Path, allow_same: bool) -> bool {
    let Ok(s) = fs::canonicalize(src) else { return false };
    let mut d = target.to_path_buf();
    let mut exact = true;
    loop {
        let probe = if d.as_os_str().is_empty() { Path::new(".") } else { d.as_path() };
        if let Ok(c) = fs::canonicalize(probe) {
            return c.starts_with(&s) && !(exact && allow_same && c == s);
        }
        if !d.pop() {
            return false;
        }
        exact = false;
    }
}

fn cmd_cp(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    let mut recursive = false;
    let mut paths: Vec<&str> = Vec::new();
    for a in args {
        if a == "-r" || a == "-R" || a == "--recursive" {
            recursive = true;
        } else if (a.starts_with('-') && a.len() > 1) || wildcard(a) {
            return None;
        } else {
            paths.push(a);
        }
    }
    if paths.len() < 2 {
        return None;
    }
    let dst = path_of(cwd, paths[paths.len() - 1]);
    let srcs = &paths[..paths.len() - 1];

    if srcs.len() > 1 && !dst.is_dir() {
        return Some(fail("cp: target must be a directory for multiple sources".into()));
    }
    if recursive && srcs.iter().any(|s| has_dir_link(&path_of(cwd, s), 0)) {
        return None;
    }

    let mut err = String::new();
    let mut code = 0;
    for s in srcs {
        let src = path_of(cwd, s);
        if src.is_dir() && !recursive {
            err.push_str(&format!(
                "cp: {}: is a directory (use -r)\n",
                src.display()
            ));
            code = 1;
            continue;
        }
        let target = if dst.is_dir() {
            dst.join(src.file_name().unwrap_or_default())
        } else {
            dst.clone()
        };
        if into_self(&src, &target, false) {
            err.push_str(&format!(
                "cp: {} -> {}: destination is the source or inside it\n",
                src.display(),
                target.display()
            ));
            code = 1;
            continue;
        }
        if let Err(e) = copy_rec(&src, &target, 0) {
            err.push_str(&format!("cp: {} -> {}: {e}\n", src.display(), target.display()));
            code = 1;
        }
    }
    Some(BuiltinOut {
        code,
        stdout: String::new(),
        stderr: err,
        elapsed: Duration::ZERO,
    })
}

fn cmd_mv(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    let mut paths: Vec<&str> = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 {
            return None;
        }
        if wildcard(a) {
            return None;
        }
        paths.push(a);
    }
    if paths.len() < 2 {
        return None;
    }
    let dst = path_of(cwd, paths[paths.len() - 1]);
    let srcs = &paths[..paths.len() - 1];
    if srcs.len() > 1 && !dst.is_dir() {
        return Some(fail("mv: target must be a directory for multiple sources".into()));
    }

    let mut err = String::new();
    let mut code = 0;
    for s in srcs {
        let src = path_of(cwd, s);
        let target = if dst.is_dir() {
            dst.join(src.file_name().unwrap_or_default())
        } else {
            dst.clone()
        };
        if into_self(&src, &target, true) {
            err.push_str(&format!(
                "mv: {} -> {}: cannot move a directory into itself\n",
                src.display(),
                target.display()
            ));
            code = 1;
            continue;
        }
        if let Err(e) = fs::rename(&src, &target) {
            // Copy+delete only across volumes (17 = ERROR_NOT_SAME_DEVICE, 18 = EXDEV); any other
            // failure (locked dir, non-empty target) must not merge or duplicate the tree.
            if !matches!(e.raw_os_error(), Some(17 | 18)) {
                err.push_str(&format!("mv: {} -> {}: {e}\n", src.display(), target.display()));
                code = 1;
                continue;
            }
            if let Err(e2) = copy_rec(&src, &target, 0) {
                err.push_str(&format!(
                    "mv: {} -> {}: {e}; {e2}\n",
                    src.display(),
                    target.display()
                ));
                code = 1;
                continue;
            }
            let gone = if src.is_dir() {
                fs::remove_dir_all(&src)
            } else {
                fs::remove_file(&src)
            };
            if let Err(e) = gone {
                err.push_str(&format!("mv: {}: copied, but removing the source failed: {e}\n", src.display()));
                code = 1;
            }
        }
    }
    Some(BuiltinOut {
        code,
        stdout: String::new(),
        stderr: err,
        elapsed: Duration::ZERO,
    })
}

fn cmd_touch(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    if args.is_empty() || args.iter().any(|a| a.starts_with('-') || wildcard(a)) {
        return None;
    }
    let readonly = |a: &String| fs::metadata(path_of(cwd, a)).is_ok_and(|m| m.permissions().readonly());
    if args.iter().any(readonly) {
        return None;
    }
    let mut err = String::new();
    let mut code = 0;
    for a in args {
        let path = path_of(cwd, a);
        let res = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|f| f.set_modified(SystemTime::now()));
        if let Err(e) = res {
            err.push_str(&format!("touch: {}: {e}\n", path.display()));
            code = 1;
        }
    }
    Some(BuiltinOut {
        code,
        stdout: String::new(),
        stderr: err,
        elapsed: Duration::ZERO,
    })
}

// ---------------------------------------------------------------- misc

fn cmd_which(args: &[String]) -> Option<BuiltinOut> {
    if args.is_empty() || args.iter().any(|a| a.starts_with('-') || a.contains(['\\', '/'])) {
        return None;
    }
    let mut out = String::new();
    let mut code = 0;
    for name in args {
        match resolve::find_on_path(name) {
            Some(p) => out.push_str(&format!("{}\n", p.display())),
            None => {
                out.push_str(&format!("{name}: not found\n"));
                code = 1;
            }
        }
    }
    Some(BuiltinOut {
        code,
        stdout: out,
        stderr: String::new(),
        elapsed: Duration::ZERO,
    })
}

fn cmd_basename(args: &[String]) -> Option<BuiltinOut> {
    // A second operand is a suffix to strip.
    if args.len() != 1 || args.iter().any(|a| a.starts_with('-')) {
        return None;
    }
    let mut out = String::new();
    for a in args {
        let p = Path::new(a);
        out.push_str(&format!(
            "{}\n",
            p.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    Some(ok(out))
}

fn cmd_dirname(args: &[String]) -> Option<BuiltinOut> {
    if args.is_empty() || args.iter().any(|a| a.starts_with('-')) {
        return None;
    }
    let mut out = String::new();
    for a in args {
        let p = Path::new(a);
        let parent = p.parent().filter(|x| !x.as_os_str().is_empty());
        out.push_str(&format!(
            "{}\n",
            parent.map(|x| x.display().to_string()).unwrap_or_else(|| ".".into())
        ));
    }
    Some(ok(out))
}

fn cmd_realpath(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    if args.is_empty() || args.iter().any(|a| a.starts_with('-')) {
        return None;
    }
    let mut out = String::new();
    for a in args {
        let p = Path::new(a);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else if let Some(c) = cwd {
            c.join(p)
        } else {
            p.to_path_buf()
        };
        match fs::canonicalize(&abs) {
            Ok(c) => {
                let s = c.display().to_string();
                let trimmed = s.strip_prefix(r"\\?\").unwrap_or(&s);
                out.push_str(&format!("{trimmed}\n"));
            }
            Err(e) => {
                return Some(BuiltinOut {
                    code: 1,
                    stdout: String::new(),
                    stderr: format!("realpath: {a}: {e}\n"),
                    elapsed: Duration::ZERO,
                });
            }
        }
    }
    Some(ok(out))
}

fn cmd_whoami(args: &[String]) -> Option<BuiltinOut> {
    if !args.is_empty() {
        return None;
    }
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok()?;
    let domain = std::env::var("USERDOMAIN").ok().filter(|d| !d.is_empty());
    let line = match domain {
        Some(d) => format!("{d}\\{user}\n"),
        None => format!("{user}\n"),
    };
    Some(ok(line))
}

fn cmd_hostname(args: &[String]) -> Option<BuiltinOut> {
    if !args.is_empty() {
        return None;
    }
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()?;
    Some(ok(format!("{host}\n")))
}

fn cmd_seq(args: &[String]) -> Option<BuiltinOut> {
    if args.is_empty() || args.len() > 3 || args.iter().any(|a| a.starts_with('-')) {
        return None;
    }
    let nums: Vec<f64> = args
        .iter()
        .map(|a| a.parse().ok())
        .collect::<Option<Vec<_>>>()?;
    let (first, step, last) = match nums.as_slice() {
        [last] => (1.0, 1.0, *last),
        [first, last] => (*first, 1.0, *last),
        [first, step, last] => (*first, *step, *last),
        _ => return None,
    };
    if step == 0.0 || !first.is_finite() || !last.is_finite() || !step.is_finite() {
        return None;
    }
    let count = ((last - first) / step).floor() + 1.0;
    if !count.is_finite() || !(0.0..=1_000_000.0).contains(&count) {
        return None;
    }
    if args.iter().any(|a| a.contains(['e', 'E'])) {
        return None;
    }
    let places = args.iter().map(|a| a.split_once('.').map_or(0, |(_, f)| f.len())).max().unwrap_or(0);
    let mut out = String::new();
    for i in 0..count as u64 {
        let _ = writeln!(out, "{:.*}", places, first + step * i as f64);
    }
    Some(ok(out))
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cp_and_mv_refuse_to_nest_a_dir_in_itself() {
        let root = std::env::temp_dir().join(format!("qx-nest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("d/sub")).unwrap();
        fs::write(root.join("d/f.txt"), "x").unwrap();
        let d = root.join("d");
        let s = |p: PathBuf| p.to_string_lossy().into_owned();

        assert!(into_self(&d, &d.join("sub"), false));
        assert!(into_self(&d, &d.join("sub/new"), false));
        assert!(into_self(&d, &d, false));
        assert!(!into_self(&d, &d, true));
        assert!(!into_self(&d, &root.join("d2"), false));
        assert!(!into_self(&root.join("missing"), &d, false));

        let out = cmd_cp(&["-r".into(), s(d.clone()), s(d.join("sub"))], None).unwrap();
        assert_eq!(out.code, 1);
        assert!(!d.join("sub/d").exists());
        let out = cmd_mv(&[s(d.clone()), s(d.join("sub"))], None).unwrap();
        assert_eq!(out.code, 1);
        assert!(d.join("f.txt").exists() && !d.join("sub/d").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "qx-builtin-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn ls_lists_and_hides_dotfiles() {
        let d = tmp("ls");
        fs::write(d.join("a.txt"), "a").unwrap();
        fs::write(d.join(".hidden"), "h").unwrap();
        fs::create_dir(d.join("sub")).unwrap();

        let args = vec![".".to_string()];
        let out = cmd_ls(&args, Some(&d), false).unwrap();
        assert_eq!(out.code, 0);
        assert!(out.stdout.contains("a.txt"));
        assert!(out.stdout.contains("sub"));
        assert!(!out.stdout.contains(".hidden"));

        let args = vec!["-a".to_string(), ".".to_string()];
        assert!(cmd_ls(&args, Some(&d), false).unwrap().stdout.contains(".hidden"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ls_sample_names_dirs_in_the_omitted_range() {
        let d = tmp("lsdirs");
        for i in 0..300 {
            fs::write(d.join(format!("a{i:03}.txt")), "x").unwrap();
            fs::write(d.join(format!("c{i:03}.rs")), "x").unwrap();
        }
        fs::create_dir(d.join("bdir")).unwrap();
        let out = cmd_ls(&[String::from(".")], Some(&d), false).unwrap().stdout;
        assert!(out.contains("dirs inside: bdir ..."), "{out}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ls_collects_all_before_truncating() {
        let d = tmp("lsbig");
        for i in 0..250 {
            fs::write(d.join(format!("f{i:03}.txt")), "x").unwrap();
        }
        fs::create_dir(d.join("sub")).unwrap();
        fs::write(d.join("sub/inside.txt"), "x").unwrap();

        let out = cmd_ls(&[String::from(".")], Some(&d), false).unwrap().stdout;
        assert!(out.starts_with("[qx] 251 entries (1 dirs, 250 files)"));
        assert!(out.contains("f000.txt"));
        assert!(out.contains("f249.txt"));
        assert!(out.contains("more entries"));
        assert!(out.lines().count() < LS_HEADER_THRESHOLD);

        let rec = cmd_ls(
            &[String::from("-R"), String::from(".")],
            Some(&d),
            false,
        )
        .unwrap()
        .stdout;
        assert!(rec.contains("sub:"));
        assert!(rec.contains("inside.txt"));

        let dirs = cmd_ls(
            &[String::from("-d"), String::from("sub")],
            Some(&d),
            false,
        )
        .unwrap()
        .stdout;
        assert_eq!(dirs, "sub\n");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn text_commands() {
        let d = tmp("text");
        fs::write(d.join("f.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let arg = vec!["f.txt".to_string()];

        assert_eq!(cmd_wc(&arg, Some(&d)).unwrap().stdout.trim(), "4 4 19 f.txt");
        assert_eq!(
            cmd_head(&arg, Some(&d), false).unwrap().stdout,
            "one\ntwo\nthree\nfour\n"
        );
        assert_eq!(
            cmd_head(
                &[String::from("-n"), String::from("2"), String::from("f.txt")],
                Some(&d),
                false,
            )
                .unwrap()
                .stdout,
            "one\ntwo\n"
        );
        assert_eq!(
            cmd_head(&[String::from("-n2"), String::from("f.txt")], Some(&d), true)
                .unwrap()
                .stdout,
            "three\nfour\n"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn tail_reads_backwards_in_chunks() {
        let d = tmp("tail");
        let path = d.join("many.txt");
        let text: String = (0..20_000).map(|i| format!("line{i}\n")).collect();
        fs::write(&path, text).unwrap();
        assert_eq!(
            read_head_tail(&path, 2, true).unwrap(),
            "line19998\nline19999\n"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn mkdir_cp_mv_rm_roundtrip() {
        let d = tmp("mut");
        let mk = vec!["-p".to_string(), "a/b".to_string()];
        assert_eq!(cmd_mkdir(&mk, Some(&d)).unwrap().code, 0);

        fs::write(d.join("a/b/x.txt"), "x").unwrap();
        let cp = vec!["-r".to_string(), "a/b".to_string(), "c".to_string()];
        assert_eq!(cmd_cp(&cp, Some(&d)).unwrap().code, 0);
        assert!(d.join("c/x.txt").exists());

        let mv = vec!["c".to_string(), "moved".to_string()];
        assert_eq!(cmd_mv(&mv, Some(&d)).unwrap().code, 0);
        assert!(d.join("moved/x.txt").exists());

        let rm = vec!["-rf".to_string(), "a".to_string(), "moved".to_string()];
        assert_eq!(cmd_rm(&rm, Some(&d)).unwrap().code, 0);
        assert!(!d.join("a").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn rm_refuses_dangerous_paths() {
        let d = tmp("danger");
        assert!(dangerous(Path::new("C:\\")));
        assert!(dangerous(Path::new(".")));
        assert!(!dangerous(&d));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn misc_builtins() {
        assert_eq!(
            try_exec("seq", &[String::from("3")], None, &[]).unwrap().stdout,
            "1\n2\n3\n"
        );
        let stepped = vec![
            String::from("1"),
            String::from("2"),
            String::from("5"),
        ];
        assert_eq!(
            try_exec("seq", &stepped, None, &[]).unwrap().stdout,
            "1\n3\n5\n"
        );
        assert!(!try_exec("hostname", &[], None, &[]).unwrap().stdout.trim().is_empty());
        assert!(!try_exec("whoami", &[], None, &[]).unwrap().stdout.trim().is_empty());
        let rp = try_exec("realpath", &[String::from(".")], None, &[]).unwrap();
        assert_eq!(rp.code, 0);
        assert!(rp.stdout.trim().len() > 1);
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qx-bi-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn run(name: &str, args: &[&str], cwd: &Path) -> Option<BuiltinOut> {
        let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        try_exec(name, &a, Some(cwd), &[])
    }

    #[test]
    fn tail_from_line_and_cmd_switches_and_msys_paths_go_to_the_real_tool() {
        let d = scratch("fallback");
        fs::write(d.join("f.txt"), "a\nb\nc\n").unwrap();
        assert!(run("tail", &["-n", "+2", "f.txt"], &d).is_none());
        assert!(run("tail", &["-n+2", "f.txt"], &d).is_none());
        assert_eq!(run("tail", &["-n", "2", "f.txt"], &d).unwrap().stdout, "b\nc\n");
        for (n, a) in [("dir", "/b"), ("copy", "/y"), ("move", "/y"), ("del", "/q"), ("ls", "/tmp/x"), ("cat", "/tmp/x")] {
            assert!(run(n, &[a], &d).is_none(), "{n} {a}");
        }
        assert_eq!(run("echo", &["/b"], &d).unwrap().stdout, "/b\n");
        assert!(run("echo", &["-e", "a\\nb"], &d).is_none());
    }

    #[test]
    fn small_emulation_gaps_fall_back() {
        let d = scratch("gaps");
        fs::write(d.join("f.txt"), "a\nb\n").unwrap();
        assert!(run("basename", &["f.txt", ".txt"], &d).is_none());
        assert_eq!(run("basename", &["f.txt"], &d).unwrap().stdout, "f.txt\n");
        assert!(run("ls", &["-lh"], &d).is_none());
        assert!(run("sleep", &["120"], &d).is_none());
        assert!(run("where", &["git"], &d).is_none());
        fs::write(d.join("ro.txt"), "x").unwrap();
        let mut perm = fs::metadata(d.join("ro.txt")).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(d.join("ro.txt"), perm).unwrap();
        assert!(run("touch", &["ro.txt"], &d).is_none());
        assert!(run("touch", &["new.txt"], &d).is_some() && d.join("new.txt").is_file());
    }

    #[test]
    fn wc_names_the_file_and_seq_keeps_the_operands_precision() {
        let d = scratch("wc");
        fs::write(d.join("f.txt"), "a b\nc\n").unwrap();
        assert_eq!(run("wc", &["-l", "f.txt"], &d).unwrap().stdout, "2 f.txt\n");
        assert_eq!(run("seq", &["0.1", "0.1", "0.5"], &d).unwrap().stdout, "0.1\n0.2\n0.3\n0.4\n0.5\n");
        assert_eq!(run("seq", &["3"], &d).unwrap().stdout, "1\n2\n3\n");
        assert!(run("seq", &["1e3"], &d).is_none());
    }

    #[test]
    fn missing_targets_are_errors_not_invented() {
        let d = scratch("missing");
        fs::write(d.join("f.txt"), "x").unwrap();
        let ls = run("ls", &["-R", "nope"], &d).unwrap();
        assert_eq!(ls.code, 1, "{}", ls.stderr);
        let cp = run("cp", &["f.txt", "nodir/x"], &d).unwrap();
        assert_eq!(cp.code, 1);
        assert!(!d.join("nodir").exists());
        fs::create_dir_all(d.join("t")).unwrap();
        assert_eq!(run("cp", &["f.txt", "t"], &d).unwrap().code, 0);
        assert!(d.join("t/f.txt").is_file());
        assert_eq!(run("cp", &["-r", "t", "t2"], &d).unwrap().code, 0);
        assert!(d.join("t2/f.txt").is_file());
        assert_eq!(run("cp", &["-r", "t", "no/t3"], &d).unwrap().code, 1);
    }

    #[test]
    fn rm_f_removes_a_dangling_link_and_cp_r_leaves_linked_trees_alone() {
        let d = scratch("links");
        fs::create_dir_all(d.join("tree/sub")).unwrap();
        fs::write(d.join("target.txt"), "x").unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::fs::symlink_file;
            if symlink_file(d.join("target.txt"), d.join("link.txt")).is_ok() {
                fs::remove_file(d.join("target.txt")).unwrap();
                assert_eq!(run("rm", &["-f", "link.txt"], &d).unwrap().code, 0);
                assert!(fs::symlink_metadata(d.join("link.txt")).is_err());
            }
        }
        assert!(!has_dir_link(&d.join("tree"), 0));
        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(d.join("tree/sub"), d.join("tree/jump")).is_ok() {
            assert!(has_dir_link(&d.join("tree"), 0));
            assert!(run("cp", &["-r", "tree", "tree2"], &d).is_none());
        }
    }

    /// The built-in has to be indistinguishable from the real grep for every shape it accepts,
    /// so this runs both and compares stdout (sorted: qx emits path order) and the exit code.
    #[test]
    fn grep_matches_the_real_tool_or_declines() {
        let Some(grep) = real_grep() else {
            eprintln!("skipped: no grep.exe next to Git");
            return;
        };
        let d = scratch("grep");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("a.txt"), "alpha\nbeta\nalpha gamma\n").unwrap();
        fs::write(d.join("b.rs"), "let alpha = 1;\nlet gamma = 2;\n").unwrap();
        fs::write(d.join("sub/c.txt"), "alpha\nALPHA\n").unwrap();

        // No binary file yet: a binary match makes the built-in decline (GNU says
        // "Binary file ... matches"), which the recursive cases below must not trip over.
        let taken: Vec<Vec<&str>> = vec![
            vec!["-n", "alpha", "a.txt"],
            vec!["-n", "alpha", "a.txt", "b.rs"],
            vec!["-in", "alpha", "sub/c.txt"],
            vec!["-rn", "alpha", "."],
            vec!["-rn", "alpha", "sub"],
            vec!["-rl", "alpha", "."],
            vec!["-rln", "gamma", "."],
            vec!["-n", "nope", "a.txt"],
            vec!["-rn", "--include=*.rs", "alpha", "."],
        ];
        for case in taken {
            let mine = run("grep", &case, &d).expect("built-in declined a shape it claims");
            let (code, theirs) = real_grep_run(grep, &case, &d);
            let mut a: Vec<&str> = mine.stdout.lines().collect();
            let mut b: Vec<&str> = theirs.lines().collect();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(a, b, "stdout differs for {case:?}");
            assert_eq!(mine.code, code, "exit differs for {case:?}");
        }

        // Shapes a literal in-process scan must never guess at.
        fs::write(d.join("bin.dat"), b"alpha\0binary").unwrap();
        let declined: Vec<Vec<&str>> = vec![
            vec!["-c", "alpha", "a.txt"],
            vec!["alpha", "bin.dat"],
            vec!["a.p", "a.txt"],
            vec!["-v", "alpha", "a.txt"],
            vec!["alpha"],
            vec!["-rn", "alpha", "missing"],
            vec!["-rn", "alpha", "."],
        ];
        for case in declined {
            assert!(run("grep", &case, &d).is_none(), "should decline {case:?}");
        }
        let _ = fs::remove_dir_all(&d);
    }

    fn real_grep() -> Option<&'static str> {
        [
            r"C:\Program Files\Git\usr\bin\grep.exe",
            r"C:\Program Files\Git\bin\grep.exe",
        ]
        .into_iter()
        .find(|p| Path::new(p).exists())
    }

    fn real_grep_run(grep: &str, args: &[&str], cwd: &Path) -> (i32, String) {
        let out = std::process::Command::new(grep)
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("real grep");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }
}
