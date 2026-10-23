//! MCP (stdio) server exposing `run` (commands) and `fs` (filesystem).
//!
//! Tool names, fallback pointers and output shape come from `client`, so the same
//! server speaks correctly to dsh, Claude Code, Codex, OpenCode, Hermes and pi.

use crate::client::{self, Client, Native, Structured};
use crate::{builtin, compact, digest, exec, guard, npm, redact};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Bounds the server: concurrent commands, how many more may queue, and how long
/// shutdown waits for them.
struct Limits {
    workers: usize,
    queue: usize,
    drain: Duration,
}

impl Limits {
    fn from_env() -> Self {
        let workers = std::env::var("QX_MCP_WORKERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(4)
            .clamp(1, 16);
        let queue = std::env::var("QX_MCP_QUEUE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(64)
            .min(1024);
        Limits { workers, queue, drain: Duration::from_secs(3) }
    }
}

pub fn serve() -> i32 {
    // The server outlives its calls, so a parked PowerShell is ready for the next one.
    #[cfg(windows)]
    exec::keep_powershell_warm();
    let stdin = std::io::stdin();
    serve_io(stdin.lock(), std::io::stdout(), Limits::from_env())
}

/// Reads requests from `reader`, answers on `out`. Separated from `serve` so a test can drive it.
fn serve_io<R: BufRead, W: Write + Send + 'static>(mut reader: R, out: W, limits: Limits) -> i32 {
    // One process serves one session, so this starts fresh: a shutdown flag from a previous
    // session must never refuse the first call of this one.
    SHUTDOWN.store(false, Ordering::SeqCst);
    // `QX_CLIENT` (or generic) until `initialize` names the client.
    client::set_current(Client::detect(None, |k| std::env::var(k).ok()));
    let out = Arc::new(Mutex::new(out));
    let gate = Gate::new(limits.workers);
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let limit = limits.workers + limits.queue;

    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Bytes, not `lines()`: one invalid UTF-8 byte must not end the session.
        if !matches!(reader.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
            break;
        }
        let line = String::from_utf8_lossy(&buf).into_owned();
        if line.trim().is_empty() {
            continue;
        }
        // Parse once here: routing on the decoded method cannot be fooled by text that merely
        // mentions `tools/call` in its params.
        let req = match serde_json::from_str::<Value>(line.trim()) {
            Ok(req) => req,
            Err(_) => {
                send(&out, &rpc_error(Value::Null, -32700, "parse error"));
                continue;
            }
        };
        // Only a call runs long. ping, cancel and listing answer at once, even when every worker is busy.
        if route(&req) == Route::Inline {
            if let Some(resp) = respond_value(&req) {
                send(&out, &resp);
            }
            continue;
        }
        // Admission: a call past `workers + queue` is refused at once, so a runaway client cannot
        // queue unbounded work. The wait for a slot happens on the worker thread, so this loop keeps
        // reading (cancel, ping) while every slot is busy.
        active.fetch_add(1, Ordering::SeqCst);
        let n = active.load(Ordering::SeqCst);
        if n > limit {
            active.fetch_sub(1, Ordering::SeqCst);
            let text = format!(
                "qx busy: {} calls already running or queued (limit {}+{}); retry shortly",
                n - 1,
                limits.workers,
                limits.queue
            );
            // Every request that carries an id gets an answer, and a batch stays a batch.
            send_all(&out, &req, |id| {
                tool_result(Some(id), &text, true, json!({ "source": "busy" }))
            });
            continue;
        }
        let out = out.clone();
        let active = active.clone();
        let gate = gate.clone();
        std::thread::spawn(move || {
            let _active = Active(active);
            let _slot = gate.acquire();
            if SHUTDOWN.load(Ordering::SeqCst) {
                // The client is gone: never start work nobody will read, but still answer
                // every id that was waiting.
                send_all(&out, &req, |id| {
                    tool_text(Some(id), "qx is shutting down; the call was not run", true)
                });
                return;
            }
            if let Some(resp) = respond_value(&req) {
                send(&out, &resp);
            }
        });
    }

    // The client closed stdin. Stop the work rather than let it run to its own timeout: children
    // die with qx's job object, and a call still waiting for a slot never starts.
    cancel_all();
    let deadline = std::time::Instant::now() + limits.drain;
    while active.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    0
}

/// Inline answers and worker answers differ only in where they run, never in content: this is the
/// routing rule, so a `tools/call` notification is never handed to a worker.
#[derive(Debug, PartialEq)]
enum Route {
    Inline,
    Worker,
}

fn is_worker(req: &Value) -> bool {
    req.get("method").and_then(Value::as_str) == Some("tools/call") && req.get("id").is_some()
}

fn route(req: &Value) -> Route {
    let worker = match req {
        Value::Array(batch) => batch.iter().any(is_worker),
        one => is_worker(one),
    };
    if worker {
        Route::Worker
    } else {
        Route::Inline
    }
}

/// Request ids a refused reply must cover: every request that carries one, so a batch
/// stays a batch and no id is left waiting.
fn send_all<W: Write>(out: &Mutex<W>, req: &Value, reply: impl Fn(Value) -> Value) {
    match req {
        Value::Array(batch) => {
            let replies: Vec<Value> = batch
                .iter()
                .filter_map(|item| item.get("id").cloned())
                .map(&reply)
                .collect();
            if !replies.is_empty() {
                send(out, &Value::Array(replies));
            }
        }
        one => {
            if let Some(id) = one.get("id").cloned() {
                send(out, &reply(id));
            }
        }
    }
}

fn send<W: Write>(out: &Mutex<W>, resp: &Value) {
    if let Ok(s) = serde_json::to_string(resp) {
        // A poisoned lock still holds a usable writer.
        let mut o = out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(o, "{s}");
        let _ = o.flush();
    }
}

/// Set once the client has closed its end: no command may start after it.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Running `tools/call`s by request id, so `notifications/cancelled` can stop the right one.
static INFLIGHT: Mutex<Vec<(String, Arc<AtomicBool>)>> = Mutex::new(Vec::new());

/// Cancels that named no running call: a call still waiting for a worker slot (or one that has
/// already finished). `Inflight::begin` honours the first kind; the list is capped so the second cannot grow it.
static EARLY_CANCELS: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Inflight(Option<String>, Option<Arc<AtomicBool>>);

impl Inflight {
    /// Whether the client cancelled this call (or went away) before it started: it must not run at all.
    fn cancelled(&self) -> bool {
        self.1.as_ref().is_some_and(|f| f.load(Ordering::Relaxed))
    }

    /// Registers the calling worker thread's command for cancellation until dropped.
    fn begin(id: &Option<Value>) -> Self {
        let Some(key) = id.as_ref().map(Value::to_string) else {
            return Inflight(None, None);
        };
        // Lock order is always INFLIGHT then EARLY_CANCELS, held across check-and-insert, so a cancel
        // lands either in the flag or in the early list this reads, never between the two.
        let flag = {
            let mut running = INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
            let mut early = EARLY_CANCELS.lock().unwrap_or_else(|e| e.into_inner());
            let before = early.len();
            early.retain(|k| *k != key);
            let flag = Arc::new(AtomicBool::new(
                SHUTDOWN.load(Ordering::SeqCst) || early.len() != before,
            ));
            running.push((key.clone(), flag.clone()));
            flag
        };
        exec::set_cancel(Some(flag.clone()));
        Inflight(Some(key), Some(flag))
    }
}

impl Drop for Inflight {
    fn drop(&mut self) {
        if let Some(key) = &self.0 {
            INFLIGHT.lock().unwrap_or_else(|e| e.into_inner()).retain(|(k, _)| k != key);
            exec::set_cancel(None);
        }
    }
}

fn cancel(id: &Value) {
    let key = id.to_string();
    let mut found = false;
    let running = INFLIGHT.lock().unwrap_or_else(|e| e.into_inner()); // held to the end: see `Inflight::begin`
    for (k, flag) in running.iter() {
        if *k == key {
            flag.store(true, Ordering::Relaxed);
            found = true;
        }
    }
    if !found {
        let mut early = EARLY_CANCELS.lock().unwrap_or_else(|e| e.into_inner());
        if early.len() >= 64 {
            early.remove(0);
        }
        early.push(key);
    }
}

/// The client is gone: refuse new work and stop what runs. Children are killed by qx's job object.
fn cancel_all() {
    SHUTDOWN.store(true, Ordering::SeqCst);
    let running = INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    for (_, flag) in running.iter() {
        flag.store(true, Ordering::Relaxed);
    }
}

/// Counting gate: at most N commands run at once; the rest wait on their own worker thread.
struct Gate {
    slots: std::sync::Mutex<usize>,
    cv: std::sync::Condvar,
}

impl Gate {
    fn new(n: usize) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            slots: std::sync::Mutex::new(n),
            cv: std::sync::Condvar::new(),
        })
    }

    fn acquire(self: &std::sync::Arc<Self>) -> Slot {
        let mut n = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        while *n == 0 {
            n = self.cv.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n -= 1;
        Slot(self.clone())
    }
}

struct Slot(std::sync::Arc<Gate>);

impl Drop for Slot {
    fn drop(&mut self) {
        let mut n = self.0.slots.lock().unwrap_or_else(|e| e.into_inner());
        *n += 1;
        self.0.cv.notify_one();
    }
}

/// Decrements the in-flight counter even if a worker panics, so shutdown
/// cannot hang waiting for a thread that never finished.
struct Active(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// One input line to its reply. Malformed input and handler panics still get an
/// answer: a client waiting on an id would otherwise hang forever.
#[cfg(test)]
fn respond(line: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(line) {
        Ok(req) => respond_value(&req),
        Err(_) => Some(rpc_error(Value::Null, -32700, "parse error")),
    }
}

/// The same answer for an already-parsed request, so the server parses each line once.
fn respond_value(req: &Value) -> Option<Value> {
    if let Value::Array(batch) = req {
        if batch.is_empty() {
            return Some(rpc_error(Value::Null, -32600, "invalid request: empty batch"));
        }
        let replies: Vec<Value> = batch.iter().filter_map(guarded).collect();
        return (!replies.is_empty()).then_some(Value::Array(replies));
    }
    guarded(req)
}

fn guarded(req: &Value) -> Option<Value> {
    if !req.is_object() {
        return Some(rpc_error(Value::Null, -32600, "invalid request"));
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(req))).unwrap_or_else(|_| {
        let id = req.get("id").cloned()?;
        if req.get("method").and_then(Value::as_str) == Some("tools/call") {
            return Some(tool_text(Some(id), "qx internal error: the command handler panicked", true));
        }
        Some(rpc_error(id, -32603, "internal error"))
    })
}

fn handle(req: &Value) -> Option<Value> {
    let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
    if method == "notifications/cancelled" {
        if let Some(rid) = req.pointer("/params/requestId") {
            cancel(rid);
        }
        return None;
    }
    // No `id` means a notification: it is never answered, and never run.
    let id = req.get("id").cloned()?;
    match method {
        "initialize" => {
            let ver = req
                .pointer("/params/protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05");
            // The client names itself here; from now on every message is written for it.
            client::set_current(Client::detect(
                req.pointer("/params/clientInfo/name").and_then(Value::as_str),
                |k| std::env::var(k).ok(),
            ));
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": ver,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "qx", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": client::current().instructions()
                }
            }))
        }
        "ping" => Some(ok(Some(id), json!({}))),
        "tools/list" => Some(ok(
            Some(id),
            json!({ "tools": [run_schema(), fs_schema()] }),
        )),
        "tools/call" => Some(call_tool(req, Some(id))),
        _ => Some(rpc_error(id, -32601, "method not found")),
    }
}

fn ok(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Codex and Claude Code never show the model `structuredContent`, so advertising an
/// `outputSchema` for them would promise data that never arrives.
fn strip_output_schema(schema: &mut Value, c: Client) {
    if c.structured() == Structured::Off {
        if let Value::Object(m) = schema {
            m.remove("outputSchema");
        }
    }
}

fn run_schema() -> Value {
    let c = client::current();
    let mut schema = json!({
        "name": "run",
        "description": format!(
            "Run a local command fast. Prefer `program` + `args` (direct exec, no shell); a PowerShell cmdlet (`Get-ChildItem`) or a `.ps1` path also works as `program`. A single `command` runs without a shell; otherwise `shell` decides: auto = PowerShell for cmdlet/variable syntax, Git Bash when a pipeline runs a POSIX utility (ls, cat, head, grep, wc…), else cmd. Children never prompt: stdin is empty, PowerShell runs -NonInteractive, git gets GIT_EDITOR=true and GIT_TERMINAL_PROMPT=0. In-process built-ins: pwd, echo, ls, cat, head, tail, wc, grep, mkdir, rm, cp, mv, touch, which, basename, dirname, realpath, whoami, hostname, seq, sleep. Output is compacted, preserves errors from the middle, and redacts secrets. On failure, fall back to the native {} for sandbox escalation, background jobs, or PTY work.",
            c.native(Native::Shell)
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "program": { "type": "string", "description": "Executable name or path, e.g. git, cargo, python, node." },
                "args": { "type": "array", "items": { "type": "string" }, "description": "Arguments for program." },
                "command": { "type": "string", "description": "Full shell command line for pipes/redirects/chaining." },
                "shell": { "type": "string", "enum": ["auto", "cmd", "bash", "powershell"], "default": "auto", "description": "auto picks PowerShell for cmdlet/variable syntax, Git Bash when a pipeline runs a POSIX utility, else cmd. PowerShell runs -NoProfile -NonInteractive." },
                "cwd": { "type": "string", "description": "Working directory." },
                "timeout_ms": { "type": "integer", "default": 120000, "minimum": 1000, "description": "Kill the tree after this long (clamped to 1000..3600000); the reply carries exit_code 124 and timed_out true." },
                "mode": { "type": "string", "enum": ["compact", "full"], "default": "compact" }
            }
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "blocked": { "type": "boolean" },
                "source": { "type": "string" },
                "exit_code": { "type": "integer" },
                "duration_ms": { "type": "integer" },
                "timed_out": { "type": "boolean" },
                "truncated": { "type": "boolean" },
                "redactions": { "type": "integer" },
                "profile": { "type": "string" },
                "omitted_lines": { "type": "integer" },
                "stdout": { "type": "string" },
                "stderr": { "type": "string" },
                "guard_warning": { "type": ["string", "null"] },
                "error": { "type": "string" }
            }
        }
    });
    strip_output_schema(&mut schema, c);
    schema
}

fn fs_schema() -> Value {
    let c = client::current();
    let mut schema = json!({
        "name": "fs",
        "description": format!(
            "Fast in-process filesystem operations for the workspace. op=read: line-numbered UTF-8 read (path, offset?, limit?). op=write: create or replace (path, content); an existing file needs a prior qx read. op=edit: literal replace (path, old_string, new_string, replace_all?); needs a prior qx read. op=glob: path discovery (pattern; path?). op=grep: literal content search (pattern; path?, include?, ignore_case?); one output line is path:line: text. op=list/stat. Pass cwd to pin the root; it defaults to where the server started. Paths outside cwd are refused. qx skips generated dirs (node_modules, target, dist, build, ...) during recursive walks for speed; pass one as `path` to search it directly, or use the native {glob}/{grep} when the whole ignored tree matters. qx grep is literal, not regex. On any error use the native {read}/{write}/{edit}/{glob}/{grep} tool. Native tools remain authoritative for sandbox escalation, images, and regex. qx mutations are atomic and read-before-write guarded, but do not replace the client's sandbox policy.",
            glob = c.native(Native::Glob),
            grep = c.native(Native::Grep),
            read = c.native(Native::Read),
            write = c.native(Native::Write),
            edit = c.native(Native::Edit),
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "op": { "type": "string", "enum": ["read", "write", "edit", "glob", "grep", "list", "stat"] },
                "cwd": { "type": "string", "description": "Session working directory; the root every path is confined to. Defaults to where the server was started." },
                "path": { "type": "string", "description": "File or directory, absolute or relative to cwd. Defaults to '.' for glob, grep, and list. Alias: file_path." },
                "file_path": { "type": "string", "description": format!("Alias for path, matching the native {}/{}/{} tools.", c.native(Native::Read), c.native(Native::Write), c.native(Native::Edit)) },
                "content": { "type": "string", "description": "write: complete new file content." },
                "old_string": { "type": "string", "description": "edit: literal text to replace." },
                "new_string": { "type": "string", "description": "edit: replacement text." },
                "replace_all": { "type": "boolean", "default": false },
                "pattern": { "type": "string", "description": "glob: path glob (supports *, **, ?, [class], {a,b}). grep: literal text." },
                "include": { "type": "string", "description": "grep: one glob limiting which files are searched." },
                "offset": { "type": "integer", "default": 1, "description": "read: 1-based first line." },
                "limit": { "type": "integer", "default": 2000, "description": "read: maximum lines (cap 2000)." },
                "max_results": { "type": "integer", "default": 100, "description": "glob: maximum paths (cap 1000)." },
                "max_matches": { "type": "integer", "default": 250, "description": "grep: maximum matches (cap 1000)." },
                "max_line_chars": { "type": "integer", "default": 2000, "description": "read/grep: clip each line (cap 8000)." },
                "ignore_case": { "type": "boolean", "default": false, "description": "ASCII case-insensitive glob/grep." }
            },
            "required": ["op"]
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "op": { "type": "string" },
                "path": { "type": "string" },
                "file_path": { "type": "string" },
                "code": { "type": "string" },
                "count": { "type": "integer" },
                "shown": { "type": "integer" },
                "files": { "type": "integer" },
                "lines": { "type": "integer" },
                "total_lines": { "type": "integer" },
                "end_line": { "type": "integer" },
                "truncated": { "type": "boolean" },
                "scan_capped": { "type": "boolean" },
                "skipped_binary": { "type": "integer" },
                "skipped_large": { "type": "integer" },
                "created": { "type": "boolean" },
                "replacements": { "type": "integer" },
                "bytes": { "type": "integer" },
                "duration_ms": { "type": "integer" },
                "fallback": { "type": "string" }
            }
        }
    });
    strip_output_schema(&mut schema, c);
    schema
}

/// The command line the digest classifies. Plain words stop after four args (long paths and
/// test names would only confuse the classifier), but every `-flag` counts: `-- --nocapture`
/// and `-s` ask for the output itself.
pub(crate) fn run_label(program: Option<&str>, args: &[String], cmdline: Option<&str>) -> String {
    let Some(p) = program else {
        return cmdline.unwrap_or("").to_string();
    };
    let mut l = p.to_string();
    for (i, a) in args.iter().enumerate() {
        if !(a.starts_with('-') || (i < 4 && l.len() <= 80)) {
            continue;
        }
        if l.len() >= 400 {
            break;
        }
        l.push(' ');
        l.push_str(a);
    }
    l
}

fn call_tool(req: &Value, id: Option<Value>) -> Value {
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    if name == "fs" {
        let reply = crate::fsops::call(&args);
        return tool_result(id, &reply.text, reply.is_error, reply.structured);
    }
    if name != "run" {
        return tool_text(id, "unknown tool (expected `run` or `fs`)", true);
    }

    // A field of the wrong type is an error: ignoring it would run the command somewhere else
    // (`cwd`), for another time (`timeout_ms`), or as a different command (`program`).
    let text_arg = |key: &str| -> Result<Option<&str>, String> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.as_str())),
            Some(other) => Err(format!("`{key}` must be a string, got {other}")),
        }
    };
    let (program, cmdline, cwd_text) = match (text_arg("program"), text_arg("command"), text_arg("cwd")) {
        (Ok(p), Ok(c), Ok(d)) => (p, c, d),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return tool_text(id, &e, true),
    };
    // An empty `program` or `command` is an absent one.
    let program = program.filter(|p| !p.trim().is_empty());
    let cmdline = cmdline.filter(|c| !c.trim().is_empty());
    if program.is_some() && cmdline.is_some() {
        return tool_text(id, "give `program` + `args`, or `command`, not both", true);
    }
    let shell = args.get("shell").and_then(|v| v.as_str()).unwrap_or("auto");

    // A dropped non-string argument would silently run a different command.
    let list: Vec<String> = match args.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => {
            let mut list = Vec::with_capacity(a.len());
            for (i, x) in a.iter().enumerate() {
                match x.as_str() {
                    Some(s) => list.push(s.to_string()),
                    None => return tool_text(id, &format!("args[{i}] must be a string, got {x}"), true),
                }
            }
            list
        }
        Some(_) => return tool_text(id, "`args` must be an array of strings", true),
    };
    if program.is_none() && !list.is_empty() {
        // `{"command":"git stash","args":["pop"]}` would otherwise run `git stash`.
        return tool_text(id, "`args` go with `program`; with `command`, put the arguments in the command line", true);
    }
    // Absolute from here on: the guard, the built-ins and the child all mean the same directory.
    let cwd = cwd_text.map(|t| std::path::absolute(t).unwrap_or_else(|_| t.into()));
    // A bad cwd must fail loudly: built-ins would otherwise run against the wrong place.
    if let Some(d) = cwd.as_ref().filter(|d| !d.is_dir()) {
        return tool_text(id, &format!("cwd is not a directory: {}", d.display()), true);
    }
    let timeout_ms: u64 = match args.get("timeout_ms") {
        None | Some(Value::Null) => 120_000,
        Some(v) => {
            // Whole-number floats (`600000.0`) are what JavaScript and Python callers produce.
            let whole = v.as_u64().or_else(|| v.as_f64().filter(|f| *f >= 0.0 && f.fract() == 0.0 && *f < 1e15).map(|f| f as u64));
            match whole {
                Some(n) => n,
                None => {
                    return tool_text(id, &format!("`timeout_ms` must be a whole number of milliseconds, got {v}"), true)
                }
            }
        }
    }
    .clamp(1_000, 3_600_000);
    let full = args.get("mode").and_then(|v| v.as_str()) == Some("full");
    let label = run_label(program, &list, cmdline);

    let guard_cmd = match program {
        Some(p) => std::iter::once(p).chain(list.iter().map(String::as_str)).collect::<Vec<_>>().join(" "),
        None => cmdline.unwrap_or("").to_string(),
    };
    let guard_cmd = guard_cmd.as_str();
    let inflight = Inflight::begin(&id);
    // Cancelled while it waited for a worker slot, or the client already left: run nothing.
    // Built-ins would otherwise finish their file changes and a program would be spawned just to be killed.
    if inflight.cancelled() {
        return tool_result(
            id,
            "exit_code: 124\ntimed_out: true\n(cancelled before it started; not run)\n",
            true,
            json!({"timed_out": true, "exit_code": 124, "source": "cancelled"}),
        );
    }
    let decision = if let Some(p) = program {
        guard::check(p, &list, cwd.as_deref())
    } else if let Some(c) = cmdline {
        guard::check_shell(c, cwd.as_deref())
    } else {
        guard::Decision::allow()
    };
    if !decision.allowed() {
        guard::record("mcp", guard_cmd, &decision);
        return tool_result(
            id,
            &format!(
                "[qx guard] BLOCKED: {}\nThe command was not executed. Change the command, or ask the user to set QX_GUARD=off.",
                decision.summary()
            ),
            true,
            json!({
                "blocked": true,
                "source": "guard",
                "exit_code": 126,
                "guard_warning": decision.summary()
            }),
        );
    }
    let guard_warning = if decision.level == guard::Level::Warn {
        Some(decision.summary())
    } else {
        None
    };
    guard::record("mcp", guard_cmd, &decision);

    let timeout = Duration::from_millis(timeout_ms);
    // mode "full" promises the untouched text: built-ins must not sample their own output either.
    let full_env: Vec<(String, String)> = if full { vec![("QX_LS_FULL".into(), "1".into())] } else { Vec::new() };
    let (outcome, source) = if let Some(p) = program {
        match run_direct(p, &list, &full_env, cwd.as_deref(), timeout, "program") {
            Ok(x) => x,
            Err(e) => return tool_text(id, &e, true),
        }
    } else if let Some(c) = cmdline {
        // The guard above judged the whole line; env prefixes only change what runs.
        let (mut envs, c) = exec::peel_env(c);
        envs.extend(full_env);
        let direct = exec::split_simple(c)
            .and_then(|(prog, sargs)| run_direct(&prog, &sargs, &envs, cwd.as_deref(), timeout, "direct").ok());
        match direct {
            Some(x) => x,
            None => match exec::run_shell(c, shell, &envs, &cwd, Some(timeout)) {
                Ok(o) => (o, "shell"),
                Err(e) => return tool_text(id, &e, true),
            },
        }
    } else {
        return tool_text(id, "provide `program` + `args`, or `command`", true);
    };

    let strip = |s: String| if s.contains('\x1b') { compact::strip_ansi(&s) } else { s };
    let mut so = strip(outcome.stdout);
    let mut se = strip(outcome.stderr);
    // Redact first, like the CLI: compaction must not cut a secret block in half.
    let mut redactions = 0usize;
    if std::env::var("QX_REDACT").ok().as_deref() != Some("off") {
        let (a, n) = redact::redact(&so);
        so = a;
        redactions += n;
        let (b, n) = redact::redact(&se);
        se = b;
        redactions += n;
    }
    let mut truncated = outcome.cut;
    let mut profile = "";
    let mut omitted = 0usize;
    // Content a digest dropped, as opposed to noise (see `Digest::lossy`): worth a spool file.
    let mut lossy = false;
    // The text before compaction, kept only so a cut can be spooled to disk.
    let mut original = (String::new(), String::new());
    if !full {
        if let Some(d) = digest::digest(&label, &so, &se, outcome.code) {
            lossy = d.lossy();
            original = (std::mem::replace(&mut so, d.stdout), std::mem::replace(&mut se, d.stderr));
            profile = d.profile;
            omitted = d.omitted;
            truncated |= d.clipped;
            if matches!(profile, "build" | "test" | "traceback") {
                compact::relative_to_cwd(&mut so, &mut se, cwd.as_deref());
            }
        } else {
            let (a, cut) = compact::compact(&so, 200, 80, 60);
            let (b, cut_err) = compact::compact(&se, 120, 50, 40);
            original = (std::mem::replace(&mut so, a), std::mem::replace(&mut se, b));
            truncated |= cut | cut_err;
        }
    }
    // A cut or a digest that dropped content must never lose facts for good: the agent can grep the full text.
    // `mode:"full"` keeps no original, so there is nothing to spool.
    let spool = if !full && (truncated || lossy) { compact::spool(&original.0, &original.1) } else { None };

    let mut text = String::new();
    if let Some(w) = &guard_warning {
        text.push_str(&format!("[qx guard] warning: {w}\n"));
    }
    // Success is the common case and is already in structuredContent: spend tokens only on failure.
    if outcome.code != 0 {
        text.push_str(&format!("exit_code: {}\n", outcome.code));
    }
    if outcome.timed_out {
        text.push_str("timed_out: true\n");
    }
    if !so.is_empty() {
        text.push_str("\n[stdout]\n");
        text.push_str(&so);
        if !so.ends_with('\n') {
            text.push('\n');
        }
    }
    if !se.is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&se);
        if !se.ends_with('\n') {
            text.push('\n');
        }
    }
    if so.is_empty() && se.is_empty() {
        text.push_str("\n(no output)\n");
    }
    if let Some(p) = &spool {
        text.push_str(&format!("\n[qx] full output: {}\n", p.display()));
    }
    if redactions > 0 {
        text.push_str(&format!("\n[qx] redacted {redactions} secret(s)\n"));
    }

    let mut meta = json!({
        "source": source,
        "exit_code": outcome.code,
        "duration_ms": outcome.elapsed.as_millis() as u64,
        "stdout": so,
    });
    // A key left out means its default (false, 0, "", null): constants cost tokens on every call.
    for (key, value, present) in [
        ("timed_out", json!(true), outcome.timed_out),
        ("truncated", json!(true), truncated),
        ("full_output", json!(spool.as_ref().map(|p| p.display().to_string())), spool.is_some()),
        ("redactions", json!(redactions), redactions > 0),
        ("profile", json!(profile), !profile.is_empty()),
        ("omitted_lines", json!(omitted), omitted > 0),
        ("stderr", json!(se), !se.is_empty()),
        ("guard_warning", json!(guard_warning), guard_warning.is_some()),
    ] {
        if present {
            meta[key] = value;
        }
    }
    tool_result(id, &text, outcome.code != 0, meta)
}

/// Run without a shell: the npm fast path, a built-in, then the real program.
/// `plain` names the source when the real program ran. Err is the message for the agent.
fn run_direct(
    program: &str,
    args: &[String],
    envs: &[(String, String)],
    cwd: Option<&std::path::Path>,
    timeout: Duration,
    plain: &'static str,
) -> Result<(exec::Outcome, &'static str), String> {
    let started = |e| format!("failed to start: {e}");
    if !exec::sets_npm_config(envs) {
        if let Some(mut cmds) = npm::direct(program, args, cwd) {
            for c in &mut cmds {
                exec::apply_envs(c, envs);
            }
            return exec::run_chain(cmds, Some(timeout)).map(|o| (o, "direct")).map_err(started);
        }
    }
    // Built-ins run in this thread: the call's deadline lets them stop on time, like a child.
    struct Deadline;
    impl Drop for Deadline {
        fn drop(&mut self) {
            exec::set_deadline(None);
        }
    }
    let _deadline = Deadline;
    exec::set_deadline(Some(std::time::Instant::now() + timeout));
    let stopped = |mut o: exec::Outcome| {
        o.timed_out = true;
        o.code = 124;
        o
    };
    if let Some(b) = builtin::try_exec(program, args, cwd, envs) {
        let o: exec::Outcome = b.into();
        return Ok((if exec::should_stop() { stopped(o) } else { o }, "builtin"));
    }
    // A built-in that declined because it was stopped must not start the real tool instead.
    if exec::should_stop() {
        let none = builtin::BuiltinOut { code: 124, stdout: String::new(), stderr: String::new(), elapsed: Duration::ZERO };
        return Ok((stopped(none.into()), "builtin"));
    }
    let mut cmd = exec::build_command_env(program, args, cwd, envs)?;
    exec::apply_envs(&mut cmd, envs);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    exec::run_captured(cmd, Some(timeout)).map(|o| (o, plain)).map_err(started)
}

fn tool_text(id: Option<Value>, text: &str, is_error: bool) -> Value {
    tool_result(id, text, is_error, json!({}))
}

fn tool_result(id: Option<Value>, text: &str, is_error: bool, extra: Value) -> Value {
    let mut structured = json!({});
    if let Value::Object(src) = extra {
        if let Value::Object(dst) = &mut structured {
            for (k, v) in src {
                dst.insert(k, v);
            }
        }
    }
    let mut result = json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error
    });
    // The output policy decides whether the JSON copy exists at all: dsh, Claude Code and Hermes
    // reject a result that has a schema but no `structuredContent`, and Hermes prints both copies.
    match client::current().structured() {
        Structured::Off => {}
        Structured::Meta => {
            if let Value::Object(m) = &mut structured {
                m.remove("stdout");
                m.remove("stderr");
            }
            result["structuredContent"] = structured;
        }
        Structured::Full => result["structuredContent"] = structured,
    }
    ok(id, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_carries_routing_instructions() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2024-11-05" }
        });
        let res = client::scoped(Client::Dsh, || handle(&req)).expect("initialize must answer");
        assert_eq!(res.pointer("/result/serverInfo/name"), Some(&json!("qx")));
        let instructions = res
            .pointer("/result/instructions")
            .and_then(Value::as_str)
            .expect("initialize must carry instructions");
        assert!(instructions.contains("mcp__qx__run"));
        assert!(instructions.contains("mcp__qx__fs"));
        assert!(instructions.contains("program"));
        assert!(instructions.contains("cwd"));
        assert!(instructions.contains("compact"));
        assert!(instructions.contains("fall back"));
        assert!(instructions.len() < 1024);
    }

    #[test]
    fn initialize_learns_the_client_from_client_info() {
        let _global = client::lock_global();
        // A parent that sets QX_CLIENT (a qx-spawned test run does) would win over clientInfo,
        // which is the documented order; this test is about the clientInfo path.
        let saved = std::env::var_os("QX_CLIENT");
        std::env::remove_var("QX_CLIENT");
        let init = |name: &str| {
            handle(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "clientInfo": { "name": name } }
            }))
            .expect("initialize must answer")
        };
        let previous = client::current();
        init("codex-mcp-client");
        assert_eq!(client::current(), Client::Codex);
        init("dsh-mcp-client");
        assert_eq!(client::current(), Client::Dsh);
        client::set_current(previous);
        if let Some(value) = saved {
            std::env::set_var("QX_CLIENT", value);
        }
    }

    #[test]
    fn output_policy_follows_the_matrix() {
        let _server = server_lock(); // a neighbour's EOF must not cancel these calls
        let list = |c: Client| {
            client::scoped(c, || {
                handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}})).unwrap()
            })
        };
        let has_schema = |v: &Value| v.pointer("/result/tools/0/outputSchema").is_some();
        assert!(has_schema(&list(Client::Dsh)));
        assert!(has_schema(&list(Client::Hermes)));
        assert!(!has_schema(&list(Client::Codex)));
        assert!(!has_schema(&list(Client::ClaudeCode)));

        let run = |c: Client| client::scoped(c, || call(json!({"program":"echo","args":["x"],"mode":"full"})));
        assert!(run(Client::Dsh).pointer("/result/structuredContent/stdout").is_some());
        assert!(run(Client::Codex).pointer("/result/structuredContent").is_none());
        let hermes = run(Client::Hermes);
        assert!(hermes.pointer("/result/structuredContent/stdout").is_none());
        assert!(hermes.pointer("/result/structuredContent/exit_code").is_some());
        // The text keeps the output either way: that is what Codex and Hermes show.
        assert!(run(Client::Codex)
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap()
            .contains('x'));
    }

    #[test]
    fn tools_list_carries_run_and_fs() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        });
        let res = handle(&req).expect("tools/list must answer");
        let names: Vec<&str> = res
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .expect("tools array")
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(names, vec!["run", "fs"]);
    }

    fn call(args: Value) -> Value {
        call_id(9, args)
    }

    fn call_id(id: u64, args: Value) -> Value {
        respond(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"run","arguments":args}}).to_string())
            .expect("a call is always answered")
    }

    #[test]
    fn notifications_are_never_answered_or_run() {
        let note = json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"run","arguments":{"program":"echo","args":["x"]}}});
        assert!(respond(&note.to_string()).is_none());
        assert!(respond(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        assert_eq!(respond("[]").unwrap().pointer("/error/code"), Some(&json!(-32600)));
        assert_eq!(respond("5").unwrap().pointer("/error/code"), Some(&json!(-32600)));
    }

    #[test]
    fn a_non_string_argument_is_an_error_not_a_different_command() {
        let r = call(json!({"program":"echo","args":["a", 5]}));
        assert_eq!(r.pointer("/result/isError"), Some(&json!(true)));
        assert!(r.pointer("/result/content/0/text").and_then(Value::as_str).unwrap().contains("args[1]"));
        let r = call(json!({"program":"echo","args":"a b"}));
        assert_eq!(r.pointer("/result/isError"), Some(&json!(true)));
    }

    #[test]
    fn a_builtin_stops_on_timeout_and_on_cancel() {
        let _server = server_lock();
        let t = std::time::Instant::now();
        let r = client::scoped(Client::Dsh, || call_id(80, json!({"program":"sleep","args":["5"],"timeout_ms":1000})));
        assert!(t.elapsed() < Duration::from_secs(3), "timeout ignored: {:?}", t.elapsed());
        assert_eq!(r.pointer("/result/structuredContent/timed_out"), Some(&json!(true)), "{r}");
        let worker = std::thread::spawn(|| {
            client::scoped(Client::Dsh, || call_id(81, json!({"program":"sleep","args":["5"],"timeout_ms":60000})))
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(respond(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":81}}"#).is_none());
        let t = std::time::Instant::now();
        let r = worker.join().unwrap();
        assert!(t.elapsed() < Duration::from_secs(3), "cancel ignored: {:?}", t.elapsed());
        assert_eq!(r.pointer("/result/structuredContent/timed_out"), Some(&json!(true)), "{r}");
    }

    #[test]
    fn mixed_run_shapes_are_errors_not_other_commands() {
        let _server = server_lock();
        // `args` without `program` used to be dropped, running only the command's first words.
        let r = call(json!({"command":"echo hi","args":["x"]}));
        assert_eq!(r.pointer("/result/isError"), Some(&json!(true)), "{r}");
        let text = r.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default();
        assert!(text.contains("args"), "{text}");
        // An empty `program` beside a `command` is no program at all.
        let r = call(json!({"program":"","command":"echo hi"}));
        assert_ne!(r.pointer("/result/isError"), Some(&json!(true)), "{r}");
    }

    #[test]
    fn wrong_typed_cwd_timeout_and_program_are_errors_not_defaults() {
        let _server = server_lock();
        let err = |args: Value| -> String {
            let r = call(args);
            assert_eq!(r.pointer("/result/isError"), Some(&json!(true)), "{r}");
            r.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default().to_string()
        };
        // A non-string cwd used to run the command (even a delete) in qx's own directory.
        assert!(err(json!({"program":"echo","args":["x"],"cwd":123})).contains("cwd"));
        assert!(err(json!({"program":"echo","args":["x"],"timeout_ms":"500"})).contains("timeout_ms"));
        assert!(err(json!({"program":"echo","args":["x"],"timeout_ms":-5})).contains("timeout_ms"));
        assert!(err(json!({"program":7})).contains("program"));
        assert!(err(json!({"program":"echo","command":"echo y"})).contains("not both"));
        // A whole-number float is what JavaScript and Python callers produce.
        let ok = call(json!({"program":"echo","args":["x"],"timeout_ms":5000.0}));
        assert_ne!(ok.pointer("/result/isError"), Some(&json!(true)), "{ok}");
    }

    #[cfg(windows)]
    #[test]
    fn a_call_cancelled_before_it_starts_does_not_run() {
        let _server = server_lock();
        let d = std::env::temp_dir().join(format!("qx-mcp-cancelfirst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let cwd = d.to_string_lossy().to_string();
        for (id, args, marker) in [
            (900u64, json!({"program":"mkdir","args":["made"],"cwd":cwd}), "made"),
            (901u64, json!({"command":"cmd /c echo x> marker.txt","cwd":cwd}), "marker.txt"),
        ] {
            let note = format!(r#"{{"jsonrpc":"2.0","method":"notifications/cancelled","params":{{"requestId":{id}}}}}"#);
            assert!(respond(&note).is_none());
            let r = client::scoped(Client::Dsh, || call_id(id, args));
            assert!(!d.join(marker).exists(), "the cancelled call still ran ({marker}): {r}");
            assert_eq!(r.pointer("/result/structuredContent/timed_out"), Some(&json!(true)), "{r}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_env_prefix_reaches_builtins_and_the_log_keeps_args() {
        let _server = server_lock();
        let d = std::env::temp_dir().join(format!("qx-mcp-env-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..230 {
            std::fs::write(d.join(format!("f{i:03}.txt")), "").unwrap();
        }
        let cwd = d.to_string_lossy().to_string();
        let plain = client::scoped(Client::Dsh, || call(json!({"command":"ls","cwd":cwd})));
        let prefixed = client::scoped(Client::Dsh, || {
            call(json!({"command":"QX_LS_FULL=1 ls","cwd":cwd,"mode":"full"}))
        });
        // README: mode "full" returns the untouched text, so the built-in's sampling stays off too.
        let full = client::scoped(Client::Dsh, || call(json!({"command":"ls","cwd":cwd,"mode":"full"})));
        let program = client::scoped(Client::Dsh, || call(json!({"program":"ls","cwd":cwd,"mode":"full"})));
        let text = |r: &Value| r.pointer("/result/structuredContent/stdout").and_then(Value::as_str).unwrap().to_string();
        assert!(text(&plain).contains("more entries") || text(&plain).lines().count() < 230);
        for r in [&prefixed, &full, &program] {
            assert_eq!(text(r).lines().count(), 230, "{r}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn cancel_stops_the_running_command() {
        let _server = server_lock();
        let worker = std::thread::spawn(|| {
            client::scoped(Client::Dsh, || {
                call_id(77, json!({"program":"ping","args":["-n","30","127.0.0.1"],"timeout_ms":60000}))
            })
        });
        std::thread::sleep(Duration::from_millis(600));
        assert!(respond(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":77}}"#).is_none());
        let t = std::time::Instant::now();
        let r = worker.join().unwrap();
        assert!(t.elapsed() < Duration::from_secs(10), "cancel did not stop the ping");
        assert_eq!(r.pointer("/result/structuredContent/timed_out"), Some(&json!(true)));
    }

    #[cfg(windows)]
    #[test]
    fn a_cancel_that_arrives_before_the_call_gets_a_slot_is_not_lost() {
        let _server = server_lock();
        // The reader answers `notifications/cancelled` while the call still waits for a worker slot.
        assert!(respond(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":78}}"#).is_none());
        let t = std::time::Instant::now();
        let r = client::scoped(Client::Dsh, || {
            call_id(78, json!({"program":"ping","args":["-n","30","127.0.0.1"],"timeout_ms":60000}))
        });
        assert!(t.elapsed() < Duration::from_secs(10), "the early cancel was dropped");
        assert_eq!(r.pointer("/result/structuredContent/timed_out"), Some(&json!(true)));
    }

    #[test]
    fn a_cancel_racing_begin_always_lands() {
        // Touches the process-wide cancel registry, so it never overlaps a server test.
        let _server = server_lock();
        for i in 5000..5400 {
            let canceller = std::thread::spawn(move || cancel(&json!(i)));
            let guard = Inflight::begin(&Some(json!(i)));
            canceller.join().unwrap();
            let key = i.to_string();
            let hit = INFLIGHT.lock().unwrap().iter().any(|(k, f)| *k == key && f.load(Ordering::Relaxed));
            drop(guard);
            assert!(hit, "cancel {i} was lost between the two lists");
        }
    }

    #[test]
    fn the_digest_label_keeps_trailing_flags() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let l = run_label(Some("cargo"), &s(&["test", "--workspace", "--all-features", "--", "--nocapture"]), None);
        assert!(l.ends_with("--nocapture"), "{l}");
        // the same label makes the digest hand back a passing run's own output
        assert!(digest::digest(&l, "running 1 test\nhello\ntest a ... ok\ntest result: ok. 1 passed\n", "", 0).is_none());
        let paths: Vec<String> = (0..50).map(|i| format!("path{i}/file.rs")).collect();
        assert!(run_label(Some("rg"), &paths, None).len() < 100);
        assert_eq!(run_label(None, &[], Some("echo hi")), "echo hi");
    }

    #[test]
    fn malformed_requests_get_json_rpc_errors() {
        let code = |line: &str| respond(line).unwrap()["error"]["code"].clone();
        assert_eq!(code("{bad"), json!(-32700));
        assert_eq!(code("[]"), json!(-32600));
        assert_eq!(code("5"), json!(-32600));
        let batch = respond("[1]").unwrap();
        assert_eq!(batch[0]["error"]["code"], json!(-32600));
    }

    #[test]
    fn route_is_by_method_not_by_text() {
        assert_eq!(
            route(&json!({"jsonrpc":"2.0","id":"tools/call","method":"ping"})),
            Route::Inline
        );
        assert_eq!(
            route(&json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1,"reason":"tools/call"}})),
            Route::Inline
        );
        // A notification is never run, so it must never take a worker slot.
        assert_eq!(
            route(&json!({"jsonrpc":"2.0","method":"tools/call","params":{}})),
            Route::Inline
        );
        assert_eq!(
            route(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{}})),
            Route::Worker
        );
        assert_eq!(
            route(&json!([{"jsonrpc":"2.0","id":4,"method":"ping"},{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{}}])),
            Route::Worker
        );
    }

    #[test]
    fn unknown_methods_like_server_discover_get_method_not_found() {
        // dsh probes a throwaway sibling qx with this before it starts the real one.
        let r = respond(r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}"#).unwrap();
        assert_eq!(r.pointer("/error/code"), Some(&json!(-32601)));
    }

    /// Replies land here instead of stdout, so `serve_io` can be driven by a test.
    // Only the Windows server tests drive these two.
    #[cfg_attr(not(windows), allow(dead_code))]
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    impl Sink {
        /// Every reply, with a batch reply spread out so an id can be looked up directly.
        fn replies(&self) -> Vec<Value> {
            let buf = self.0.lock().unwrap_or_else(|e| e.into_inner());
            let mut out = Vec::new();
            for line in String::from_utf8_lossy(&buf).lines() {
                let value: Value = serde_json::from_str(line).expect("every reply is one JSON line");
                match value {
                    Value::Array(items) => out.extend(items),
                    one => out.push(one),
                }
            }
            out
        }
        fn reply(&self, id: u64) -> Option<Value> {
            self.replies().into_iter().find(|v| v.get("id") == Some(&json!(id)))
        }
    }

    /// A line source that answers everything at once and only reports EOF once a call has really
    /// taken the worker slot. Which of the two calls wins that slot is a thread-startup race, so
    /// the winner's id is recorded for the test instead of being assumed.
    #[cfg_attr(not(windows), allow(dead_code))]
    struct SlowEof(std::io::Cursor<Vec<u8>>, Arc<std::sync::Mutex<Option<String>>>);

    #[cfg_attr(not(windows), allow(dead_code))]
    impl SlowEof {
        fn new(text: &str) -> (Self, Arc<std::sync::Mutex<Option<String>>>) {
            let slot = Arc::new(std::sync::Mutex::new(None));
            (SlowEof(std::io::Cursor::new(text.as_bytes().to_vec()), slot.clone()), slot)
        }
        fn wait_for_the_running_call(&mut self) {
            if self.1.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                return;
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while std::time::Instant::now() < deadline {
                let started = INFLIGHT
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .map(|(k, _)| k.clone())
                    .find(|k| k == "11" || k == "12");
                if let Some(k) = started {
                    *self.1.lock().unwrap_or_else(|e| e.into_inner()) = Some(k);
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let keys: Vec<String> = INFLIGHT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(k, _)| k.clone())
                .collect();
            panic!("no call registered itself for cancellation; inflight={keys:?}");
        }
    }

    impl std::io::Read for SlowEof {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.read(buf)?;
            if n == 0 {
                self.wait_for_the_running_call();
            }
            Ok(n)
        }
    }

    impl BufRead for SlowEof {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            if self.0.fill_buf()?.is_empty() {
                self.wait_for_the_running_call();
            }
            self.0.fill_buf()
        }
        fn consume(&mut self, n: usize) {
            self.0.consume(n)
        }
    }

    /// `SHUTDOWN` and the current client are process-wide, so the tests that drive a whole
    /// server run one at a time and never overlap a test that changes either.
    static SERVER: Mutex<()> = Mutex::new(());

    fn server_lock() -> (std::sync::MutexGuard<'static, ()>, std::sync::MutexGuard<'static, ()>) {
        let global = client::lock_global();
        let guard = SERVER.lock().unwrap_or_else(|e| e.into_inner());
        SHUTDOWN.store(false, Ordering::SeqCst);
        (guard, global)
    }

    #[cfg(windows)]
    #[test]
    fn qx_npm_run_plain_script_end_to_end() {
        // A plain script must not pay for cmd.exe, and it must still see the script's own output.
        if crate::resolve::find_on_path("node").is_none() {
            println!("skip: node is not on PATH");
            return;
        }
        let _server = server_lock();
        let d = std::env::temp_dir().join(format!("qx-mcp-npmplain-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("package.json"), r#"{"scripts":{"v":"node --version"}}"#).unwrap();
        let cwd = d.to_string_lossy().to_string();
        let r = client::scoped(Client::Dsh, || {
            call(json!({"program":"npm","args":["run","v"],"cwd":cwd,"mode":"full"}))
        });
        let text = r
            .pointer("/result/structuredContent/stdout")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(r.pointer("/result/structuredContent/exit_code"), Some(&json!(0)), "{r}");
        assert!(text.starts_with('v'), "stdout: {text}");
        assert_eq!(r.pointer("/result/structuredContent/source"), Some(&json!("direct")));
    }

    #[cfg(windows)]
    #[test]
    fn a_declined_builtin_reaches_git_coreutils() {
        // Runs a real child, so it must not overlap a server test that sets `SHUTDOWN`.
        let _server = server_lock();
        // `wc` with two files is what the built-in declines: the model must still get an answer.
        if crate::resolve::git_coreutil("wc").is_none() {
            println!("skip: Git for Windows coreutils are not installed here");
            return;
        }
        let d = std::env::temp_dir().join(format!("qx-mcp-coreutil-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap_or_else(|e| panic!("create_dir_all({}): {e}", d.display()));
        std::fs::write(d.join("a.txt"), "1\n").unwrap();
        std::fs::write(d.join("b.txt"), "1\n2\n").unwrap();
        let cwd = d.to_string_lossy().to_string();
        let r = client::scoped(Client::Dsh, || {
            call(json!({"program":"wc","args":["-l","a.txt","b.txt"],"cwd":cwd,"mode":"full"}))
        });
        let stderr = r
            .pointer("/result/structuredContent/stderr")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // A sandbox that denies msys its named objects, signal pipe or file mapping cannot run
        // this at all; that is the environment refusing, not the fallback being wrong.
        if stderr.contains("fatal error") && stderr.to_ascii_lowercase().contains("usr\\bin\\wc.exe") {
            println!("skip: this process cannot run msys binaries (named-object access denied)");
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        let text = r
            .pointer("/result/structuredContent/stdout")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(
            r.pointer("/result/structuredContent/exit_code"),
            Some(&json!(0)),
            "reply: {r}"
        );
        assert!(text.contains("total"), "{text}");
        assert!(text.contains('3'), "{text}");
    }

    #[cfg(windows)]
    #[test]
    fn a_busy_batch_answers_every_id() {
        let _server = server_lock();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"run","arguments":{"program":"ping","args":["-n","8","127.0.0.1"],"timeout_ms":60000}}}"#,
            "\n",
            r#"[{"jsonrpc":"2.0","id":32,"method":"ping"},{"jsonrpc":"2.0","id":33,"method":"tools/call","params":{"name":"run","arguments":{"command":"pwd"}}}]"#,
            "\n",
        );
        let sink = Sink::default();
        let limits = Limits { workers: 1, queue: 0, drain: Duration::from_secs(3) };
        let code = serve_io(std::io::Cursor::new(input.as_bytes().to_vec()), sink.clone(), limits);
        assert_eq!(code, 0);
        let replies = sink.replies();
        let by_id = |id: u64| replies.iter().find(|v| v.get("id") == Some(&json!(id))).cloned();
        // A busy batch must answer its non-call requests too, and stay an array.
        let ping = by_id(32).unwrap_or_else(|| panic!("id 32 unanswered: {replies:?}"));
        assert!(ping.get("result").is_some(), "{ping}");
        let refused = by_id(33).unwrap_or_else(|| panic!("id 33 unanswered: {replies:?}"));
        // The text, not `structuredContent`: which one a reply carries depends on the client
        // profile, and `CLAUDECODE=1` in the test's own environment selects one without it.
        assert!(refused.pointer("/result/content/0/text").and_then(Value::as_str).unwrap().contains("qx busy"), "{refused}");
    }

    #[cfg(windows)]
    #[test]
    fn eof_cancels_running_and_queued_calls_promptly() {
        let _server = server_lock();
        // The pause before EOF is what makes "running" and "queued" deterministic: the first call
        // has really started when the client closes.
        let limits = Limits { workers: 1, queue: 4, drain: Duration::from_secs(3) };
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"run","arguments":{"program":"ping","args":["-n","30","127.0.0.1"],"timeout_ms":60000}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"run","arguments":{"program":"ping","args":["-n","30","127.0.0.1"],"timeout_ms":60000}}}"#,
            "\n",
        );
        let sink = Sink::default();
        let (src, started) = SlowEof::new(input);
        let t = std::time::Instant::now();
        let code = serve_io(src, sink.clone(), limits);
        assert!(t.elapsed() < Duration::from_secs(6), "shutdown took {:?}", t.elapsed());
        assert_eq!(code, 0);
        // The call that took the only slot is killed; the one still queued never starts.
        let running_id = started.lock().unwrap_or_else(|e| e.into_inner()).clone().expect("a call took the slot");
        let queued_id = if running_id == "11" { 12 } else { 11 };
        let running = sink.reply(running_id.parse::<u64>().unwrap()).expect("the running call answers");
        assert!(running.pointer("/result/content/0/text").and_then(Value::as_str).unwrap().contains("timed_out: true"), "{running}");
        let queued = sink.reply(queued_id).expect("the queued call answers");
        assert_eq!(queued.pointer("/result/isError"), Some(&json!(true)));
        let text = queued.pointer("/result/content/0/text").and_then(Value::as_str).unwrap();
        assert!(text.contains("qx is shutting down"), "queued call text: {text}");
    }

    #[cfg(windows)]
    #[test]
    fn calls_beyond_the_limit_get_busy() {
        let _server = server_lock();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"run","arguments":{"program":"ping","args":["-n","30","127.0.0.1"],"timeout_ms":60000}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":22,"method":"tools/call","params":{"name":"run","arguments":{"command":"pwd"}}}"#,
            "\n",
        );
        let sink = Sink::default();
        let limits = Limits { workers: 1, queue: 0, drain: Duration::from_secs(3) };
        let code = serve_io(std::io::Cursor::new(input.as_bytes().to_vec()), sink.clone(), limits);
        assert_eq!(code, 0);
        let busy = sink.reply(22).expect("the over-limit call answers");
        assert_eq!(busy.pointer("/result/isError"), Some(&json!(true)));
        let text = busy.pointer("/result/content/0/text").and_then(Value::as_str).unwrap();
        assert!(text.contains("qx busy"), "over-limit text: {text}");
    }
}
