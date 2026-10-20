//! MCP (stdio) server exposing `run` (commands) and `fs` (filesystem).

use crate::{builtin, compact, digest, exec, guard, npm, redact};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// MCP server instructions are injected into the harness system prompt by
/// clients such as dsh. Keep this short: it is paid on every request, so it
/// only states the routing rule and the output contract.
const MCP_INSTRUCTIONS: &str = r#"qx is the preferred tool layer for this session. Call `mcp__qx__run` for commands: prefer `program` + `args`, pass the session `cwd`; for env vars use `command: "NAME=v prog args"` (runs prog directly, ~0.15 s) instead of a powershell wrapper (~0.4 s start, ~1.5 s with Start-Process), and keep `mode: "compact"`. Call `mcp__qx__fs` for filesystem work: `op=read|write|edit|glob|grep|list|stat`, always with `cwd`. Use qx fs first for read, glob, and literal grep; qx grep is literal-only, so use native grep for regex. qx fs is workspace-confined, and write/edit require a prior qx read. If a qx call errors, or the task needs dsh sandbox escalation, images, or regex, fall back to the native dsh tool of the same purpose. Native tools always remain available. If a qx `run` is truncated, read its `full_output` spool instead of re-running."#;

pub fn serve() -> i32 {
    let workers = std::env::var("QX_MCP_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(4)
        .clamp(1, 16);

    let stdin = std::io::stdin();
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let gate = Gate::new(workers);
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut reader = BufReader::new(stdin.lock());
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
        // Only `tools/call` runs long. ping, cancel and listing answer at once, even when every worker is busy.
        if !line.contains("tools/call") {
            if let Some(resp) = respond(line.trim()) {
                send(&out, &resp);
            }
            continue;
        }
        // Backpressure: the gate bounds running commands, but the wait for a slot happens on the
        // worker so this loop keeps reading (cancel, ping) while every slot is busy.
        active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let out = out.clone();
        let active = active.clone();
        let gate = gate.clone();
        std::thread::spawn(move || {
            let _active = Active(active);
            let _slot = gate.acquire();
            if let Some(resp) = respond(line.trim()) {
                send(&out, &resp);
            }
        });
    }

    // Let in-flight requests finish so their replies are flushed.
    while active.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    0
}

fn send(out: &Mutex<std::io::Stdout>, resp: &Value) {
    if let Ok(s) = serde_json::to_string(resp) {
        // A poisoned lock still holds a usable stdout.
        let mut o = out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(o, "{s}");
        let _ = o.flush();
    }
}

/// Running `tools/call`s by request id, so `notifications/cancelled` can stop the right one.
static INFLIGHT: Mutex<Vec<(String, Arc<AtomicBool>)>> = Mutex::new(Vec::new());

/// Cancels that named no running call: a call still waiting for a worker slot (or one that has
/// already finished). `Inflight::begin` honours the first kind; the list is capped so the second cannot grow it.
static EARLY_CANCELS: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Inflight(Option<String>);

impl Inflight {
    /// Registers the calling worker thread's command for cancellation until dropped.
    fn begin(id: &Option<Value>) -> Self {
        let Some(key) = id.as_ref().map(Value::to_string) else {
            return Inflight(None);
        };
        // Lock order is always INFLIGHT then EARLY_CANCELS, held across check-and-insert, so a cancel
        // lands either in the flag or in the early list this reads, never between the two.
        let flag = {
            let mut running = INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
            let mut early = EARLY_CANCELS.lock().unwrap_or_else(|e| e.into_inner());
            let before = early.len();
            early.retain(|k| *k != key);
            let flag = Arc::new(AtomicBool::new(early.len() != before));
            running.push((key.clone(), flag.clone()));
            flag
        };
        exec::set_cancel(Some(flag));
        Inflight(Some(key))
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
fn respond(line: &str) -> Option<Value> {
    let Ok(req) = serde_json::from_str::<Value>(line) else {
        return Some(rpc_error(Value::Null, -32700, "parse error"));
    };
    if let Value::Array(batch) = &req {
        if batch.is_empty() {
            return Some(rpc_error(Value::Null, -32600, "invalid request: empty batch"));
        }
        let replies: Vec<Value> = batch.iter().filter_map(guarded).collect();
        return (!replies.is_empty()).then_some(Value::Array(replies));
    }
    guarded(&req)
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
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": ver,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "qx", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": MCP_INSTRUCTIONS
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

fn run_schema() -> Value {
    json!({
        "name": "run",
        "description": "Run a local command fast. Prefer `program` + `args` (direct exec, no shell). Simple `command` strings are also split without a shell; use pipes/redirects only when needed. In-process built-ins: pwd, echo, ls, cat, head, tail, wc, mkdir, rm, cp, mv, touch, which, realpath, whoami, hostname, seq, sleep. Output is compacted, preserves errors from the middle, and redacts secrets. On failure, fall back to native bash/pwsh for sandbox escalation, background jobs, or PTY work.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "program": { "type": "string", "description": "Executable name or path, e.g. git, cargo, python, node." },
                "args": { "type": "array", "items": { "type": "string" }, "description": "Arguments for program." },
                "command": { "type": "string", "description": "Full shell command line for pipes/redirects/chaining." },
                "shell": { "type": "string", "enum": ["auto", "cmd", "bash", "powershell"], "default": "auto", "description": "auto picks powershell for cmdlet syntax, bash for POSIX syntax, else cmd. powershell skips the user profile." },
                "cwd": { "type": "string", "description": "Working directory." },
                "timeout_ms": { "type": "integer", "default": 120000 },
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
    })
}

fn fs_schema() -> Value {
    json!({
        "name": "fs",
        "description": "Fast in-process filesystem operations for the workspace. op=read: line-numbered UTF-8 read (path, offset?, limit?). op=write: create or replace (path, content); an existing file needs a prior qx read. op=edit: literal replace (path, old_string, new_string, replace_all?); needs a prior qx read. op=glob: path discovery (pattern; path?). op=grep: literal content search (pattern; path?, include?, ignore_case?); one output line is path:line: text. op=list/stat. Always pass cwd; paths outside cwd are refused. qx skips generated dirs (node_modules, target, dist, build, ...) during recursive walks for speed; pass one as `path` to search it directly, or use native dsh glob/grep when the whole ignored tree matters. qx grep is literal, not regex. On any error use the native dsh read/write/edit/glob/grep tool. Native tools remain authoritative for sandbox escalation, images, and regex. qx mutations are atomic and read-before-write guarded, but do not replace dsh's sandbox policy.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "op": { "type": "string", "enum": ["read", "write", "edit", "glob", "grep", "list", "stat"] },
                "cwd": { "type": "string", "description": "Session working directory. qx confines every path to this root." },
                "path": { "type": "string", "description": "File or directory, absolute or relative to cwd. Defaults to '.' for glob, grep, and list. Alias: file_path." },
                "file_path": { "type": "string", "description": "Alias for path, matching the native dsh read/write/edit tools." },
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
            "required": ["op", "cwd"]
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

    let program = args.get("program").and_then(|v| v.as_str());
    let cmdline = args.get("command").and_then(|v| v.as_str());
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
    let cwd = args
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from);
    // A bad cwd must fail loudly: built-ins would otherwise run against the wrong place.
    if let Some(d) = cwd.as_ref().filter(|d| !d.is_dir()) {
        return tool_text(id, &format!("cwd is not a directory: {}", d.display()), true);
    }
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(120_000)
        .clamp(1_000, 3_600_000);
    let full = args.get("mode").and_then(|v| v.as_str()) == Some("full");
    let label = if let Some(p) = program {
        let mut l = p.to_string();
        for a in list.iter().take(4) {
            if l.len() > 80 {
                break;
            }
            l.push(' ');
            l.push_str(a);
        }
        l
    } else {
        cmdline.unwrap_or("").to_string()
    };

    let guard_cmd = match program {
        Some(p) => std::iter::once(p).chain(list.iter().map(String::as_str)).collect::<Vec<_>>().join(" "),
        None => cmdline.unwrap_or("").to_string(),
    };
    let guard_cmd = guard_cmd.as_str();
    let _inflight = Inflight::begin(&id);
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
    let (outcome, source) = if let Some(p) = program {
        match run_direct(p, &list, &[], cwd.as_deref(), timeout, "program") {
            Ok(x) => x,
            Err(e) => return tool_text(id, &e, true),
        }
    } else if let Some(c) = cmdline {
        // The guard above judged the whole line; env prefixes only change what runs.
        let (envs, c) = exec::peel_env(c);
        let direct = exec::split_simple(c)
            .and_then(|(prog, sargs)| run_direct(&prog, &sargs, &envs, cwd.as_deref(), timeout, "direct").ok());
        match direct {
            Some(x) => x,
            None => {
                let mut cmd = match exec::build_shell(c, shell) {
                    Ok(x) => x,
                    Err(e) => return tool_text(id, &e, true),
                };
                exec::apply_envs(&mut cmd, &envs);
                exec::apply_cwd(&mut cmd, &cwd);
                match exec::run_captured(cmd, Some(timeout)) {
                    Ok(o) => (o, "shell"),
                    Err(e) => return tool_text(id, &format!("failed to start: {e}"), true),
                }
            }
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
    if let Some(b) = builtin::try_exec(program, args, cwd, envs) {
        return Ok((b.into(), "builtin"));
    }
    let mut cmd = exec::build_command_at(program, args, cwd)?;
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
    ok(
        id,
        json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": structured,
            "isError": is_error
        }),
    )
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
        let res = handle(&req).expect("initialize must answer");
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
    fn an_env_prefix_reaches_builtins_and_the_log_keeps_args() {
        let d = std::env::temp_dir().join(format!("qx-mcp-env-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..230 {
            std::fs::write(d.join(format!("f{i:03}.txt")), "").unwrap();
        }
        let cwd = d.to_string_lossy().to_string();
        let plain = call(json!({"command":"ls","cwd":cwd,"mode":"full"}));
        let full = call(json!({"command":"QX_LS_FULL=1 ls","cwd":cwd,"mode":"full"}));
        let text = |r: &Value| r.pointer("/result/structuredContent/stdout").and_then(Value::as_str).unwrap().to_string();
        assert!(text(&plain).contains("more entries") || text(&plain).lines().count() < 230);
        assert_eq!(text(&full).lines().count(), 230);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn cancel_stops_the_running_command() {
        let worker = std::thread::spawn(|| {
            call_id(77, json!({"program":"ping","args":["-n","30","127.0.0.1"],"timeout_ms":60000}))
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
        // The reader answers `notifications/cancelled` while the call still waits for a worker slot.
        assert!(respond(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":78}}"#).is_none());
        let t = std::time::Instant::now();
        let r = call_id(78, json!({"program":"ping","args":["-n","30","127.0.0.1"],"timeout_ms":60000}));
        assert!(t.elapsed() < Duration::from_secs(10), "the early cancel was dropped");
        assert_eq!(r.pointer("/result/structuredContent/timed_out"), Some(&json!(true)));
    }

    #[test]
    fn a_cancel_racing_begin_always_lands() {
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
    fn malformed_requests_get_json_rpc_errors() {
        let code = |line: &str| respond(line).unwrap()["error"]["code"].clone();
        assert_eq!(code("{bad"), json!(-32700));
        assert_eq!(code("[]"), json!(-32600));
        assert_eq!(code("5"), json!(-32600));
        let batch = respond("[1]").unwrap();
        assert_eq!(batch[0]["error"]["code"], json!(-32600));
    }
}
