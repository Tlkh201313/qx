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
mod index;
mod mcp;
mod npm;
mod python;
mod redact;
mod resolve;
mod runmem;
mod setup;
mod stats;
#[cfg_attr(test, allow(dead_code))] // `mcp::serve` starts it outside tests only
mod warmup;

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
    /// --max-lines, --head or --tail was given.
    limited: bool,
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
            limited: false,
            timeout: None,
            cwd: None,
            shell: "auto".to_string(),
        }
    }
}

fn main() {
    exec::seal_std_handles();
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
        "gain" => gain(&args[1..]),
        "mcp" | "serve" => mcp::serve(),
        "which" => which(&args[1..]),
        "check" | "policy" => check_cmd(&args[1..]),
        "audit" => audit_cmd(&args[1..]),
        "proxy" => run_cli(&args[1..], true),
        _ => run_cli(args, false),
    }
}

/// Whether output streams live to the terminal: an explicit --stream/--capture decides; otherwise
/// a terminal streams, unless QX_REDACT=1 asks for redaction, which only captured output can get.
fn streams(tty: bool, explicit: Option<bool>, redact: Option<&str>) -> bool {
    explicit.unwrap_or(tty && !matches!(redact, Some("1" | "on")))
}

/// The value after a flag. A missing value, or another `--flag` in its place, is an error: `qx -c --raw`
/// must not run `--raw` as the command, and `--timeout --raw` must not swallow the flag.
fn flag_value<'a>(flag: &str, v: Option<&'a String>, what: &str) -> Result<&'a String, String> {
    v.filter(|v| !v.starts_with("--")).ok_or_else(|| format!("{flag} needs {what}"))
}

/// The number after a flag. `--timeout` (`secs`) also takes `30s`, `5m` and `1h`. A missing or bad value
/// is an error: a silent default would turn `--timeout 5m` into 120 s.
fn flag_num(flag: &str, v: Option<&String>, secs: bool) -> Result<u64, String> {
    let what = if secs { "a number of seconds (or 30s, 5m, 1h)" } else { "a whole number" };
    let v = flag_value(flag, v, what)?;
    let (num, scale) = match v.char_indices().last() {
        Some((i, 's')) if secs => (&v[..i], 1),
        Some((i, 'm')) if secs => (&v[..i], 60),
        Some((i, 'h')) if secs => (&v[..i], 3600),
        _ => (v.as_str(), 1),
    };
    num.parse::<u64>().ok().and_then(|n| n.checked_mul(scale)).ok_or_else(|| format!("{flag} needs {what} (got '{v}')"))
}

/// Seconds left until `deadline` (unix seconds): what a stage of a chained line may still use.
fn secs_until(deadline: u64) -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    deadline.saturating_sub(now)
}

/// The guard's verdict on a CLI command. For `-c` the `NAME=value` words in front of the line are
/// judged too, as the MCP path does: their values reach the child unseen by the line check.
fn cli_decision(shell_cmd: Option<&str>, shell: &str, rest: &[String], cwd: Option<&std::path::Path>) -> guard::Decision {
    let Some(sh) = shell_cmd else {
        return match rest.split_first() {
            Some((prog, args)) => guard::check(prog, args, cwd),
            None => guard::Decision::allow(),
        };
    };
    // Under PowerShell a simple-looking line still means PowerShell (`rd a,b` is Remove-Item on a list).
    let simple = if exec::shell_for(sh, shell) == "powershell" { None } else { exec::split_simple(sh) };
    let mut decision = if let Some((prog, sargs)) = simple {
        guard::check(&prog, &sargs, cwd)
    } else {
        guard::check_shell(sh, shell, cwd)
    };
    guard::merge(&mut decision, guard::check_env(&exec::peel_env(sh).0));
    decision
}

fn run_cli(args: &[String], force_raw: bool) -> i32 {
    let mut o = RunOpts::default();
    // `echo data | qx grep x`: the command reads qx's stdin. At a terminal it already does.
    // Not in tests: they share one process, and a test run's stdin is never a terminal, so this
    // would switch off the parked PowerShell (which refuses a call that reads stdin) for every test.
    #[cfg(not(test))]
    exec::INHERIT_STDIN.store(!std::io::stdin().is_terminal(), std::sync::atomic::Ordering::Relaxed);
    let tty = o.stream;
    let mut explicit_stream: Option<bool> = None;
    if force_raw {
        explicit_stream = Some(false);
        o.compact = false;
    }

    let mut i = 0usize;
    let mut shell_cmd: Option<String> = None;
    let mut deadline: Option<u64> = None;

    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            i += 1;
            break;
        }
        match a {
            "--stream" => {
                explicit_stream = Some(true);
                i += 1;
            }
            "--capture" => {
                explicit_stream = Some(false);
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
            "--max-lines" | "--head" | "--tail" | "--timeout" | "--deadline" => {
                i += 1;
                let n = match flag_num(a, args.get(i), a == "--timeout") {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("qx: {e}");
                        return 2;
                    }
                };
                match a {
                    "--timeout" => o.timeout = Some(Duration::from_secs(n)),
                    // The hook gives every stage of a chained line the same end time (unix seconds).
                    "--deadline" => deadline = Some(n),
                    _ => {
                        let n = usize::try_from(n).unwrap_or(usize::MAX);
                        match a {
                            "--max-lines" => o.max_lines = n,
                            "--head" => o.head = n,
                            _ => o.tail = n,
                        }
                        o.limited = true;
                    }
                }
                i += 1;
            }
            "--cwd" | "--shell" | "-c" | "--command" => {
                i += 1;
                let v = match flag_value(a, args.get(i), "a value") {
                    Ok(v) => v.clone(),
                    Err(e) => {
                        eprintln!("qx: {e}");
                        return 2;
                    }
                };
                match a {
                    "--cwd" => o.cwd = Some(std::path::absolute(&v).unwrap_or_else(|_| PathBuf::from(&v))),
                    "--shell" => o.shell = v,
                    // Flags may follow the command line (`qx -c "..." --cwd D`); they still apply.
                    _ => shell_cmd = Some(v),
                }
                i += 1;
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

    if let Some(d) = deadline {
        let left = Duration::from_secs(secs_until(d));
        o.timeout = Some(o.timeout.map_or(left, |t| t.min(left)));
    }
    o.stream = streams(tty, explicit_stream, std::env::var("QX_REDACT").ok().as_deref());
    if shell_cmd.is_some() && i < args.len() {
        eprintln!("qx: unexpected arguments after -c: {} (put them inside the quoted command)", args[i..].join(" "));
        return 2;
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
    let decision = cli_decision(shell_cmd.as_deref(), &o.shell, args.get(i..).unwrap_or(&[]), o.cwd.as_deref());
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
        let (mut envs, run) = exec::peel_env(&sh);
        if !o.compact {
            // --raw promises the untouched text: built-ins must not sample their own output either.
            envs.push(("QX_LS_FULL".into(), "1".into()));
        }
        let run = run.to_string();
        // A quoted command with no shell syntax does not need a shell at all.
        if let Some((prog, sargs)) = exec::split_simple(&run) {
            if std::env::var_os("QX_TRACE").is_some() {
                eprintln!("[trace] split: {prog:?} {sargs:?}");
            }
            if let Some(out) = run_builtin(&prog, &sargs, &envs, &o) {
                return emit(out, &o, &sh);
            }
            if let Some(code) = direct_exec(&prog, &sargs, &envs, &o, &sh) {
                return code;
            }
            if let Ok(mut c) = exec::build_command_env(&prog, &sargs, o.cwd.as_deref(), &envs) {
                exec::apply_envs(&mut c, &envs);
                exec::apply_cwd(&mut c, &o.cwd);
                return execute(vec![c], &o, &sh);
            }
        }
        match exec::build_shell_in(&run, &o.shell, o.cwd.as_deref()) {
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
        // The same label the MCP server builds: every flag counts (`-- --nocapture` keeps test output).
        let label = mcp::run_label(Some(program), rest, None);
        if let Some(code) = direct_exec(program, rest, &[], &o, &label) {
            return code;
        }
        let full_env: Vec<(String, String)> = if o.compact { Vec::new() } else { vec![("QX_LS_FULL".into(), "1".into())] };
        if let Some(out) = run_builtin(program, rest, &full_env, &o) {
            return emit(out, &o, &label);
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

/// A built-in runs in this process, so `--timeout` is its deadline too (a child is killed by it).
fn run_builtin(program: &str, args: &[String], envs: &[(String, String)], o: &RunOpts) -> Option<exec::Outcome> {
    exec::set_deadline(o.timeout.map(|t| Instant::now() + t));
    let b = builtin::try_exec(program, args, o.cwd.as_deref(), envs);
    let stopped = exec::should_stop();
    exec::set_deadline(None);
    let mut out: exec::Outcome = b?.into();
    // A built-in that finished its work right at the deadline did the work: it is reported done.
    if stopped && out.code != 0 {
        out.timed_out = true;
        out.code = 124;
    }
    Some(out)
}

/// `npm run/test/start` and local-bin `npx` without npm's ~100 ms CLI startup.
/// Strictly falls back (None) to the real tool.
fn direct_exec(program: &str, args: &[String], envs: &[(String, String)], o: &RunOpts, label: &str) -> Option<i32> {
    if exec::needs_real_npm(envs) {
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
    let do_redact = setting != Some("off") && (matches!(setting, Some("1" | "on")) || !o.stream);
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
    let mut was_lossy = false;
    if o.compact {
        if so.contains('\x1b') {
            so = compact::strip_ansi(&so);
        }
        if se.contains('\x1b') {
            se = compact::strip_ansi(&se);
        }
        digest::drop_git_eol_warnings(label, &mut se);
        let (kept_so, kept_se, lossy, profile) = match digest::digest(label, &so, &se, out.code) {
            Some(d) => {
                let lossy = d.lossy();
                (d.stdout, d.stderr, lossy, d.profile)
            }
            // Claude Code shows this much of a Bash result itself: cutting below it only hides text,
            // unless the caller asked for a cut with --max-lines, --head or --tail.
            None if !o.limited && claude_shows_whole(so.len() + se.len()) => (std::mem::take(&mut so), std::mem::take(&mut se), false, ""),
            None => {
                let (max, head, tail) = if o.limited { (o.max_lines, o.head, o.tail) } else { digest::plain_budget(label, (o.max_lines, o.head, o.tail)) };
                let (a, cut) = compact::compact(&so, max, head, tail);
                let (b, cut_err) = compact::compact(&se, o.max_lines.min(120), o.head.min(50), o.tail.min(40));
                (a, b, cut | cut_err, "")
            }
        };
        if lossy {
            was_lossy = true;
            spool = compact::spool(&so, &se);
        }
        so = kept_so;
        se = kept_se;
        digest::shorten_paths(profile, label, &mut so, &mut se, o.cwd.as_deref());
    }

    let bytes_out = so.len() + se.len();
    write_out(&so, &se);
    if let Some(p) = &spool {
        eprintln!("[qx] full output: {}", p.display());
    } else if was_lossy {
        eprintln!("[qx] output was cut and could not be saved; re-run with --raw for all of it");
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

/// One CLI run into the ledger. Only the command's first word is kept, never its arguments.
fn record_stat(label: &str, ms: u64, code: i32, bytes_in: usize, bytes_out: usize) {
    stats::record_now(stats::Entry {
        tool: "cli",
        key: stats::first_word(label),
        ms,
        code,
        raw: bytes_in,
        reply: bytes_out,
        flags: "",
    });
}

/// `qx gain [--since 7d]`: what the ledger holds, per tool and command.
fn gain(rest: &[String]) -> i32 {
    let since = match rest.iter().position(|a| a == "--since") {
        Some(i) => match rest.get(i + 1).and_then(|s| stats::parse_since(s)) {
            Some(s) => s,
            None => {
                eprintln!("usage: qx gain [--since 7d|12h|30m]");
                return 2;
            }
        },
        None => 0,
    };
    stats::report(since)
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
        guard::check_shell(&rest[0], "auto", cwd.as_deref())
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
  qx gain [--since 7d]               calls, timings and output savings per tool (QX_STATS=off stops recording)
  qx check <program|command>         dry-run the safety gate
  qx audit [N]                       review recent warnings/blocks

FLAGS
  --stream            live output, no capture (default when stdout is a TTY)
  --capture           buffer output (default when piped)
  --raw               no compaction
  --max-lines N       compaction threshold (default 200)
  --head N            lines kept at the start (default 80)
  --tail N            lines kept at the end (default 60)
  --timeout SECS      kill after N seconds (30, 30s, 5m, 1h)
  --deadline EPOCH    kill at this unix time (the hook shares one end time across a chained line)
  --cwd DIR           working directory
  --shell cmd|bash|powershell    shell for -c (default auto: cmdlet or $variable
                                 syntax -> powershell, a POSIX utility in a pipeline
                                 -> Git Bash, else cmd)

SAFETY GATE
  QX_GUARD=safe|strict|off   default safe (deny catastrophic, warn risky)
  QX_REDACT=1|off            1: redact at a terminal too (output is captured); off: never
  QX_ALLOW_ROOTS=C:\a;C:\b   extra roots where deletion is normal
  QX_ALLOW_FORCE_PUSH=1      permit git push --force
  QX_AUDIT=1                 log every command, not only warnings/blocks
  QX_NPM_FAST=off            disable the npm run/test/start fast path
  QX_NODE_CACHE=off          disable Node's compile cache
  QX_PYTHON_FAST=1|off       default skips startup hooks keeping system certs; 1 drops certs (fastest), off disables
  QX_PS_PROFILE=1            keep the PowerShell profile (qx adds -NoProfile by default)
  QX_MCP_WORKERS=N           parallel MCP processes (default 8, max 32)
  QX_MCP_QUEUE=N             calls allowed to wait for a slot (default 64, max 1024)
  QX_WARM=off                no startup warm-up; QX_PS_WARM=off: no parked PowerShell
  QX_INDEX=off               no workspace file index for fs glob/grep
  QX_FS_THREADS=N            fs grep scan threads (default CPUs/2 up to 8; max 32)
  QX_TRACE=1                 print phase timings and the -c split to stderr

BUILT-INS (no process spawn)
  pwd echo true false sleep clear cls
  ls dir cat type head tail wc grep sed
  mkdir rm del erase cp copy mv move touch
  which basename dirname realpath
  whoami hostname seq
  start-sleep get-content gc test-path write-output write-host (PowerShell spellings)
  grep takes literal patterns, -i -n -l -r -v -q -c -A/-B/-C -F -e PAT; else the real grep.
  sed answers sed -n 'A,Bp' line ranges; `where` is always the real where.exe.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn flags_after_dash_c_are_honoured_and_strays_refused() {
        // `--cwd` after `-c` used to be dropped, so the command ran somewhere else.
        let missing = std::env::temp_dir().join("qx-no-such-dir-for-cli-test");
        assert_eq!(run_cli(&s(&["--capture", "-c", "exit 3", "--cwd", &missing.to_string_lossy()]), false), 2);
        assert_eq!(run_cli(&s(&["--capture", "-c", "exit 0", "stray"]), false), 2);
        assert_eq!(run_cli(&s(&["--capture", "-c", "exit 3"]), false), 3);
    }

    #[test]
    fn a_string_flag_without_its_value_is_an_error() {
        // `qx -c --raw` used to run `--raw` as the command.
        for bad in [&["-c", "--raw"][..], &["-c"], &["--shell", "--capture", "-c", "exit 0"], &["--cwd", "--raw", "-c", "exit 0"]] {
            let mut a = s(&["--capture"]);
            a.extend(s(bad));
            assert_eq!(run_cli(&a, false), 2, "{bad:?}");
        }
    }

    #[test]
    fn a_dash_c_env_prefix_is_judged_by_the_guard() {
        // `GIT_EXTERNAL_DIFF` makes an allowed `git diff` run any program; the line check alone never sees it.
        let d = cli_decision(Some("GIT_EXTERNAL_DIFF=x git diff"), "auto", &[], None);
        assert!(d.level != guard::Level::Allow, "{}", d.summary());
    }

    #[test]
    fn a_simple_line_under_powershell_is_judged_as_powershell() {
        // `rd` is Remove-Item in PowerShell, which takes `a,b` as two paths.
        let d = cli_decision(Some(r"rd x,C:\Windows"), "powershell", &[], None);
        assert_eq!(d.level, guard::Level::Deny, "{}", d.summary());
    }

    #[test]
    fn a_bad_flag_number_is_an_error_not_a_default() {
        let n = |flag: &str, v: &str, secs| flag_num(flag, Some(&v.to_string()), secs);
        assert_eq!(n("--timeout", "30", true), Ok(30));
        assert_eq!(n("--timeout", "30s", true), Ok(30));
        assert_eq!(n("--timeout", "5m", true), Ok(300));
        assert_eq!(n("--timeout", "1h", true), Ok(3600));
        assert_eq!(n("--head", "20", false), Ok(20));
        for (flag, v, secs) in [("--timeout", "0.5", true), ("--timeout", "abc", true), ("--head", "5m", false), ("--head", "-1", false)] {
            let e = n(flag, v, secs).unwrap_err();
            assert!(e.starts_with(flag) && e.contains(&format!("got '{v}'")), "{e}");
        }
        assert!(flag_num("--timeout", None, true).unwrap_err().contains("needs a number of seconds"));
        // `--timeout --raw cmd` must not swallow --raw as the number.
        assert!(flag_num("--timeout", Some(&"--raw".to_string()), true).is_err());
        // Through the command line: exit 2 before anything runs; the accepted forms still run.
        for bad in [&["--timeout", "0.5"][..], &["--timeout", "--raw"], &["--head"], &["--tail", "x"], &["--deadline", "soon"]] {
            let mut a = s(bad);
            a.extend(s(&["--capture", "-c", "exit 3"]));
            assert_eq!(run_cli(&a, false), 2, "{bad:?}");
        }
        assert_eq!(run_cli(&s(&["--capture", "--timeout", "5m", "-c", "exit 3"]), false), 3);
    }

    #[test]
    fn a_deadline_cuts_a_command_short_and_a_past_one_stops_it_at_once() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let t = std::time::Instant::now();
        let code = run_cli(&s(&["--capture", "--deadline", &(now + 2).to_string(), "ping", "-n", "9", "127.0.0.1"]), false);
        assert_eq!(code, 124);
        assert!(t.elapsed().as_secs() < 6, "{:?}", t.elapsed());
        assert_eq!(secs_until(0), 0);
        assert_eq!(run_cli(&s(&["--capture", "--deadline", "1", "-c", "exit 0"]), false), 124);
        // Plenty of time left: the command runs as before.
        assert_eq!(run_cli(&s(&["--capture", "--deadline", &(now + 600).to_string(), "-c", "exit 3"]), false), 3);
    }

    #[test]
    fn a_builtin_run_from_the_command_line_stops_at_the_timeout_flag() {
        let d = std::env::temp_dir().join("qx-cli-builtin-timeout-test");
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("a")).unwrap();
        fs::write(d.join("a").join("f"), "x").unwrap();
        let cwd = d.to_string_lossy().to_string();
        assert_eq!(run_cli(&s(&["--capture", "--cwd", &cwd, "cp", "-r", "a", "kept"]), false), 0);
        assert!(d.join("kept").join("f").exists());
        // A timeout already over: the copy stops before it starts instead of running without a limit.
        assert_eq!(run_cli(&s(&["--capture", "--timeout", "0", "--cwd", &cwd, "cp", "-r", "a", "never"]), false), 124);
        assert!(!d.join("never").join("f").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn an_explicit_redact_request_captures_instead_of_streaming() {
        // At a terminal, QX_REDACT=1 can only be honoured on captured output.
        assert!(!streams(true, None, Some("1")));
        assert!(!streams(true, None, Some("on")));
        assert!(streams(true, None, None));
        assert!(streams(true, None, Some("off")));
        assert!(streams(true, Some(true), Some("1")), "an explicit --stream wins");
        assert!(!streams(false, None, None));
    }
}