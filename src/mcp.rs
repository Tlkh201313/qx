//! Minimal MCP (stdio) server exposing one tool: `run`.

use crate::{builtin, compact, digest, exec, guard, npm, redact};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

fn debug() -> bool {
    std::env::var("QX_DEBUG").ok().as_deref() == Some("1")
}

pub fn serve() -> i32 {
    let workers = std::env::var("QX_MCP_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(4)
        .clamp(1, 16);

    let stdin = std::io::stdin();
    let out = std::sync::Arc::new(std::sync::Mutex::new(std::io::stdout()));
    let gate = Gate::new(workers);
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    for line in BufReader::new(stdin.lock()).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        // Backpressure: keep the number of in-flight commands bounded.
        let slot = gate.acquire();
        active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let out = out.clone();
        let active = active.clone();
        std::thread::spawn(move || {
            let _slot = slot;
            let _active = Active(active);
            let line = line.trim();
            if let Ok(req) = serde_json::from_str::<Value>(line) {
                if let Some(resp) = handle(&req) {
                    if let Ok(s) = serde_json::to_string(&resp) {
                        if let Ok(mut o) = out.lock() {
                            let _ = writeln!(o, "{s}");
                            let _ = o.flush();
                        }
                    }
                }
            } else if debug() {
                eprintln!("qx mcp: bad json: {line}");
            }
        });
    }

    // Let in-flight requests finish so their replies are flushed.
    while active.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    0
}

/// Counting gate with real backpressure: the reader stops accepting new
/// requests while every worker is busy.
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

fn handle(req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
    match method {
        "initialize" => {
            let ver = req
                .pointer("/params/protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05");
            Some(json!({
                "jsonrpc": "2.0",
                "id": id.unwrap_or(Value::Null),
                "result": {
                    "protocolVersion": ver,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "qx", "version": env!("CARGO_PKG_VERSION") }
                }
            }))
        }
        "notifications/initialized" | "initialized" => None,
        "ping" => Some(ok(id, json!({}))),
        "tools/list" => Some(ok(id, json!({ "tools": [tool_schema()] }))),
        "tools/call" => Some(call_tool(req, id)),
        _ => match id {
            Some(i) => Some(rpc_error(i, -32601, "method not found")),
            None => None,
        },
    }
}

fn ok(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_schema() -> Value {
    json!({
        "name": "run",
        "description": "Run a local command fast. Prefer `program` + `args` (direct exec, no shell). Simple `command` strings are also split without a shell; use pipes/redirects only when needed. In-process built-ins: pwd, echo, ls, cat, head, tail, wc, mkdir, rm, cp, mv, touch, which, realpath, whoami, hostname, seq, sleep. Output is compacted, preserves errors from the middle, and redacts secrets.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "program": { "type": "string", "description": "Executable name or path, e.g. git, cargo, python, node." },
                "args": { "type": "array", "items": { "type": "string" }, "description": "Arguments for program." },
                "command": { "type": "string", "description": "Full shell command line for pipes/redirects/chaining." },
                "shell": { "type": "string", "enum": ["auto", "cmd", "bash"], "default": "auto" },
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
                "guard_warning": { "type": ["string", "null"] },
                "error": { "type": "string" }
            }
        }
    })
}

fn call_tool(req: &Value, id: Option<Value>) -> Value {
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name != "run" {
        return tool_text(id, "unknown tool (expected `run`)", true);
    }
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    let program = args.get("program").and_then(|v| v.as_str());
    let cmdline = args.get("command").and_then(|v| v.as_str());
    let shell = args.get("shell").and_then(|v| v.as_str()).unwrap_or("auto");

    let list: Vec<String> = args
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let cwd = args
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from);
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(120_000)
        .max(1_000);
    let full = args.get("mode").and_then(|v| v.as_str()) == Some("full");
    let label = if let Some(p) = program {
        let mut l = p.to_string();
        for a in list.iter().take(2) {
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

    let guard_cmd = program.or(cmdline).unwrap_or("");
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

    let mut source = "program";
    let outcome = if let Some(p) = program {
        if let Some(cmd) = npm::fast_command(p, &list, cwd.as_deref())
            .or_else(|| npm::fast_npx_command(p, &list, cwd.as_deref()))
        {
            source = "direct";
            match exec::run_captured(cmd, Some(Duration::from_millis(timeout_ms))) {
                Ok(o) => o,
                Err(e) => return tool_text(id, &format!("failed to start: {e}"), true),
            }
        } else if let Some(b) = builtin::try_exec(p, &list, cwd.as_deref()) {
            source = "builtin";
            exec::Outcome {
                code: b.code,
                stdout: b.stdout,
                stderr: b.stderr,
                elapsed: b.elapsed,
                timed_out: false,
            }
        } else {
            let mut cmd = match exec::build_command(p, &list) {
                Ok(c) => c,
                Err(e) => return tool_text(id, &e, true),
            };
            if let Some(d) = &cwd {
                cmd.current_dir(d);
            }
            match exec::run_captured(cmd, Some(Duration::from_millis(timeout_ms))) {
                Ok(o) => o,
                Err(e) => return tool_text(id, &format!("failed to start: {e}"), true),
            }
        }
    } else if let Some(c) = cmdline {
        let simple = exec::split_simple(c);
        let fast = simple
            .as_ref()
            .and_then(|(prog, sargs)| {
                fast_outcome(
                    prog,
                    sargs,
                    cwd.as_deref(),
                    Duration::from_millis(timeout_ms),
                )
            });
        match fast {
            Some(o) => {
                source = match &simple {
                    Some((prog, _)) if builtin::is_builtin(prog) => "builtin",
                    _ => "direct",
                };
                o
            }
            None => {
                source = "shell";
                let mut cmd = match exec::build_shell(c, shell) {
                    Ok(x) => x,
                    Err(e) => return tool_text(id, &e, true),
                };
                if let Some(d) = &cwd {
                    cmd.current_dir(d);
                }
                match exec::run_captured(cmd, Some(Duration::from_millis(timeout_ms))) {
                    Ok(o) => o,
                    Err(e) => return tool_text(id, &format!("failed to start: {e}"), true),
                }
            }
        }
    } else {
        return tool_text(id, "provide `program` + `args`, or `command`", true);
    };

    let mut so = compact::strip_ansi(&outcome.stdout);
    let mut se = compact::strip_ansi(&outcome.stderr);
    let mut truncated = false;
    let mut profile = "";
    let mut omitted = 0usize;
    if !full {
        if let Some(d) = digest::digest(&label, &so, &se, outcome.code) {
            so = d.stdout;
            se = d.stderr;
            profile = d.profile;
            omitted = d.omitted;
            truncated |= d.clipped;
        } else {
            let (a, cut) = compact::compact(&so, 200, 80, 60);
            so = a;
            truncated |= cut;
            let (b, cut) = compact::compact(&se, 120, 50, 40);
            se = b;
            truncated |= cut;
        }
    }

    let mut redactions = 0usize;
    if std::env::var("QX_REDACT").ok().as_deref() != Some("off") {
        let (a, n) = redact::redact(&so);
        so = a;
        redactions += n;
        let (b, n) = redact::redact(&se);
        se = b;
        redactions += n;
    }

    let mut text = String::new();
    if let Some(w) = &guard_warning {
        text.push_str(&format!("[qx guard] warning: {w}\n"));
    }
    text.push_str(&format!(
        "exit_code: {}\nduration_ms: {}\n",
        outcome.code,
        outcome.elapsed.as_millis()
    ));
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
    if redactions > 0 {
        text.push_str(&format!("\n[qx] redacted {redactions} secret(s)\n"));
    }

    tool_result(
        id,
        &text,
        outcome.code != 0,
        json!({
            "source": source,
            "exit_code": outcome.code,
            "duration_ms": outcome.elapsed.as_millis() as u64,
            "timed_out": outcome.timed_out,
            "truncated": truncated,
            "redactions": redactions,
            "profile": profile,
            "omitted_lines": omitted,
            "guard_warning": guard_warning
        }),
    )
}

/// Direct execution for shell-free command lines; None means "use the shell".
fn fast_outcome(
    program: &str,
    args: &[String],
    cwd: Option<&std::path::Path>,
    timeout: Duration,
) -> Option<exec::Outcome> {
    if let Some(cmd) = npm::fast_command(program, args, cwd)
        .or_else(|| npm::fast_npx_command(program, args, cwd))
    {
        return exec::run_captured(cmd, Some(timeout)).ok();
    }
    if let Some(b) = builtin::try_exec(program, args, cwd) {
        return Some(exec::Outcome {
            code: b.code,
            stdout: b.stdout,
            stderr: b.stderr,
            elapsed: b.elapsed,
            timed_out: false,
        });
    }
    let mut cmd = exec::build_command(program, args).ok()?;
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    exec::run_captured(cmd, Some(timeout)).ok()
}

fn tool_text(id: Option<Value>, text: &str, is_error: bool) -> Value {
    tool_result(id, text, is_error, json!({}))
}

fn tool_result(id: Option<Value>, text: &str, is_error: bool, extra: Value) -> Value {
    let mut structured = json!({ "blocked": false });
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
