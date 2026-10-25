//! MCP (stdio) server exposing `run` (commands) and `fs` (filesystem).
//!
//! Tool names, fallback pointers and output shape come from `client`, so the same
//! server speaks correctly to dsh, Claude Code, Codex, OpenCode, Hermes and pi.

use crate::client::{self, Client, Native, Structured};
use crate::{builtin, compact, digest, exec, guard, npm, redact};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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
            .unwrap_or(8)
            .clamp(1, 32);
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
    #[cfg(not(test))]
    crate::warmup::start();
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
    // The gate bounds spawned processes only (`spawn_slot`): reads, greps and built-ins never queue
    // behind a long build.
    *SPAWN_GATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Gate::new(limits.workers));
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let limit = limits.workers + limits.queue;

    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Bytes, not `lines()`: one invalid UTF-8 byte must not end the session.
        if !matches!(reader.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
            break;
        }
        let arrived = std::time::Instant::now();
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
        std::thread::spawn(move || {
            let _active = Active(active);
            if SHUTDOWN.load(Ordering::SeqCst) {
                // The client is gone: never start work nobody will read, but still answer
                // every id that was waiting.
                send_all(&out, &req, |id| {
                    tool_text(Some(id), "qx is shutting down; the call was not run", true)
                });
                return;
            }
            let started = arrived.elapsed();
            if let Some(resp) = respond_value(&req) {
                let done = arrived.elapsed();
                send(&out, &resp);
                if trace_calls() {
                    eprintln!("[call] start +{:?} work {:?} total {:?}", started, done - started, arrived.elapsed());
                }
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

/// `QX_TRACE_CALLS=1`: one stderr line per call with where its time went.
fn trace_calls() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("QX_TRACE_CALLS").is_some())
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
    cancel_jobs();
    let running = INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    for (_, flag) in running.iter() {
        flag.store(true, Ordering::Relaxed);
    }
}

/// The process-spawn gate of the running server (none for the CLI and most tests).
static SPAWN_GATE: Mutex<Option<Arc<Gate>>> = Mutex::new(None);

/// A slot for one real process. Waits for a free one until `deadline`, and gives up at once when the
/// call is cancelled; `None` inside `Ok` means there is no gate to wait on. Err is the reply text.
pub(crate) fn spawn_slot(deadline: std::time::Instant) -> Result<Option<Slot>, String> {
    let gate = SPAWN_GATE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(gate) = gate else { return Ok(None) };
    match gate.acquire_until(deadline, &exec::should_stop) {
        Some(slot) => Ok(Some(slot)),
        None if exec::should_stop() => Err("cancelled before it started; not run".to_string()),
        None => Err("every command slot stayed busy until the timeout (long commands are running); not started. Retry, or raise QX_MCP_WORKERS".to_string()),
    }
}

/// Counting gate: at most N processes run at once; the rest wait on their own worker thread.
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

    fn acquire_until(self: &std::sync::Arc<Self>, deadline: std::time::Instant, stop: &dyn Fn() -> bool) -> Option<Slot> {
        let mut n = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        while *n == 0 {
            if stop() || std::time::Instant::now() >= deadline {
                return None;
            }
            n = self.cv.wait_timeout(n, Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
        }
        *n -= 1;
        Some(Slot(self.clone()))
    }
}

pub(crate) struct Slot(std::sync::Arc<Gate>);

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
/// `outputSchema` for them would promise data that never arrives. Claude Code hides MCP
/// tools behind its tool search once they pass 10% of the context, and a model that must
/// look qx up first runs `Bash` instead: `anthropic/alwaysLoad` keeps both tools listed.
/// A server entry's own `"alwaysLoad": false` still wins.
fn fit_schema(schema: &mut Value, c: Client) {
    if let Value::Object(m) = schema {
        if c.structured() == Structured::Off {
            m.remove("outputSchema");
        }
        if c == Client::ClaudeCode {
            m.insert("_meta".into(), json!({ "anthropic/alwaysLoad": true }));
        }
    }
}

fn run_schema() -> Value {
    let c = client::current();
    let mut schema = json!({
        "name": "run",
        "description": format!(
            "Run a local command (like {}, but faster). `program` + `args` execs directly; `command` is a shell line: auto picks PowerShell for cmdlets, `$var`, `a; b` and single quotes, Git Bash for POSIX pipelines, else cmd. Common tools run in-process (ls cat head tail wc grep sed mkdir rm cp mv touch which ...). Stdin is empty; output is compacted (errors kept) and secrets redacted; a cut output is saved to `full_output`. After `timeout_ms` (default 120000, max 270000) a running command becomes a background job: reply names its id; follow with `job`.",
            c.native(Native::Shell)
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "program": { "type": "string", "description": "Executable name or path, e.g. git, cargo, python, node." },
                "args": { "type": "array", "items": { "type": "string" }, "description": "JSON array of strings for program, e.g. [\"-n\",\"foo\",\"src\"]. Never a bare string." },
                "command": { "type": "string", "description": "Full shell command line for pipes/redirects/chaining." },
                "shell": { "type": "string", "enum": ["auto", "cmd", "bash", "powershell"], "default": "auto", "description": "auto picks PowerShell for cmdlet/variable syntax, Git Bash when a pipeline runs a POSIX utility, else cmd. PowerShell runs -NoProfile -NonInteractive." },
                "cwd": { "type": "string", "description": "Working directory." },
                "timeout_ms": { "type": "integer", "default": 120000, "description": "Wait this long (max 270000). A command still running then keeps going as a background job and the reply names its id." },
                "mode": { "type": "string", "enum": ["compact", "full"], "default": "compact" },
                "background": { "type": "boolean", "description": "Start and return a job id at once." },
                "job": { "type": "string", "description": "Follow up a background job by id (or `list`) instead of running a command." },
                "wait_ms": { "type": "integer", "description": "With `job`: wait this long for it to finish (default 15000)." },
                "kill": { "type": "boolean", "description": "With `job`: stop it." }
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
                "job": { "type": "string" },
                "running": { "type": "boolean" },
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
    fit_schema(&mut schema, c);
    schema
}

fn fs_schema() -> Value {
    let c = client::current();
    let mut schema = json!({
        "name": "fs",
        "description": "In-process files in the workspace. op=read (path, offset?, limit?; also reads a dir, any path, files of any size); write (path, content; replacing a file needs a prior read); edit (path, old_string, new_string, replace_all?; exact unique match, no prior read); glob (pattern, path?; newest first); grep (pattern is a REGEX, path?, include?, ignore_case?, literal?; output grouped by file); list; stat. BATCH to save round trips: `ops:[{op,...},...]` runs up to 32 in order (cwd is shared), `paths:[..]` reads/stats many files, `edits:[{old_string,new_string,replace_all?}]` applies several edits to one file atomically. Pass `cwd` (the session directory). Writes stay inside cwd; reads can go anywhere. Generated dirs (node_modules, target, build, dist, vendor...) are skipped unless named in the path/pattern or all=true; the reply says what was skipped.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "op": { "type": "string", "enum": ["read", "write", "edit", "glob", "grep", "list", "stat"] },
                "cwd": { "type": "string", "description": "Session working directory (writes stay inside it). Omitted: the last one given." },
                "path": { "type": "string", "description": "File or directory (default '.'). Alias: file_path." },
                "file_path": { "type": "string", "description": "Alias for path." },
                "content": { "type": "string", "description": "write: complete new file content." },
                "old_string": { "type": "string", "description": "edit: literal text to replace." },
                "new_string": { "type": "string", "description": "edit: replacement text." },
                "replace_all": { "type": "boolean", "default": false },
                "ops": { "type": "array", "items": { "type": "object" }, "description": "Batch: [{op, path, ...}, ...] in one call; each item takes the same arguments as a single call." },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "read/stat: many files in one call." },
                "edits": { "type": "array", "items": { "type": "object" }, "description": "edit: [{old_string, new_string, replace_all?}, ...] on one path, all or nothing." },
                "pattern": { "type": "string", "description": "glob: path glob (*, **, ?, [class], {a,b}). grep: regex." },
                "include": { "type": "string", "description": "grep: one glob limiting which files are searched." },
                "offset": { "type": "integer", "default": 1, "description": "read: 1-based first line." },
                "limit": { "type": "integer", "default": 2000, "description": "read: maximum lines (cap 2000)." },
                "max_results": { "type": "integer", "default": 100, "description": "glob: maximum paths (cap 1000)." },
                "max_matches": { "type": "integer", "default": 250, "description": "grep: maximum matches (cap 1000)." },
                "max_line_chars": { "type": "integer", "default": 2000, "description": "read/grep: clip each line (cap 8000)." },
                "ignore_case": { "type": "boolean", "default": false },
                "literal": { "type": "boolean", "description": "grep: match pattern as plain text." },
                "all": { "type": "boolean", "description": "glob/grep: also search node_modules, target, build, dist, vendor..." }
            },
            "required": []
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
    fit_schema(&mut schema, c);
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

/// A foreground call waits at most this long: dsh gives up on a tool call at 300 s, and an answer
/// that arrives after that is lost, so a command still running here becomes a background job.
const SOFT_MS_DEFAULT: u64 = 270_000;
/// Claude Code moves an MCP call still running after 120 s into a task that dies with the session:
/// answer with a job handle before that happens.
const SOFT_MS_CLAUDE: u64 = 100_000;

fn soft_ms() -> u64 {
    match client::current() {
        Client::ClaudeCode => SOFT_MS_CLAUDE,
        _ => SOFT_MS_DEFAULT,
    }
}
/// A background job is killed after this long unless the call asked for less.
const JOB_MAX_MS: u64 = 3_600_000;
/// Live background jobs; below the process slots so a few jobs never starve ordinary commands.
const MAX_LIVE_JOBS: usize = 6;

type RunResult = Result<(exec::Outcome, &'static str), String>;

/// What `render` needs to turn an outcome into a reply, kept with a job so a later poll renders it
/// exactly as the foreground call would have.
#[derive(Clone)]
struct RunCtx {
    label: String,
    cwd: Option<std::path::PathBuf>,
    full: bool,
    guard_warning: Option<String>,
    /// Said when the call named no `cwd`: the command ran in qx's own directory, which is rarely meant.
    cwd_note: Option<String>,
}

/// One command running on its own thread. The call that started it waits a while; when the command
/// outlives that wait it keeps running here and the model polls it by id.
struct JobRec {
    id: std::sync::Mutex<String>,
    ctx: RunCtx,
    started: std::time::Instant,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<Option<Arc<exec::Progress>>>>,
    result: Mutex<Option<RunResult>>,
    cv: std::sync::Condvar,
}

static JOBS: Mutex<Vec<Arc<JobRec>>> = Mutex::new(Vec::new());
static JOB_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl JobRec {
    fn id(&self) -> String {
        self.id.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn done(&self) -> bool {
        self.result.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    /// Wait until the command finishes, `until` passes, or the client cancels the waiting call
    /// (which stops the command and gives it 3 s to report). `None` means still running.
    fn wait(&self, until: std::time::Instant, call: &Inflight) -> Option<RunResult> {
        let mut grace: Option<std::time::Instant> = None;
        let mut guard = self.result.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(r) = guard.take() {
                return Some(r);
            }
            if call.cancelled() && grace.is_none() {
                self.cancel.store(true, Ordering::Relaxed);
                grace = Some(std::time::Instant::now() + Duration::from_secs(3));
            }
            let limit = grace.unwrap_or(until);
            let now = std::time::Instant::now();
            if now >= limit {
                return None;
            }
            guard = self
                .cv
                .wait_timeout(guard, (limit - now).min(Duration::from_millis(100)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// Run `work` on its own thread. Its cancel flag and progress slot belong to the job, so cancelling
/// the original request later does not reach a command that was already handed over.
fn spawn_runner(ctx: RunCtx, work: impl FnOnce() -> RunResult + Send + 'static) -> Arc<JobRec> {
    let rec = Arc::new(JobRec {
        id: std::sync::Mutex::new(String::new()),
        ctx,
        started: std::time::Instant::now(),
        cancel: Arc::new(AtomicBool::new(SHUTDOWN.load(Ordering::SeqCst))),
        progress: Arc::new(Mutex::new(None)),
        result: Mutex::new(None),
        cv: std::sync::Condvar::new(),
    });
    let r = rec.clone();
    let spawned = std::thread::Builder::new().name("qx-run".into()).spawn(move || {
        exec::set_cancel(Some(r.cancel.clone()));
        exec::set_progress_slot(Some(r.progress.clone()));
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
            .unwrap_or_else(|_| Err("qx internal error: the command handler panicked".to_string()));
        *r.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(res);
        r.cv.notify_all();
    });
    if spawned.is_err() {
        *rec.result.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Err("could not start a worker thread".to_string()));
    }
    rec
}

/// Make `rec` pollable. False when too many jobs are alive already.
fn register_job(rec: &Arc<JobRec>) -> bool {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    // Results nobody fetched are dropped oldest first, so the list cannot grow without bound.
    while jobs.iter().filter(|j| j.done()).count() > 16 {
        if let Some(i) = jobs.iter().position(|j| j.done()) {
            jobs.remove(i);
        }
    }
    if jobs.iter().filter(|j| !j.done()).count() >= MAX_LIVE_JOBS {
        return false;
    }
    let n = JOB_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    *rec.id.lock().unwrap_or_else(|e| e.into_inner()) = format!("j{n}");
    jobs.push(rec.clone());
    true
}

fn find_job(id: &str) -> Option<Arc<JobRec>> {
    JOBS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|j| j.id() == id).cloned()
}

fn forget_job(id: &str) {
    JOBS.lock().unwrap_or_else(|e| e.into_inner()).retain(|j| j.id() != id);
}

/// The client is gone: stop every background job too.
fn cancel_jobs() {
    for j in JOBS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        j.cancel.store(true, Ordering::Relaxed);
    }
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// The reply for a command that is still running: its newest output and how to follow it up.
fn running_reply(id: Option<Value>, rec: &JobRec, headline: &str) -> Value {
    let (so, se) = rec.progress.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or_else(
        || (String::new(), String::new()),
        |p| p.peek(64 * 1024),
    );
    let tidy = |s: String| {
        let s = if s.contains('\x1b') { compact::strip_ansi(&s) } else { s };
        let s = if std::env::var("QX_REDACT").ok().as_deref() == Some("off") { s } else { redact::redact(&s).0 };
        compact::compact(&s, 40, 10, 30).0
    };
    let (so, se) = (tidy(so), tidy(se));
    let job = rec.id();
    let mut text = format!(
        "[qx] {headline}\nFollow: {{job:\"{job}\", wait_ms:60000}}; stop: {{job:\"{job}\", kill:true}}\n"
    );
    if !so.is_empty() {
        text.push_str("\n[stdout, newest]\n");
        text.push_str(&so);
        if !so.ends_with('\n') {
            text.push('\n');
        }
    }
    if !se.is_empty() {
        text.push_str("\n[stderr, newest]\n");
        text.push_str(&se);
        if !se.ends_with('\n') {
            text.push('\n');
        }
    }
    tool_result(
        id,
        &text,
        false,
        json!({ "job": job, "running": true, "elapsed_ms": rec.started.elapsed().as_millis() as u64 }),
    )
}

/// `run {job: "j1" | "list", wait_ms?, kill?}`: look at a background job, wait for it, or stop it.
fn job_call(id: Option<Value>, job: &str, wait_ms: u64, kill: bool) -> Value {
    let call = Inflight::begin(&id);
    if job == "list" {
        let jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
        if jobs.is_empty() {
            return tool_text(id, "[qx] no background jobs", false);
        }
        let mut text = String::new();
        for j in jobs.iter() {
            text.push_str(&format!(
                "{} {} {} {}\n",
                j.id(),
                if j.done() { "finished" } else { "running" },
                human(j.started.elapsed()),
                j.ctx.label.chars().take(100).collect::<String>()
            ));
        }
        return tool_text(id, &text, false);
    }
    let Some(rec) = find_job(job) else {
        let known: Vec<String> = JOBS.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|j| j.id()).collect();
        return tool_text(
            id,
            &format!("[qx] no job {job}; known: {} (jobs are lost when qx restarts)", if known.is_empty() { "none".into() } else { known.join(", ") }),
            true,
        );
    };
    let until = if kill {
        rec.cancel.store(true, Ordering::Relaxed);
        std::time::Instant::now() + Duration::from_secs(5)
    } else {
        std::time::Instant::now() + Duration::from_millis(wait_ms.min(soft_ms()))
    };
    match rec.wait(until, &call) {
        Some(res) => {
            forget_job(job);
            let prefix = format!("[qx] job {job} finished after {}\n", human(rec.started.elapsed()));
            finish(id, res, &rec.ctx, &prefix)
        }
        None => {
            let headline = if kill {
                format!("job {job} was asked to stop and has not exited yet ({} so far)", human(rec.started.elapsed()))
            } else {
                format!("job {job} still running ({})", human(rec.started.elapsed()))
            };
            running_reply(id, &rec, &headline)
        }
    }
}

fn finish(id: Option<Value>, res: RunResult, ctx: &RunCtx, prefix: &str) -> Value {
    match res {
        Ok((outcome, source)) => render(id, outcome, source, ctx, prefix),
        Err(e) => tool_text(id, &e, true),
    }
}

/// A string argument under any of the names a model may use (dsh's own shell tool says `workdir`).
fn text_arg<'a>(args: &'a Value, keys: &[&str]) -> Result<Option<&'a str>, String> {
    for key in keys {
        match args.get(*key) {
            None | Some(Value::Null) => continue,
            Some(Value::String(s)) => return Ok(Some(s.as_str())),
            Some(other) => return Err(format!("`{key}` must be a string, got {other}")),
        }
    }
    Ok(None)
}

/// Whole numbers arrive as `5000`, `5000.0` or `"5000"`; anything else is an error, because guessing
/// a time limit is worse than asking.
fn number_arg(args: &Value, keys: &[&str]) -> Result<Option<u64>, String> {
    for key in keys {
        let Some(v) = args.get(*key).filter(|v| !v.is_null()) else { continue };
        let whole = v
            .as_u64()
            .or_else(|| v.as_f64().filter(|f| *f >= 0.0 && f.fract() == 0.0 && *f < 1e15).map(|f| f as u64))
            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()));
        return match whole {
            Some(n) => Ok(Some(n)),
            None => Err(format!("`{key}` must be a whole number, got {v}")),
        };
    }
    Ok(None)
}

fn flag_arg(args: &Value, keys: &[&str]) -> bool {
    keys.iter().any(|k| match args.get(*k) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    })
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
    let (program, cmdline, cwd_text) = match (
        text_arg(&args, &["program"]),
        text_arg(&args, &["command"]),
        text_arg(&args, &["cwd", "workdir"]),
    ) {
        (Ok(p), Ok(c), Ok(d)) => (p, c, d),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return tool_text(id, &e, true),
    };
    let (timeout_arg, wait_arg) = match (number_arg(&args, &["timeout_ms", "timeoutMs"]), number_arg(&args, &["wait_ms"])) {
        (Ok(t), Ok(w)) => (t, w),
        (Err(e), _) | (_, Err(e)) => return tool_text(id, &e, true),
    };
    if let Some(job) = text_arg(&args, &["job"]).ok().flatten().filter(|j| !j.trim().is_empty()) {
        return job_call(id, job.trim(), wait_arg.unwrap_or(15_000), flag_arg(&args, &["kill"]));
    }
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
        Some(Value::String(s)) => crate::guard::args_from_text(s),
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
    // No `cwd`: the one the session last named (see `fsops::last_cwd`), else qx's own directory.
    let cwd = match (cwd, cwd_text) {
        (Some(d), _) => {
            crate::fsops::note_cwd(&d);
            Some(d)
        }
        (None, _) => crate::fsops::last_cwd().filter(|d| d.is_dir()),
    };
    let timeout_ms = timeout_arg.unwrap_or(120_000).clamp(1_000, 3_600_000);
    let full = args.get("mode").and_then(|v| v.as_str()) == Some("full");
    let background = flag_arg(&args, &["background", "run_in_background"]);
    let kill_on_timeout = flag_arg(&args, &["kill_on_timeout"]);
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
                "[qx guard] BLOCKED: {}\nThe command was not executed. Rewrite it with explicit paths (no pipes into delete, no computed targets), or use the native shell tool if the user really wants it.",
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
    if program.is_none() && cmdline.is_none() {
        return tool_text(id, "provide `program` + `args`, or `command`", true);
    }

    // The foreground wait never passes dsh's own limit; what is still running then is a job.
    let promote = std::env::var("QX_PROMOTE").ok().as_deref() != Some("off") && !kill_on_timeout;
    let window = Duration::from_millis(if promote { timeout_ms.min(soft_ms()) } else { timeout_ms });
    let hard = if promote { Duration::from_millis(JOB_MAX_MS) } else { window };
    // mode "full" promises the untouched text: built-ins must not sample their own output either.
    let full_env: Vec<(String, String)> = if full { vec![("QX_LS_FULL".into(), "1".into())] } else { Vec::new() };
    let ctx = RunCtx {
        label,
        cwd: cwd.clone(),
        full,
        guard_warning,
        cwd_note: if cwd_text.is_none() {
            cwd.clone()
                .or_else(|| std::env::current_dir().ok())
                .map(|d| format!("[qx] no cwd given: ran in {}\n", d.display()))
        } else {
            None
        },
    };
    let (program, cmdline) = (program.map(str::to_string), cmdline.map(str::to_string));
    let shell = shell.to_string();
    let rec = spawn_runner(
        ctx.clone(),
        move || {
            if let Some(p) = program {
                run_direct(&p, &list, &full_env, cwd.as_deref(), window, hard, "program")
            } else {
                let c = cmdline.unwrap_or_default();
                // The guard judged the whole line; env prefixes only change what runs.
                let (mut envs, c) = exec::peel_env(&c);
                envs.extend(full_env);
                let direct = exec::split_simple(c)
                    .and_then(|(prog, sargs)| run_direct(&prog, &sargs, &envs, cwd.as_deref(), window, hard, "direct").ok());
                match direct {
                    Some(x) => Ok(x),
                    None => {
                        let _slot = match spawn_slot(std::time::Instant::now() + window) {
                            Ok(s) => s,
                            Err(msg) => return Ok((busy_outcome(msg), "busy")),
                        };
                        exec::run_shell(c, &shell, &envs, &cwd, Some(hard)).map(|o| (o, "shell"))
                    }
                }
            }
        },
    );

    // `background` still gives a command that ends at once a plain answer.
    let wait = if background { Duration::from_millis(300) } else { window + Duration::from_millis(400) };
    match rec.wait(std::time::Instant::now() + wait, &inflight) {
        Some(res) => finish(id, res, &ctx, ""),
        None if inflight.cancelled() => tool_result(
            id,
            "exit_code: 124\ntimed_out: true\n(cancelled)\n",
            true,
            json!({"timed_out": true, "exit_code": 124, "source": "cancelled"}),
        ),
        None => {
            if !promote || !register_job(&rec) {
                // No job to hand over to: stop it, as a timeout always did.
                rec.cancel.store(true, Ordering::Relaxed);
                let until = std::time::Instant::now() + Duration::from_secs(5);
                return match rec.wait(until, &inflight) {
                    Some(res) => finish(id, res, &ctx, ""),
                    None => tool_result(id, "exit_code: 124\ntimed_out: true\n(stopped; too many background jobs)\n", true, json!({"timed_out": true, "exit_code": 124})),
                };
            }
            let headline = if background {
                format!("job {} started", rec.id())
            } else {
                format!("still running after {}; moved to background job {}", human(rec.started.elapsed()), rec.id())
            };
            running_reply(id, &rec, &headline)
        }
    }
}

/// Everything after the command ended: strip, redact, digest or compact, spool what was cut, and
/// write the reply text and its structured copy.
fn render(id: Option<Value>, outcome: exec::Outcome, source: &str, ctx: &RunCtx, prefix: &str) -> Value {
    let (full, cwd, guard_warning) = (ctx.full, ctx.cwd.as_deref(), ctx.guard_warning.clone());
    let label = ctx.label.as_str();
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
    let mut spool = None;
    if !full {
        // The text before compaction, kept only so a cut can be spooled to disk.
        let original: (String, String);
        compact::drop_build_noise(&mut se);
        compact::drop_build_noise(&mut so);
        if let Some(d) = digest::digest(label, &so, &se, outcome.code) {
            lossy = d.lossy();
            original = (std::mem::replace(&mut so, d.stdout), std::mem::replace(&mut se, d.stderr));
            profile = d.profile;
            omitted = d.omitted;
            truncated |= d.clipped;
            if matches!(profile, "build" | "test" | "traceback") {
                compact::relative_to_cwd(&mut so, &mut se, cwd);
            }
        } else {
            let (a, cut) = compact::compact(&so, 200, 80, 60);
            let (b, cut_err) = compact::compact(&se, 120, 50, 40);
            original = (std::mem::replace(&mut so, a), std::mem::replace(&mut se, b));
            truncated |= cut | cut_err;
        }
        // A cut or a digest that dropped content must never lose facts for good: the agent can grep the full text.
        if truncated || lossy {
            spool = compact::spool(&original.0, &original.1);
        }
    } else if so.len() > FULL_TEXT_CAP || se.len() > FULL_TEXT_CAP {
        // "Full" is still bounded: millions of characters in one reply cost more than they help. The
        // end is kept (that is where a failure is), the whole text goes to a file.
        spool = compact::spool(&so, &se);
        truncated |= cap_text(&mut so, FULL_TEXT_CAP);
        truncated |= cap_text(&mut se, FULL_TEXT_CAP);
    }

    let mut text = String::from(prefix);
    if let Some(n) = &ctx.cwd_note {
        text.push_str(n);
    }
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
    // A clean run says nothing but its output: the section header is ~4 tokens on every call.
    let plain = outcome.code == 0 && !outcome.timed_out && se.is_empty();
    if !so.is_empty() {
        if !plain {
            text.push_str("\n[stdout]\n");
        }
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
    } else if (truncated || lossy) && !full {
        text.push_str("\n[qx] output was cut and could not be saved; re-run with mode:\"full\" for all of it\n");
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
    // A command that ran is not a tool fault: a failing test or an empty `grep` is a result the model
    // reads (dsh turns `isError` into a thrown error, which sends the model looking for a fallback).
    let fault = outcome.timed_out || (outcome.code != 0 && client::current().exit_is_error());
    tool_result(id, &text, fault, meta)
}

/// The most text one `mode:"full"` reply carries.
const FULL_TEXT_CAP: usize = 96 * 1024;

/// Keep the first fifth and the last four fifths of `s` within `max` bytes. True when it cut.
fn cap_text(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    let head_len = max / 5;
    let mut h = head_len;
    while !s.is_char_boundary(h) {
        h -= 1;
    }
    let mut t = s.len() - (max - head_len);
    while !s.is_char_boundary(t) {
        t += 1;
    }
    let cut = t - h;
    *s = format!("{}\n[qx] ... {cut} bytes omitted ...\n{}", &s[..h], &s[t..]);
    true
}

/// Run without a shell: the npm fast path, a built-in, then the real program.
/// `plain` names the source when the real program ran. Err is the message for the agent.
fn run_direct(
    program: &str,
    args: &[String],
    envs: &[(String, String)],
    cwd: Option<&std::path::Path>,
    timeout: Duration,
    hard: Duration,
    plain: &'static str,
) -> Result<(exec::Outcome, &'static str), String> {
    let started = |e| format!("failed to start: {e}");
    if !exec::needs_real_npm(envs) {
        if let Some(mut cmds) = npm::direct(program, args, cwd) {
            for c in &mut cmds {
                exec::apply_envs(c, envs);
            }
            let _slot = match spawn_slot(std::time::Instant::now() + timeout) {
                Ok(s) => s,
                Err(msg) => return Ok((busy_outcome(msg), "busy")),
            };
            return exec::run_chain(cmds, Some(hard)).map(|o| (o, "direct")).map_err(started);
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
        // A built-in that finished its work right at the deadline did the work: report it done.
        return Ok((if exec::should_stop() && o.code != 0 { stopped(o) } else { o }, "builtin"));
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
    let _slot = match spawn_slot(std::time::Instant::now() + timeout) {
        Ok(s) => s,
        Err(msg) => return Ok((busy_outcome(msg), "busy")),
    };
    exec::run_captured(cmd, Some(hard)).map(|o| (o, plain)).map_err(started)
}

/// A call that never got a process slot: reported like a timeout, with the reason, and never run.
fn busy_outcome(msg: String) -> exec::Outcome {
    exec::Outcome { code: 124, stdout: String::new(), stderr: msg, elapsed: Duration::ZERO, timed_out: true, cut: false }
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
        // Only Claude Code reads the key; the others get the tools exactly as before.
        let always_loaded = |v: &Value, i: usize| {
            v.pointer(&format!("/result/tools/{i}/_meta/anthropic~1alwaysLoad")) == Some(&json!(true))
        };
        let claude = list(Client::ClaudeCode);
        assert!(always_loaded(&claude, 0) && always_loaded(&claude, 1));
        for c in [Client::Dsh, Client::Codex, Client::OpenCode, Client::Hermes, Client::Pi, Client::Generic] {
            assert!(list(c).pointer("/result/tools/0/_meta").is_none(), "{c:?}");
        }

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
        // A string is split into words (some models stringify the array); any other type is refused.
        let r = call(json!({"program":"echo","args":"a b"}));
        assert_ne!(r.pointer("/result/isError"), Some(&json!(true)));
        assert!(r.pointer("/result/content/0/text").and_then(Value::as_str).unwrap().contains("a b"));
        let r = call(json!({"program":"echo","args":7}));
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
        assert!(err(json!({"program":"echo","args":["x"],"timeout_ms":"soon"})).contains("timeout_ms"));
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
        assert!(text.contains("cancelled") || text.contains("shutting down"), "queued call text: {text}");
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
