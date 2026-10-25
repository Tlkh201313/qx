//! In-process filesystem operations for the MCP `fs` tool.
//!
//! The fast paths are deliberately workspace-confined: every path is resolved
//! against an explicit `cwd`, symlinks may not escape that root, and mutations
//! require a prior read of the same version. Writes are atomic (temp + replace)
//! and never touch VCS metadata. When an operation cannot be completed safely,
//! the reply carries a stable code and tells the caller to use the client's own
//! tool instead.

use crate::client::{current, Native};
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_READ_BYTES: u64 = 32 * 1024 * 1024;

/// The current client's name for one of its own tools, for messages shaped `the native {} tool`.
fn native(n: Native) -> &'static str {
    current().native(n)
}
const MAX_WRITE_BYTES: usize = 16 * 1024 * 1024;
const MAX_EDIT_BYTES: usize = 1024 * 1024;
const MAX_GREP_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Past this a file is skipped (and said so); between the two it is streamed.
const MAX_GREP_STREAM_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_WALK_FILES: usize = 200_000;
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

/// The last `cwd` a call named. dsh gives the server no workspace of its own, so a call that omits
/// `cwd` most likely means the one the session has been using; the reply says which it used.
static LAST_CWD: Mutex<Option<PathBuf>> = Mutex::new(None);

pub(crate) fn last_cwd() -> Option<PathBuf> {
    LAST_CWD.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub(crate) fn note_cwd(dir: &Path) {
    *LAST_CWD.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.to_path_buf());
    #[cfg(not(test))]
    prime(dir);
}

/// Pull a workspace into the OS file cache in the background the first time it is named, so the first
/// glob or grep over it is not a cold disk walk. Bounded in entries and bytes; never blocks a call.
pub(crate) fn prime(dir: &Path) {
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
                        if md.len() <= MAX_FILE {
                            bytes += md.len();
                            let _ = fs::read(e.path());
                        }
                    }
                }
            }
        }
    });
}

pub fn call(args: &Value) -> Reply {
    let started = Instant::now();
    let defaulted = matches!(args.get("cwd"), None | Some(Value::Null));
    let op = args
        .get("op")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut reply = match batch_items(args, &op) {
        Some(Ok(items)) => run_batch(&items),
        Some(Err(e)) => e.reply("batch"),
        None => match dispatch(&op, args) {
            Ok(r) => r,
            Err(e) => e.reply(&op),
        },
    };
    if defaulted && !reply.is_error {
        let used = last_cwd().or_else(|| std::env::current_dir().ok());
        if let Some(d) = used {
            reply.text.push_str(&format!("\n[qx] no cwd given: used {}", d.display()));
        }
    }
    if let Value::Object(map) = &mut reply.structured {
        map.insert(
            "duration_ms".to_string(),
            json!(started.elapsed().as_millis() as u64),
        );
    }
    reply
}

const MAX_BATCH: usize = 32;
/// Past this much combined text a batch stops running further operations: the model asks again for the rest.
const MAX_BATCH_BYTES: usize = 256 * 1024;

/// `ops: [{op, ...}]` runs several operations in one round trip; `paths: [..]` is the same for
/// many `read`s or `stat`s. Each item inherits the call's other arguments (cwd above all).
fn batch_items(args: &Value, op: &str) -> Option<Result<Vec<Value>, FsError>> {
    let inherit = |item: &Value| -> Value {
        let mut m = item.as_object().cloned().unwrap_or_default();
        if let Some(outer) = args.as_object() {
            for (k, v) in outer {
                if !matches!(k.as_str(), "ops" | "paths" | "op" | "path" | "file_path") && !m.contains_key(k) {
                    m.insert(k.clone(), v.clone());
                }
            }
        }
        Value::Object(m)
    };
    if let Some(ops) = args.get("ops").filter(|v| !v.is_null()) {
        let Some(list) = ops.as_array().filter(|a| !a.is_empty() && a.iter().all(Value::is_object)) else {
            return Some(Err(FsError::arg("ops must be a non-empty array of {op, ...} objects")));
        };
        if list.len() > MAX_BATCH {
            return Some(Err(FsError::arg(format!("ops is capped at {MAX_BATCH} per call"))));
        }
        if list.iter().any(|i| i.get("ops").is_some() || i.get("paths").is_some()) {
            return Some(Err(FsError::arg("ops cannot nest")));
        }
        return Some(Ok(list.iter().map(inherit).collect()));
    }
    if matches!(op, "read" | "stat") {
        if let Some(paths) = args.get("paths").and_then(Value::as_array).filter(|a| !a.is_empty()) {
            if paths.len() > MAX_BATCH {
                return Some(Err(FsError::arg(format!("paths is capped at {MAX_BATCH} per call"))));
            }
            let own = json!({ "op": op });
            return Some(Ok(paths
                .iter()
                .map(|p| {
                    let mut item = inherit(&own);
                    item["path"] = p.clone();
                    item
                })
                .collect()));
        }
    }
    None
}

fn run_batch(items: &[Value]) -> Reply {
    let n = items.len();
    let mut text = String::new();
    let mut results = Vec::with_capacity(n);
    let (mut failed, mut stopped) = (0usize, false);
    for (i, item) in items.iter().enumerate() {
        let op = item.get("op").and_then(Value::as_str).unwrap_or("").to_string();
        let what = path_arg(item)
            .or_else(|| opt_str(item, "pattern"))
            .unwrap_or("")
            .to_string();
        text.push_str(&format!("== {}/{} {} {} ==\n", i + 1, n, op, what));
        let mutates = matches!(op.as_str(), "write" | "edit");
        if stopped {
            text.push_str("skipped: an earlier write or edit in this batch failed\n\n");
            results.push(json!({ "op": op, "path": what, "ok": false, "skipped": true }));
            failed += 1;
            continue;
        }
        if text.len() > MAX_BATCH_BYTES {
            text.push_str("skipped: batch output cap reached; ask for the rest in another call\n\n");
            results.push(json!({ "op": op, "path": what, "ok": false, "skipped": true }));
            failed += 1;
            continue;
        }
        let r = match dispatch(&op, item) {
            Ok(r) => r,
            Err(e) => e.reply(&op),
        };
        if r.is_error {
            failed += 1;
            stopped = mutates;
        }
        results.push(json!({ "op": op, "path": what, "ok": !r.is_error }));
        text.push_str(r.text.trim_end());
        text.push_str("\n\n");
    }
    Reply {
        text: text.trim_end().to_string(),
        structured: json!({ "op": "batch", "count": n, "failed": failed, "results": results }),
        is_error: failed == n,
    }
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

/// Models send `60`, `"60"` or `60.0` and often a value just outside the range: take what can only
/// mean a number and clamp it. A hard error here costs a whole round trip for no gain.
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
            .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as usize))
            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<usize>().ok()))
            .ok_or_else(|| FsError::arg(format!("{key} must be an integer")))?,
    };
    Ok(n.clamp(min, max))
}
fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    match args.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => true,
        Some(Value::String(s)) if s.eq_ignore_ascii_case("false") => false,
        _ => default,
    }
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
        // Omitting `cwd` means "where the server was started", like the CLI. Every fs call
        // otherwise fails on a field the model has no reason to know.
        let p = match args.get("cwd") {
            None | Some(Value::Null) => match last_cwd() {
                Some(d) => d,
                None => std::env::current_dir().map_err(|e| FsError::io("cannot resolve cwd", e))?,
            },
            Some(Value::String(s)) if !s.trim().is_empty() => PathBuf::from(s),
            Some(_) => {
                return Err(FsError::arg("cwd must be a non-empty string").fb("tool"));
            }
        };
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
        if matches!(args.get("cwd"), Some(Value::String(_))) {
            note_cwd(&p);
        }
        Ok(Self { cwd })
    }

    fn candidate(&self, raw: &str) -> PathBuf {
        let raw = raw.trim();
        // A path pasted with its quotes, or `~/x`: say what was meant instead of failing.
        let raw = raw
            .strip_prefix('"').and_then(|r| r.strip_suffix('"'))
            .or_else(|| raw.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
            .unwrap_or(raw);
        let home = if raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\") { crate::resolve::home_dir() } else { None };
        let p = match home {
            Some(h) => h.join(raw[1..].trim_start_matches(['/', '\\'])),
            None => PathBuf::from(raw),
        };
        if p.is_absolute() {
            p
        } else {
            self.cwd.join(p)
        }
    }

    fn display(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.cwd).unwrap_or(path);
        let s = rel.to_string_lossy().replace('\\', "/");
        let s = s.strip_prefix("//?/").map(str::to_string).unwrap_or(s);
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

    /// Reads (read, glob, grep, list, stat) may look anywhere the user can: the dsh read tool is not
    /// confined to the workspace, and a fallback for every path outside `cwd` costs a round trip.
    /// Only mutations stay inside `cwd`.
    fn resolve_read(&self, raw: &str) -> Result<PathBuf, FsError> {
        let candidate = self.candidate(raw);
        fs::canonicalize(&candidate).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                FsError::new("FS_NOT_FOUND", format!("cannot access \"{raw}\": not found"))
            }
            _ => FsError::io(format!("cannot access \"{raw}\""), e),
        })
    }

    /// Resolve a file for creation or replacement. Existing parent directories
    /// may be created, but every existing ancestor is canonicalized and checked
    /// first, so a symlink or junction cannot walk the write outside the root.
    fn resolve_for_write(&self, raw: &str) -> Result<(PathBuf, bool), FsError> {
        // Relative paths are joined onto a verbatim `\\?\` cwd, which skips Win32 name rules: `nul`
        // or `name.` would become real, undeletable files and `a.txt:x` an alternate data stream.
        if let Some(bad) = Path::new(raw).components().find_map(|c| match c {
            Component::Normal(s) => win_name_problem(&s.to_string_lossy()),
            _ => None,
        }) {
            return Err(FsError::arg(format!("\"{raw}\": {bad}")).fb("write"));
        }
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
        // Before any directory is made: a refused `.git/x/y` must leave no `.git/x` behind.
        self.check_mutable(&candidate)?;
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
        // The whole path, not just the part below cwd: a caller may pick cwd = `.git/hooks`.
        for c in path.components() {
            if let Component::Normal(s) = c {
                let s = s.to_string_lossy();
                if [".git", ".hg", ".svn"].iter().any(|v| s.eq_ignore_ascii_case(v)) {
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

/// Why Windows cannot hold a file or directory with this name, if it cannot.
fn win_name_problem(name: &str) -> Option<&'static str> {
    if name.contains(':') {
        return Some("a ':' in a name makes an alternate data stream, not a file");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("Windows strips a trailing '.' or ' ', so the file could not be opened again");
    }
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit() && stem.as_bytes()[3] != b'0');
    reserved.then_some("a reserved device name (CON, PRN, AUX, NUL, COM1-9, LPT1-9)")
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

/// Keyed by the file alone: the same file reached through another `cwd` is the same file.
fn remember(_cwd: &Path, path: &Path, version: Version) {
    observations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((PathBuf::new(), path.to_path_buf()), version);
}

fn check_observed(_cwd: &Path, path: &Path, current: Version, display: &str) -> Result<(), FsError> {
    let map = observations().lock().unwrap_or_else(|e| e.into_inner());
    match map.get(&(PathBuf::new(), path.to_path_buf())) {
        None => Err(FsError::new(
            "FS_NOT_OBSERVED",
            format!(
                "cannot modify \"{display}\": read it first (op=read; a partial read counts), then write; or use the native {} tool",
                native(Native::Write)
            ),
        )),
        // Content decides, not the clock: a `touch` or a save without changes is not a conflict.
        Some(v) if v.len != current.len || v.hash != current.hash => Err(FsError::new(
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

/// `ReplaceFileW` keeps what belongs to the file rather than its bytes: alternate data streams
/// (a download's `Zone.Identifier`), ACLs, attributes and creation time. Where it cannot (some
/// file systems and shares), a plain replacing move still lands the new content.
#[cfg(windows)]
fn replace_file(temp: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn ReplaceFileW(
            replaced: *const u16,
            replacement: *const u16,
            backup: *const u16,
            flags: u32,
            exclude: *mut std::ffi::c_void,
            reserved: *mut std::ffi::c_void,
        ) -> i32;
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    const REPLACEFILE_IGNORE_MERGE_ERRORS: u32 = 0x2;
    const REPLACEFILE_IGNORE_ACL_ERRORS: u32 = 0x4;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let wide = |p: &Path| p.as_os_str().encode_wide().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (old, new) = (wide(temp), wide(target));
    let null = std::ptr::null_mut();
    let flags = REPLACEFILE_IGNORE_MERGE_ERRORS | REPLACEFILE_IGNORE_ACL_ERRORS;
    // SAFETY: NUL-terminated paths; no backup file, no exclusions.
    if unsafe { ReplaceFileW(new.as_ptr(), old.as_ptr(), std::ptr::null(), flags, null, null) } != 0 {
        return Ok(());
    }
    // Every failure leaves the replacement under its own name, and the target either as it was or
    // gone; the move puts the new content in place either way.
    // SAFETY: as above.
    if unsafe { MoveFileExW(old.as_ptr(), new.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
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
    // An editor, indexer or virus scanner may hold the file for a moment without delete sharing.
    let mut attempt = 0;
    loop {
        match replace_file(&temp.path, path) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 5 && matches!(e.raw_os_error(), Some(5 | 32 | 33)) => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(40 * attempt));
            }
            Err(e) => return Err(FsError::io(format!("cannot replace \"{}\"", path.display()), e)),
        }
    }
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
    let path = root.resolve_read(raw).map_err(|e| e.fb("read"))?;
    if fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() > MAX_READ_BYTES) {
        return read_large(root, &path, raw, offset, limit, max_line_chars);
    }
    if fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
        // A directory is a listing: the same answer `list` gives, in one call instead of an error and a retry.
        return list(root, &json!({ "path": raw }));
    }
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

/// A file too big to load: stream it to the requested window. Not observed, so it cannot be
/// written back whole; `edit` still works on files under its own size cap.
fn read_large(root: &Root, path: &Path, raw: &str, offset: usize, limit: usize, max_line_chars: usize) -> Result<Reply, FsError> {
    use std::io::{BufRead, BufReader};
    let f = fs::File::open(path).map_err(|e| FsError::io(format!("cannot read \"{raw}\""), e).fb("read"))?;
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut r = BufReader::with_capacity(1 << 20, f);
    let head = r.fill_buf().map_err(|e| FsError::io(format!("cannot read \"{raw}\""), e).fb("read"))?;
    if head.iter().take(8192).any(|&b| b == 0) {
        return Err(FsError::new("FS_BINARY", format!("cannot read \"{raw}\" as text: binary data")).fb("read"));
    }
    let (mut buf, mut body) = (Vec::new(), String::new());
    let (mut n, mut shown, mut more) = (0usize, 0usize, false);
    let mut end_line = offset.saturating_sub(1);
    loop {
        buf.clear();
        let got = r.read_until(b'\n', &mut buf).map_err(|e| FsError::io(format!("cannot read \"{raw}\""), e).fb("read"))?;
        if got == 0 {
            break;
        }
        n += 1;
        if n < offset {
            continue;
        }
        if shown >= limit {
            more = true;
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);
        let mut one = String::new();
        push_usize(&mut one, n);
        one.push_str(": ");
        push_clipped(&mut one, line, max_line_chars);
        if body.len() + one.len() + 1 > DEFAULT_READ_BYTES {
            more = true;
            break;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&one);
        shown += 1;
        end_line = n;
    }
    let display = root.display(path);
    let footer = if more {
        format!("(Showing lines {offset}-{end_line} of a {size}-byte file. Use offset={} to continue.)", end_line + 1)
    } else {
        format!("(End of file - total {n} lines)")
    };
    Ok(Reply {
        text: format!("<path>{display}</path>\n<type>file</type>\n<content>\n{body}\n\n{footer}\n</content>"),
        structured: json!({ "op": "read", "path": display, "lines": shown, "end_line": end_line, "truncated": more, "bytes": size }),
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
        return Ok(ReadWindow {
            text: format!("<path>{display}</path>\n<type>file</type>\n<content>\n(offset {offset} is past the end: the file has {total} lines)\n</content>"),
            lines: 0,
            total,
            end_line: total,
            truncated: false,
        });
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
    // A UTF-8 BOM is not part of the first line; write keeps it (see `carry_over_encoding`).
    if let Some(rest) = s.strip_prefix('\u{feff}') {
        return std::borrow::Cow::Owned(rest.replace('\0', ""));
    }
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

    let mut written = std::borrow::Cow::Borrowed(content);
    if existed {
        let (before, current) = read_file(&path, "write")?;
        check_observed(&root.cwd, &path, current, &root.display(&path))
            .map_err(|e| e.fb("write"))?;
        // `read` showed lossy, LF-only text: writing it back must not re-encode or flatten the file.
        match carry_over_encoding(&before, content) {
            Ok(text) => written = std::borrow::Cow::Owned(text),
            Err(()) => {
                return Err(FsError::new(
                    "FS_ENCODING",
                    format!("\"{raw}\" is not plain UTF-8 text (UTF-16, a legacy code page or NUL bytes); qx write would corrupt it"),
                )
                .fb("write"));
            }
        }
        atomic_replace(&path, written.as_bytes(), current).map_err(|e| e.fb("write"))?;
    } else {
        create_noreplace(&path, content.as_bytes()).map_err(|e| e.fb("write"))?;
    }
    let content = written.as_ref();
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

/// One `{old_string, new_string, replace_all?}` replacement, applied to `text` in memory.
fn apply_edit(text: &str, raw: &str, e: &Value, label: &str) -> Result<(String, usize), FsError> {
    let old = match e.get("old_string").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s,
        _ => return Err(FsError::arg(format!("{label}old_string must be a non-empty string"))),
    };
    let new = match e.get("new_string").and_then(Value::as_str) {
        Some(s) => s,
        None => return Err(FsError::arg(format!("{label}new_string must be a string"))),
    };
    if old == new {
        return Err(FsError::arg(format!("{label}old_string and new_string must differ")));
    }
    if old.len() > MAX_EDIT_BYTES || new.len() > MAX_EDIT_BYTES {
        return Err(FsError::new(
            "FS_TOO_LARGE",
            format!("{label}edit strings are capped at {MAX_EDIT_BYTES} bytes"),
        )
        .fb("edit"));
    }
    let replace_all = bool_arg(e, "replace_all", false);
    let (old, new) = match_line_endings(text, old, new);
    let count = text.matches(&old).count();
    if count == 0 {
        // Say where the closest thing is: usually an indentation or a stale line, found in one step.
        let first = old.lines().map(str::trim).find(|l| l.len() > 3).unwrap_or("");
        let hint = text
            .lines()
            .position(|l| !first.is_empty() && l.contains(first))
            .map(|i| format!("; its first line appears at line {} (whitespace or later lines differ)", i + 1))
            .unwrap_or_default();
        return Err(FsError::new(
            "FS_NO_MATCH",
            format!("{label}old_string was not found in \"{raw}\"{hint}"),
        )
        .fb("edit"));
    }
    if count > 1 && !replace_all {
        return Err(FsError::new(
            "FS_AMBIGUOUS",
            format!("{label}old_string occurs {count} times in \"{raw}\"; include more context or set replace_all"),
        )
        .fb("edit"));
    }
    let updated = if replace_all { text.replace(&old, &new) } else { text.replacen(&old, &new, 1) };
    Ok((updated, if replace_all { count } else { 1 }))
}

fn edit(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let raw = req_path(args)?;
    // `edits` is a batch on one file: applied in order to the text as the earlier ones left it, written
    // once, and all-or-nothing, so a model fixes five places in one round trip instead of five.
    let batch: Option<&Vec<Value>> = args.get("edits").and_then(Value::as_array).filter(|a| !a.is_empty());
    if batch.is_none() && args.get("edits").is_some_and(|v| !v.is_null()) {
        return Err(FsError::arg("edits must be a non-empty array of {old_string, new_string, replace_all?}"));
    }
    if batch.is_some_and(|b| b.len() > 64) {
        return Err(FsError::arg("edits is capped at 64 per call"));
    }
    let _guard = mutation_lock().lock().unwrap_or_else(|e| e.into_inner());
    let path = root.resolve_existing(raw).map_err(|e| e.fb("edit"))?;
    root.check_mutable(&path).map_err(|e| e.fb("edit"))?;
    let (bytes, current) = read_file(&path, "edit")?;
    // No prior read is required: `old_string` must match the current text exactly once, which is a
    // stronger check than "was read", and `atomic_replace` still refuses a file that changed meanwhile.

    // Strict: a lossy decode would rewrite UTF-16/Latin-1 files as UTF-8 and drop NULs on save.
    // The UTF-16 BOMs are never valid UTF-8, so this one check covers them too.
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Err(FsError::new(
            "FS_ENCODING",
            format!("\"{raw}\" is not valid UTF-8 (UTF-16 or a legacy code page); qx edit would corrupt it"),
        )
        .fb("edit"));
    };
    let mut updated = text.to_string();
    let mut replacements = 0;
    match batch {
        Some(list) => {
            for (i, e) in list.iter().enumerate() {
                let label = format!("edits[{i}]: ");
                let (next, n) = apply_edit(&updated, raw, e, &label)?;
                updated = next;
                replacements += n;
            }
        }
        None => {
            let (next, n) = apply_edit(text, raw, args, "")?;
            updated = next;
            replacements = n;
        }
    }
    let replace_all = batch.is_none() && bool_arg(args, "replace_all", false);
    atomic_replace(&path, updated.as_bytes(), current).map_err(|e| e.fb("edit"))?;
    let version = version_after(&path, updated.as_bytes()).map_err(|e| e.fb("edit"))?;
    remember(&root.cwd, &path, version);
    let display = root.display(&path);
    let suffix = match (batch, replace_all) {
        (Some(b), _) => format!(" {} edits applied.", b.len()),
        (None, true) => " All occurrences were successfully replaced.".to_string(),
        _ => String::new(),
    };
    Ok(Reply {
        text: format!("The file {display} has been updated successfully.{suffix}"),
        structured: json!({
            "op": "edit",
            "path": display,
            "replacements": replacements,
            "bytes": updated.len(),
        }),
        is_error: false,
    })
}
/// The model sees LF-only text (`read` strips the CRs), so `old` and `new` arrive with LF. Try `old`
/// verbatim first (it may sit in the LF part of a mixed file); only when it is absent, spell it in the
/// file's other ending. `new` then takes the ending of the text it replaces, whether or not `old` itself
/// held a line break.
fn match_line_endings(text: &str, old: &str, new: &str) -> (String, String) {
    let crlf = text.matches("\r\n").count();
    let lf_only = text.matches('\n').count().saturating_sub(crlf);
    let mut old = old.to_string();
    if !text.contains(&old) {
        if crlf > 0 && old.contains('\n') && !old.contains("\r\n") {
            old = old.replace('\n', "\r\n");
        } else if crlf == 0 && old.contains("\r\n") {
            old = old.replace("\r\n", "\n");
        }
    }
    let new = if old.contains("\r\n") || (crlf > lf_only && !old.contains('\n')) {
        new.replace("\r\n", "\n").replace('\n', "\r\n")
    } else if crlf == 0 {
        new.replace("\r\n", "\n")
    } else {
        new.to_string()
    };
    (old, new)
}

/// What `write` must keep of the file it replaces: the model read LF-only text without the BOM.
fn carry_over_encoding(before: &[u8], content: &str) -> Result<String, ()> {
    let Ok(old) = std::str::from_utf8(before) else {
        return Err(()); // UTF-16 or a legacy code page: rewriting it as UTF-8 would corrupt it
    };
    // `read` hides NUL bytes, so content made from what it showed would silently drop them.
    if old.contains('\0') && !content.contains('\0') {
        return Err(());
    }
    let crlf = old.matches("\r\n").count();
    let lf_only = old.matches('\n').count().saturating_sub(crlf);
    let mut out = content.to_string();
    if crlf > lf_only && !out.contains('\r') {
        out = out.replace('\n', "\r\n");
    }
    if old.starts_with('\u{feff}') && !out.starts_with('\u{feff}') {
        out.insert(0, '\u{feff}');
    }
    Ok(out)
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
    let root_path = root.resolve_read(raw_path).map_err(|e| e.fb("glob"))?;
    let max_results = usize_arg(
        args,
        "max_results",
        DEFAULT_GLOB_RESULTS,
        1,
        MAX_GLOB_RESULTS,
    )?;
    let ignore_case = bool_arg(args, "ignore_case", false);
    // Asking for a generated directory by name (`node_modules/**`) is asking to look inside it.
    let all = bool_arg(args, "all", false) || patterns.iter().any(|p| names_skipped_dir(p)) || names_skipped_dir(raw_path);
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
        if matches_any(&patterns, &rel, &rel, ignore_case) {
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
            dirs.sort();
            dirs.dedup_by(|b, a| b.starts_with(&format!("{a}/")) || b == a);
            hints = dirs;
        }
        let (parts, skipped_dirs) = walk_hinted(&root_path, all, &hints, GlobPart::default, |part, p, meta| {
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
            if matches_any(&patterns, &rel, &base, ignore_case) {
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
    if !skipped.is_empty() {
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
}

fn keep_newest(hits: &mut Newest, max_results: usize, modified: SystemTime, p: &Path) {
    if hits.len() < max_results || hits.peek().is_some_and(|Reverse((t, _))| modified >= *t) {
        hits.push(Reverse((modified, Reverse(p.to_path_buf()))));
        if hits.len() > max_results {
            hits.pop();
        }
    }
}

fn matches_any(patterns: &[String], rel: &str, base: &str, ignore_case: bool) -> bool {
    patterns.iter().any(|p| {
        let text = if p.contains('/') { rel } else { base };
        glob_match(p, text, ignore_case)
    })
}

// --------------------------------------------------------------------- grep

/// How one `grep` call matches a line: a regex (what the dsh grep tool takes) or plain text.
enum Matcher {
    Re(regex::Regex),
    Lit { needle: String, ci: bool },
}

impl Matcher {
    fn is_match(&self, line: &str) -> bool {
        match self {
            Matcher::Re(re) => re.is_match(line),
            Matcher::Lit { needle, ci: false } => line.contains(needle.as_str()),
            Matcher::Lit { needle, ci: true } => contains_ascii_ci(line, needle),
        }
    }
}

fn grep(root: &Root, args: &Value) -> Result<Reply, FsError> {
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
    let include_patterns = include.map(|s| expand_braces(&s.replace('\\', "/")));
    let ignore_case = bool_arg(args, "ignore_case", false);
    let all = bool_arg(args, "all", false)
        || names_skipped_dir(raw_path)
        || include_patterns.as_ref().is_some_and(|ps| ps.iter().any(|p| names_skipped_dir(p)));
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

    // Regex by default, like the dsh grep tool. A pattern that is not a valid regex (`foo(`, a lone `[`)
    // is searched as plain text instead of failing, and the reply says so.
    let mut literal_fallback = false;
    let matcher = if bool_arg(args, "literal", false) {
        Matcher::Lit {
            needle: if ignore_case { needle.to_ascii_lowercase() } else { needle.to_string() },
            ci: ignore_case,
        }
    } else {
        match regex::RegexBuilder::new(needle)
            .case_insensitive(ignore_case)
            .size_limit(32 * 1024 * 1024)
            .build()
        {
            Ok(re) => Matcher::Re(re),
            Err(_) => {
                literal_fallback = true;
                Matcher::Lit {
                    needle: if ignore_case { needle.to_ascii_lowercase() } else { needle.to_string() },
                    ci: ignore_case,
                }
            }
        }
    };

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
        files.retain(|(p, _)| {
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

    let trace = std::env::var_os("QX_FS_TRACE").is_some();
    let t_scan = Instant::now();
    let panicked = AtomicBool::new(false);
    let fast = fast_regex(&matcher, needle, bool_arg(args, "literal", false));
    let fast = fast.as_ref();
    let matcher = &matcher;
    // Threads cost ~60 us each to start: only as many as the work is worth. A file whose text is
    // cached takes ~10 us; one that must be opened and read, ~150 us.
    let cold = files.iter().filter(|(p, m)| m.map_or(true, |m| content_get(p, m).is_none())).count();
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
                if let Ok(v) = h.join() {
                    out.extend(v);
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
        for m in &sf.matches {
            if shown >= max_matches {
                break 'files;
            }
            out.push('\n');
            push_usize(&mut out, m.line);
            out.push_str(": ");
            out.push_str(&m.text);
            shown += 1;
        }
    }
    if total == 0 {
        out.push_str("No matches found.\n");
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
    if skipped_binary > 0 || skipped_large > 0 || read_errors > 0 {
        out.push_str(&format!(
            "[qx] skipped {skipped_binary} binary, {skipped_large} large, {read_errors} unreadable file(s).\n"
        ));
    }
    if !skipped_dirs.is_empty() {
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
const MAX_CONTENT_BYTES: usize = 128 << 20;
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
    if CONTENT_BYTES.load(Ordering::Relaxed) + size > MAX_CONTENT_BYTES {
        return;
    }
    if let Ok(mut guard) = CONTENT.write() {
        let map = guard.get_or_insert_with(HashMap::new);
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
    let text = crate::exec::decode(&bytes);
    if let Some(m) = indexed {
        if bytes.len() <= MAX_CACHED_FILE {
            content_put(path, m, Cached::Text(std::sync::Arc::from(&*text)));
        }
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
    match fast {
        Some(re) => scan_buffer(text, 1, re, matcher, max_line_chars, max_store, sf),
        None => {
            for (idx, line) in text.split('\n').enumerate() {
                let line = line.strip_suffix('\r').unwrap_or(line);
                if !matcher.is_match(line) {
                    continue;
                }
                sf.total += 1;
                if sf.matches.len() < max_store {
                    sf.matches.push(Match { line: idx + 1, text: clip_line(line, max_line_chars) });
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
            sf.matches.push(Match { line: line_no, text: clip_line(line, max_line_chars) });
        }
    }
}

/// The multi-line search expression for `scan_buffer`, when it is certain to find every line the
/// per-line matcher would accept: a pattern with no anchors or flags that mean something different
/// on a whole text than on one line.
fn fast_regex(matcher: &Matcher, pattern: &str, literal: bool) -> Option<regex::Regex> {
    let source = match matcher {
        Matcher::Lit { needle, ci } if literal || !pattern.is_empty() => {
            let _ = ci;
            if needle.is_empty() {
                return None;
            }
            format!("{}{}", if *ci { "(?i)" } else { "" }, regex::escape(needle))
        }
        Matcher::Re(_) => {
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
            format!("(?mR){p}")
        }
        _ => return None,
    };
    regex::RegexBuilder::new(&source).size_limit(32 * 1024 * 1024).build().ok()
}

/// A file too big to load whole is searched line by line.
fn scan_stream(path: &Path, matcher: &Matcher, fast: Option<&regex::Regex>, max_line_chars: usize, max_store: usize, sf: &mut ScanFile) {
    use std::io::{BufRead, BufReader, Read};
    let Ok(f) = fs::File::open(path) else {
        sf.error = true;
        return;
    };
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
    if let Some(re) = fast {
        // 4 MB at a time, cut at the last line break: the whole-buffer search at memory speed.
        const CHUNK: usize = 4 << 20;
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
            carry.extend_from_slice(&block[..n]);
            let cut = if n == 0 { carry.len() } else { carry.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1) };
            if cut > 0 {
                let text = String::from_utf8_lossy(&carry[..cut]);
                scan_buffer(&text, first_line, re, matcher, max_line_chars, max_store, sf);
                first_line += carry[..cut].iter().filter(|&&b| b == b'\n').count();
                carry.drain(..cut);
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
        match r.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
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
            sf.matches.push(Match { line: idx, text: clip_line(line, max_line_chars) });
        }
    }
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

/// What a search left out on purpose, so "no matches" is never mistaken for "searched everywhere".
fn skip_note(skipped: &[String]) -> String {
    format!("[qx] not searched: {} (all=true includes them)\n", skipped.join(", "))
}

// --------------------------------------------------------------------- list

fn list(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let raw = path_arg(args).unwrap_or(".");
    let path = root.resolve_read(raw).map_err(|e| e.fb("list"))?;
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
    if rows.is_empty() {
        out.push_str("(empty)\n");
    }
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
    let path = root.resolve_read(raw).map_err(|e| e.fb("stat"))?;
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
    if !all {
        if let Some((snap, prefix)) = crate::index::snapshot(root) {
            return walk_snapshot(&snap, &prefix, hints, init, visit);
        }
    }
    walk_live(root, all, init, visit)
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
/// timing).
pub(crate) fn walk_live<S, I, F>(root: &Path, all: bool, init: I, visit: F) -> (Vec<S>, Vec<String>)
where
    S: Send,
    I: Fn() -> S + Sync,
    F: Fn(&mut S, &Path, &FileMeta) + Sync,
{
    let walk = Walk {
        root,
        all,
        skipped: Mutex::new(std::collections::BTreeSet::new()),
        state: Mutex::new(WalkState {
            dirs: vec![(root.to_path_buf(), 0)],
            pending: 1,
        }),
        wake: Condvar::new(),
        files: AtomicUsize::new(0),
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
    (parts, skipped)
}

struct Walk<'a> {
    root: &'a Path,
    /// Also enter the generated directories of `skip_dir` (never VCS metadata).
    all: bool,
    /// Generated directories left out, so a reply can say what it did not search.
    skipped: Mutex<std::collections::BTreeSet<String>>,
    state: Mutex<WalkState>,
    wake: Condvar,
    files: AtomicUsize,
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
        if depth > MAX_WALK_DEPTH || self.files.load(Ordering::Relaxed) >= MAX_WALK_FILES {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
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
/// `**/*LIT`, where LIT has no glob character. `**/LIT` needs LIT to start a path segment, while the `*`
/// of `**/*LIT` may start inside a name; `*` cannot cross '/'. `None` means "use the general matcher".
fn glob_fast(pattern: &str, text: &str, ignore_case: bool) -> Option<bool> {
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
    if !plain(lit) {
        return None;
    }
    let (t, l) = (text.as_bytes(), lit.as_bytes());
    let eq = |a: &[u8]| if ignore_case { a.eq_ignore_ascii_case(l) } else { a == l };
    Some(if anywhere {
        t.len() >= l.len()
            && eq(&t[t.len() - l.len()..])
            && (!segment || t.len() == l.len() || t[t.len() - l.len() - 1] == b'/')
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
            crate::client::scoped(c, || {
                invoke(&dir, "write", json!({"path":"a.txt","content":"new\n"}))
            })
        };
        let r = refused(crate::client::Client::Dsh);
        assert!(r.is_error);
        assert_eq!(r.structured["code"], "FS_NOT_OBSERVED");
        assert!(r.text.contains("native `write`"), "{}", r.text);
        let codex = refused(crate::client::Client::Codex);
        assert!(codex.text.contains("native `apply_patch`"), "{}", codex.text);
        let generic = refused(crate::client::Client::Generic);
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
            assert_eq!(r.structured["code"], "FS_ENCODING", "{name}: {}", r.text);
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
        // Reads may go anywhere; only mutations are confined to cwd.
        let r = invoke(&cwd, "read", json!({"path": base.join("secret.txt")}));
        assert!(!r.is_error && r.text.contains("1: nope"), "{}", r.text);
        let r = invoke(&cwd, "write", json!({"path": base.join("new.txt"), "content": "x"}));
        assert!(r.is_error);
        assert_eq!(r.structured["code"], "FS_OUTSIDE_WORKSPACE");
        assert!(!base.join("new.txt").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn vcs_metadata_is_protected_whatever_the_cwd() {
        let base = tmp("vcs-cwd");
        let hooks = base.join("repo").join(".git").join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        // A cwd chosen inside .git must not turn its contents into ordinary workspace files.
        let r = invoke(&hooks, "write", json!({"path":"pre-commit","content":"#!/bin/sh\necho hi\n"}));
        assert!(r.is_error, "{}", r.text);
        assert_eq!(r.structured["code"], "FS_PROTECTED");
        assert!(!hooks.join("pre-commit").exists());
        // Refused before any directory on the way is made.
        let repo = base.join("repo");
        let r = invoke(&repo, "write", json!({"path":".git/zzz/y.txt","content":"x"}));
        assert_eq!(r.structured["code"], "FS_PROTECTED", "{}", r.text);
        assert!(!repo.join(".git").join("zzz").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(windows)]
    #[test]
    fn edit_and_write_keep_alternate_data_streams() {
        let dir = tmp("ads");
        fs::write(dir.join("x.txt"), "main content\n").unwrap();
        // A neutral stream: security software watches writes to `Zone.Identifier`.
        fs::write(dir.join("x.txt:qx.note"), "kept\n").unwrap();
        assert!(!invoke(&dir, "read", json!({"path":"x.txt"})).is_error);
        let r = invoke(&dir, "edit", json!({"path":"x.txt","old_string":"main","new_string":"new"}));
        assert!(!r.is_error, "{}", r.text);
        let r = invoke(&dir, "write", json!({"path":"x.txt","content":"replaced\n"}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(fs::read_to_string(dir.join("x.txt")).unwrap(), "replaced\n");
        assert_eq!(fs::read_to_string(dir.join("x.txt:qx.note")).unwrap(), "kept\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_refuses_names_windows_cannot_handle() {
        let dir = tmp("win-names");
        for bad in ["nul", "log-12:30.txt", "notes.md.", "x.txt ", "sub/aux.rs", "COM1.log"] {
            let r = invoke(&dir, "write", json!({"path":bad,"content":"x"}));
            assert!(r.is_error, "{bad:?} was written");
            assert_eq!(r.structured["code"], "FS_BAD_ARG", "{bad:?}");
        }
        // `a:b/...` is drive-relative on Windows (drive A:): refused, whatever the code.
        assert!(invoke(&dir, "write", json!({"path":"a:b/c.txt","content":"x"})).is_error);
        assert!(!dir.join("log-12").exists() && !dir.join("sub").exists() && !dir.join("a").exists());
        // (`NUL.txt` is NUL too, per Windows naming rules, but `null` and `con-notes` are not.)
        for ok in ["con-notes.md", "console.log", "null/x.txt.bak"] {
            let r = invoke(&dir, "write", json!({"path":ok,"content":"x"}));
            assert!(!r.is_error, "{ok:?}: {}", r.text);
        }
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
    fn confinement_holds_against_dotdot_and_junctions() {
        let base = tmp("confine");
        let (work, outside) = (base.join("work"), base.join("outside"));
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "secret\n").unwrap();
        let r = invoke(&work, "write", json!({"path":"../escape.txt","content":"x"}));
        assert_eq!(r.structured["code"], "FS_OUTSIDE_WORKSPACE", "{}", r.text);
        assert!(!base.join("escape.txt").exists());
        let link = work.join("out");
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());
        if !made {
            println!("skip: cannot create a junction here");
            let _ = fs::remove_dir_all(&base);
            return;
        }
        let r = invoke(&work, "read", json!({"path":"out/secret.txt"}));
        assert!(!r.is_error, "{}", r.text); // reads follow the junction; walks and writes do not
        let r = invoke(&work, "write", json!({"path":"out/new.txt","content":"x"}));
        assert!(r.is_error && !outside.join("new.txt").exists(), "{}", r.text);
        let r = invoke(&work, "grep", json!({"pattern":"secret"}));
        assert!(!r.text.contains("secret.txt"), "{}", r.text);
        let r = invoke(&work, "glob", json!({"pattern":"**/*.txt"}));
        assert!(!r.text.contains("secret.txt"), "{}", r.text);
        let _ = fs::remove_dir(&link);
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
        assert!(!r.text.contains("node_modules/x.rs"));
        assert!(r.text.contains("not searched: node_modules"), "{}", r.text);
        // Naming the directory, or asking for all, looks inside it.
        let r = invoke(&dir, "glob", json!({"pattern":"node_modules/**/*.rs"}));
        assert!(r.text.contains("node_modules/x.rs"), "{}", r.text);
        let r = invoke(&dir, "glob", json!({"pattern":"**/*.rs", "all": true}));
        assert!(r.text.contains("node_modules/x.rs"), "{}", r.text);
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
    fn write_refuses_files_it_would_corrupt_and_keeps_bom_and_line_endings() {
        let dir = tmp("write-encoding");
        let latin1: &[u8] = b"caf\xe9\nx\n";
        let utf16: &[u8] = b"\xff\xfeh\0i\0\n\0x\0\n\0";
        // `read` refuses a NUL in the first 8 KiB only; past it, text made from what read showed
        // would drop the NUL.
        let nul = [b"x\n".repeat(5000), b"a\0b\n".to_vec()].concat();
        for (name, bytes) in [("l1.txt", latin1), ("u16.txt", utf16), ("zeros.txt", &nul[..])] {
            fs::write(dir.join(name), bytes).unwrap();
            assert!(!invoke(&dir, "read", json!({"path":name})).is_error);
            let r = invoke(&dir, "write", json!({"path":name,"content":"replacement\n"}));
            assert!(r.is_error, "{name}: {}", r.text);
            assert_eq!(r.structured["code"], "FS_ENCODING", "{name}: {}", r.text);
            assert_eq!(fs::read(dir.join(name)).unwrap(), bytes, "{name} was rewritten");
        }
        // The model never saw the CRs or the BOM; the file must not lose them.
        for (name, before, content, after) in [
            ("crlf.txt", &b"a\r\nb\r\n"[..], "a\nb\nc\n", &b"a\r\nb\r\nc\r\n"[..]),
            ("bom.txt", &b"\xef\xbb\xbfhello\n"[..], "hi\n", &b"\xef\xbb\xbfhi\n"[..]),
            ("bomkept.txt", &b"\xef\xbb\xbfhello\n"[..], "\u{feff}hi\n", &b"\xef\xbb\xbfhi\n"[..]),
            ("lf.txt", &b"a\nb\n"[..], "a\nc\n", &b"a\nc\n"[..]),
        ] {
            fs::write(dir.join(name), before).unwrap();
            assert!(!invoke(&dir, "read", json!({"path":name})).is_error);
            let r = invoke(&dir, "write", json!({"path":name,"content":content}));
            assert!(!r.is_error, "{name}: {}", r.text);
            assert_eq!(fs::read(dir.join(name)).unwrap(), after, "{name}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_keeps_crlf_for_multiline_new_text_and_matches_mixed_files() {
        let dir = tmp("edit-crlf");
        fs::write(dir.join("a.txt"), "a\r\nfoo();\r\nb\r\n").unwrap();
        assert!(!invoke(&dir, "read", json!({"path":"a.txt"})).is_error);
        let r = invoke(&dir, "edit", json!({"path":"a.txt","old_string":"foo();","new_string":"foo();\nbar();"}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"a\r\nfoo();\r\nbar();\r\nb\r\n");
        // a multi-line match inside the LF part of a file that also has CRLF
        fs::write(dir.join("m.txt"), "x\r\ny\nz\nw\n").unwrap();
        assert!(!invoke(&dir, "read", json!({"path":"m.txt"})).is_error);
        let r = invoke(&dir, "edit", json!({"path":"m.txt","old_string":"y\nz","new_string":"y\nq"}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(fs::read(dir.join("m.txt")).unwrap(), b"x\r\ny\nq\nw\n");
        // the old behaviour stays: an LF old_string finds CRLF text
        fs::write(dir.join("c.txt"), "p\r\nq\r\n").unwrap();
        assert!(!invoke(&dir, "read", json!({"path":"c.txt"})).is_error);
        let r = invoke(&dir, "edit", json!({"path":"c.txt","old_string":"p\nq","new_string":"p\nr"}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(fs::read(dir.join("c.txt")).unwrap(), b"p\r\nr\r\n");
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

    #[test]
    fn batch_ops_paths_and_edits_save_round_trips() {
        let dir = tmp("batch");
        fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
        fs::write(dir.join("b.txt"), "one\n").unwrap();
        let r = invoke(&dir, "read", json!({"paths":["a.txt","b.txt","missing.txt"]}));
        assert!(!r.is_error && r.text.contains("== 1/3 read a.txt") && r.text.contains("1: one") && r.text.contains("FS_NOT_FOUND"), "{}", r.text);
        assert_eq!(r.structured["failed"], 1);

        // Edits apply in order to the evolving text, and all-or-nothing.
        let r = invoke(&dir, "edit", json!({"path":"a.txt","edits":[
            {"old_string":"alpha","new_string":"A"}, {"old_string":"A\nbeta","new_string":"AB"}]}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "AB\n");
        let r = invoke(&dir, "edit", json!({"path":"a.txt","edits":[
            {"old_string":"AB","new_string":"x"}, {"old_string":"zzz","new_string":"y"}]}));
        assert!(r.is_error && r.text.contains("edits[1]"), "{}", r.text);
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "AB\n");

        // A failed write/edit stops the rest of a mixed batch; reads before it still answer.
        let r = call(&json!({"cwd": dir, "ops":[
            {"op":"read","path":"b.txt"}, {"op":"edit","path":"b.txt","old_string":"nope","new_string":"x"},
            {"op":"read","path":"b.txt"}]}) );
        assert!(!r.is_error && r.text.contains("skipped"), "{}", r.text);
        let r = call(&json!({"cwd": dir, "ops":[{"op":"read","ops":[]}]}));
        assert!(r.is_error, "{}", r.text);
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
        // A generated directory stays out, and the reply says so.
        assert!(glob("**/*.rs").contains("node_modules"), "{}", glob("**/*.rs"));
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
                let (parts, _) = walk_live(&canon, false, Vec::<String>::new, |v, p, _| {
                    v.push(p.strip_prefix(&canon).unwrap().to_string_lossy().replace('\\', "/"));
                });
                for p in parts {
                    want.extend(p);
                }
                want.sort();
                let (snap, _) = crate::index::snapshot(&canon).expect("index stays valid");
                let got: Vec<String> = snap.files.iter().map(|e| e.rel.clone()).collect();
                assert_eq!(got, want, "after step {step}");
            }
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
    }}
