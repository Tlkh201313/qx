//! qx — fast local command executor for coding agents.

mod builtin;
mod client;
mod compact;
mod deploy;
mod digest;
mod doctor;
mod exec;
mod fsops;
#[cfg(test)]
mod fuzz;
mod guard;
mod hook;
mod mcp;
mod npm;
mod python;
mod redact;
mod resolve;
mod setup;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct RunOpts {
    stream: bool,
    compact: bool,
    max_lines: usize,
    head: usize,
    tail: usize,
    timeout: Option<Duration>,
    cwd: Option<PathBuf>,
    shell: String,
}

impl Default for RunOpts {
    fn default() -> Self {
        Self {
            // A terminal wants live output; an agent wants a captured, compact reply.
            stream: std::io::stdout().is_terminal(),
            compact: true,
            max_lines: 200,
            head: 80,
            tail: 60,
            timeout: None,
            cwd: None,
            shell: "auto".to_string(),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(dispatch(&args));
}

/// The first words `dispatch` takes as qx's own commands: `qx which git` is not `which git`.
pub const SUBCOMMANDS: &[&str] = &[
    "help", "version", "doctor", "bench", "deploy", "setup", "hook", "gain", "mcp", "serve", "which", "check",
    "policy", "audit", "proxy",
];

fn dispatch(args: &[String]) -> i32 {
    let Some(first) = args.first() else {
        print_help();
        return 0;
    };

    match first.as_str() {
        "-h" | "--help" | "help" => {
            print_help();
            0
        }
        "-V" | "--version" | "version" => {
            println!("qx {VERSION}");
            0
        }
        "doctor" | "bench" => doctor::run(),
        "deploy" => deploy::run(&args[1..]),
        "setup" => setup::run(&args[1..]),
        "hook" => hook::run(&args[1..]),
        "gain" => gain(),
        "mcp" | "serve" => mcp::serve(),
        "which" => which(&args[1..]),
        "check" | "policy" => check_cmd(&args[1..]),
        "audit" => audit_cmd(&args[1..]),
        "proxy" => run_cli(&args[1..], true),
        _ => run_cli(args, false),
    }
}

fn parse_num(v: Option<&String>, fallback: usize) -> usize {
    v.and_then(|s| s.parse::<usize>().ok()).unwrap_or(fallback)
}

fn run_cli(args: &[String], force_raw: bool) -> i32 {
    let mut o = RunOpts::default();
    // `echo data | qx grep x`: the command reads qx's stdin. At a terminal it already does.
    exec::INHERIT_STDIN.store(!std::io::stdin().is_terminal(), std::sync::atomic::Ordering::Relaxed);
    if force_raw {
        o.stream = false;
        o.compact = false;
    }

    let mut i = 0usize;
    let mut shell_cmd: Option<String> = None;

    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            i += 1;
            break;
        }
        match a {
            "--stream" => {
                o.stream = true;
                i += 1;
            }
            "--capture" => {
                o.stream = false;
                i += 1;
            }
            "--raw" | "--full" | "--no-compact" => {
                o.compact = false;
                i += 1;
            }
            "--compact" => {
                o.compact = true;
                i += 1;
            }
            "--max-lines" => {
                i += 1;
                o.max_lines = parse_num(args.get(i), o.max_lines);
                i += 1;
            }
            "--head" => {
                i += 1;
                o.head = parse_num(args.get(i), o.head);
                i += 1;
            }
            "--tail" => {
                i += 1;
                o.tail = parse_num(args.get(i), o.tail);
                i += 1;
            }
            "--timeout" => {
                i += 1;
                o.timeout = Some(Duration::from_secs(parse_num(args.get(i), 120) as u64));
                i += 1;
            }
            "--cwd" => {
                i += 1;
                o.cwd = args.get(i).map(PathBuf::from);
                i += 1;
            }
            "--shell" => {
                i += 1;
                o.shell = args.get(i).cloned().unwrap_or_else(|| "auto".to_string());
                i += 1;
            }
            "-c" | "--command" => {
                i += 1;
                shell_cmd = args.get(i).cloned();
                i += 1;
                break;
            }
            "-h" | "--help" => {
                print_help();
                return 0;
            }
            _ if a.starts_with('-') && a.len() > 1 => {
                eprintln!("qx: unknown flag {a} (put `--` before a program that starts with -)");
                return 2;
            }
            _ => break,
        }
    }

    // A bad cwd must fail loudly: built-ins would otherwise run against the wrong place.
    if let Some(d) = o.cwd.as_ref().filter(|d| !d.is_dir()) {
        eprintln!("qx: --cwd is not a directory: {}", d.display());
        return 2;
    }

    // Safety gate: qx is auto-approved in Claude Code, so this is the boundary.
    // It runs before built-ins too, because rm/cp/mv are built-ins.
    let guard_command = match &shell_cmd {
        Some(sh) => sh.clone(),
        None => args.get(i..).unwrap_or(&[]).join(" "),
    };
    let decision = if let Some(sh) = &shell_cmd {
        if let Some((prog, sargs)) = exec::split_simple(sh) {
            guard::check(&prog, &sargs, o.cwd.as_deref())
        } else {
            guard::check_shell(sh, o.cwd.as_deref())
        }
    } else if let Some(prog) = args.get(i) {
        guard::check(prog, &args[i + 1..], o.cwd.as_deref())
    } else {
        guard::Decision::allow()
    };
    if !decision.allowed() {
        eprintln!("[qx guard] BLOCKED: {}", decision.summary());
        guard::record("cli", &guard_command, &decision);
        return 126;
    }
    if decision.level == guard::Level::Warn {
        eprintln!("[qx guard] warning: {}", decision.summary());
    }
    guard::record("cli", &guard_command, &decision);

    let (mut cmd, label) = if let Some(sh) = shell_cmd {
        // `CI=1 npm test`: the prefix goes to the child only, never into qx's own environment
        // (that would also reach qx's switches such as QX_REDACT).
        let (envs, run) = exec::peel_env(&sh);
        let run = run.to_string();
        // A quoted command with no shell syntax does not need a shell at all.
        if let Some((prog, sargs)) = exec::split_simple(&run) {
            if std::env::var_os("QX_TRACE").is_some() {
                eprintln!("[trace] split: {prog:?} {sargs:?}");
            }
            if let Some(b) = builtin::try_exec(&prog, &sargs, o.cwd.as_deref(), &envs) {
                return emit(b.into(), &o, &sh);
            }
            if let Some(code) = direct_exec(&prog, &sargs, &envs, &o, &sh) {
                return code;
            }
            if let Ok(mut c) = exec::build_command_at(&prog, &sargs, o.cwd.as_deref()) {
                exec::apply_envs(&mut c, &envs);
                exec::apply_cwd(&mut c, &o.cwd);
                return execute(vec![c], &o, &sh);
            }
        }
        match exec::build_shell(&run, &o.shell) {
            Ok(mut c) => {
                exec::apply_envs(&mut c, &envs);
                (c, sh)
            }
            Err(e) => {
                eprintln!("qx: {e}");
                return 2;
            }
        }
    } else {
        let Some(program) = args.get(i) else {
            eprintln!("qx: no command given (try `qx --help`)");
            return 2;
        };
        let rest = &args[i + 1..];
        let mut label = program.clone();
        for a in rest.iter().take(4) {
            if label.len() > 80 {
                break;
            }
            label.push(' ');
            label.push_str(a);
        }
        if let Some(code) = direct_exec(program, rest, &[], &o, &label) {
            return code;
        }
        if let Some(b) = builtin::try_exec(program, rest, o.cwd.as_deref(), &[]) {
            return emit(b.into(), &o, &label);
        }
        match exec::build_command_at(program, rest, o.cwd.as_deref()) {
            Ok(c) => (c, label),
            Err(e) => {
                eprintln!("qx: {e}");
                return 127;
            }
        }
    };

    exec::apply_cwd(&mut cmd, &o.cwd);
    execute(vec![cmd], &o, &label)
}

/// `npm run/test/start` and local-bin `npx` without npm's ~100 ms CLI startup.
/// Strictly falls back (None) to the real tool.
fn direct_exec(program: &str, args: &[String], envs: &[(String, String)], o: &RunOpts, label: &str) -> Option<i32> {
    if exec::sets_npm_config(envs) {
        return None;
    }
    let mut cmds = npm::direct(program, args, o.cwd.as_deref())?;
    for c in &mut cmds {
        exec::apply_envs(c, envs);
    }
    Some(execute(cmds, o, label))
}

/// Run commands in order, stopping at the first failure (npm pre/main/post).
fn execute(cmds: Vec<Command>, o: &RunOpts, label: &str) -> i32 {
    if o.stream {
        let start = Instant::now();
        let mut code = 0;
        for mut cmd in cmds {
            code = match exec::run_stream(&mut cmd, o.timeout) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("qx: {e}");
                    return 127;
                }
            };
            if code != 0 {
                break;
            }
        }
        if let (124, Some(t)) = (code, o.timeout) {
            eprintln!("[qx] timed out after {}s", t.as_secs());
        }
        if std::env::var("QX_TIME").ok().as_deref() == Some("1") {
            eprintln!("[qx] {} ms, exit {}", start.elapsed().as_millis(), code);
        }
        return code;
    }

    match exec::run_chain(cmds, o.timeout) {
        Ok(out) => emit(out, o, label),
        Err(e) => {
            eprintln!("qx: {e}");
            127
        }
    }
}

/// `head` closing the pipe must not panic qx (`print!` does): the write error is the reader's choice.
fn write_out(so: &str, se: &str) {
    if !so.is_empty() {
        let mut out = std::io::stdout();
        let _ = out.write_all(so.as_bytes()).and_then(|()| out.flush());
    }
    if !se.is_empty() {
        let mut err = std::io::stderr();
        let _ = err.write_all(se.as_bytes()).and_then(|()| err.flush());
    }
}

/// Redact on an ANSI-free copy: a colour code between a key and its `=` would hide the secret.
/// With no secret found the original bytes (colour included) are kept.
fn redact_text(s: String) -> (String, usize) {
    if !s.contains('\x1b') {
        return redact::redact(&s);
    }
    let (r, n) = redact::redact(&compact::strip_ansi(&s));
    if n > 0 { (r, n) } else { (s, 0) }
}

/// Print a captured result through the compaction pipeline.
/// Built-ins also land here, with their measured elapsed time.
fn emit(out: exec::Outcome, o: &RunOpts, label: &str) -> i32 {
    let bytes_in = out.stdout.len() + out.stderr.len();
    let mut so = out.stdout;
    let mut se = out.stderr;

    // Secrets never reach the model in captured output. A human at a TTY sees
    // raw output unless QX_REDACT=1; QX_REDACT=off disables it entirely.
    let setting = std::env::var("QX_REDACT").ok();
    let setting = setting.as_deref();
    let do_redact = setting != Some("off") && (setting == Some("1") || !o.stream);
    let mut redactions = 0usize;
    if do_redact {
        let (a, n) = redact_text(so);
        so = a;
        redactions += n;
        let (b, n) = redact_text(se);
        se = b;
        redactions += n;
    }

    if o.stream {
        write_out(&so, &se);
        if redactions > 0 {
            eprintln!("[qx] redacted {redactions} secret(s)");
        }
        return out.code;
    }

    // Compaction or a digest that dropped content leaves the full text on disk, as the MCP path does,
    // so a slow or non-repeatable command never has to be re-run to see what was cut.
    let mut spool = None;
    if o.compact {
        if so.contains('\x1b') {
            so = compact::strip_ansi(&so);
        }
        if se.contains('\x1b') {
            se = compact::strip_ansi(&se);
        }
        let (kept_so, kept_se, lossy, profile) = match digest::digest(label, &so, &se, out.code) {
            Some(d) => {
                let lossy = d.lossy();
                (d.stdout, d.stderr, lossy, d.profile)
            }
            // Claude Code shows this much of a Bash result itself: cutting below it only hides text.
            None if claude_shows_whole(so.len() + se.len()) => (std::mem::take(&mut so), std::mem::take(&mut se), false, ""),
            None => {
                let (a, cut) = compact::compact(&so, o.max_lines, o.head, o.tail);
                let (b, cut_err) = compact::compact(&se, o.max_lines.min(120), o.head.min(50), o.tail.min(40));
                (a, b, cut | cut_err, "")
            }
        };
        if lossy {
            spool = compact::spool(&so, &se);
        }
        so = kept_so;
        se = kept_se;
        if matches!(profile, "build" | "test" | "traceback") {
            compact::relative_to_cwd(&mut so, &mut se, o.cwd.as_deref());
        }
    }

    let bytes_out = so.len() + se.len();
    write_out(&so, &se);
    if let Some(p) = &spool {
        eprintln!("[qx] full output: {}", p.display());
    }
    if out.timed_out {
        eprintln!(
            "[qx] timed out after {}s",
            o.timeout.unwrap_or_default().as_secs()
        );
    }
    if redactions > 0 {
        eprintln!("[qx] redacted {redactions} secret(s)");
    }

    record_stat(label, out.elapsed.as_millis() as u64, out.code, bytes_in, bytes_out);

    if std::env::var("QX_TIME").ok().as_deref() == Some("1") {
        eprintln!("[qx] {} ms, exit {}", out.elapsed.as_millis(), out.code);
    }
    out.code
}

/// Inside Claude Code's shell (`CLAUDECODE` set), whether `bytes` of output fit the Bash tool's
/// own limit (`BASH_MAX_OUTPUT_LENGTH` characters, 30000 by default). Bytes never undercount.
fn claude_shows_whole(bytes: usize) -> bool {
    std::env::var_os("CLAUDECODE").is_some()
        && bytes <= std::env::var("BASH_MAX_OUTPUT_LENGTH").ok().and_then(|v| v.parse().ok()).unwrap_or(30_000)
}

fn stats_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("qx").join("stats.jsonl"))
}

fn record_stat(label: &str, ms: u64, code: i32, bytes_in: usize, bytes_out: usize) {
    // Opt-in: the whole point of qx is to avoid per-command overhead.
    if std::env::var("QX_STATS").map(|v| v != "1").unwrap_or(true) {
        return;
    }
    let Some(path) = stats_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let line = serde_json::json!({
        "ts": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "cmd": label.chars().take(80).collect::<String>(),
        "ms": ms,
        "code": code,
        "in": bytes_in,
        "out": bytes_out,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

fn gain() -> i32 {
    let Some(path) = stats_path() else {
        println!("stats path unavailable");
        return 0;
    };
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => {
            println!("no stats yet — run commands with QX_STATS=1, then `qx gain`");
            return 0;
        }
    };

    let mut calls = 0u64;
    let mut ms = 0u128;
    let mut bin = 0u64;
    let mut bout = 0u64;
    let mut per: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

    for line in data.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        calls += 1;
        ms += v.get("ms").and_then(|x| x.as_u64()).unwrap_or(0) as u128;
        bin += v.get("in").and_then(|x| x.as_u64()).unwrap_or(0);
        bout += v.get("out").and_then(|x| x.as_u64()).unwrap_or(0);
        if let Some(c) = v.get("cmd").and_then(|x| x.as_str()) {
            let head = c.split_whitespace().next().unwrap_or("?").to_string();
            *per.entry(head).or_insert(0) += 1;
        }
    }

    let saved = bin.saturating_sub(bout);
    let pct = (saved * 100).checked_div(bin).unwrap_or(0);
    println!("qx gain");
    println!("  calls      : {calls}");
    println!("  exec time  : {:.1}s", ms as f64 / 1000.0);
    println!("  output     : {} KiB -> {} KiB ({}% saved)", bin / 1024, bout / 1024, pct);
    if !per.is_empty() {
        let mut rows: Vec<_> = per.into_iter().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1));
        println!("  top:");
        for (name, n) in rows.into_iter().take(8) {
            println!("    {n:>5}  {name}");
        }
    }
    0
}

fn check_cmd(rest: &[String]) -> i32 {
    let json_out = rest.iter().any(|a| a == "--json");
    let rest: Vec<String> = rest
        .iter()
        .filter(|a| a.as_str() != "--json")
        .cloned()
        .collect();
    if rest.is_empty() {
        eprintln!("usage: qx check [--json] <program> [args...]");
        eprintln!("       qx check [--json] \"<full command line>\"");
        return 2;
    }
    let cwd = std::env::current_dir().ok();
    let decision = if rest.len() == 1 {
        guard::check_shell(&rest[0], cwd.as_deref())
    } else {
        guard::check(&rest[0], &rest[1..], cwd.as_deref())
    };
    let label = match decision.level {
        guard::Level::Allow => "ALLOW",
        guard::Level::Warn => "WARN",
        guard::Level::Deny => "DENY",
    };
    if json_out {
        let verdict = label.to_ascii_lowercase();
        let value = serde_json::json!({
            "verdict": verdict,
            "reasons": decision.reasons,
            "mode": match guard::mode() {
                guard::Mode::Safe => "safe",
                guard::Mode::Strict => "strict",
                guard::Mode::Off => "off",
            }
        });
        println!("{value}");
    } else if decision.reasons.is_empty() {
        println!("{label}: ok");
    } else {
        println!("{label}: {}", decision.summary());
    }
    match decision.level {
        guard::Level::Allow => 0,
        guard::Level::Warn => 1,
        guard::Level::Deny => 2,
    }
}

fn audit_cmd(rest: &[String]) -> i32 {
    let n = rest
        .first()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(20)
        .max(1);
    let Some(base) = std::env::var_os("LOCALAPPDATA") else {
        return 0;
    };
    let path = PathBuf::from(base).join("qx").join("audit.jsonl");
    let Ok(data) = std::fs::read_to_string(path) else {
        println!("no audit log yet (qx writes one for every warning or block)");
        return 0;
    };
    let lines: Vec<&str> = data.lines().collect();
    for line in lines.iter().rev().take(n).collect::<Vec<_>>().into_iter().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ts = v.get("ts").and_then(|x| x.as_u64()).unwrap_or(0);
        let verdict = v.get("verdict").and_then(|x| x.as_str()).unwrap_or("?");
        let cmd = v.get("cmd").and_then(|x| x.as_str()).unwrap_or("");
        let reasons = v
            .get("reasons")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default();
        let suffix = if reasons.is_empty() {
            String::new()
        } else {
            format!("  [{reasons}]")
        };
        println!("{ts} {verdict:<5} {cmd}{suffix}");
    }
    0
}

fn which(names: &[String]) -> i32 {
    if names.is_empty() {
        eprintln!("usage: qx which <command>...");
        return 2;
    }
    for name in names {
        if builtin::is_builtin(name) {
            println!("{name} -> qx builtin (in-process, no spawn)");
            continue;
        }
        match resolve::resolve(name, None) {
            resolve::Target::Direct { exe, prefix } => {
                let venv = python::venv_redirect(&exe)
                    .map(|v| format!(" [venv → {}]", v.base.display()))
                    .unwrap_or_default();
                if prefix.is_empty() {
                    println!("{name} -> {}{venv}", exe.display());
                } else {
                    println!("{name} -> {} {}{venv}", exe.display(), prefix.join(" "));
                }
            }
            resolve::Target::Shim(p) => {
                println!("{name} -> {} [shim: needs cmd.exe]", p.display());
            }
            resolve::Target::Ps1(p) => {
                println!("{name} -> {} [PowerShell script]", p.display());
            }
            resolve::Target::Missing => println!("{name} -> not found"),
        }
    }
    0
}

fn print_help() {
    println!(
        r#"qx {VERSION} — fast local command executor

USAGE
  qx [flags] <program> [args...]     direct exec, shims bypassed
  qx [flags] -c "<command line>"     shell string (auto: powershell, Git Bash, or cmd)
  qx mcp                             MCP stdio server (tools: run, fs)
  qx doctor                          diagnose why commands are slow
  qx deploy [--to PATH] [--dry-run]  install this build as the canonical qx.exe
  qx setup [all|CLIENT...] [--dry-run] [--remove]
                                     register qx in each agent CLI's config
  qx hook claude                     Claude Code PreToolUse hook (`qx setup` registers it)
  qx which <command>...              show the real binary that will run
  qx gain                            output savings (needs QX_STATS=1)
  qx check <program|command>         dry-run the safety gate
  qx audit [N]                       review recent warnings/blocks

FLAGS
  --stream            live output, no capture (default when stdout is a TTY)
  --capture           buffer output (default when piped)
  --raw               no compaction
  --max-lines N       compaction threshold (default 200)
  --head N            lines kept at the start (default 80)
  --tail N            lines kept at the end (default 60)
  --timeout SECS      kill after N seconds
  --cwd DIR           working directory
  --shell cmd|bash|powershell    shell for -c (default auto: cmdlet or $variable
                                 syntax -> powershell, a POSIX utility in a pipeline
                                 -> Git Bash, else cmd)

SAFETY GATE
  QX_GUARD=safe|strict|off   default safe (deny catastrophic, warn risky)
  QX_REDACT=on|off           secret redaction (default on for captured output)
  QX_ALLOW_ROOTS=C:\a;C:\b   extra roots where deletion is normal
  QX_ALLOW_FORCE_PUSH=1      permit git push --force
  QX_AUDIT=1                 log every command, not only warnings/blocks
  QX_NPM_FAST=off            disable the npm run/test/start fast path
  QX_NODE_CACHE=off          disable Node's compile cache
  QX_PYTHON_FAST=1|off       default skips startup hooks keeping system certs; 1 drops certs (fastest), off disables
  QX_PS_PROFILE=1            keep the PowerShell profile (qx adds -NoProfile by default)
  QX_MCP_WORKERS=N           parallel MCP tool calls (default 4)
  QX_FS_THREADS=N            fs grep scan threads (default CPUs/2, max 8)
  QX_TRACE=1                 print phase timings and the -c split to stderr

BUILT-INS (no process spawn)
  pwd echo true false sleep clear cls
  ls dir cat type head tail wc grep
  mkdir rm del cp copy mv move touch
  which where basename dirname realpath
  whoami hostname seq
  grep takes literal patterns, -i -n -l -r -v -q -c -A/-B/-C -F -e PAT; else the real grep.
  head -c N / tail -c N read one byte window when it is text; binary runs the real tool.
  Unknown flags fall back to the real executable.

EXAMPLES
  qx git status
  qx cargo test --quiet
  qx npm run build
  qx npx tsc --noEmit
  qx -c "npm run build 2>&1 | tail -30"
  qx mcp
"#
    );
}
