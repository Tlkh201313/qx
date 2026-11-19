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

mod mutate;
mod path;
mod read;
mod search;
use mutate::*;
use path::*;
use read::*;
use search::*;
pub(crate) use search::{content_forget, drop_ignored_cache, git_ignores_dir, prime, skip_dir_name, vcs_dir_name, walk_every_file, walk_live, walk_lost};
#[cfg(test)]
use tests::{invoke, tmp};

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
const DEFAULT_READ_BYTES: usize = CLIENT_REPLY_BYTES - 1_000;
/// Claude Code refuses a tool reply of about 50k characters outright; no reply goes past this.
pub(crate) const CLIENT_REPLY_BYTES: usize = 49_000;
const DEFAULT_GLOB_RESULTS: usize = 100;
const MAX_GLOB_RESULTS: usize = 1_000;
const DEFAULT_GREP_MATCHES: usize = 250;
const MAX_GREP_MATCHES: usize = 1_000;
const DEFAULT_GREP_LINE_CHARS: usize = 2_000;
const MAX_LIST_ENTRIES: usize = 500;
const MAX_GREP_CONTEXT: usize = 5;

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

/// The `op` named, or the only one the arguments can mean. Models drop `op` from `{paths}`,
/// `{path, edits}` and the like; the call would otherwise fail and be sent again. `pattern` is
/// glob or grep, so it stays an error.
fn op_of(args: &Value) -> String {
    match args.get("op").and_then(Value::as_str).filter(|o| !o.is_empty()) {
        Some(o) => o.to_string(),
        None if args.get("edits").is_some() || args.get("old_string").is_some() => "edit".into(),
        None if args.get("content").is_some() => "write".into(),
        None if args.get("paths").is_some() || args.get("path").is_some() || args.get("file_path").is_some() => "read".into(),
        None => String::new(),
    }
}

pub fn call(args: &Value) -> Reply {
    let started = Instant::now();
    let defaulted = matches!(args.get("cwd"), None | Some(Value::Null));
    let op = op_of(args);
    let mut reply = match batch_items(args, &op) {
        Some(Ok(items)) => run_batch(&items),
        Some(Err(e)) => e.reply("batch"),
        None => match dispatch(&op, args) {
            Ok(r) => r,
            Err(e) => e.reply(&op),
        },
    };
    if reply.text.len() > CLIENT_REPLY_BYTES {
        let total = reply.text.len();
        let mut cut = CLIENT_REPLY_BYTES;
        while !reply.text.is_char_boundary(cut) {
            cut -= 1;
        }
        reply.text.truncate(cut);
        reply.text.push_str(&format!("\n[qx] reply cut at {cut} of {total} bytes; narrow the pattern or path, or ask for less per call"));
        if let Value::Object(map) = &mut reply.structured {
            map.insert("truncated".to_string(), json!(true));
        }
    }
    let unknown = unknown_keys(args);
    if !unknown.is_empty() {
        reply.text.push_str(&format!("\n[qx] ignored (not fs options): {}", unknown.join(", ")));
    }
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

/// Every key the `fs` tool reads (its input schema in `mcp.rs`). Anything else is a typo or a
/// guess, and a silently ignored `ignorecase` returns the wrong answer without a sign of it.
const KNOWN_KEYS: &[&str] = &[
    "op", "cwd", "path", "file_path", "content", "old_string", "new_string", "replace_all", "ops", "paths", "edits",
    "pattern", "include", "offset", "limit", "max_results", "max_matches", "max_line_chars", "ignore_case", "literal",
    "all", "context", "-C", "-A", "-B", "fresh", "n", "grep", "searches", "show",
];

/// The keys of the call (and of each `ops` item) that no operation reads, in first-seen order.
fn unknown_keys(args: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let items = args.get("ops").and_then(Value::as_array).into_iter().flatten();
    for obj in std::iter::once(args).chain(items).filter_map(Value::as_object) {
        for k in obj.keys() {
            if !KNOWN_KEYS.contains(&k.as_str()) && !out.contains(k) {
                out.push(k.clone());
            }
        }
    }
    out
}

const MAX_BATCH: usize = 32;
/// Past this much combined text a batch stops running further operations: the model asks again for the rest.
const MAX_BATCH_BYTES: usize = CLIENT_REPLY_BYTES - 9_000;

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
    let mut capped: Vec<usize> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let op = op_of(item);
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
            capped.push(i + 1);
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
    // The reply may be cut at the client cap, taking the per-op skip lines with it: say it first.
    if let (Some(a), Some(b)) = (capped.first(), capped.last()) {
        let ops = if a == b { format!("op {a}") } else { format!("ops {a}-{b}") };
        text.insert_str(0, &format!("[qx] {ops} of {n} not run (batch output cap); ask for them in another call\n\n"));
    }
    Reply {
        text: text.trim_end().to_string(),
        structured: json!({ "op": "batch", "count": n, "failed": failed, "results": results }),
        is_error: failed == n,
    }
}

fn dispatch(op: &str, args: &Value) -> Result<Reply, FsError> {
    // The last cwd of "some" session is a fine place to read from, never to create or change files in.
    if matches!(op, "write" | "edit")
        && matches!(args.get("cwd"), None | Some(Value::Null))
        && path_arg(args).is_some_and(|p| !Path::new(p.trim()).is_absolute() && !p.trim().starts_with('~'))
    {
        return Err(FsError::arg("write and edit with a relative path need `cwd` (or an absolute path)"));
    }
    let root = Root::new(args)?;
    match op {
        "read" => read(&root, args),
        "write" => write(&root, args),
        "edit" => edit(&root, args),
        "glob" => searched(&root, args, "glob", glob),
        "grep" => searched(&root, args, "grep", grep),
        "list" => list(&root, args),
        "stat" => stat(&root, args),
        "history" => history_op(&root, args),
        _ => Err(FsError::arg(
            "op must be one of read, write, edit, glob, grep, list, stat, history",
        )),
    }
}

/// Runs a `glob`/`grep` and notes it in the repo history (never replayed: the tree may differ next time).
fn searched(root: &Root, args: &Value, op: &str, run: fn(&Root, &Value) -> Result<Reply, FsError>) -> Result<Reply, FsError> {
    let started = Instant::now();
    let reply = run(root, args)?;
    if !reply.is_error {
        let pattern = opt_str(args, "pattern").unwrap_or("");
        let cmd = match path_arg(args).filter(|_| op == "grep") {
            Some(p) => format!("grep {pattern} {p}"),
            None => format!("{op} {pattern}"),
        };
        let hits = reply.structured["count"].as_u64();
        crate::history::record(&root.cwd, &cmd, crate::history::Kind::Search, 0, started.elapsed().as_millis() as u64, "", hits, None);
    }
    Ok(reply)
}

/// `op=history`: what ran in this repo (`n`, `grep`, `searches`) or one entry in full (`show`).
fn history_op(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let text = match opt_str(args, "show").filter(|k| !k.trim().is_empty()) {
        Some(key) => crate::history::show(&root.cwd, key.trim())
            .ok_or_else(|| FsError::arg(format!("no history entry {}", key.trim())))?,
        None => {
            let n = usize_arg(args, "n", 20, 1, 500)?;
            let grep = opt_str(args, "grep").filter(|g| !g.is_empty());
            crate::history::list(&root.cwd, n, grep, bool_arg(args, "searches", false), false)
        }
    };
    Ok(Reply { text, structured: json!({ "op": "history" }), is_error: false })
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(super) fn tmp(name: &str) -> PathBuf {
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

    pub(super) fn invoke(dir: &Path, op: &str, extra: Value) -> Reply {
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
    fn a_batch_cut_at_the_output_cap_names_the_ops_it_did_not_run_first() {
        let dir = tmp("batch-cap");
        fs::write(dir.join("big.txt"), ("x".repeat(60) + "\n").repeat(800)).unwrap();
        let mut ops = vec![json!({"op":"read","path":"big.txt"})];
        for i in 0..12 {
            fs::write(dir.join(format!("e{i}.txt")), "a\n").unwrap();
            ops.push(json!({"op":"edit","path":format!("e{i}.txt"),"old_string":"a","new_string":"b"}));
        }
        let r = call(&json!({"cwd": dir.to_string_lossy(), "ops": ops}));
        assert!(r.text.starts_with("[qx] ops 2-13 of 13 not run (batch output cap)"), "{}", &r.text[..200]);
        assert_eq!(fs::read_to_string(dir.join("e0.txt")).unwrap(), "a\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_reply_goes_past_the_cap_claude_code_rejects() {
        let dir = tmp("reply-cap");
        let line = format!("needle {}\n", "x".repeat(1_500));
        fs::write(dir.join("a.txt"), line.repeat(200)).unwrap();
        let r = invoke(&dir, "grep", json!({"pattern": "needle"}));
        assert!(r.text.len() <= CLIENT_REPLY_BYTES + 200, "{} bytes", r.text.len());
        assert!(r.text.contains("[qx] reply cut at"), "{}", &r.text[r.text.len().saturating_sub(200)..]);
        assert_eq!(r.structured["truncated"], true);
        // A read stops at its own window, under the cap, with the way to go on.
        fs::write(dir.join("b.txt"), line.repeat(200)).unwrap();
        let r = scoped_lean(|| invoke(&dir, "read", json!({"path": "b.txt"})));
        assert!(r.text.len() <= CLIENT_REPLY_BYTES && r.text.contains("offset="), "{} bytes", r.text.len());
        let _ = fs::remove_dir_all(&dir);
    }

    fn scoped_lean<T>(f: impl FnOnce() -> T) -> T {
        crate::client::scoped(crate::client::Client::ClaudeCode, f)
    }

    #[test]
    fn a_missing_op_is_inferred_only_when_the_arguments_mean_one_thing() {
        let dir = tmp("op-infer");
        fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        let cwd = dir.to_string_lossy();
        let r = call(&json!({"cwd": cwd, "paths": ["a.txt"]}));
        assert!(!r.is_error && r.text.contains("1: one"), "{}", r.text);
        let r = call(&json!({"cwd": cwd, "path": "a.txt", "edits": [{"old_string": "two", "new_string": "2"}]}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n2\n");
        let r = call(&json!({"cwd": cwd, "ops": [{"path": "a.txt"}, {"path": "b.txt", "content": "x"}]}));
        assert!(r.text.contains("1: one") && fs::read_to_string(dir.join("b.txt")).unwrap() == "x", "{}", r.text);
        // A pattern is glob or grep: still an error.
        let r = call(&json!({"cwd": cwd, "pattern": "*.txt"}));
        assert!(r.is_error && r.text.contains("op must be one of"), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_option_no_operation_reads_is_named_not_silently_dropped() {
        let dir = tmp("unknown-keys");
        fs::write(dir.join("a.txt"), "Needle\n").unwrap();
        let r = invoke(&dir, "grep", json!({"pattern":"needle","ignorecase":true,"-A":1}));
        assert!(r.text.contains("[qx] ignored (not fs options): ignorecase"), "{}", r.text);
        assert!(!r.text.contains("-A"), "a real option was reported: {}", r.text);
        let r = invoke(&dir, "grep", json!({"pattern":"needle","ignore_case":true}));
        assert!(!r.text.contains("ignored (not fs options)"), "{}", r.text);
        let r = invoke(&dir, "stat", json!({"ops":[{"op":"stat","path":"a.txt","fast":1}]}));
        assert!(r.text.contains("ignored (not fs options): fast"), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
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
}
