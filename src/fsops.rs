//! In-process filesystem operations for the MCP `fs` tool.
//!
//! The fast paths are deliberately workspace-confined: every path is resolved
//! against an explicit `cwd`, symlinks may not escape that root, and mutations
//! require a prior read of the same version. Writes are atomic (temp + replace)
//! and never touch VCS metadata. When an operation cannot be completed safely,
//! the reply carries a stable code and tells the caller to use the client's own
//! tool instead.

use crate::client::{self, current, Native};
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_READ_BYTES: u64 = 32 * 1024 * 1024;

/// The current client's name for one of its own tools, for messages shaped `the native {} tool`.
fn native(n: Native) -> &'static str {
    current().native(n)
}
const MAX_WRITE_BYTES: usize = 16 * 1024 * 1024;
const MAX_EDIT_BYTES: usize = 1024 * 1024;
const MAX_GREP_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_WALK_FILES: usize = 200_000;
const MAX_WALK_DEPTH: usize = 64;
const DEFAULT_READ_LINES: usize = 2_000;
const MAX_READ_LINES: usize = 2_000;
const DEFAULT_READ_LINE_CHARS: usize = 2_000;
const MAX_READ_LINE_CHARS: usize = 8_000;
const DEFAULT_READ_BYTES: usize = 50 * 1024;
const DEFAULT_GLOB_RESULTS: usize = 100;
const MAX_GLOB_RESULTS: usize = 1_000;
const DEFAULT_GREP_MATCHES: usize = 250;
const MAX_GREP_MATCHES: usize = 1_000;
const DEFAULT_GREP_LINE_CHARS: usize = 2_000;
const MAX_LIST_ENTRIES: usize = 500;

pub struct Reply {
    pub text: String,
    pub structured: Value,
    pub is_error: bool,
}

#[derive(Debug)]
struct FsError {
    code: &'static str,
    message: String,
    fallback: &'static str,
}

impl FsError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            fallback: "tool",
        }
    }

    fn fb(mut self, fallback: &'static str) -> Self {
        self.fallback = fallback;
        self
    }

    fn arg(message: impl Into<String>) -> Self {
        Self::new("FS_BAD_ARG", message)
    }

    fn io(message: impl Into<String>, error: std::io::Error) -> Self {
        Self::new("FS_IO", format!("{}: {error}", message.into()))
    }

    fn reply(&self, op: &str) -> Reply {
        // The pointer must name a tool this client actually has.
        let fallback = match Native::from_name(self.fallback) {
            Some(n) => format!("the native {} tool", current().native(n)),
            None => "the native tool for this operation".to_string(),
        };
        Reply {
            text: format!(
                "Error: {}\n[qx] code: {}; fallback: use {}.",
                self.message, self.code, fallback
            ),
            structured: json!({
                "op": op,
                "code": self.code,
                "fallback": self.fallback,
            }),
            is_error: true,
        }
    }
}

pub fn call(args: &Value) -> Reply {
    let started = Instant::now();
    let op = args
        .get("op")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut reply = match dispatch(&op, args) {
        Ok(r) => r,
        Err(e) => e.reply(&op),
    };
    if let Value::Object(map) = &mut reply.structured {
        map.insert(
            "duration_ms".to_string(),
            json!(started.elapsed().as_millis() as u64),
        );
    }
    reply
}

fn dispatch(op: &str, args: &Value) -> Result<Reply, FsError> {
    let root = Root::new(args)?;
    match op {
        "read" => read(&root, args),
        "write" => write(&root, args),
        "edit" => edit(&root, args),
        "glob" => glob(&root, args),
        "grep" => grep(&root, args),
        "list" => list(&root, args),
        "stat" => stat(&root, args),
        _ => Err(FsError::arg(
            "op must be one of read, write, edit, glob, grep, list, stat",
        )),
    }
}

// ---------------------------------------------------------------- arguments

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, FsError> {
    match args.get(key).and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(FsError::arg(format!("{key} must be a non-empty string"))),
    }
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// Native read/write/edit call the target `file_path`; qx's unified tool
/// calls it `path`. Accept both so a model switching tools does not need a
/// different argument shape.
fn path_arg(args: &Value) -> Option<&str> {
    opt_str(args, "path").or_else(|| opt_str(args, "file_path"))
}

fn req_path(args: &Value) -> Result<&str, FsError> {
    match path_arg(args) {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(FsError::arg(
            "path (or file_path) must be a non-empty string",
        )),
    }
}

fn usize_arg(
    args: &Value,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, FsError> {
    let n = match args.get(key) {
        None | Some(Value::Null) => default,
        Some(v) => v
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| FsError::arg(format!("{key} must be an integer")))?,
    };
    if !(min..=max).contains(&n) {
        return Err(FsError::arg(format!(
            "{key} must be between {min} and {max}"
        )));
    }
    Ok(n)
}

fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

// --------------------------------------------------------------------- root

/// The client sends the same session cwd on every call. Canonicalizing it each time
/// costs a Win32 handle round-trip for no new information; cache the answer by
/// the input path. A deleted or replaced workspace invalidates naturally at the
/// next filesystem operation, and the cache is bounded.
fn canonical_cwd(p: &Path) -> Result<PathBuf, FsError> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock() {
        if let Some(hit) = map.get(p) {
            return Ok(hit.clone());
        }
    }
    let cwd = fs::canonicalize(p)
        .map_err(|e| FsError::new("FS_BAD_CWD", format!("cwd is not accessible: {e}")))?;
    if let Ok(mut map) = cache.lock() {
        if map.len() >= 64 {
            map.clear();
        }
        map.insert(p.to_path_buf(), cwd.clone());
    }
    Ok(cwd)
}

struct Root {
    cwd: PathBuf,
}

impl Root {
    fn new(args: &Value) -> Result<Self, FsError> {
        let raw = req_str(args, "cwd").map_err(|e| e.fb("tool"))?;
        let p = PathBuf::from(raw);
        let p = if p.is_absolute() {
            p
        } else {
            std::env::current_dir()
                .map_err(|e| FsError::io("cannot resolve cwd", e))?
                .join(p)
        };
        let cwd = canonical_cwd(&p)?;
        if !cwd.is_dir() {
            return Err(FsError::new(
                "FS_BAD_CWD",
                format!("cwd is not a directory: {}", cwd.display()),
            ));
        }
        Ok(Self { cwd })
    }

    fn candidate(&self, raw: &str) -> PathBuf {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else {
            self.cwd.join(p)
        }
    }

    fn display(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.cwd).unwrap_or(path);
        let s = rel.to_string_lossy().replace('\\', "/");
        if s.is_empty() {
            ".".to_string()
        } else {
            s
        }
    }

    fn ensure_inside(&self, path: &Path) -> Result<(), FsError> {
        if path == self.cwd || path.starts_with(&self.cwd) {
            Ok(())
        } else {
            Err(FsError::new(
                "FS_OUTSIDE_WORKSPACE",
                format!(
                    "path escapes the workspace: {} (use the native {} for paths outside cwd)",
                    path.display(),
                    native(Native::Shell)
                ),
            ))
        }
    }

    fn resolve_existing(&self, raw: &str) -> Result<PathBuf, FsError> {
        let candidate = self.candidate(raw);
        let canonical = fs::canonicalize(&candidate).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => FsError::new(
                "FS_NOT_FOUND",
                format!("cannot access \"{raw}\": not found"),
            ),
            _ => FsError::io(format!("cannot access \"{raw}\""), e),
        })?;
        self.ensure_inside(&canonical)?;
        Ok(canonical)
    }

    /// Resolve a file for creation or replacement. Existing parent directories
    /// may be created, but every existing ancestor is canonicalized and checked
    /// first, so a symlink or junction cannot walk the write outside the root.
    fn resolve_for_write(&self, raw: &str) -> Result<(PathBuf, bool), FsError> {
        let candidate = self.candidate(raw);
        let name = candidate
            .file_name()
            .ok_or_else(|| FsError::arg("path must name a file"))?
            .to_os_string();

        let nearest = nearest_existing(&candidate).ok_or_else(|| {
            FsError::new(
                "FS_OUTSIDE_WORKSPACE",
                format!("no existing ancestor for {}", candidate.display()),
            )
        })?;
        let nearest = fs::canonicalize(&nearest)
            .map_err(|e| FsError::io("cannot resolve path ancestor", e))?;
        self.ensure_inside(&nearest)?;

        if let Ok(md) = fs::symlink_metadata(&candidate) {
            if md.file_type().is_symlink() {
                return Err(FsError::new(
                    "FS_SYMLINK",
                    format!(
                        "refusing to replace symlink \"{raw}\"; use the native {} tool",
                        native(Native::Write)
                    ),
                ));
            }
            if !md.is_file() {
                return Err(FsError::new(
                    "FS_NOT_FILE",
                    format!("cannot modify \"{raw}\": not a regular file"),
                ));
            }
            let canonical = fs::canonicalize(&candidate)
                .map_err(|e| FsError::io(format!("cannot resolve \"{raw}\""), e))?;
            self.ensure_inside(&canonical)?;
            return Ok((canonical, true));
        }

        let parent = candidate
            .parent()
            .ok_or_else(|| FsError::arg("path must have a parent directory"))?;
        fs::create_dir_all(parent)
            .map_err(|e| FsError::io(format!("cannot create {}", parent.display()), e))?;
        let parent = fs::canonicalize(parent)
            .map_err(|e| FsError::io("cannot resolve parent directory", e))?;
        self.ensure_inside(&parent)?;
        let target = parent.join(name);
        if let Ok(md) = fs::symlink_metadata(&target) {
            if md.file_type().is_symlink() {
                return Err(FsError::new(
                    "FS_SYMLINK",
                    format!("refusing to replace symlink \"{raw}\""),
                ));
            }
            if !md.is_file() {
                return Err(FsError::new(
                    "FS_NOT_FILE",
                    format!("cannot modify \"{raw}\": not a regular file"),
                ));
            }
            let canonical = fs::canonicalize(&target)
                .map_err(|e| FsError::io(format!("cannot resolve \"{raw}\""), e))?;
            self.ensure_inside(&canonical)?;
            return Ok((canonical, true));
        }
        Ok((target, false))
    }

    fn check_mutable(&self, path: &Path) -> Result<(), FsError> {
        let rel = path.strip_prefix(&self.cwd).unwrap_or(path);
        for c in rel.components() {
            if let Component::Normal(s) = c {
                let s = s.to_string_lossy();
                if matches!(s.as_ref(), ".git" | ".hg" | ".svn") {
                    return Err(FsError::new(
                        "FS_PROTECTED",
                        format!("refusing to modify VCS metadata: {}", path.display()),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut p = path.to_path_buf();
    loop {
        if fs::symlink_metadata(&p).is_ok() {
            return Some(p);
        }
        if !p.pop() {
            return None;
        }
    }
}

// --------------------------------------------------------------- observation

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Version {
    len: u64,
    modified: Option<SystemTime>,
    hash: u64,
}

impl Version {
    fn of(bytes: &[u8], modified: Option<SystemTime>) -> Self {
        Self {
            len: bytes.len() as u64,
            modified,
            hash: hash_bytes(bytes),
        }
    }
}

/// A cheap content fingerprint: full FNV for small files, first+last 8 KiB for
/// large ones. Length and mtime are also compared, so a same-size, same-time
/// middle-only edit is the only case this can miss.
fn hash_bytes(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for &b in bytes.iter().take(8192) {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    if bytes.len() > 1_048_576 {
        let start = bytes.len().saturating_sub(8192);
        for &b in &bytes[start..] {
            h ^= b as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
    } else {
        for &b in bytes.iter().skip(8192) {
            h ^= b as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
    }
    h
}

type ObsKey = (PathBuf, PathBuf);

fn observations() -> &'static Mutex<HashMap<ObsKey, Version>> {
    static MAP: OnceLock<Mutex<HashMap<ObsKey, Version>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember(cwd: &Path, path: &Path, version: Version) {
    observations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((cwd.to_path_buf(), path.to_path_buf()), version);
}

fn check_observed(cwd: &Path, path: &Path, current: Version, display: &str) -> Result<(), FsError> {
    let map = observations().lock().unwrap_or_else(|e| e.into_inner());
    match map.get(&(cwd.to_path_buf(), path.to_path_buf())) {
        None => Err(FsError::new(
            "FS_NOT_OBSERVED",
            format!(
                "cannot modify \"{display}\": file has not been read — read it with qx fs op=read first, or use the native {} tool",
                native(Native::Write)
            ),
        )),
        Some(v) if *v != current => Err(FsError::new(
            "FS_STALE",
            format!(
                "cannot modify \"{display}\": file changed since it was read — re-read it, or use the native {} tool",
                native(Native::Write)
            ),
        )),
        Some(_) => Ok(()),
    }
}

fn mutation_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ---------------------------------------------------------------------- I/O

fn read_file(path: &Path, fallback: &'static str) -> Result<(Vec<u8>, Version), FsError> {
    let md = fs::metadata(path)
        .map_err(|e| FsError::io(format!("cannot read \"{}\"", path.display()), e).fb(fallback))?;
    if !md.is_file() {
        return Err(FsError::new(
            "FS_NOT_FILE",
            format!("cannot read \"{}\": not a regular file", path.display()),
        )
        .fb(fallback));
    }
    if md.len() > MAX_READ_BYTES {
        return Err(FsError::new(
            "FS_TOO_LARGE",
            format!(
                "cannot read \"{}\": file is {} bytes (qx cap {}); use the native {} tool",
                path.display(),
                md.len(),
                MAX_READ_BYTES,
                native(Native::Read)
            ),
        )
        .fb(fallback));
    }
    let bytes = fs::read(path)
        .map_err(|e| FsError::io(format!("cannot read \"{}\"", path.display()), e).fb(fallback))?;
    if bytes.len() as u64 > MAX_READ_BYTES {
        return Err(FsError::new(
            "FS_TOO_LARGE",
            format!(
                "cannot read \"{}\": file grew past the qx cap",
                path.display()
            ),
        )
        .fb(fallback));
    }
    let version = Version::of(&bytes, md.modified().ok());
    Ok((bytes, version))
}

fn version_after(path: &Path, bytes: &[u8]) -> Result<Version, FsError> {
    let md = fs::metadata(path)
        .map_err(|e| FsError::io(format!("cannot stat \"{}\"", path.display()), e))?;
    Ok(Version::of(bytes, md.modified().ok()))
}

fn write_temp(parent: &Path, bytes: &[u8]) -> Result<Temp, FsError> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    for _ in 0..100 {
        let path = parent.join(format!(
            ".qx-tmp-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(bytes).and_then(|_| f.sync_all()) {
                    let _ = fs::remove_file(&path);
                    return Err(FsError::io("cannot write temporary file", e));
                }
                return Ok(Temp { path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(FsError::io("cannot create temporary file", e)),
        }
    }
    Err(FsError::new(
        "FS_IO",
        "cannot allocate a temporary file after 100 attempts",
    ))
}

struct Temp {
    path: PathBuf,
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(windows)]
fn replace_file(temp: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let old: Vec<u16> = temp
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let new: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let ok = unsafe {
        MoveFileExW(
            old.as_ptr(),
            new.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(temp: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(temp, target)
}

fn atomic_replace(path: &Path, bytes: &[u8], expected: Version) -> Result<(), FsError> {
    let parent = path
        .parent()
        .ok_or_else(|| FsError::arg("path must have a parent directory"))?;
    let temp = write_temp(parent, bytes)?;

    // Re-check immediately before replacing. This is the last point at which a
    // concurrent writer can be detected; the provider window is one rename.
    let md = fs::metadata(path).map_err(|_| {
        FsError::new(
            "FS_STALE",
            format!("cannot modify \"{}\": file disappeared", path.display()),
        )
    })?;
    if md.len() != expected.len || md.modified().ok() != expected.modified {
        return Err(FsError::new(
            "FS_STALE",
            format!(
                "cannot modify \"{}\": file changed since it was read",
                path.display()
            ),
        ));
    }
    replace_file(&temp.path, path)
        .map_err(|e| FsError::io(format!("cannot replace \"{}\"", path.display()), e))?;
    Ok(())
}

fn create_noreplace(path: &Path, bytes: &[u8]) -> Result<(), FsError> {
    let parent = path
        .parent()
        .ok_or_else(|| FsError::arg("path must have a parent directory"))?;
    let temp = write_temp(parent, bytes)?;
    match fs::hard_link(&temp.path, path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(FsError::new(
            "FS_EXISTS",
            format!(
                "cannot create \"{}\": file appeared after the observation check",
                path.display()
            ),
        )),
        Err(_) => {
            // Filesystems without hard links (some FAT mounts) still get the
            // no-replace guarantee from create_new, at the cost of atomicity.
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|e| FsError::io(format!("cannot create \"{}\"", path.display()), e))?;
            if let Err(e) = f.write_all(bytes).and_then(|_| f.sync_all()) {
                drop(f);
                let _ = fs::remove_file(path);
                return Err(FsError::io(
                    format!("cannot write \"{}\"", path.display()),
                    e,
                ));
            }
            Ok(())
        }
    }
}

// --------------------------------------------------------------------- read

fn read(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let raw = req_path(args)?;
    let offset = usize_arg(args, "offset", 1, 1, usize::MAX / 2)?;
    let limit = usize_arg(args, "limit", DEFAULT_READ_LINES, 1, MAX_READ_LINES)?;
    let max_line_chars = usize_arg(
        args,
        "max_line_chars",
        DEFAULT_READ_LINE_CHARS,
        80,
        MAX_READ_LINE_CHARS,
    )?;
    let path = root.resolve_existing(raw).map_err(|e| e.fb("read"))?;
    let (bytes, version) = read_file(&path, "read")?;
    if bytes
        .iter()
        .take(8192)
        .any(|&b| b == 0 && !(bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF])))
    {
        return Err(FsError::new(
            "FS_BINARY",
            format!("cannot read \"{raw}\" as text: binary data"),
        )
        .fb("read"));
    }
    let text = decode_text(&bytes);
    let out = render_read(
        &root.display(&path),
        &text,
        offset,
        limit,
        max_line_chars,
        DEFAULT_READ_BYTES,
    )?;
    remember(&root.cwd, &path, version);
    Ok(Reply {
        text: out.text,
        structured: json!({
            "op": "read",
            "path": root.display(&path),
            "lines": out.lines,
            "total_lines": out.total,
            "end_line": out.end_line,
            "truncated": out.truncated,
            "bytes": bytes.len(),
        }),
        is_error: false,
    })
}

struct ReadWindow {
    text: String,
    lines: usize,
    total: usize,
    end_line: usize,
    truncated: bool,
}

fn render_read(
    display: &str,
    text: &str,
    offset: usize,
    limit: usize,
    max_line_chars: usize,
    max_bytes: usize,
) -> Result<ReadWindow, FsError> {
    let total = count_lines(text);
    if offset > total && !(total == 0 && offset == 1) {
        return Err(FsError::new(
            "FS_OFFSET",
            format!("offset {offset} is out of range for \"{display}\" ({total} lines)"),
        ));
    }

    let mut body = String::with_capacity(max_bytes.min(text.len() + 128));
    let mut linebuf = String::with_capacity(256);
    let mut shown = 0usize;
    let mut end_line = offset.saturating_sub(1);
    let mut cut_by_bytes = false;

    for (idx, line) in text.split('\n').enumerate() {
        let n = idx + 1;
        if n > total {
            break;
        }
        if n < offset {
            continue;
        }
        if shown >= limit {
            break;
        }
        linebuf.clear();
        push_usize(&mut linebuf, n);
        linebuf.push_str(": ");
        push_clipped(
            &mut linebuf,
            line.strip_suffix('\r').unwrap_or(line),
            max_line_chars,
        );
        let add = linebuf.len() + usize::from(!body.is_empty());
        if body.len() + add > max_bytes {
            cut_by_bytes = true;
            break;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&linebuf);
        shown += 1;
        end_line = n;
    }

    let footer = if cut_by_bytes {
        format!(
            "(Output capped. Showing lines {offset}-{end_line}. Use offset={} to continue.)",
            end_line + 1
        )
    } else if end_line < total {
        format!(
            "(Showing lines {offset}-{end_line} of {total}. Use offset={} to continue.)",
            end_line + 1
        )
    } else {
        format!("(End of file - total {total} lines)")
    };
    let body = if body.is_empty() {
        footer
    } else {
        format!("{body}\n\n{footer}")
    };
    Ok(ReadWindow {
        text: format!("<path>{display}</path>\n<type>file</type>\n<content>\n{body}\n</content>"),
        lines: shown,
        total,
        end_line,
        truncated: cut_by_bytes || end_line < total,
    })
}

fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.bytes().filter(|&b| b == b'\n').count() + usize::from(!text.ends_with('\n'))
    }
}

fn decode_text(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return std::borrow::Cow::Owned(crate::exec::decode(bytes));
    }
    let s = String::from_utf8_lossy(bytes);
    if s.contains('\0') {
        std::borrow::Cow::Owned(s.replace('\0', ""))
    } else {
        s
    }
}

fn push_usize(out: &mut String, mut n: usize) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.push_str(std::str::from_utf8(&buf[i..]).unwrap_or("0"));
}

fn push_clipped(out: &mut String, line: &str, max: usize) {
    let end = line.char_indices().nth(max).map_or(line.len(), |(idx, _)| idx);
    out.push_str(&line[..end]);
    if end < line.len() {
        out.push_str("... (line truncated to ");
        push_usize(out, max);
        out.push_str(" chars)");
    }
}

fn clip_line(line: &str, max: usize) -> String {
    let mut out = String::with_capacity(line.len().min(max + 64));
    push_clipped(&mut out, line, max);
    out
}

// -------------------------------------------------------------------- write

fn write(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let raw = req_path(args)?;
    let content = match args.get("content").and_then(Value::as_str) {
        Some(s) => s,
        None => return Err(FsError::arg("content must be a string")),
    };
    if content.len() > MAX_WRITE_BYTES {
        return Err(FsError::new(
            "FS_TOO_LARGE",
            format!(
                "content is {} bytes (qx cap {}); use the native {} tool",
                content.len(),
                MAX_WRITE_BYTES,
                native(Native::Write)
            ),
        )
        .fb("write"));
    }
    let _guard = mutation_lock().lock().unwrap_or_else(|e| e.into_inner());
    let (path, existed) = root.resolve_for_write(raw).map_err(|e| e.fb("write"))?;
    root.check_mutable(&path).map_err(|e| e.fb("write"))?;

    if existed {
        let (_, current) = read_file(&path, "write")?;
        check_observed(&root.cwd, &path, current, &root.display(&path))
            .map_err(|e| e.fb("write"))?;
        atomic_replace(&path, content.as_bytes(), current).map_err(|e| e.fb("write"))?;
    } else {
        create_noreplace(&path, content.as_bytes()).map_err(|e| e.fb("write"))?;
    }
    let version = version_after(&path, content.as_bytes()).map_err(|e| e.fb("write"))?;
    remember(&root.cwd, &path, version);
    let display = root.display(&path);
    let verb = if existed {
        "Updated file"
    } else {
        "Created file"
    };
    Ok(Reply {
        text: format!("<path>{display}</path>\n<type>file</type>\n<content>\n{verb}\n</content>"),
        structured: json!({
            "op": "write",
            "path": display,
            "created": !existed,
            "bytes": content.len(),
        }),
        is_error: false,
    })
}

// --------------------------------------------------------------------- edit

fn edit(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let raw = req_path(args)?;
    let old = match args.get("old_string").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s,
        _ => return Err(FsError::arg("old_string must be a non-empty string")),
    };
    let new = match args.get("new_string").and_then(Value::as_str) {
        Some(s) => s,
        None => return Err(FsError::arg("new_string must be a string")),
    };
    if old == new {
        return Err(FsError::arg("old_string and new_string must differ"));
    }
    if old.len() > MAX_EDIT_BYTES || new.len() > MAX_EDIT_BYTES {
        return Err(FsError::new(
            "FS_TOO_LARGE",
            format!("edit strings are capped at {MAX_EDIT_BYTES} bytes"),
        )
        .fb("edit"));
    }
    let replace_all = bool_arg(args, "replace_all", false);
    let _guard = mutation_lock().lock().unwrap_or_else(|e| e.into_inner());
    let path = root.resolve_existing(raw).map_err(|e| e.fb("edit"))?;
    root.check_mutable(&path).map_err(|e| e.fb("edit"))?;
    let (bytes, current) = read_file(&path, "edit")?;
    check_observed(&root.cwd, &path, current, &root.display(&path)).map_err(|e| e.fb("edit"))?;

    // Strict: a lossy decode would rewrite UTF-16/Latin-1 files as UTF-8 and drop NULs on save.
    // The UTF-16 BOMs are never valid UTF-8, so this one check covers them too.
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Err(FsError::new(
            "FS_ENCODING",
            format!("\"{raw}\" is not valid UTF-8 (UTF-16 or a legacy code page); qx edit would corrupt it"),
        )
        .fb("edit"));
    };
    let (old, new) = match_line_endings(text, old, new);
    let count = text.matches(&old).count();
    if count == 0 {
        return Err(FsError::new(
            "FS_NO_MATCH",
            format!("old_string was not found in \"{raw}\""),
        )
        .fb("edit"));
    }
    if count > 1 && !replace_all {
        return Err(FsError::new(
            "FS_AMBIGUOUS",
            format!(
                "old_string occurs {count} times in \"{raw}\"; include more context or set replace_all"
            ),
        )
        .fb("edit"));
    }
    let updated = if replace_all {
        text.replace(&old, &new)
    } else {
        text.replacen(&old, &new, 1)
    };
    atomic_replace(&path, updated.as_bytes(), current).map_err(|e| e.fb("edit"))?;
    let version = version_after(&path, updated.as_bytes()).map_err(|e| e.fb("edit"))?;
    remember(&root.cwd, &path, version);
    let display = root.display(&path);
    let suffix = if replace_all {
        " All occurrences were successfully replaced."
    } else {
        ""
    };
    Ok(Reply {
        text: format!("The file {display} has been updated successfully.{suffix}"),
        structured: json!({
            "op": "edit",
            "path": display,
            "replacements": if replace_all { count } else { 1 },
            "bytes": updated.len(),
        }),
        is_error: false,
    })
}

fn match_line_endings(text: &str, old: &str, new: &str) -> (String, String) {
    if text.contains("\r\n") && old.contains('\n') && !old.contains("\r\n") {
        (old.replace('\n', "\r\n"), new.replace('\n', "\r\n"))
    } else if !text.contains("\r\n") && old.contains("\r\n") {
        (old.replace("\r\n", "\n"), new.replace("\r\n", "\n"))
    } else {
        (old.to_string(), new.to_string())
    }
}

// --------------------------------------------------------------------- glob

fn glob(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let pattern = req_str(args, "pattern")?;
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
    let patterns = expand_braces(&pattern.replace('\\', "/"));
    let raw_path = path_arg(args).unwrap_or(".");
    let root_path = root.resolve_existing(raw_path).map_err(|e| e.fb("glob"))?;
    let max_results = usize_arg(
        args,
        "max_results",
        DEFAULT_GLOB_RESULTS,
        1,
        MAX_GLOB_RESULTS,
    )?;
    let ignore_case = bool_arg(args, "ignore_case", false);

    // The `max_results` newest hits, kept as a min-heap whose top is the worst one kept, so memory
    // stays bounded and a late, newer file still displaces an older one.
    let mut hits: BinaryHeap<Reverse<(SystemTime, Reverse<PathBuf>)>> = BinaryHeap::new();
    let keep = |hits: &mut BinaryHeap<Reverse<(SystemTime, Reverse<PathBuf>)>>, modified: SystemTime, p: &Path| {
        if hits.len() < max_results || hits.peek().is_some_and(|Reverse((t, _))| modified >= *t) {
            hits.push(Reverse((modified, Reverse(p.to_path_buf()))));
            if hits.len() > max_results {
                hits.pop();
            }
        }
    };
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
        if matches_any(&patterns, &rel, &rel, ignore_case) {
            count = 1;
            keep(&mut hits, md.modified().unwrap_or(UNIX_EPOCH), &root_path);
        }
    } else {
        // Match inside the walk: only hits are stored, and a hit's mtime comes from the directory entry
        // (free on Windows) instead of a second stat.
        let mut walked = 0usize;
        walk_files(&root_path, |p, entry| {
            walked += 1;
            let rel = p
                .strip_prefix(&root_path)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/");
            let base = p
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            if matches_any(&patterns, &rel, &base, ignore_case) {
                count += 1;
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                keep(&mut hits, modified, p);
            }
        });
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

fn matches_any(patterns: &[String], rel: &str, base: &str, ignore_case: bool) -> bool {
    patterns.iter().any(|p| {
        let text = if p.contains('/') { rel } else { base };
        glob_match(p, text, ignore_case)
    })
}

// --------------------------------------------------------------------- grep

fn grep(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let needle = req_str(args, "pattern")?;
    let raw_path = path_arg(args).unwrap_or(".");
    let root_path = root.resolve_existing(raw_path).map_err(|e| e.fb("grep"))?;
    let include = opt_str(args, "include");
    if let Some(inc) = include {
        if inc.contains(',') {
            return Err(FsError::arg(format!(
                "include takes one glob; comma-separated lists are not supported (use the native {} tool)",
                native(Native::Grep)
            ))
            .fb("grep"));
        }
        if inc.starts_with('!') {
            return Err(FsError::new(
                "FS_UNSUPPORTED",
                format!(
                    "negated include globs are not supported by qx; use the native {} tool",
                    native(Native::Grep)
                ),
            )
            .fb("grep"));
        }
    }
    let include_patterns = include.map(|s| expand_braces(&s.replace('\\', "/")));
    let ignore_case = bool_arg(args, "ignore_case", false);
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
        80,
        MAX_READ_LINE_CHARS,
    )?;

    let mut files: Vec<PathBuf> = Vec::new();
    let md = fs::metadata(&root_path)
        .map_err(|e| FsError::io(format!("cannot stat \"{raw_path}\""), e).fb("grep"))?;
    let mut scan_capped = false;
    if md.is_file() {
        files.push(root_path.clone());
    } else {
        walk_files(&root_path, |p, _| files.push(p.to_path_buf()));
        scan_capped = files.len() >= MAX_WALK_FILES;
    }
    // No sort here: matching files are sorted by path below, which fixes the output order.
    if let Some(patterns) = &include_patterns {
        files.retain(|p| {
            let rel = p
                .strip_prefix(&root_path)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/");
            let base = p
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            matches_any(patterns, &rel, &base, ignore_case)
        });
    }

    let threads = fs_threads().min(files.len().max(1));
    let chunk = (files.len() + threads - 1) / threads.max(1);
    let panicked = AtomicBool::new(false);
    let mut scans: Vec<ScanFile> = if threads <= 1 || files.len() < 24 {
        files
            .iter()
            .map(|p| scan_file(p, needle, ignore_case, max_line_chars, max_matches))
            .collect()
    } else {
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for part in files.chunks(chunk) {
                handles.push(scope.spawn(move || {
                    part.iter()
                        .map(|p| scan_file(p, needle, ignore_case, max_line_chars, max_matches))
                        .collect::<Vec<_>>()
                }));
            }
            let mut out = Vec::with_capacity(files.len());
            for h in handles {
                match h.join() {
                    Ok(v) => out.extend(v),
                    Err(_) => {
                        panicked.store(true, Ordering::Relaxed);
                    }
                }
            }
            out
        })
    };

    let total: usize = scans.iter().map(|s| s.total).sum();
    let skipped_binary: usize = scans.iter().filter(|s| s.binary).count();
    let skipped_large: usize = scans.iter().filter(|s| s.large).count();
    let read_errors: usize = scans.iter().filter(|s| s.error).count();
    scans.retain(|s| s.total > 0); // only files with matches are listed: sort just those
    scans.sort_by(|a, b| a.path.cmp(&b.path));
    let with_matches = scans.len();

    let mut out = String::new();
    let mut shown = 0usize;
    'files: for sf in &scans {
        for m in &sf.matches {
            if shown >= max_matches {
                break 'files;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!(
                "{}:{}: {}",
                root.display(&sf.path),
                m.line,
                m.text
            ));
            shown += 1;
        }
    }
    if total == 0 {
        out.push_str("No matches found.\n");
    } else if total > shown {
        out.push_str(&format!(
            "\n[qx] showing {shown} of {total} matches; narrow the search or use the native {} tool.\n",
            native(Native::Grep)
        ));
    }
    if skipped_binary > 0 || skipped_large > 0 || read_errors > 0 {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!(
            "[qx] skipped {skipped_binary} binary, {skipped_large} large, {read_errors} unreadable file(s).\n"
        ));
    }
    if scan_capped {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("[qx] scan stopped at the file budget; results may be incomplete.\n");
    }
    if panicked.load(Ordering::Relaxed) {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
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

fn scan_file(
    path: &Path,
    needle: &str,
    ignore_case: bool,
    max_line_chars: usize,
    max_store: usize,
) -> ScanFile {
    let mut sf = ScanFile {
        path: path.to_path_buf(),
        matches: Vec::new(),
        total: 0,
        binary: false,
        large: false,
        error: false,
    };
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
        sf.large = true;
        return sf;
    }
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => {
            sf.error = true;
            return sf;
        }
    };
    if bytes.iter().take(8192).any(|&b| b == 0) {
        sf.binary = true;
        return sf;
    }
    let text = crate::exec::decode(&bytes);
    let needle_lower = if ignore_case {
        needle.to_ascii_lowercase()
    } else {
        String::new()
    };
    for (idx, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let hit = if ignore_case {
            contains_ascii_ci(line, &needle_lower)
        } else {
            line.contains(needle)
        };
        if !hit {
            continue;
        }
        sf.total += 1;
        if sf.matches.len() < max_store {
            sf.matches.push(Match {
                line: idx + 1,
                text: clip_line(line, max_line_chars),
            });
        }
    }
    sf
}

fn contains_ascii_ci(hay: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    let h = hay.as_bytes();
    let n = needle_lower.as_bytes();
    if n.len() > h.len() {
        return false;
    }
    h.windows(n.len())
        .any(|w| w.iter().zip(n).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

// --------------------------------------------------------------------- list

fn list(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let raw = path_arg(args).unwrap_or(".");
    let path = root.resolve_existing(raw).map_err(|e| e.fb("list"))?;
    let md = fs::metadata(&path)
        .map_err(|e| FsError::io(format!("cannot stat \"{raw}\""), e).fb("list"))?;
    let mut rows: Vec<(u8, String, u64, String)> = Vec::new();
    if md.is_dir() {
        let entries = fs::read_dir(&path)
            .map_err(|e| FsError::io(format!("cannot list \"{raw}\""), e).fb("list"))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let (kind, size, extra) = if ft.is_symlink() {
                let target = fs::read_link(entry.path())
                    .map(|p| format!(" -> {}", p.display()))
                    .unwrap_or_default();
                (2u8, 0u64, target)
            } else if ft.is_dir() {
                (0, 0, String::new())
            } else {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                (1, size, String::new())
            };
            rows.push((kind, name, size, extra));
        }
    } else {
        rows.push((
            1,
            path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| raw.to_string()),
            md.len(),
            String::new(),
        ));
    }
    rows.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });

    let shown = rows.len().min(MAX_LIST_ENTRIES);
    let mut out = String::new();
    for (kind, name, size, extra) in rows.iter().take(shown) {
        match kind {
            0 => out.push_str(&format!("d {name}/\n")),
            2 => out.push_str(&format!("l {name}{extra}\n")),
            _ => out.push_str(&format!("f {size} {name}\n")),
        }
    }
    if rows.len() > shown {
        out.push_str(&format!(
            "[qx] showing {shown} of {} entries; use the native {} tool for the rest.\n",
            rows.len(),
            native(Native::Glob)
        ));
    }
    Ok(Reply {
        text: out,
        structured: json!({
            "op": "list",
            "path": root.display(&path),
            "count": rows.len(),
            "shown": shown,
            "truncated": rows.len() > shown,
        }),
        is_error: false,
    })
}

fn stat(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let raw = req_path(args)?;
    let path = root.resolve_existing(raw).map_err(|e| e.fb("stat"))?;
    let md = fs::metadata(&path)
        .map_err(|e| FsError::io(format!("cannot stat \"{raw}\""), e).fb("stat"))?;
    let kind = if md.is_dir() {
        "dir"
    } else if md.is_file() {
        "file"
    } else {
        "other"
    };
    let modified = md
        .modified()
        .map(crate::builtin::format_time)
        .unwrap_or_else(|_| "unknown".to_string());
    let text = format!(
        "path: {}\ntype: {kind}\nsize: {}\nmodified: {modified}\nreadonly: {}",
        root.display(&path),
        md.len(),
        md.permissions().readonly()
    );
    Ok(Reply {
        text,
        structured: json!({
            "op": "stat",
            "path": root.display(&path),
            "kind": kind,
            "size": md.len(),
            "readonly": md.permissions().readonly(),
        }),
        is_error: false,
    })
}

// ------------------------------------------------------------------- walking

fn walk_files<F: FnMut(&Path, &fs::DirEntry)>(root: &Path, mut visit: F) {
    let mut files = 0usize;
    walk_dir(root, root, 0, &mut files, &mut visit);
}

fn walk_dir<F: FnMut(&Path, &fs::DirEntry)>(
    root: &Path,
    dir: &Path,
    depth: usize,
    files: &mut usize,
    visit: &mut F,
) {
    if depth > MAX_WALK_DEPTH || *files >= MAX_WALK_FILES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *files >= MAX_WALK_FILES {
            return;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if skip_dir(&name) {
                continue;
            }
            if let Some(child) = child_dir(&entry, root) {
                walk_dir(root, &child, depth + 1, files, visit);
            }
        } else if ft.is_file() {
            *files += 1;
            visit(&entry.path(), &entry);
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

fn skip_dir(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if matches!(lower.as_str(), ".git" | ".hg" | ".svn") {
        return true;
    }
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

fn glob_match(pattern: &str, text: &str, ignore_case: bool) -> bool {
    glob_fast(pattern, text, ignore_case).unwrap_or_else(|| glob_general(pattern, text, ignore_case))
}

fn glob_general(pattern: &str, text: &str, ignore_case: bool) -> bool {
    let p: Vec<char> = if ignore_case {
        pattern.to_ascii_lowercase().chars().collect()
    } else {
        pattern.chars().collect()
    };
    let t: Vec<char> = if ignore_case {
        text.to_ascii_lowercase().chars().collect()
    } else {
        text.chars().collect()
    };
    let mut memo = vec![0u8; (p.len() + 1) * (t.len() + 1)];
    glob_go(&p, 0, &t, 0, &mut memo)
}

/// Allocation-free answer for the shapes that cover nearly every real pattern: `LIT`, `*LIT`, `**/LIT` and
/// `**/*LIT`, where LIT has no glob character. `**/` may start at any offset (so `**/name` also matches
/// `xname`, as `glob_go` does) and `*` cannot cross '/'. `None` means "use the general matcher".
fn glob_fast(pattern: &str, text: &str, ignore_case: bool) -> Option<bool> {
    let plain = |s: &str| !s.contains(['*', '?', '[', '\\']);
    let (lit, anywhere, star) = if let Some(rest) = pattern.strip_prefix("**/") {
        let lit = rest.strip_prefix('*').unwrap_or(rest);
        (lit, true, false)
    } else if let Some(lit) = pattern.strip_prefix('*') {
        (lit, false, true)
    } else {
        (pattern, false, false)
    };
    if !plain(lit) {
        return None;
    }
    let (t, l) = (text.as_bytes(), lit.as_bytes());
    let eq = |a: &[u8]| if ignore_case { a.eq_ignore_ascii_case(l) } else { a == l };
    Some(if anywhere {
        t.len() >= l.len() && eq(&t[t.len() - l.len()..])
    } else if star {
        t.len() >= l.len() && eq(&t[t.len() - l.len()..]) && !t[..t.len() - l.len()].contains(&b'/')
    } else {
        t.len() == l.len() && eq(t)
    })
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
                        glob_go(p, j + 1, t, k, memo) || (k < t.len() && glob_go(p, i, t, k + 1, memo))
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
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp(name: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "qx-fsops-{}-{}-{name}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn invoke(dir: &Path, op: &str, extra: Value) -> Reply {
        let mut args = json!({
            "op": op,
            "cwd": dir.to_string_lossy(),
        });
        if let (Value::Object(dst), Value::Object(src)) = (&mut args, extra) {
            for (k, v) in src {
                dst.insert(k, v);
            }
        }
        call(&args)
    }

    #[test]
    fn read_numbers_lines_and_observes_the_file() {
        let dir = tmp("read");
        fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let r = invoke(&dir, "read", json!({"path":"a.txt"}));
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("1: one"));
        assert!(r.text.contains("3: three"));
        assert!(r.text.contains("End of file - total 3 lines"));
        assert_eq!(r.structured["total_lines"], 3);

        let r = invoke(&dir, "write", json!({"path":"a.txt","content":"changed\n"}));
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("Updated file"));
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "changed\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_existing_without_read_is_refused() {
        let dir = tmp("not-observed");
        fs::write(dir.join("a.txt"), "old\n").unwrap();
        let refused = |c| {
            client::scoped(c, || invoke(&dir, "write", json!({"path":"a.txt","content":"new\n"})))
        };
        let r = refused(client::Client::Dsh);
        assert!(r.is_error);
        assert_eq!(r.structured["code"], "FS_NOT_OBSERVED");
        assert!(r.text.contains("native `write`"), "{}", r.text);
        let codex = refused(client::Client::Codex);
        assert!(codex.text.contains("native `apply_patch`"), "{}", codex.text);
        let generic = refused(client::Client::Generic);
        assert!(generic.text.contains("native write tool"), "{}", generic.text);
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "old\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_new_file_and_edit_replace_all() {
        let dir = tmp("edit");
        let r = invoke(
            &dir,
            "write",
            json!({"path":"src/a.rs","content":"let x = 1;\nlet x = 2;\n"}),
        );
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("Created file"));
        let r = invoke(
            &dir,
            "edit",
            json!({"path":"src/a.rs","old_string":"let x","new_string":"let y"}),
        );
        assert!(r.is_error);
        assert_eq!(r.structured["code"], "FS_AMBIGUOUS");
        let r = invoke(
            &dir,
            "edit",
            json!({
                "path":"src/a.rs","old_string":"let x","new_string":"let y","replace_all":true
            }),
        );
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(r.structured["replacements"], 2);
        assert_eq!(
            fs::read_to_string(dir.join("src/a.rs")).unwrap(),
            "let y = 1;\nlet y = 2;\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_refuses_files_it_would_corrupt_and_keeps_nuls() {
        let dir = tmp("edit-encoding");
        let latin1: &[u8] = b"caf\xe9 line1\nline2\n";
        let utf16: &[u8] = b"\xff\xfeh\0i\0\n\0x\0\n\0";
        for (name, bytes) in [("l1.txt", latin1), ("u16.txt", utf16)] {
            fs::write(dir.join(name), bytes).unwrap();
            let r = invoke(&dir, "read", json!({"path":name}));
            assert!(!r.is_error, "{}", r.text);
            let r = invoke(&dir, "edit", json!({"path":name,"old_string":"line2","new_string":"x"}));
            assert!(r.is_error, "{}", r.text);
            assert_eq!(r.structured["code"], "FS_ENCODING", "{name}");
            assert_eq!(fs::read(dir.join(name)).unwrap(), bytes, "{name} must stay byte-identical");
        }
        fs::write(dir.join("n.txt"), b"a\0b\nline2\n").unwrap();
        invoke(&dir, "read", json!({"path":"n.txt"}));
        let r = invoke(&dir, "edit", json!({"path":"n.txt","old_string":"line2","new_string":"x"}));
        if !r.is_error {
            assert_eq!(fs::read(dir.join("n.txt")).unwrap(), b"a\0b\nx\n");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_read_is_refused() {
        let dir = tmp("stale");
        fs::write(dir.join("a.txt"), "one\n").unwrap();
        let r = invoke(&dir, "read", json!({"path":"a.txt"}));
        assert!(!r.is_error, "{}", r.text);
        fs::write(dir.join("a.txt"), "two\n").unwrap();
        let r = invoke(&dir, "write", json!({"path":"a.txt","content":"three\n"}));
        assert!(r.is_error);
        assert_eq!(r.structured["code"], "FS_STALE");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_refuses_a_path_outside_cwd() {
        let base = tmp("outside");
        let cwd = base.join("work");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(base.join("secret.txt"), "nope\n").unwrap();
        let r = invoke(&cwd, "read", json!({"path": base.join("secret.txt")}));
        assert!(r.is_error);
        assert_eq!(r.structured["code"], "FS_OUTSIDE_WORKSPACE");
        let _ = fs::remove_dir_all(&base);
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
        assert!(!r.text.contains("node_modules"));
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
        assert!(r.text.contains("src/a.rs:2: needle here"));
        assert!(r.text.contains("src/b.txt:1: needle here too"));
        assert_eq!(r.structured["count"], 2);

        let r = invoke(&dir, "grep", json!({"pattern":"needle","include":"*.rs"}));
        assert!(r.text.contains("src/a.rs"));
        assert!(!r.text.contains("src/b.txt"));

        // qx grep is literal: regex metacharacters do not change the match.
        let r = invoke(&dir, "grep", json!({"pattern":"n.*e"}));
        assert!(r.text.contains("No matches found"));
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
        assert!(r.text.contains("a.txt:1: needle"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_and_stat_work() {
        let dir = tmp("list");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.rs"), "abc").unwrap();
        let r = invoke(&dir, "list", json!({}));
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("d src/"));
        let r = invoke(&dir, "stat", json!({"path":"src/a.rs"}));
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("type: file"));
        assert!(r.text.contains("size: 3"));
        let _ = fs::remove_dir_all(&dir);
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
}
