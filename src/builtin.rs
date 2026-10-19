//! Commands implemented natively: no process creation at all.
//!
//! Every handler is conservative: if it sees a flag or shape it does not fully
//! understand it returns `None` and qx falls back to the real executable.

use crate::resolve;
use std::fs;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct BuiltinOut {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

pub fn try_exec(program: &str, args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    if std::env::var("QX_NO_BUILTINS").ok().as_deref() == Some("1") {
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
        "ls" | "dir" => cmd_ls(args, cwd),
        "cat" | "type" => cmd_cat(args, cwd),
        "head" => cmd_head(args, cwd, false),
        "tail" => cmd_head(args, cwd, true),
        "wc" => cmd_wc(args, cwd),
        "mkdir" => cmd_mkdir(args, cwd),
        "rm" | "del" | "erase" => cmd_rm(args, cwd),
        "cp" | "copy" => cmd_cp(args, cwd),
        "mv" | "move" => cmd_mv(args, cwd),
        "touch" => cmd_touch(args, cwd),
        "which" | "where" => cmd_which(args),
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

pub fn is_builtin(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "pwd"
            | "echo"
            | "true"
            | "false"
            | "ls"
            | "dir"
            | "cat"
            | "type"
            | "head"
            | "tail"
            | "wc"
            | "mkdir"
            | "rm"
            | "del"
            | "erase"
            | "cp"
            | "copy"
            | "mv"
            | "move"
            | "touch"
            | "which"
            | "where"
            | "basename"
            | "dirname"
            | "realpath"
            | "whoami"
            | "hostname"
            | "seq"
            | "sleep"
            | "clear"
            | "cls"
    )
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
            "-e" | "-E" => {}
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
    if !(0.0..=3600.0).contains(&secs) {
        return None;
    }
    std::thread::sleep(Duration::from_secs_f64(secs));
    Some(ok(String::new()))
}

// ---------------------------------------------------------------- listing

fn cmd_ls(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    let mut all = false;
    let mut long = false;
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
                    'a' | 'A' => all = true,
                    'l' => long = true,
                    '1' | 'h' => {}
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
    let multiple = paths.len() > 1;
    let mut first = true;
    for p in paths {
        let path = path_of(cwd, p);
        if multiple {
            if !first {
                out.push('\n');
            }
            out.push_str(&format!("{}:\n", p));
        }
        first = false;
        match list_one(&path, all, long) {
            Ok(s) => out.push_str(&s),
            Err(e) => {
                return Some(fail(format!("ls: {}: {e}", path.display())));
            }
        }
    }
    Some(ok(out))
}

fn list_one(path: &Path, all: bool, long: bool) -> std::io::Result<String> {
    let md = fs::metadata(path)?;
    if md.is_file() {
        return Ok(entry_line(
            &path.file_name().unwrap_or_default().to_string_lossy(),
            &md,
            long,
        ));
    }

    let mut entries: Vec<(String, fs::Metadata)> = Vec::new();
    let mut truncated = false;
    for e in fs::read_dir(path)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if !all && name.starts_with('.') {
            continue;
        }
        let md = e.metadata()?;
        entries.push((name, md));
        if entries.len() >= 20_000 {
            truncated = true;
            break;
        }
    }
    entries.sort_by(|a, b| {
        a.0.to_ascii_lowercase()
            .cmp(&b.0.to_ascii_lowercase())
    });

    let mut out = String::new();
    for (name, md) in entries {
        out.push_str(&entry_line(&name, &md, long));
    }
    if truncated {
        out.push_str("[qx] listing truncated at 20000 entries\n");
    }
    Ok(out)
}

fn entry_line(name: &str, md: &fs::Metadata, long: bool) -> String {
    if !long {
        return format!("{name}\n");
    }
    let is_dir = md.is_dir();
    let mode = if is_dir {
        "drwxr-xr-x"
    } else {
        let ext = Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(ext.as_str(), "exe" | "cmd" | "bat" | "ps1" | "com") {
            "-rwxr-xr-x"
        } else {
            "-rw-r--r--"
        }
    };
    let time = md.modified().unwrap_or(UNIX_EPOCH);
    format!(
        "{mode} {:>10} {} {name}\n",
        md.len(),
        fmt_time(time)
    )
}

fn fmt_time(t: SystemTime) -> String {
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
        } else if a.starts_with('-') && a.len() > 1 {
            return None;
        } else if wildcard(a) {
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

fn cmd_head(args: &[String], cwd: Option<&Path>, tail: bool) -> Option<BuiltinOut> {
    let mut count = 10usize;
    let mut files: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-n" || a == "--lines" {
            i += 1;
            count = args.get(i)?.parse().ok()?;
            i += 1;
        } else if let Some(rest) = a.strip_prefix("-n") {
            count = rest.parse().ok()?;
            i += 1;
        } else if a.starts_with("--lines=") {
            count = a[8..].parse().ok()?;
            i += 1;
        } else if a.starts_with('-') && a.len() > 1 {
            return None;
        } else if wildcard(a) {
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
    let mut buf = Vec::new();
    let text = if tail {
        let start = len.saturating_sub(READ_CAP);
        f.seek(SeekFrom::Start(start))?;
        f.read_to_end(&mut buf)?;
        let text = crate::exec::decode(&buf);
        if start > 0 {
            match text.find('\n') {
                Some(i) => text[i + 1..].to_string(),
                None => String::new(),
            }
        } else {
            text
        }
    } else {
        f.take(READ_CAP).read_to_end(&mut buf)?;
        crate::exec::decode(&buf)
    };

    let mut out = String::new();
    if tail {
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
    } else {
        for line in text.lines().take(count) {
            out.push_str(line);
            out.push('\n');
        }
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
    if !any {
        parts.push(files[0].to_string());
    }
    Some(ok(format!("{}\n", parts.join(" "))))
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
        } else if a.starts_with('-') && a.len() > 1 {
            return None;
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
    s.trim_end_matches(|c| c == '\\' || c == '/').to_string()
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
        if !path.exists() {
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
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
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
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
        return Ok(());
    }
    if md.is_dir() {
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_rec(&e.path(), &dst.join(e.file_name()), depth + 1)?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
        Ok(())
    }
}

fn cmd_cp(args: &[String], cwd: Option<&Path>) -> Option<BuiltinOut> {
    let mut recursive = false;
    let mut paths: Vec<&str> = Vec::new();
    for a in args {
        if a == "-r" || a == "-R" || a == "--recursive" {
            recursive = true;
        } else if a.starts_with('-') && a.len() > 1 {
            return None;
        } else if wildcard(a) {
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
        if let Err(e) = fs::rename(&src, &target) {
            // Cross-volume: copy then delete.
            if let Err(e2) = copy_rec(&src, &target, 0) {
                err.push_str(&format!(
                    "mv: {} -> {}: {e}; {e2}\n",
                    src.display(),
                    target.display()
                ));
                code = 1;
                continue;
            }
            let _ = if src.is_dir() {
                fs::remove_dir_all(&src)
            } else {
                fs::remove_file(&src)
            };
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
    if args.is_empty() || args.iter().any(|a| a.starts_with('-')) {
        return None;
    }
    let mut out = String::new();
    let mut code = 0;
    for name in args {
        match resolve::resolve(name) {
            resolve::Target::Direct { exe, prefix } => {
                if prefix.is_empty() {
                    out.push_str(&format!("{}\n", exe.display()));
                } else {
                    out.push_str(&format!("{} {}\n", exe.display(), prefix.join(" ")));
                }
            }
            resolve::Target::Shim(p) => out.push_str(&format!("{}\n", p.display())),
            resolve::Target::Ps1(p) => out.push_str(&format!("{}\n", p.display())),
            resolve::Target::Missing => {
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
    if args.is_empty() || args.iter().any(|a| a.starts_with('-')) {
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
    if !count.is_finite() || count < 0.0 || count > 1_000_000.0 {
        return None;
    }
    let all_int = nums.iter().all(|n| n.fract() == 0.0);
    let mut out = String::new();
    let n = count as u64;
    for i in 0..n {
        let v = first + step * i as f64;
        if all_int {
            out.push_str(&format!("{}\n", v as i64));
        } else {
            out.push_str(&format!("{v}\n"));
        }
    }
    Some(ok(out))
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

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
        let out = cmd_ls(&args, Some(&d)).unwrap();
        assert_eq!(out.code, 0);
        assert!(out.stdout.contains("a.txt"));
        assert!(out.stdout.contains("sub"));
        assert!(!out.stdout.contains(".hidden"));

        let args = vec!["-a".to_string(), ".".to_string()];
        assert!(cmd_ls(&args, Some(&d)).unwrap().stdout.contains(".hidden"));
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
            try_exec("seq", &[String::from("3")], None).unwrap().stdout,
            "1\n2\n3\n"
        );
        let stepped = vec![
            String::from("1"),
            String::from("2"),
            String::from("5"),
        ];
        assert_eq!(
            try_exec("seq", &stepped, None).unwrap().stdout,
            "1\n3\n5\n"
        );
        assert!(!try_exec("hostname", &[], None).unwrap().stdout.trim().is_empty());
        assert!(!try_exec("whoami", &[], None).unwrap().stdout.trim().is_empty());
        let rp = try_exec("realpath", &[String::from(".")], None).unwrap();
        assert_eq!(rp.code, 0);
        assert!(rp.stdout.trim().len() > 1);
    }
}
