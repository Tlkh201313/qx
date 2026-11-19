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
    let mut name = program.to_ascii_lowercase();
    // PowerShell cmdlets with a POSIX twin run as that twin: no 50-90 ms for a shell to read a file.
    let translated = ps_cmdlet(&name, args);
    let args: &[String] = match &translated {
        Some((n, a)) => {
            name = (*n).to_string();
            a
        }
        None => args,
    };
    // grep checks its own operands: a pattern such as `/api/users` is not a path.
    if name != "grep" && PATHY.contains(&name.as_str()) && args.iter().any(|a| a.starts_with('/')) {
        return None;
    }
    // `Test-Path env:FOO`, `ls HKCU:\`: a PowerShell provider path, not a file under the cwd.
    // grep and sed take patterns and scripts, where a colon is ordinary text.
    if !matches!(name.as_str(), "grep" | "sed" | "echo" | "write-output" | "write-host")
        && (PATHY.contains(&name.as_str()) || name == "test-path")
        && args.iter().any(|a| !a.starts_with('-') && a.find(':').is_some_and(|i| i >= 2))
    {
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
        "sed" => cmd_sed(args, cwd),
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
        "test-path" => cmd_test_path(args, cwd),
        "write-output" | "write-host" => {
            if args.iter().any(|a| a.starts_with('-')) {
                None
            } else {
                let sep = if name == "write-host" { " " } else { "\n" };
                Some(ok(format!("{}\n", args.join(sep))))
            }
        }
        _ => None,
    }?;
    out.elapsed = start.elapsed();
    Some(out)
}

/// Every name `try_exec` may answer in-process.
pub const NAMES: &[&str] = &[
    "pwd", "echo", "true", "false", "ls", "dir", "cat", "type", "head", "tail", "wc", "mkdir", "rm",
    "del", "erase", "cp", "copy", "mv", "move", "touch", "which", "basename", "dirname", "realpath",
    "whoami", "hostname", "seq", "sleep", "clear", "cls", "grep", "sed", "start-sleep", "get-content", "gc",
    "test-path", "write-output", "write-host",
];

/// `Start-Sleep` and `Get-Content` spelled as the built-in that does the same. Any parameter that is
/// not understood returns None, and the real cmdlet runs.
fn ps_cmdlet(name: &str, args: &[String]) -> Option<(&'static str, Vec<String>)> {
    let mut it = args.iter();
    match name {
        "start-sleep" => {
            let mut secs: Option<f64> = None;
            while let Some(a) = it.next() {
                let low = a.to_ascii_lowercase();
                let ms = matches!(low.as_str(), "-m" | "-ms" | "-milliseconds");
                if ms || matches!(low.as_str(), "-s" | "-seconds") {
                    let v: f64 = it.next()?.parse().ok()?;
                    secs = Some(if ms { v / 1000.0 } else { v });
                } else if secs.is_none() && !a.starts_with('-') {
                    secs = Some(a.parse().ok()?);
                } else {
                    return None;
                }
            }
            Some(("sleep", vec![secs?.to_string()]))
        }
        "get-content" | "gc" => {
            let (mut path, mut head, mut tail) = (None, None, None);
            let mut args_it = args.iter();
            while let Some(a) = args_it.next() {
                let low = a.to_ascii_lowercase();
                match low.as_str() {
                    "-path" | "-literalpath" | "-lp" => path = Some(args_it.next()?.clone()),
                    "-totalcount" | "-head" | "-first" => head = Some(args_it.next()?.clone()),
                    "-tail" | "-last" => tail = Some(args_it.next()?.clone()),
                    "-raw" => {}
                    _ if a.starts_with('-') || path.is_some() => return None,
                    _ => path = Some(a.clone()),
                }
            }
            let path = path?;
            match (head, tail) {
                (Some(n), None) => Some(("head", vec!["-n".into(), n, path])),
                (None, Some(n)) => Some(("tail", vec!["-n".into(), n, path])),
                (None, None) => Some(("cat", vec![path])),
                _ => None,
            }
        }
        _ => None,
    }
}

fn cmd_test_path(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    let (mut path, mut kind) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.to_ascii_lowercase().as_str() {
            "-path" | "-literalpath" | "-lp" => path = Some(it.next()?.clone()),
            "-pathtype" => kind = Some(it.next()?.to_ascii_lowercase()),
            _ if a.starts_with('-') || path.is_some() => return None,
            _ => path = Some(a.clone()),
        }
    }
    let path = path?;
    if wildcard(&path) || path.contains('[') {
        return None;
    }
    let p = path_of(cwd, &path);
    let yes = match kind.as_deref() {
        None | Some("any") => p.exists(),
        Some("leaf") => p.is_file(),
        Some("container") => p.is_dir(),
        _ => return None,
    };
    Some(ok(format!("{}\n", if yes { "True" } else { "False" })))
}

/// Names that take file paths. A `/x` argument is a cmd.exe switch (`dir /b`) or an MSYS path
/// (`/tmp/x`); neither is a Windows path, so the real tool gets it.
const PATHY: &[&str] = &[
    "ls", "dir", "cat", "type", "head", "tail", "wc", "mkdir", "rm", "del", "erase", "cp", "copy", "mv", "move",
    "touch", "realpath", "sed", "grep",
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
    // In slices, so a cancel or the call's timeout ends it like it ends a real `sleep`.
    let end = std::time::Instant::now() + Duration::from_secs_f64(secs);
    loop {
        if crate::exec::should_stop() {
            return Some(stopped());
        }
        let left = end.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Some(ok(String::new()));
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

/// A built-in cut short by a cancel or the call's timeout: exit 124, like a killed child.
fn stopped() -> BuiltinOut {
    BuiltinOut {
        code: 124,
        stdout: String::new(),
        stderr: "qx: stopped (cancelled or timed out)\n".to_string(),
        elapsed: Duration::ZERO,
    }
}

/// `remove_dir_all`, one top-level entry at a time so a cancel or timeout stops it between entries.
/// Each entry still goes through std's own removal, which never follows a link and clears read-only.
fn remove_tree(path: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(path)? {
        if crate::exec::should_stop() {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "stopped (cancelled or timed out)"));
        }
        let p = entry?.path();
        let md = fs::symlink_metadata(&p)?;
        if md.is_dir() && !md.file_type().is_symlink() {
            fs::remove_dir_all(&p)?;
        } else if md.file_type().is_symlink() && fs::metadata(&p).is_ok_and(|m| m.is_dir()) {
            fs::remove_dir(&p)?; // a directory link: the link only
        } else {
            fs::remove_dir_all(&p).or_else(|_| fs::remove_file(&p))?;
        }
    }
    fs::remove_dir(path)
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
    readonly: bool,
}

struct LsWalk {
    shown: usize,
    limit: usize,
    capped: bool,
    depth_capped: bool,
    err: String,
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
                    "--color" | "--color=auto" | "--color=never" => continue,
                    // Colours and directory grouping are not produced here: the real ls answers.
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
    // A long listing would show the target's type and size instead of the link: the real ls answers.
    if opts.long && paths.iter().any(|p| fs::symlink_metadata(path_of(cwd, p)).is_ok_and(|m| m.file_type().is_symlink())) {
        return None;
    }

    let mut out = String::new();
    if opts.dirs_as_files {
        let mut rows = Vec::new();
        for p in paths {
            let path = path_of(cwd, p);
            match fs::metadata(&path) {
                Ok(md) => rows.push((p, md.is_dir(), md.len(), md.modified().ok(), md.permissions().readonly())),
                Err(e) => return Some(fail(format!("ls: {}: {e}", path.display()))),
            }
        }
        let style = LongStyle::new(opts.full, rows.iter().map(|r| (r.2, r.3, r.1)));
        if opts.long && !rows.is_empty() {
            out.push_str(&style.header());
        }
        for (p, is_dir, len, modified, readonly) in rows {
            write_entry(&mut out, p, is_dir, len, modified, readonly, opts.long, &style);
        }
        return Some(ok(out));
    }

    if opts.recursive {
        let mut walk = LsWalk {
            shown: 0,
            limit: LS_R_LINE_CAP,
            capped: false,
            depth_capped: false,
            err: String::new(),
        };
        for p in &paths {
            let path = path_of(cwd, p);
            if let Err(e) = fs::metadata(&path) {
                return Some(fail(format!("ls: {}: {e}", path.display())));
            }
            list_recursive(&path, p, &opts, &mut out, &mut walk, 0)?;
        }
        if walk.capped {
            let _ = writeln!(out, "[qx] -R capped at {LS_R_LINE_CAP} displayed entries");
        }
        if walk.depth_capped {
            let _ = writeln!(out, "[qx] -R depth capped at {LS_MAX_DEPTH}");
        }
        // An unreadable subdirectory is an error, not a quiet gap in an otherwise successful listing.
        return Some(BuiltinOut { code: i32::from(!walk.err.is_empty()), stdout: out, stderr: walk.err, elapsed: Duration::ZERO });
    }

    // Like ls: the file operands first, as one listing, then each directory under a `name:` header, so a
    // file named after a directory is never read as one of its entries. One bad operand does not hide
    // the others: its error goes to stderr and the exit code is 1.
    let multiple = paths.len() > 1;
    let mut err = String::new();
    let mut files: Vec<(&str, fs::Metadata)> = Vec::new();
    let mut dirs: Vec<(&str, PathBuf)> = Vec::new();
    for p in paths {
        let path = path_of(cwd, p);
        match fs::metadata(&path) {
            Ok(md) if md.is_dir() => dirs.push((p, path)),
            Ok(md) => files.push((p, md)),
            Err(e) => {
                let _ = writeln!(err, "ls: {}: {e}", path.display());
            }
        }
    }
    if opts.long && !files.is_empty() {
        let style = LongStyle::new(opts.full, files.iter().map(|(_, md)| (md.len(), md.modified().ok(), false)));
        out.push_str(&style.header());
        for (p, md) in &files {
            write_entry(&mut out, p, false, md.len(), md.modified().ok(), md.permissions().readonly(), true, &style);
        }
    } else {
        for (p, _) in &files {
            out.push_str(p);
            out.push('\n');
        }
    }
    for (p, path) in dirs {
        if multiple {
            if !out.is_empty() {
                out.push('\n');
            }
            let _ = writeln!(out, "{p}:");
        }
        match list_dir(&path, &opts) {
            Ok(s) => out.push_str(&s),
            // A symlink in a long listing (see `collect_entries`): the real ls answers.
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => return None,
            Err(e) => {
                let _ = writeln!(err, "ls: {}: {e}", path.display());
            }
        }
    }
    Some(BuiltinOut { code: i32::from(!err.is_empty()), stdout: out, stderr: err, elapsed: Duration::ZERO })
}

fn list_dir(path: &Path, opts: &LsOpts) -> std::io::Result<String> {
    let (mut entries, total, dirs, files, capped) = collect_entries(path, opts)?;
    sort_entries(&mut entries, opts);
    let mut walk = LsWalk {
        shown: 0,
        limit: usize::MAX,
        capped: false,
        depth_capped: false,
        err: String::new(),
    };
    let mut out = String::new();
    write_listing(&mut out, &entries, total, dirs, files, capped, opts, &mut walk);
    Ok(out)
}

/// `None` declines the whole command (a symlink in a long listing).
fn list_recursive(
    path: &Path,
    display: &str,
    opts: &LsOpts,
    out: &mut String,
    walk: &mut LsWalk,
    depth: usize,
) -> Option<()> {
    if walk.capped {
        return Some(());
    }
    if depth > LS_MAX_DEPTH {
        walk.depth_capped = true;
        return Some(());
    }

    let (mut entries, total, dirs, files, capped) = match collect_entries(path, opts) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => return None,
        Err(e) => {
            let _ = writeln!(walk.err, "ls: {}: {e}", path.display());
            return Some(());
        }
    };
    sort_entries(&mut entries, opts);
    let _ = writeln!(out, "{display}:");
    write_listing(out, &entries, total, dirs, files, capped, opts, walk);
    if walk.capped {
        return Some(());
    }

    let children: Vec<String> = entries
        .iter()
        .filter(|e| e.is_dir)
        .map(|e| e.name.clone())
        .collect();
    for name in children {
        if walk.capped {
            return Some(());
        }
        let child = path.join(&name);
        let child_display = Path::new(display).join(&name).display().to_string();
        list_recursive(&child, &child_display, opts, out, walk, depth + 1)?;
    }
    Some(())
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
        let ft = e.file_type().ok();
        // `-l` would show the target's size and type, not the link: `Unsupported` makes ls decline.
        if opts.long && ft.is_some_and(|t| t.is_symlink()) {
            return Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
        }
        let is_dir = ft.is_some_and(|t| t.is_dir());
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
        let readonly = md.as_ref().is_some_and(|m| m.permissions().readonly());
        entries.push(LsEntry {
            lower: name.to_ascii_lowercase(),
            name,
            is_dir,
            len,
            modified,
            readonly,
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
    let style = LongStyle::new(full, entries.iter().map(|e| (e.len, e.modified, e.is_dir)));
    if opts.long && n > 0 {
        out.push_str(&style.header());
    }
    if full || n <= LS_SAMPLE_HEAD + LS_SAMPLE_TAIL {
        for e in entries {
            if !take_line(walk) {
                break;
            }
            write_entry(out, &e.name, e.is_dir, e.len, e.modified, e.readonly, opts.long, &style);
        }
    } else {
        for e in &entries[..LS_SAMPLE_HEAD] {
            if !take_line(walk) {
                break;
            }
            write_entry(out, &e.name, e.is_dir, e.len, e.modified, e.readonly, opts.long, &style);
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
                write_entry(out, &e.name, e.is_dir, e.len, e.modified, e.readonly, opts.long, &style);
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

/// Layout of one long listing. Compact (default): no synthetic mode string, size padded to the
/// widest file size (directories print none), zone plus any shared date and minute stated once in a
/// header line. `full` keeps the classic `-rw-r--r-- <size:10> <date time UTC> name` rows.
struct LongStyle {
    full: bool,
    width: usize,
    /// Date shared by every entry; rows then carry only `HH:MM`.
    date: Option<String>,
    /// `HH:MM` shared by every entry (implies a shared date); rows then carry no time at all.
    minute: Option<String>,
}

impl LongStyle {
    fn new<I: IntoIterator<Item = (u64, Option<SystemTime>, bool)>>(full: bool, items: I) -> LongStyle {
        let mut width = 1;
        let mut date: Option<String> = None;
        let mut minute: Option<String> = None;
        let (mut same_date, mut same_minute) = (true, true);
        for (len, modified, is_dir) in items {
            if !is_dir {
                width = width.max(len.to_string().len());
            }
            let t = modified.unwrap_or(UNIX_EPOCH);
            let (d, m) = (ymd_string(t), hm_string(t));
            match &date {
                None => date = Some(d),
                Some(prev) if *prev != d => same_date = false,
                _ => {}
            }
            match &minute {
                None => minute = Some(m),
                Some(prev) if *prev != m => same_minute = false,
                _ => {}
            }
        }
        let date = if same_date { date } else { None };
        let minute = if same_date && same_minute { minute } else { None };
        LongStyle { full, width, date, minute }
    }

    /// One line stating what the rows omit; empty in full mode.
    fn header(&self) -> String {
        match (&self.date, &self.minute, self.full) {
            (_, _, true) => String::new(),
            (Some(d), Some(m), false) => format!("(UTC {d} {m})\n"),
            (Some(d), None, false) => format!("(UTC {d})\n"),
            (None, _, false) => "(times UTC)\n".to_string(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_entry(
    out: &mut String,
    name: &str,
    is_dir: bool,
    len: u64,
    modified: Option<SystemTime>,
    readonly: bool,
    long: bool,
    style: &LongStyle,
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
    let exec = !is_dir && matches!(ext.as_str(), "exe" | "cmd" | "bat" | "ps1" | "com");
    let t = modified.unwrap_or(UNIX_EPOCH);
    if style.full {
        // On Windows a folder's read-only attribute is only a marker; it does not stop writes into it.
        let w = if readonly && !is_dir { '-' } else { 'w' };
        let mode = if is_dir {
            format!("dr{w}xr-xr-x")
        } else if exec {
            format!("-r{w}xr-xr-x")
        } else {
            format!("-r{w}-r--r--")
        };
        let _ = write!(out, "{mode} {len:>10} ");
        write_time(out, t);
        out.push(' ');
        out.push_str(name);
        out.push('\n');
        return;
    }
    // A directory's size is always 0 on Windows: it carries nothing, so the column shows `-`. Never empty:
    // with the time hoisted into the header, `2024 notes/` would read as a 2024-byte entry `notes/`.
    if is_dir {
        let _ = write!(out, "{:>w$} ", "-", w = style.width);
    } else {
        let _ = write!(out, "{len:>w$} ", w = style.width);
    }
    if style.minute.is_none() {
        if style.date.is_none() {
            out.push_str(&ymd_string(t));
            out.push(' ');
        }
        out.push_str(&hm_string(t));
        out.push(' ');
    }
    out.push_str(name);
    // ls -F style markers: `/` directory, `*` executable; ` [ro]` only for a set read-only bit.
    if is_dir {
        out.push('/');
    } else if exec {
        out.push('*');
    }
    if readonly && !is_dir {
        out.push_str(" [ro]");
    }
    out.push('\n');
}

fn write_time(out: &mut String, t: SystemTime) {
    out.push_str(&format_time(t));
}

fn ymd_hm(t: SystemTime) -> (i64, u32, u32, u64, u64) {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u64;
    let (y, m, d) = civil_from_days(days);
    (y, m, d, rem / 3600, (rem % 3600) / 60)
}

fn ymd_string(t: SystemTime) -> String {
    let (y, m, d, _, _) = ymd_hm(t);
    format!("{y:04}-{m:02}-{d:02}")
}

fn hm_string(t: SystemTime) -> String {
    let (_, _, _, h, mi) = ymd_hm(t);
    format!("{h:02}:{mi:02}")
}

pub(crate) fn format_time(t: SystemTime) -> String {
    format!("{} {} UTC", ymd_string(t), hm_string(t))
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
    Ok((crate::exec::decode_owned(buf), cut))
}

/// A whole file for `cat`. Past the cap it keeps the start and the last 2 MiB (a log's failure is at its
/// end) with a marker for the middle, as a command's captured output does; the flag says it did.
fn read_text_ends(path: &Path) -> std::io::Result<(String, bool)> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut bom = [0u8; 2];
    let got = f.read(&mut bom)?;
    f.seek(SeekFrom::Start(0))?;
    // UTF-16 has to be decoded from its start, so it keeps the plain cap.
    if len <= READ_CAP || (got == 2 && (bom == [0xFF, 0xFE] || bom == [0xFE, 0xFF])) {
        drop(f);
        let (mut text, cut) = read_text_capped(path)?;
        if cut {
            text.push_str("\n[qx] cat truncated at 32 MiB\n");
        }
        return Ok((text, cut));
    }
    let tail_len = READ_CAP / 16;
    let mut head = Vec::new();
    // 256 bytes of room for the marker line, so the joined text always fits under the cap `cat` enforces
    // afterwards (that cut would otherwise take the end this function exists to keep).
    f.by_ref().take(READ_CAP - tail_len - 256).read_to_end(&mut head)?;
    let mut tail = Vec::new();
    f.seek(SeekFrom::Start(len - tail_len))?;
    f.take(tail_len).read_to_end(&mut tail)?;
    // Whole lines on both sides of the gap.
    if let Some(i) = head.iter().rposition(|&b| b == b'\n') {
        head.truncate(i + 1);
    }
    if let Some(i) = tail.iter().position(|&b| b == b'\n') {
        tail.drain(..=i);
    }
    let omitted = len - head.len() as u64 - tail.len() as u64;
    head.extend_from_slice(format!("[qx] ... {omitted} bytes omitted (cat reads at most 32 MiB) ...\n").as_bytes());
    head.extend_from_slice(&tail);
    Ok((crate::exec::decode_owned(head), true))
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
        match read_text_ends(&path) {
            Ok((text, _cut)) => {
                if number {
                    // Each line keeps its own terminator: CRLF stays CRLF, a missing final newline stays missing.
                    for line in text.split_inclusive('\n') {
                        n += 1;
                        let _ = write!(out, "{n:>6}\t{line}");
                    }
                } else if out.is_empty() {
                    out = text; // the common case: one file, no second copy
                } else {
                    out.push_str(&text);
                }
                if out.len() > READ_CAP as usize {
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

/// `-n` counts lines, `-c` counts bytes; the last one on the line wins, as in GNU.
enum Window {
    Lines(usize),
    Bytes(usize),
}

fn cmd_head(args: &[String], cwd: Option<&Path>, tail: bool) -> Option<BuiltinOut> {
    let mut window = Window::Lines(10);
    let mut files: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let spec = if a == "-n" || a == "--lines" {
            i += 1;
            Some((false, args.get(i)?.as_str()))
        } else if a == "-c" || a == "--bytes" {
            i += 1;
            Some((true, args.get(i)?.as_str()))
        } else if let Some(rest) = a.strip_prefix("--lines=") {
            Some((false, rest))
        } else if let Some(rest) = a.strip_prefix("--bytes=") {
            Some((true, rest))
        } else if let Some(rest) = a.strip_prefix("-n") {
            Some((false, rest))
        } else {
            a.strip_prefix("-c").map(|rest| (true, rest))
        };
        match spec {
            Some((bytes, rest)) => {
                let n = count_arg(rest)?;
                window = if bytes { Window::Bytes(n) } else { Window::Lines(n) };
                i += 1;
            }
            None if (a.starts_with('-') && a.len() > 1) || wildcard(a) => return None,
            None => {
                files.push(a);
                i += 1;
            }
        }
    }
    if files.len() != 1 {
        return None;
    }
    let path = path_of(cwd, files[0]);
    let tool = if tail { "tail" } else { "head" };
    if let Window::Bytes(n) = window {
        return match read_bytes_window(&path, n, tail) {
            Ok(Some(t)) => Some(ok(t)),
            // Raw bytes cannot survive the reply format: a binary window runs for real.
            Ok(None) => None,
            Err(e) => Some(fail(format!("{tool}: {}: {e}", path.display()))),
        };
    }
    let Window::Lines(count) = window else { return None };
    match read_head_tail(&path, count, tail) {
        Ok(Some(t)) => Some(ok(t)),
        // Too big for the 32 MiB window: the real tool prints all of it.
        Ok(None) => None,
        Err(e) => Some(fail(format!("{tool}: {}: {e}", path.display()))),
    }
}

/// `-c`: a byte window, text only. A window that is binary or not valid UTF-8 is left to the
/// real tool, because mangled bytes would be worse than a spawn.
fn read_bytes_window(path: &Path, n: usize, tail: bool) -> std::io::Result<Option<String>> {
    // GNU tail does not open a file it will print nothing of; head does, and fails on a missing one.
    if n == 0 && tail {
        return Ok(Some(String::new()));
    }
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let want = len.min(n as u64);
    // Past the cap the window would be cut short; the real tool prints all of it.
    if want > READ_CAP {
        return Ok(None);
    }
    let start = if tail { len - want } else { 0 };
    let mut buf = vec![0u8; want as usize];
    f.seek(SeekFrom::Start(start))?;
    f.read_exact(&mut buf)?;
    if buf.contains(&0) {
        return Ok(None);
    }
    Ok(String::from_utf8(buf).ok())
}

/// Read only what `head`/`tail` needs, from a 32 MiB window. `None` when the window is too small
/// to hold the answer, so the real tool answers instead of a silently short result.
fn read_head_tail(path: &Path, count: usize, tail: bool) -> std::io::Result<Option<String>> {
    if count == 0 && tail {
        return Ok(Some(String::new()));
    }
    let mut f = File::open(path)?;
    if count == 0 {
        return Ok(Some(String::new()));
    }
    // UTF-16 (PowerShell 5.1 `>`) has to be decoded whole: a line slice or a mid-file window has no BOM.
    let mut bom = [0u8; 2];
    let got = f.read(&mut bom)?;
    f.seek(SeekFrom::Start(0))?;
    if got == 2 && (bom == [0xFF, 0xFE] || bom == [0xFE, 0xFF]) {
        let (text, cut) = read_text_capped(path)?;
        if cut {
            return Ok(None);
        }
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let pick = if tail { &lines[lines.len().saturating_sub(count)..] } else { &lines[..lines.len().min(count)] };
        return Ok(Some(pick.concat()));
    }
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
            // Verbatim: output can be redirected to a file, so CRLF and a missing final newline survive.
            out.push_str(&crate::exec::decode(&buf));
        }
        if seen < count && len > READ_CAP && reader.get_ref().limit() == 0 {
            return Ok(None);
        }
        return Ok(Some(out));
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
    if pos > 0 && newlines <= count {
        return Ok(None);
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

    let mut ring: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
    for line in text.split_inclusive('\n') {
        if ring.len() == count {
            ring.pop_front();
        }
        ring.push_back(line);
    }
    Ok(Some(ring.into_iter().collect()))
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
    let (lines, words, bytes, ascii) = match count_file(&path) {
        Ok(v) => v,
        Err(e) => return Some(fail(format!("wc: {}: {e}", path.display()))),
    };
    let any = lines_f || words_f || bytes_f;
    // GNU counts words by locale whitespace (U+00A0 and friends in UTF-8); only ASCII is exact here.
    if !ascii && (words_f || !any) {
        return None;
    }
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

/// `grep` for the shapes agents type: literal patterns with `-r`, `-n`, `-i`, `-l`, `-v`,
/// `-q`, `-c`, `-e`, `-A/-B/-C` and `--include=*.ext`. Everything else - a real regex, stdin,
/// an unreadable path, a binary or >32 MiB file - returns `None` so the real grep
/// answers: a wrong line list costs more than the 50 ms process it saves.
fn cmd_grep(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    let mut insensitive = false;
    let mut number = false;
    let mut list_only = false;
    let mut recursive = false;
    let mut fixed = false;
    let mut invert = false;
    let mut quiet = false;
    let mut count_only = false;
    // GNU: an explicit -A/-B wins over -C, and any of them (even 0) turns on `--` separators.
    let mut before: Option<usize> = None;
    let mut after: Option<usize> = None;
    let mut around: Option<usize> = None;
    let mut include: Option<String> = None;
    let mut pattern: Option<String> = None;
    // A second -e or --include adds to the first in GNU grep; this scan holds one of each.
    fn set_once(slot: &mut Option<String>, v: String) -> Option<()> {
        if slot.is_some() {
            return None;
        }
        *slot = Some(v);
        Some(())
    }
    // GNU grep takes options before or after the pattern and files; the words are what is left.
    let mut words: Vec<&String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            words.extend(&args[i + 1..]);
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
                    set_once(&mut include, v.to_string())?;
                }
                "regexp" => set_once(&mut pattern, val?.to_string())?,
                "invert-match" => invert = true,
                "quiet" | "silent" => quiet = true,
                "fixed-strings" => fixed = true,
                "count" => count_only = true,
                "after-context" => after = Some(count_arg(val?)?),
                "before-context" => before = Some(count_arg(val?)?),
                "context" => around = Some(count_arg(val?)?),
                "line-number" => number = true,
                "files-with-matches" => list_only = true,
                "recursive" => recursive = true,
                "dereference-recursive" => return None,
                // Colour is never emitted (`always` would); buffering is not observable in captured
                // output.
                "color" | "colour" if matches!(val, None | Some("never" | "auto")) => {}
                "line-buffered" => {}
                _ => return None,
            }
        } else if a.len() > 1 && a.starts_with('-') {
            let body = &a[1..];
            if body.chars().all(|c| c.is_ascii_digit()) {
                // GNU's `-3` is `-C3`.
                around = Some(count_arg(body)?);
                i += 1;
                continue;
            }
            for (pos, ch) in body.char_indices() {
                match ch {
                    'i' => insensitive = true,
                    'n' => number = true,
                    'l' => list_only = true,
                    'r' => recursive = true,
                    // -R follows symlinks and junctions; the walk never does.
                    'R' => return None,
                    'F' => fixed = true,
                    'v' => invert = true,
                    'q' => quiet = true,
                    'c' => count_only = true,
                    // qx reports no per-file errors, so silence changes nothing.
                    's' => {}
                    'e' => {
                        // `-e pat`, `-epat` and `-ne pat` all name the pattern.
                        let rest = &body[pos + ch.len_utf8()..];
                        let p = if rest.is_empty() {
                            i += 1;
                            args.get(i)?.clone()
                        } else {
                            rest.to_string()
                        };
                        set_once(&mut pattern, p)?;
                        break;
                    }
                    'A' | 'B' | 'C' => {
                        // `-A2`, `-A 2` and `-nA2` all count the same way.
                        let rest = &body[pos + ch.len_utf8()..];
                        let n = if rest.is_empty() {
                            i += 1;
                            args.get(i)?.clone()
                        } else {
                            rest.to_string()
                        };
                        let n = Some(count_arg(&n)?);
                        match ch {
                            'A' => after = n,
                            'B' => before = n,
                            _ => around = n,
                        }
                        break;
                    }
                    _ => return None,
                }
            }
        } else {
            words.push(&args[i]);
        }
        i += 1;
    }
    let context = before.is_some() || after.is_some() || around.is_some();
    let bctx = before.or(around).unwrap_or(0);
    let actx = after.or(around).unwrap_or(0);
    if context && (count_only || list_only) {
        // GNU answers these with a "context is ignored" warning; qx words warnings badly.
        return None;
    }
    let mut words = words.into_iter();
    let pattern = match pattern {
        Some(p) => p,
        None => words.next()?.clone(),
    };
    let pattern = pattern.as_str();
    // A newline separates patterns, with `-F` too.
    if pattern.is_empty() || pattern == "-" || pattern.contains('\n') || (!fixed && needs_regex(pattern)) {
        return None;
    }
    // GNU applies `--include` to the files named on the command line as well as to a walk.
    let included = |file: &Path| {
        include.as_ref().is_none_or(|g| file.file_name().is_some_and(|n| n.to_string_lossy().ends_with(&g[1..])))
    };
    let mut specs: Vec<(&str, PathBuf)> = Vec::new();
    let operands: Vec<&String> = words.collect();
    if operands.is_empty() {
        if !recursive {
            // No path means stdin, which qx never gives a child.
            return None;
        }
        // `grep -r x` searches `.` but names files without a `./` prefix.
        specs.push(("", path_of(cwd, ".")));
    } else {
        for raw in operands {
            // `-` is stdin, and a leading `/` may be a POSIX-style path path_of cannot place.
            if raw == "-" || raw.starts_with('/') {
                return None;
            }
            specs.push((raw.as_str(), path_of(cwd, raw)));
        }
    }
    // Decline on any operand qx cannot answer before the first walk, not after it.
    for (_, path) in &specs {
        fs::metadata(path).ok().filter(|m| m.is_file() || (m.is_dir() && recursive))?;
    }

    // Expand to files in the spelling grep would print: `grep -r x src` says `src/a.rs`.
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let mut walked = false;
    for (raw, path) in &specs {
        let md = fs::metadata(path).ok()?;
        if md.is_file() {
            if included(path) {
                files.push((raw.to_string(), path.clone()));
            }
            continue;
        }
        if !md.is_dir() || !recursive {
            // `grep x dir` is an error grep words better than qx can.
            return None;
        }
        walked = true;
        let base = raw.trim_end_matches(['/', '\\']);
        let mut full = false;
        // A walk that skipped anything (too deep, unreadable, too many files) is not an answer:
        // "no match" from half a tree misleads.
        let complete = crate::fsops::walk_every_file(path, |file| {
            if files.len() >= MAX_GREP_FILES || crate::exec::should_stop() {
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
            if !included(file) {
                return true;
            }
            let disp = if base.is_empty() {
                rel
            } else {
                format!("{base}/{rel}")
            };
            files.push((disp, file.to_path_buf()));
            true
        });
        if full || !complete {
            return None;
        }
    }
    if files.len() > MAX_GREP_FILES {
        return None;
    }

    // grep names files when given several operands or a directory to walk, even for a single hit
    // file; `grep -r x a.txt` does not.
    let multiple = walked || specs.len() > 1;
    // One case-insensitive matcher for the run, not a lowercased copy of every line.
    let ci = if insensitive {
        Some(regex::RegexBuilder::new(&regex::escape(pattern)).case_insensitive(true).build().ok()?)
    } else {
        None
    };
    let found_in = |s: &str| match &ci {
        Some(re) => re.is_match(s),
        None => s.contains(pattern),
    };
    let mut out = String::new();
    let mut hits = 0usize;
    let mut printed = false; // anything printed yet, across files: GNU separates groups of different files
    'files: for (disp, path) in &files {
        if crate::exec::should_stop() {
            return None;
        }
        // Size first: a multi-gigabyte log must not be read just to be declined.
        if fs::metadata(path).ok()?.len() > READ_CAP {
            return None;
        }
        let bytes = fs::read(path).ok()?;
        // GNU prints "Binary file ... matches" for a NUL anywhere, or bytes that are not text in
        // a UTF-8 locale; qx declines whenever that message could be the answer, and otherwise
        // the file is a plain miss (`.git` objects). An ASCII needle survives lossy decoding.
        let text = match std::str::from_utf8(&bytes) {
            Ok(t) if !bytes.contains(&0) => t,
            _ => {
                if invert || !pattern.is_ascii() {
                    return None;
                }
                let lossy: &str = &String::from_utf8_lossy(&bytes);
                let hit = found_in(lossy);
                if hit {
                    return None;
                }
                ""
            }
        };
        // `split_inclusive` keeps a trailing newline from inventing an empty last line, which
        // `-v` would otherwise report.
        let raw_lines: Vec<&str> = text.split_inclusive('\n').collect();
        let mut is_sel = vec![false; raw_lines.len()];
        let mut selected = Vec::new();
        for (idx, raw) in raw_lines.iter().enumerate() {
            let line = raw.strip_suffix('\n').unwrap_or(raw);
            let line = line.strip_suffix('\r').unwrap_or(line);
            let found = found_in(line);
            if found != invert {
                is_sel[idx] = true;
                selected.push(idx);
            }
        }
        if selected.is_empty() {
            // GNU counts every file it searched, zero included; `-l` wins over `-c`, and `-q` prints nothing.
            if count_only && !list_only && !quiet {
                if multiple {
                    out.push_str(disp);
                    out.push(':');
                }
                out.push_str("0\n");
            }
            continue;
        }
        hits += selected.len();
        if quiet {
            // `grep -q`: the exit code is the whole answer.
            break 'files;
        }
        if list_only {
            out.push_str(disp);
            out.push('\n');
            continue;
        }
        if count_only {
            if multiple {
                out.push_str(disp);
                out.push(':');
            }
            let _ = writeln!(out, "{}", selected.len());
            continue;
        }
        // `-A`/`-B`/`-C`: matches print with `:` separators, context lines with `-`, and `--`
        // marks where the printed groups are not adjacent.
        let mut show = vec![false; raw_lines.len()];
        for &i in &selected {
            let from = i.saturating_sub(bctx);
            let to = (i + actx).min(raw_lines.len() - 1);
            for s in &mut show[from..=to] {
                *s = true;
            }
        }
        let mut last: Option<usize> = None;
        for (idx, raw) in raw_lines.iter().enumerate() {
            if !show[idx] {
                continue;
            }
            let new_group = match last {
                Some(p) => p + 1 != idx,
                None => printed, // first line of this file, after another file's lines
            };
            if context && new_group {
                out.push_str("--\n");
            }
            printed = true;
            last = Some(idx);
            let sep = if is_sel[idx] { ':' } else { '-' };
            let line = raw.strip_suffix('\n').unwrap_or(raw);
            if multiple {
                out.push_str(disp);
                out.push(sep);
            }
            if number {
                let _ = write!(out, "{}", idx + 1);
                out.push(sep);
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
/// Characters that make the pattern a regex, or (a newline) several patterns. A literal scan
/// must never answer these.
fn needs_regex(p: &str) -> bool {
    p.chars().any(|c| ".[]()*+?^${}|\\\n".contains(c))
}

/// The one `--include` shape qx can honour exactly: `*.ext`.
fn simple_include(g: &str) -> bool {
    match g.strip_prefix("*.") {
        Some(ext) => !ext.is_empty() && !ext.chars().any(|c| "*?[]\\/".contains(c)),
        None => false,
    }
}

/// Stream the file so `wc` on a multi-gigabyte log stays bounded.
fn count_file(path: &Path) -> std::io::Result<(usize, usize, usize, bool)> {
    let mut f = File::open(path)?;
    let mut buf = [0u8; 64 * 1024];
    let (mut lines, mut words, mut bytes) = (0usize, 0usize, 0usize);
    let mut in_word = false;
    let mut ascii = true;
    loop {
        if crate::exec::should_stop() {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "stopped (cancelled or timed out)"));
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes += n;
        for &b in &buf[..n] {
            if b == b'\n' {
                lines += 1;
            }
            ascii &= b.is_ascii();
            // C `isspace` also counts the vertical tab, which `is_ascii_whitespace` leaves out.
            if b.is_ascii_whitespace() || b == 0x0B {
                in_word = false;
            } else if !in_word {
                in_word = true;
                words += 1;
            }
        }
    }
    Ok((lines, words, bytes, ascii))
}

/// `sed -n 'A,Bp'` (and `-n Ap`) prints one line range. Every other script, flag, `$`
/// address or multi-address form returns `None`: a wrong slice costs more than the 60 ms
/// process it saves, and agents read files with exactly this shape.
fn cmd_sed(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    let mut quiet = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-n" | "--quiet" | "--silent" => quiet = true,
            "--" => {
                i += 1;
                break;
            }
            _ if a.starts_with('-') && a.len() > 1 => return None,
            _ => break,
        }
        i += 1;
    }
    if !quiet {
        // `sed '1,5p'` also prints the whole file; that shape is not answered here.
        return None;
    }
    let body = args.get(i)?.trim().strip_suffix('p')?;
    i += 1;
    let (from, to) = match body.split_once(',') {
        Some((a, b)) => (count_arg(a)?, count_arg(b)?),
        None => {
            let n = count_arg(body)?;
            (n, n)
        }
    };
    if from == 0 || to < from {
        return None;
    }
    let mut paths: Vec<&str> = Vec::new();
    for raw in &args[i..] {
        if raw == "-" || raw.starts_with('-') || wildcard(raw) {
            return None;
        }
        paths.push(raw.as_str());
    }
    if paths.is_empty() {
        // No file means stdin, which qx never gives a child.
        return None;
    }
    let mut out = String::new();
    let mut line_no = 0usize;
    // The last line printed had no newline (end of a file): GNU sed adds it once more output follows.
    let mut owed_newline = false;
    let mut emit = |line: &str, out: &mut String| {
        let has_nl = line.ends_with('\n');
        let line = line.strip_suffix('\n').unwrap_or(line);
        if owed_newline {
            out.push('\n');
        }
        out.push_str(line);
        if has_nl {
            out.push('\n');
        }
        // The last line of the input keeps whatever terminator it had.
        owed_newline = !has_nl;
    };
    for raw in &paths {
        let path = path_of(cwd, raw);
        let md = fs::metadata(&path).ok()?;
        if !md.is_file() {
            return None;
        }
        if md.len() > READ_CAP {
            // Past the cap: stream the top of the file and stop at the range. A range that does not end
            // inside the first READ_CAP bytes, or a binary file, is the real tool's.
            let mut reader = BufReader::new(fs::File::open(&path).ok()?.take(READ_CAP));
            let mut buf = Vec::new();
            while line_no < to {
                buf.clear();
                if reader.read_until(b'\n', &mut buf).ok()? == 0 {
                    break;
                }
                if buf.contains(&0) || (!buf.ends_with(b"\n") && reader.get_ref().limit() == 0) {
                    return None;
                }
                line_no += 1;
                if line_no >= from {
                    emit(&crate::exec::decode(&buf), &mut out);
                }
            }
            if line_no < to && reader.get_ref().limit() == 0 {
                return None;
            }
            continue;
        }
        let bytes = fs::read(&path).ok()?;
        if bytes[..bytes.len().min(8192)].contains(&0) {
            return None;
        }
        let text = crate::exec::decode(&bytes);
        for line in text.split_inclusive('\n') {
            line_no += 1;
            if line_no > to {
                break;
            }
            if line_no >= from {
                emit(line, &mut out);
            }
        }
    }
    Some(ok(out))
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
    // cmd's mkdir makes the missing parents of a `\` path and GNU's refuses: the real one decides.
    if !parents && paths.iter().any(|p| p.contains('\\') && !path_of(cwd, p).parent().is_some_and(Path::is_dir)) {
        return None;
    }
    let mut err = String::new();
    let mut code = 0;
    for p in paths {
        let path = path_of(cwd, p);
        let res = if parents { fs::create_dir_all(&path) } else { fs::create_dir(&path) };
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

/// Paths `rm` never removes: a drive root, the user profile, and the directory the command
/// runs in (the call's `cwd` as well as the process's own).
fn dangerous(path: &Path, call_cwd: Option<&Path>) -> bool {
    let s = norm_path(path);
    if s.is_empty() || (s.len() == 2 && s.ends_with(':')) {
        return true;
    }
    if call_cwd.is_some_and(|c| s == norm_path(c)) {
        return true;
    }
    if let Some(home) = crate::resolve::home_dir() {
        if s == norm_path(&home) {
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
    // After `--` every word is a name, so `rm -- -r x` never turns recursive.
    let mut options = true;
    for a in args {
        if options && a == "--" {
            options = false;
            continue;
        }
        if options && a.starts_with('-') && a.len() > 1 {
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
        // Like GNU rm: `.` and `..` are never removed, however they are spelled.
        let dot = matches!(p.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next(), Some("." | ".."));
        if dot || dangerous(&path, cwd) {
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
                let link = fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
                // A link to a directory: std removes the link only. A real tree: entry by entry,
                // so a cancel or timeout can stop it.
                if link { fs::remove_dir_all(&path) } else { remove_tree(&path) }
            } else {
                Err(std::io::Error::other(
                    "is a directory (use -r)",
                ))
            }
        } else {
            fs::remove_file(&path)
        };
        if let Err(e) = res {
            if e.kind() == std::io::ErrorKind::Interrupted {
                return Some(stopped());
            }
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
    if crate::exec::should_stop() {
        return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "stopped (cancelled or timed out)"));
    }
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
        // CopyFileExW keeps the source's old write time; GNU `cp` stamps now, and make/cargo rely on that.
        if let Ok(f) = fs::OpenOptions::new().write(true).open(dst) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
        Ok(())
    }
}

/// A symlink or junction anywhere under `p`, `p` included: GNU `cp -r` copies it as a link and this
/// builtin would copy its target, so such a tree is left to the real tool.
fn has_link(p: &Path, depth: usize) -> bool {
    let Ok(md) = fs::symlink_metadata(p) else { return false };
    if md.file_type().is_symlink() {
        return true;
    }
    md.is_dir() && (depth > 64 || fs::read_dir(p).into_iter().flatten().flatten().any(|e| has_link(&e.path(), depth + 1)))
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
    if recursive && srcs.iter().any(|s| has_link(&path_of(cwd, s), 0)) {
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
        // `cp -r src/. dst` copies what is inside `src`; `file_name()` would drop the `.` and nest it.
        let contents = *s == "." || s.ends_with("/.") || s.ends_with("\\.");
        let target = if dst.is_dir() && !contents {
            dst.join(src.file_name().unwrap_or_default())
        } else {
            dst.clone()
        };
        if src.is_dir() && target.is_file() {
            err.push_str(&format!("cp: cannot overwrite non-directory '{}' with directory '{}'\n", target.display(), src.display()));
            code = 1;
            continue;
        }
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

/// The copy half of a move across volumes (the caller deletes the source afterwards), with GNU mv's
/// rules: a non-empty target directory is refused, an empty one is replaced, a directory never
/// replaces a file nor a file a directory. The copy goes to a sibling first and is renamed into place
/// only once complete, so a failure (a locked file, a full disk) never leaves a half tree behind.
fn move_across(src: &Path, target: &Path) -> std::io::Result<()> {
    let refuse = |why: &str| Err(std::io::Error::other(why.to_string()));
    let src_dir = fs::symlink_metadata(src)?.is_dir();
    if has_link(src, 0) {
        return refuse("a link would be copied as its target; use the real mv");
    }
    if let Ok(md) = fs::symlink_metadata(target) {
        match (src_dir, md.is_dir()) {
            (true, true) if fs::read_dir(target)?.next().is_some() => return refuse("target directory is not empty"),
            (true, true) => fs::remove_dir(target)?,
            (true, false) => return refuse("cannot overwrite a non-directory with a directory"),
            (false, true) => return refuse("cannot overwrite a directory with a non-directory"),
            (false, false) => {}
        }
    }
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = target.with_file_name(format!(".{name}.qx-mv-{}", std::process::id()));
    let cleanup = |t: &Path| {
        let _ = if src_dir { fs::remove_dir_all(t) } else { fs::remove_file(t) };
    };
    if let Err(e) = copy_rec(src, &temp, 0) {
        cleanup(&temp);
        return Err(e);
    }
    fs::rename(&temp, target).inspect_err(|_| cleanup(&temp))
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
            if let Err(e2) = move_across(&src, &target) {
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
    // A folder cannot be opened for append; the real touch updates its time.
    if args.iter().any(readonly) || args.iter().any(|a| path_of(cwd, a).is_dir()) {
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
    let mut err = String::new();
    for a in args {
        let p = Path::new(a);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else if let Some(c) = cwd {
            c.join(p)
        } else {
            p.to_path_buf()
        };
        // GNU prints the path of a file about to be created: only the last component may be missing.
        let resolved = fs::canonicalize(&abs).or_else(|e| match (abs.parent(), abs.file_name()) {
            (Some(dir), Some(name)) if e.kind() == std::io::ErrorKind::NotFound => fs::canonicalize(dir).map(|d| d.join(name)),
            _ => Err(e),
        });
        match resolved {
            Ok(c) => {
                let s = c.display().to_string();
                let trimmed = s.strip_prefix(r"\\?\").unwrap_or(&s);
                out.push_str(&format!("{trimmed}\n"));
            }
            // Like GNU: a failed operand does not hide the ones that resolved.
            Err(e) => {
                let _ = writeln!(err, "realpath: {a}: {e}");
            }
        }
    }
    Some(BuiltinOut { code: i32::from(!err.is_empty()), stdout: out, stderr: err, elapsed: Duration::ZERO })
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
    Some(ok(line.to_lowercase()))
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
    // The epsilon keeps `seq 0.1 0.1 0.3` from losing 0.3 to floating-point division.
    let count = ((last - first) / step + 1e-9).floor() + 1.0;
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
    fn seq_keeps_its_last_value_and_realpath_accepts_a_new_file() {
        let lines = |a: &[&str]| {
            let a: Vec<String> = a.iter().map(|s| s.to_string()).collect();
            try_exec("seq", &a, None, &[]).unwrap().stdout.lines().count()
        };
        assert_eq!(lines(&["0.1", "0.1", "0.3"]), 3);
        assert_eq!(lines(&["0", "0.1", "0.3"]), 4);
        assert_eq!(lines(&["1", "3"]), 3);
        let here = std::env::current_dir().unwrap();
        let o = try_exec("realpath", &["not-yet-created.txt".into()], Some(&here), &[]).unwrap();
        assert_eq!(o.code, 0, "{}", o.stderr);
        assert!(o.stdout.trim_end().ends_with("not-yet-created.txt"));
        let o = try_exec("realpath", &["no-such-dir\\x.txt".into()], Some(&here), &[]).unwrap();
        assert_eq!(o.code, 1);
        // A failed operand does not hide the one that resolved.
        let o = try_exec("realpath", &[".".into(), "no-such-dir\\x.txt".into()], Some(&here), &[]).unwrap();
        assert_eq!((o.code, o.stdout.lines().count()), (1, 1), "{} | {}", o.stdout, o.stderr);
        assert!(o.stderr.contains("no-such-dir"), "{}", o.stderr);
    }

    #[test]
    fn cp_stamps_the_copy_with_the_current_time() {
        let root = std::env::temp_dir().join(format!("qx-cpmtime-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let (a, b) = (root.join("a.txt"), root.join("b.txt"));
        fs::write(&a, "x").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(86_400);
        fs::OpenOptions::new().write(true).open(&a).unwrap().set_modified(old).unwrap();
        let o = try_exec("cp", &["a.txt".into(), "b.txt".into()], Some(&root), &[]).unwrap();
        assert_eq!(o.code, 0, "{}", o.stderr);
        let age = std::time::SystemTime::now().duration_since(fs::metadata(&b).unwrap().modified().unwrap()).unwrap();
        assert!(age < Duration::from_secs(3_600), "copy kept the source time: {age:?}");
        let _ = fs::remove_dir_all(&root);
    }

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
    fn ls_shows_file_operands_as_typed_keeps_going_after_a_bad_one_and_marks_read_only() {
        let d = tmp("ls-operands");
        fs::create_dir(d.join("a")).unwrap();
        fs::write(d.join("a").join("x.txt"), "x").unwrap();
        fs::write(d.join("y.txt"), "y").unwrap();
        let ro = d.join("ro.txt");
        fs::write(&ro, "r").unwrap();
        let mut perms = fs::metadata(&ro).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&ro, perms).unwrap();

        let args: Vec<String> = ["a/x.txt", "nope", "y.txt", "a"].map(String::from).to_vec();
        let out = cmd_ls(&args, Some(&d), false).unwrap();
        assert_eq!(out.code, 1);
        assert!(out.stderr.contains("nope"), "{}", out.stderr);
        assert_eq!(out.stdout, "a/x.txt\ny.txt\n\na:\nx.txt\n");
        // A file after a directory still comes first, so it never reads as one of the directory's entries.
        let dir_first: Vec<String> = ["a", "y.txt", "ro.txt"].map(String::from).to_vec();
        assert_eq!(cmd_ls(&dir_first, Some(&d), false).unwrap().stdout, "y.txt\nro.txt\n\na:\nx.txt\n");
        let long: Vec<String> = ["-l", "a", "y.txt", "ro.txt"].map(String::from).to_vec();
        let long = cmd_ls(&long, Some(&d), false).unwrap().stdout;
        assert_eq!(long.matches("(UTC ").count() + long.matches("(times UTC)").count(), 2, "one header per listing: {long}");
        let (files, dir) = long.split_once("\na:\n").unwrap();
        assert!(files.contains("y.txt\n") && files.contains("ro.txt [ro]") && dir.contains("x.txt"), "{long}");

        let long = cmd_ls(&["-l".to_string(), "ro.txt".to_string()], Some(&d), false).unwrap().stdout;
        assert!(long.starts_with("(UTC ") && long.trim_end().ends_with("ro.txt [ro]"), "{long}");
        assert!(!long.contains("-r-"), "{long}");

        let full = cmd_ls(&["-l".to_string(), "ro.txt".to_string()], Some(&d), true).unwrap().stdout;
        assert!(full.starts_with("-r-") && full.contains(" UTC "), "{full}");

        let mut perms = fs::metadata(&ro).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        let _ = fs::set_permissions(&ro, perms);
        let _ = fs::remove_dir_all(&d);
    }

    fn rendered(rows: &[(&str, bool, u64, u64, bool)], full: bool) -> String {
        let at = |s: u64| UNIX_EPOCH + std::time::Duration::from_secs(s);
        let style = LongStyle::new(full, rows.iter().map(|r| (r.2, Some(at(r.3)), r.1)));
        let mut out = style.header();
        for r in rows {
            write_entry(&mut out, r.0, r.1, r.2, Some(at(r.3)), r.4, true, &style);
        }
        out
    }

    #[test]
    fn long_listing_states_a_shared_date_once_and_pads_to_the_widest_size() {
        // 2021-01-01 00:00:00 UTC = 1_609_459_200
        let out = rendered(
            &[("a.txt", false, 5, 1_609_459_200, false), ("big.bin", false, 123_456, 1_609_459_200 + 3660, false)],
            false,
        );
        assert_eq!(out, "(UTC 2021-01-01)\n     5 00:00 a.txt\n123456 01:01 big.bin\n");
    }

    #[test]
    fn long_listing_keeps_full_dates_when_they_differ() {
        let out = rendered(
            &[("a", false, 1, 1_609_459_200, false), ("b", false, 22, 1_609_459_200 + 86_400, false)],
            false,
        );
        assert_eq!(out, "(times UTC)\n 1 2021-01-01 00:00 a\n22 2021-01-02 00:00 b\n");
    }

    #[test]
    fn long_listing_marks_dirs_exec_and_read_only_and_full_mode_stays_classic() {
        let rows = [
            ("d", true, 0, 1_609_459_200, false),
            ("r.txt", false, 1, 1_609_459_200, true),
            ("t.exe", false, 2, 1_609_459_200, false),
        ];
        let out = rendered(&rows, false);
        assert_eq!(out, "(UTC 2021-01-01 00:00)\n- d/\n1 r.txt [ro]\n2 t.exe*\n");
        let full = rendered(&rows, true);
        assert_eq!(
            full,
            "drwxr-xr-x          0 2021-01-01 00:00 UTC d\n-r--r--r--          1 2021-01-01 00:00 UTC r.txt\n-rwxr-xr-x          2 2021-01-01 00:00 UTC t.exe\n"
        );
    }

    #[test]
    fn long_listing_dir_rows_show_a_dash_and_do_not_widen_the_column() {
        let out = rendered(
            &[("d", true, 4096, 1_609_459_200, false), ("f", false, 10, 1_609_459_200 + 60, false)],
            false,
        );
        assert_eq!(out, "(UTC 2021-01-01)\n - 00:00 d/\n10 00:01 f\n");
        // With the time in the header too, a name that starts with a number still cannot read as a size.
        let out = rendered(&[("2024 notes", true, 0, 1_609_459_200, false), ("f", false, 3, 1_609_459_200, false)], false);
        assert_eq!(out, "(UTC 2021-01-01 00:00)\n- 2024 notes/\n3 f\n");
    }

    #[test]
    fn long_listing_hoists_a_shared_minute_into_the_header() {
        let out = rendered(
            &[("a", false, 7, 1_609_459_200 + 3660, false), ("b", false, 123, 1_609_459_200 + 3660 + 30, false)],
            false,
        );
        assert_eq!(out, "(UTC 2021-01-01 01:01)\n  7 a\n123 b\n");
        // Same minute on different days still needs per-row dates.
        let mixed = rendered(
            &[("a", false, 1, 1_609_459_200, false), ("b", false, 2, 1_609_459_200 + 86_400, false)],
            false,
        );
        assert!(mixed.starts_with("(times UTC)\n"), "{mixed}");
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
            read_head_tail(&path, 2, true).unwrap().as_deref(),
            Some("line19998\nline19999\n")
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn head_and_tail_decode_utf16_whole_and_decline_a_window_that_is_too_small() {
        let d = tmp("headtail-utf16");
        let path = d.join("u16.txt");
        // What PowerShell 5.1 `>` writes: BOM, UTF-16LE, CRLF.
        let mut bytes = vec![0xFF, 0xFE];
        for u in "one\r\nt\u{e9}w\u{f6}\r\nthree\r\n".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        fs::write(&path, bytes).unwrap();
        assert_eq!(read_head_tail(&path, 2, false).unwrap().as_deref(), Some("one\r\nt\u{e9}w\u{f6}\r\n"));
        assert_eq!(read_head_tail(&path, 2, true).unwrap().as_deref(), Some("t\u{e9}w\u{f6}\r\nthree\r\n"));

        // One 40 MiB line: neither end fits the 32 MiB window, so the real tool must answer.
        let big = d.join("big.txt");
        fs::write(&big, vec![b'x'; 40 * 1024 * 1024]).unwrap();
        assert_eq!(read_head_tail(&big, 5, true).unwrap(), None);
        assert_eq!(read_head_tail(&big, 5, false).unwrap(), None);
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
    fn mkdir_of_an_existing_folder_fails_with_a_backslash_path_too() {
        let d = tmp("mkdir-exists");
        fs::create_dir_all(d.join("sub")).unwrap();
        for p in ["sub", r".\sub"] {
            let r = cmd_mkdir(&[p.to_string()], Some(&d)).unwrap();
            assert_eq!(r.code, 1, "{p}: {}", r.stderr);
        }
        // A new nested path with a backslash: cmd's mkdir makes the parents and GNU's refuses, so the real one decides.
        assert!(cmd_mkdir(&[r"p\q".to_string()], Some(&d)).is_none());
        assert!(!d.join("p").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn rm_refuses_dangerous_paths() {
        let d = tmp("danger");
        assert!(dangerous(Path::new("C:\\"), None));
        assert!(dangerous(Path::new("."), None));
        assert!(!dangerous(&d, None));
        // the directory the call runs in, which is not the process cwd
        assert!(dangerous(&d, Some(&d)));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_move_across_volumes_never_merges_into_or_half_fills_a_target() {
        let root = tmp("mvacross");
        let (src, dst) = (root.join("src"), root.join("dst"));
        fs::create_dir_all(src.join("a")).unwrap();
        fs::write(src.join("a").join("f.txt"), "new").unwrap();
        // A non-empty target directory: GNU mv refuses; nothing may be overwritten.
        fs::create_dir_all(dst.join("a")).unwrap();
        fs::write(dst.join("a").join("f.txt"), "old").unwrap();
        fs::write(dst.join("a").join("g.txt"), "keep").unwrap();
        assert!(move_across(&src.join("a"), &dst.join("a")).is_err());
        assert_eq!(fs::read_to_string(dst.join("a").join("f.txt")).unwrap(), "old");
        assert!(fs::read_dir(&dst).unwrap().count() == 1, "no temp left behind");
        // An empty target directory is replaced, as GNU mv does.
        fs::remove_dir_all(dst.join("a")).unwrap();
        fs::create_dir(dst.join("a")).unwrap();
        move_across(&src.join("a"), &dst.join("a")).unwrap();
        assert_eq!(fs::read_to_string(dst.join("a").join("f.txt")).unwrap(), "new");
        // A directory never replaces a file, and a file never replaces a directory.
        fs::write(dst.join("file"), "x").unwrap();
        assert!(move_across(&src.join("a"), &dst.join("file")).is_err());
        assert!(move_across(&src.join("a").join("f.txt"), &dst.join("a")).is_err());
        // A file replaces a file.
        fs::write(src.join("one.txt"), "1").unwrap();
        move_across(&src.join("one.txt"), &dst.join("file")).unwrap();
        assert_eq!(fs::read_to_string(dst.join("file")).unwrap(), "1");
        assert!(fs::read_dir(&dst).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains("qx-mv")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rm_refuses_the_call_cwd_and_dot_operands() {
        let root = tmp("rmcwd");
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(root.join("f"), "x").unwrap();
        fs::write(sub.join("g"), "x").unwrap();
        let sep = std::path::MAIN_SEPARATOR;
        for operand in [
            ".".to_string(),
            format!("{}{sep}.", sub.display()),
            sub.to_string_lossy().into_owned(),
            "..".to_string(),
            // A trailing separator is still `.`/`..` (a failure here only deletes this temp tree).
            "../".to_string(),
            "..\\".to_string(),
            "./".to_string(),
        ] {
            let out = cmd_rm(&["-rf".into(), operand.clone()], Some(&sub)).unwrap();
            assert_eq!(out.code, 1, "{operand}: {}", out.stderr);
            assert!(root.join("f").exists() && sub.join("g").exists(), "{operand} removed files");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_declines_what_it_cannot_answer_exactly() {
        let d = scratch("grep-decline");
        fs::write(d.join("a.txt"), "foo\nbar\n").unwrap();
        // A second pattern or include used to replace the first.
        assert!(run("grep", &["-e", "foo", "-e", "bar", "a.txt"], &d).is_none());
        assert!(run("grep", &["-r", "--include=*.rs", "--include=*.txt", "foo", "."], &d).is_none());
        // -R follows links, which the walk never does.
        assert!(run("grep", &["-Rn", "foo", "."], &d).is_none());
        assert!(run("grep", &["--dereference-recursive", "foo", "."], &d).is_none());
        // A walk cut short (here: deeper than the walk goes) is not an answer.
        let mut deep = d.join("deep");
        for _ in 0..66 {
            deep = deep.join("d");
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("x.txt"), "needle\n").unwrap();
        assert!(run("grep", &["-rn", "needle", "deep"], &d).is_none());
        // `-F` splits on newlines too; `always` colours; a NUL past the first block, or bytes
        // that are not UTF-8, make GNU say "Binary file".
        assert!(run("grep", &["-F", "foo\nbar", "a.txt"], &d).is_none());
        assert!(run("grep", &["--color=always", "foo", "a.txt"], &d).is_none());
        assert_eq!(run("grep", &["--color=auto", "foo", "a.txt"], &d).unwrap().stdout, "foo\n");
        fs::write(d.join("nul.txt"), format!("{}a\0b\nfoo\n", "foo x\n".repeat(2000))).unwrap();
        assert!(run("grep", &["foo", "nul.txt"], &d).is_none());
        fs::write(d.join("l1.txt"), b"caf\xe9 foo\n").unwrap();
        assert!(run("grep", &["foo", "l1.txt"], &d).is_none());
        // An MSYS path is not the Windows path qx would read.
        assert!(run("grep", &["foo", "/tmp/a.txt"], &d).is_none());
        // `--include` filters the files named on the line as well.
        let r = run("grep", &["--include=*.rs", "foo", "a.txt"], &d).unwrap();
        assert_eq!((r.code, r.stdout.as_str()), (1, ""));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn grep_takes_flags_after_the_pattern_and_skips_binary_misses() {
        let d = scratch("grep-late");
        fs::create_dir(d.join("src")).unwrap();
        fs::write(d.join("src/a.txt"), "Foo /api/users\nbar\n").unwrap();
        fs::write(d.join("src/blob.bin"), b"\x00\x01\xffzzz").unwrap();
        let out = |args: &[&str]| run("grep", args, &d).map(|o| (o.code, o.stdout));
        // Options after the pattern and operands, as GNU permutes them.
        assert_eq!(out(&["-rn", "foo", "src", "-i"]), Some((0, "src/a.txt:1:Foo /api/users\n".into())));
        // A pattern with a leading slash is text, an operand with one is a path qx cannot place.
        assert_eq!(out(&["-r", "/api/users", "src"]), Some((0, "src/a.txt:Foo /api/users\n".into())));
        assert!(out(&["-r", "api", "/src"]).is_none());
        // A binary file without the needle is a plain miss; with it, or under -v, the real tool answers.
        assert_eq!(out(&["-rc", "bar", "src"]), Some((0, "src/a.txt:1\nsrc/blob.bin:0\n".into())));
        assert!(out(&["-r", "zzz", "src"]).is_none());
        assert!(out(&["-rv", "bar", "src"]).is_none());
        assert!(out(&["-r", "é", "src"]).is_none());
        // A bad second operand declines before anything is walked.
        assert!(out(&["-r", "bar", "src", "missing"]).is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn grep_context_groups_and_flags_follow_gnu() {
        let d = scratch("grep-ctx");
        fs::write(d.join("a.txt"), "alpha\nbeta\nalpha gamma\n").unwrap();
        fs::write(d.join("b.rs"), "let alpha = 1;\nlet gamma = 2;\n").unwrap();
        let out = |args: &[&str]| {
            let o = run("grep", args, &d).expect("built-in declined");
            (o.code, o.stdout)
        };
        // Any context option, even 0, separates non-adjacent groups, and groups of different files.
        assert_eq!(out(&["-0", "alpha", "a.txt"]), (0, "alpha\n--\nalpha gamma\n".into()));
        assert_eq!(
            out(&["-A1", "alpha", "a.txt", "b.rs"]),
            (0, "a.txt:alpha\na.txt-beta\na.txt:alpha gamma\n--\nb.rs:let alpha = 1;\nb.rs-let gamma = 2;\n".into())
        );
        // An explicit -B/-A wins over -C.
        assert_eq!(out(&["-C1", "-B0", "gamma", "a.txt"]), (0, "alpha gamma\n".into()));
        // -q prints nothing, counts included.
        assert_eq!(out(&["-qc", "nope", "a.txt"]), (1, String::new()));
        assert_eq!(out(&["-qc", "alpha", "a.txt"]), (0, String::new()));
        // With no path, -r searches `.` and prints names without a `./` prefix.
        let (code, text) = out(&["-rn", "gamma"]);
        assert!(code == 0 && text.starts_with("a.txt:3:") && !text.contains("./"), "{text}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cp_r_dot_merges_the_contents_into_an_existing_dir() {
        let d = scratch("cpdot");
        fs::create_dir_all(d.join("src")).unwrap();
        fs::create_dir_all(d.join("dst")).unwrap();
        fs::write(d.join("src").join("a.txt"), "a").unwrap();
        let out = run("cp", &["-r", "src/.", "dst"], &d).unwrap();
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(d.join("dst").join("a.txt").exists(), "contents not merged");
        assert!(!d.join("dst").join("src").exists(), "nested one level too deep");
        // a plain directory operand still nests under the destination
        let out = run("cp", &["-r", "src", "dst"], &d).unwrap();
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(d.join("dst").join("src").join("a.txt").exists());
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
        let me = try_exec("whoami", &[], None, &[]).unwrap().stdout;
        // whoami.exe prints `host\user` in lower case; scripts compare that string.
        assert!(!me.trim().is_empty());
        assert_eq!(me, me.to_lowercase());
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
    fn rm_double_dash_ends_options() {
        let d = scratch("rm-dashdash");
        fs::create_dir_all(d.join("victim")).unwrap();
        fs::write(d.join("victim/keep.txt"), "x").unwrap();
        let o = run("rm", &["--", "-r", "victim"], &d).unwrap();
        assert_eq!(o.code, 1, "{}", o.stderr);
        assert!(d.join("victim/keep.txt").is_file());
        fs::write(d.join("-r"), "x").unwrap();
        assert_eq!(run("rm", &["--", "-r"], &d).unwrap().code, 0);
        assert!(!d.join("-r").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ls_declines_flags_it_does_not_render_and_reports_walk_errors() {
        let d = scratch("ls-flags");
        fs::create_dir_all(d.join("zdir")).unwrap();
        fs::write(d.join("a.txt"), "x").unwrap();
        assert!(run("ls", &["--group-directories-first"], &d).is_none());
        assert!(run("ls", &["--color=always"], &d).is_none());
        assert!(run("ls", &["--color=never"], &d).is_some());
        let opts = LsOpts {
            full: false,
            all: false,
            long: false,
            recursive: true,
            reverse: false,
            dirs_as_files: false,
            sort: LsSort::Name,
        };
        let mut walk = LsWalk { shown: 0, limit: usize::MAX, capped: false, depth_capped: false, err: String::new() };
        let mut out = String::new();
        assert!(list_recursive(&d.join("gone"), "gone", &opts, &mut out, &mut walk, 0).is_some());
        assert!(out.is_empty(), "{out}");
        assert!(walk.err.contains("gone"), "{}", walk.err);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn text_builtins_keep_line_terminators_byte_exact() {
        let d = scratch("terminators");
        fs::write(d.join("crlf.txt"), "a\r\nb\r\nc").unwrap();
        fs::write(d.join("nonl.txt"), "abc").unwrap();
        let out = |name: &str, args: &[&str]| run(name, args, &d).expect("built-in declined").stdout;
        assert_eq!(out("cat", &["nonl.txt"]), "abc");
        assert_eq!(out("cat", &["nonl.txt", "nonl.txt"]), "abcabc");
        assert_eq!(out("cat", &["-n", "crlf.txt"]), "     1\ta\r\n     2\tb\r\n     3\tc");
        assert_eq!(out("head", &["-n", "1", "crlf.txt"]), "a\r\n");
        assert_eq!(out("head", &["-n", "5", "crlf.txt"]), "a\r\nb\r\nc");
        assert_eq!(out("tail", &["-n", "2", "crlf.txt"]), "b\r\nc");
        assert_eq!(out("grep", &["b", "crlf.txt"]), "b\r\n");
        assert_eq!(out("grep", &["-i", "B", "crlf.txt"]), "b\r\n");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn wc_words_split_on_vertical_tab_and_decline_non_ascii() {
        let d = scratch("wc-words");
        fs::write(d.join("vt.txt"), "a\x0bb\n").unwrap();
        fs::write(d.join("nbsp.txt"), "a\u{a0}b\n").unwrap();
        assert_eq!(run("wc", &["-w", "vt.txt"], &d).unwrap().stdout, "2 vt.txt\n");
        // GNU splits words on U+00A0 in a UTF-8 locale; the byte scan cannot, so the real wc answers.
        assert!(run("wc", &["-w", "nbsp.txt"], &d).is_none());
        assert!(run("wc", &["nbsp.txt"], &d).is_none());
        assert_eq!(run("wc", &["-l", "nbsp.txt"], &d).unwrap().stdout, "1 nbsp.txt\n");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn mkdir_touch_and_cp_match_the_tools_they_stand_in_for() {
        let d = scratch("mkdir-touch-cp");
        // cmd's mkdir makes the parents of a `\` path and GNU's does not: the real one decides.
        assert!(run("mkdir", &[r"p\q\r"], &d).is_none());
        assert_eq!(run("mkdir", &["-p", "p/q/r"], &d).unwrap().code, 0);
        assert!(d.join("p/q/r").is_dir());
        assert_eq!(run("mkdir", &["x/y"], &d).unwrap().code, 1); // GNU without -p does not
        assert!(run("touch", &["p"], &d).is_none()); // a folder goes to the real touch
        fs::write(d.join("f1"), "x").unwrap();
        let o = run("cp", &["-r", "p", "f1"], &d).unwrap();
        assert_eq!(o.code, 1);
        assert!(o.stderr.contains("cannot overwrite non-directory"), "{}", o.stderr);
        assert_eq!(fs::read_to_string(d.join("f1")).unwrap(), "x");
        let _ = fs::remove_dir_all(&d);
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
    fn head_c_reads_a_byte_window_and_declines_what_it_cannot_say() {
        let d = scratch("bytes");
        fs::write(d.join("f.txt"), "abcdefghij\n").unwrap();
        assert_eq!(run("head", &["-c", "4", "f.txt"], &d).unwrap().stdout, "abcd");
        assert_eq!(run("head", &["-c4", "f.txt"], &d).unwrap().stdout, "abcd");
        assert_eq!(run("head", &["--bytes=3", "f.txt"], &d).unwrap().stdout, "abc");
        assert_eq!(run("tail", &["-c", "4", "f.txt"], &d).unwrap().stdout, "hij\n");
        assert_eq!(run("tail", &["-c", "0", "f.txt"], &d).unwrap().stdout, "");
        // The last window flag on the line wins, as in GNU.
        assert_eq!(run("head", &["-n", "2", "-c", "2", "f.txt"], &d).unwrap().stdout, "ab");
        assert_eq!(run("head", &["-c", "2", "-n", "2", "f.txt"], &d).unwrap().stdout, "abcdefghij\n");
        // Byte counts qx would have to guess at, and windows it cannot render, run for real.
        assert!(run("head", &["-c", "4k", "f.txt"], &d).is_none());
        assert!(run("head", &["-c", "-4", "f.txt"], &d).is_none());
        fs::write(d.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        assert!(run("head", &["-c", "2", "bin.dat"], &d).is_none());
        fs::write(d.join("latin1.dat"), [b'h', 0xFF, 0xFE]).unwrap();
        assert!(run("head", &["-c", "3", "latin1.dat"], &d).is_none());
        // GNU head opens the file even to print nothing of it; tail does not.
        assert_ne!(run("head", &["-c", "0", "nofile"], &d).unwrap().code, 0);
        assert_ne!(run("head", &["-n", "0", "nofile"], &d).unwrap().code, 0);
        assert_eq!(run("tail", &["-c", "0", "nofile"], &d).unwrap().code, 0);
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
        assert!(!has_link(&d.join("tree"), 0));
        // A file link is copied as a link by GNU `cp -r` and listed as one by `ls -l`; the builtins
        // would show or copy its target, so both leave it to the real tool.
        fs::create_dir_all(d.join("ftree")).unwrap();
        fs::write(d.join("ftree/real.txt"), "x").unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(d.join("ftree/real.txt"), d.join("ftree/link.txt")).is_ok() {
            assert!(has_link(&d.join("ftree"), 0));
            assert!(run("cp", &["-r", "ftree", "ftree2"], &d).is_none());
            assert!(!d.join("ftree2").exists());
            assert!(run("ls", &["-l", "ftree"], &d).is_none());
            assert!(run("ls", &["-l", "ftree/link.txt"], &d).is_none());
            assert!(run("ls", &["ftree"], &d).is_some());
        }
        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(d.join("tree/sub"), d.join("tree/jump")).is_ok() {
            assert!(has_link(&d.join("tree"), 0));
            assert!(run("cp", &["-r", "tree", "tree2"], &d).is_none());
        }
    }

    /// The built-in has to be indistinguishable from the real grep for every shape it accepts,
    /// so this runs both and compares stdout (sorted: qx emits path order) and the exit code.
    #[test]
    fn grep_matches_the_real_tool_or_declines() {
        let Some(grep) = real_grep() else {
            oracle_missing("no grep.exe next to Git");
            return;
        };
        let d = scratch("grep");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("a.txt"), "alpha\nbeta\nalpha gamma\n").unwrap();
        fs::write(d.join("b.rs"), "let alpha = 1;\nlet gamma = 2;\n").unwrap();
        fs::write(d.join("one.txt"), "x\n").unwrap();
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
            // `-v` must not report the empty line a trailing newline would invent.
            vec!["-vn", "alpha", "a.txt"],
            vec!["-v", "x", "one.txt"],
            vec!["-v", "gamma", "a.txt"],
            vec!["-rlv", "alpha", "."],
            vec!["--invert-match", "alpha", "a.txt"],
            vec!["-q", "alpha", "a.txt"],
            vec!["-q", "nope", "a.txt"],
            vec!["-qv", "alpha", "a.txt"],
            // `-c` prints a count for every searched file, zero included.
            vec!["-c", "alpha", "a.txt"],
            vec!["-c", "nope", "a.txt"],
            vec!["-c", "alpha", "a.txt", "b.rs"],
            vec!["-cv", "alpha", "a.txt"],
            vec!["-rcn", "alpha", "."],
            vec!["-rcl", "alpha", "."],
            vec!["-cl", "alpha", "a.txt", "one.txt"],
            vec!["-e", "alpha", "a.txt"],
            vec!["-ealpha", "a.txt"],
            vec!["-ne", "gamma", "a.txt"],
            vec!["--regexp=beta", "a.txt"],
            // A single file operand is not named, even with -r.
            vec!["-rn", "alpha", "a.txt"],
        ];
        for case in taken {
            let mine = run("grep", &case, &d).unwrap_or_else(|| panic!("built-in declined {case:?}, a shape it claims"));
            let Some((code, theirs, why)) = real_tool_run(grep, &case, &d) else {
                let _ = fs::remove_dir_all(&d);
                return;
            };
            let mut a: Vec<&str> = mine.stdout.lines().collect();
            let mut b: Vec<&str> = theirs.lines().collect();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(a, b, "stdout differs for {case:?} (grep said {why:?})");
            assert_eq!(mine.code, code, "exit differs for {case:?} (grep said {why:?})");
        }

        // Context output is ordered and carries `-`/`--` separators, so compare it byte for byte.
        let ordered: Vec<Vec<&str>> = vec![
            vec!["-A1", "alpha", "a.txt"],
            vec!["-B1", "gamma", "a.txt"],
            vec!["-A1", "-B1", "gamma", "a.txt"],
            vec!["-1", "alpha", "a.txt"],
            vec!["-0", "alpha", "a.txt"],
            vec!["-A0", "alpha", "a.txt"],
            vec!["-B0", "-C1", "gamma", "a.txt"],
            vec!["-C1", "-A0", "gamma", "a.txt"],
            vec!["-nA2", "alpha", "a.txt"],
            vec!["-A1", "alpha", "a.txt", "b.rs"],
            vec!["-A1", "gamma", "a.txt", "b.rs"],
            vec!["-rA1", "alpha", "."],
            vec!["-A1", "nope", "a.txt"],
            vec!["-vA1", "alpha", "a.txt"],
        ];
        for case in ordered {
            let mine = run("grep", &case, &d).unwrap_or_else(|| panic!("built-in declined {case:?}, a context shape it claims"));
            let Some((code, theirs, why)) = real_tool_run(grep, &case, &d) else {
                let _ = fs::remove_dir_all(&d);
                return;
            };
            assert_eq!(mine.stdout, theirs, "stdout differs for {case:?} (grep said {why:?})");
            assert_eq!(mine.code, code, "exit differs for {case:?} (grep said {why:?})");
        }

        // Shapes a literal in-process scan must never guess at.
        fs::write(d.join("bin.dat"), b"alpha\0binary").unwrap();
        let declined: Vec<Vec<&str>> = vec![
            // A directory without -r is an error GNU words better.
            vec!["-cl", "alpha", "."],
            vec!["alpha", "bin.dat"],
            vec!["a.p", "a.txt"],
            vec!["alpha"],
            vec!["-rn", "alpha", "missing"],
            vec!["-rn", "alpha", "."],
            // GNU reads a newline as two patterns.
            vec!["alpha\nbeta", "a.txt"],
        ];
        for case in declined {
            assert!(run("grep", &case, &d).is_none(), "should decline {case:?}");
        }
        let _ = fs::remove_dir_all(&d);
    }

    /// The ranges pinned without an oracle, so they are checked where Git's sed cannot start.
    #[test]
    fn sed_prints_line_ranges_without_the_real_tool() {
        let d = scratch("sed-fixed");
        fs::write(d.join("five.txt"), "a\nb\nc\nd\ne\n").unwrap();
        fs::write(d.join("crlf.txt"), "a\r\nb\r\nc\r\n").unwrap();
        fs::write(d.join("nonl.txt"), "x").unwrap();
        fs::write(d.join("big.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n").unwrap();
        for (case, want) in [
            (vec!["-n", "2,4p", "five.txt"], "b\nc\nd\n"),
            (vec!["-n", "1,2p", "five.txt", "big.txt"], "a\nb\n"),
            (vec!["-n", "9,10p", "big.txt"], "9\n10\n"),
            // CRLF stays CRLF, as the real sed keeps it.
            (vec!["-n", "2,3p", "crlf.txt"], "b\r\nc\r\n"),
            (vec!["-n", "1p", "nonl.txt"], "x"),
            // GNU sed adds the newline a file lacks once more output follows it.
            (vec!["-n", "1,2p", "nonl.txt", "five.txt"], "x\na\n"),
            (vec!["-n", "1,1p", "nonl.txt", "five.txt"], "x"),
        ] {
            let out = run("sed", &case, &d).expect("built-in declined a range it claims");
            assert_eq!((out.code, out.stdout.as_str()), (0, want), "{case:?}");
        }
        let _ = fs::remove_dir_all(&d);
    }

    /// A range at the top of a file past the read cap is answered from a stream that stops at the range.
    #[test]
    fn sed_range_of_a_file_past_the_read_cap_streams_and_stops_early() {
        let d = scratch("sed-large");
        let mut big = b"a\nb\nc\nd\n".to_vec();
        big.resize(READ_CAP as usize + 4096, b'z');
        fs::write(d.join("large.txt"), &big).unwrap();
        let t = std::time::Instant::now();
        let out = run("sed", &["-n", "1,3p", "large.txt"], &d).expect("built-in declined the top of a large file");
        assert_eq!((out.code, out.stdout.as_str()), (0, "a\nb\nc\n"));
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        // A range that needs more than the cap of one line is still the real tool's.
        assert!(run("sed", &["-n", "5p", "large.txt"], &d).is_none());
        let _ = fs::remove_dir_all(&d);
    }

    /// A file past the cap keeps its start and its end: a log's failure is the last line.
    #[test]
    fn cat_of_a_file_past_the_read_cap_keeps_both_ends() {
        let d = scratch("cat-large");
        // Different line lengths put the two cut points at different offsets inside a line.
        for width in [79usize, 97, 1013] {
            let mut big = String::from("first line\n");
            let filler = "x".repeat(width) + "\n";
            while big.len() < READ_CAP as usize + (1 << 20) {
                big.push_str(&filler);
            }
            big.push_str("FAILED: the last line\n");
            fs::write(d.join("large.log"), &big).unwrap();
            let out = run("cat", &["large.log"], &d).expect("cat is a built-in");
            assert_eq!(out.code, 0);
            assert!(out.stdout.starts_with("first line\n"), "start kept (width {width})");
            assert!(out.stdout.ends_with("FAILED: the last line\n"), "end kept (width {width})");
            assert!(out.stdout.contains("bytes omitted"), "the gap is marked (width {width})");
            assert!(out.stdout.len() <= READ_CAP as usize, "{} (width {width})", out.stdout.len());
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn sed_prints_the_range_the_real_tool_prints() {
        let Some(sed) = real_sed() else {
            oracle_missing("no real sed");
            return;
        };
        let d = scratch("sed");
        fs::write(d.join("five.txt"), "a\nb\nc\nd\ne\n").unwrap();
        fs::write(d.join("nonl.txt"), "x").unwrap();
        fs::write(d.join("big.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n").unwrap();
        let taken: Vec<Vec<&str>> = vec![
            vec!["-n", "2,4p", "five.txt"],
            vec!["-n", "3p", "five.txt"],
            vec!["-n", "1,99p", "five.txt"],
            vec!["-n", "5,6p", "five.txt"],
            vec!["-n", "1,1p", "five.txt"],
            vec!["-n", "1,2p", "five.txt", "big.txt"],
            vec!["-n", "1p", "nonl.txt"],
            vec!["-n", "1,2p", "nonl.txt", "five.txt"],
            vec!["-n", "9,10p", "big.txt"],
        ];
        for case in taken {
            let mine = run("sed", &case, &d).expect("built-in declined a range it claims");
            // Git's sed is an msys binary that a restricted token cannot start: skip, like grep's oracle.
            let Some((code, theirs, why)) = real_tool_run(sed, &case, &d) else {
                let _ = fs::remove_dir_all(&d);
                return;
            };
            assert_eq!(mine.stdout, theirs, "stdout differs for {case:?} (sed said {why:?})");
            assert_eq!(mine.code, code, "exit differs for {case:?} (sed said {why:?})");
        }
        // Shapes a line-range reader must never guess at.
        let declined: Vec<Vec<&str>> = vec![
            vec!["1,2p", "five.txt"],
            vec!["-n", "$p", "five.txt"],
            vec!["-n", "1,2d", "five.txt"],
            vec!["-E", "-n", "1p", "five.txt"],
            vec!["-n", "1p"],
            vec!["-n", "0,2p", "five.txt"],
            vec!["-n", "3,1p", "five.txt"],
            vec!["-n", "1,2p", "*.txt"],
            vec!["-n", "1,2p", "missing.txt"],
            vec!["-n", "1,2p", "."],
            vec!["-n", "1,2p", "-"],
            vec!["-n", "1,2p", "-i"],
            vec!["-n", "1~2p", "five.txt"],
        ];
        for case in declined {
            assert!(run("sed", &case, &d).is_none(), "built-in took {case:?}");
        }
        let _ = fs::remove_dir_all(&d);
    }

    /// CI sets QX_REQUIRE_ORACLE=1: there a missing oracle is a failure, not a silent pass.
    fn oracle_missing(why: &str) {
        assert!(std::env::var("QX_REQUIRE_ORACLE").ok().as_deref() != Some("1"), "oracle required: {why}");
        eprintln!("skipped: {why}");
    }

    fn real_grep() -> Option<&'static str> {
        [
            r"C:\Program Files\Git\usr\bin\grep.exe",
            r"C:\Program Files\Git\bin\grep.exe",
        ]
        .into_iter()
        .find(|p| Path::new(p).exists())
    }

    fn real_sed() -> Option<&'static str> {
        [
            r"C:\Program Files\Git\usr\bin\sed.exe",
            r"C:\Program Files\Git\bin\sed.exe",
        ]
        .into_iter()
        .find(|p| Path::new(p).exists())
    }

    /// Git's grep is an msys binary, and under four real-time scanners it sometimes fails to
    /// start at all (`NtCreateDirectoryObject ... 0xC0000022`). Retry with a short backoff,
    /// then report the oracle as unavailable rather than blaming the built-in for it.
    fn real_tool_run(exe: &str, args: &[&str], cwd: &Path) -> Option<(i32, String, String)> {
        let mut last = (0, String::new(), String::new());
        for attempt in 0..3 {
            let out = std::process::Command::new(exe)
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("real tool");
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let flake = stdout.is_empty()
                && ["fatal error", "ntcreatedirectoryobject", "msys"]
                    .iter()
                    .any(|m| stderr.to_ascii_lowercase().contains(m));
            last = (out.status.code().unwrap_or(-1), stdout, stderr);
            if !flake {
                return Some(last);
            }
            std::thread::sleep(std::time::Duration::from_millis(50 * (attempt + 1) as u64));
        }
        oracle_missing(&format!("the real tool could not start here ({})", last.2.trim()));
        None
    }

    #[test]
    fn powershell_cmdlets_run_as_their_posix_twins() {
        let (n, a) = ps_cmdlet("get-content", &["-Path".into(), "f.txt".into(), "-TotalCount".into(), "3".into()]).unwrap();
        assert_eq!((n, a), ("head", vec!["-n".to_string(), "3".into(), "f.txt".into()]));
        assert_eq!(ps_cmdlet("get-content", &["f".into()]).unwrap().0, "cat");
        assert!(ps_cmdlet("get-content", &["f".into(), "-Encoding".into(), "UTF8".into()]).is_none());
        assert_eq!(ps_cmdlet("start-sleep", &["-Milliseconds".into(), "500".into()]).unwrap().1, vec!["0.5".to_string()]);
        assert!(ps_cmdlet("start-sleep", &["-Foo".into()]).is_none());
        let here = Path::new(".");
        assert_eq!(try_exec("Test-Path", &["Cargo.toml".into()], Some(here), &[]).unwrap().stdout, "True\n");
        assert_eq!(try_exec("test-path", &["nope".into(), "-PathType".into(), "Leaf".into()], Some(here), &[]).unwrap().stdout, "False\n");
        // Provider paths belong to the real PowerShell.
        for (p, a) in [("test-path", "env:PATH"), ("ls", "env:"), ("cat", "HKCU:\\Software"), ("Get-Content", "env:PATH")] {
            assert!(try_exec(p, &[a.into()], Some(here), &[]).is_none(), "{p} {a}");
        }
        assert!(try_exec("ls", &[here.to_string_lossy().into_owned()], Some(here), &[]).is_some());
        assert_eq!(try_exec("Write-Output", &["a".into(), "b".into()], None, &[]).unwrap().stdout, "a\nb\n");
        assert!(try_exec("Write-Host", &["-NoNewline".into(), "a".into()], None, &[]).is_none());
    }}
