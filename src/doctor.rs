//! Environment diagnostics: why is a command slow on this machine?

use crate::exec;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const AV_DIRS: &[(&str, &str)] = &[
    ("Avast", r"C:\Program Files\Avast Software"),
    ("Malwarebytes", r"C:\Program Files\Malwarebytes"),
    ("Bitdefender agent", r"C:\Program Files\Bitdefender Agent"),
    ("Kaspersky", r"C:\Program Files (x86)\Kaspersky Lab"),
    ("ESET", r"C:\Program Files\ESET"),
    ("Sophos", r"C:\Program Files\Sophos"),
    ("CrowdStrike", r"C:\Program Files\CrowdStrike"),
    ("SentinelOne", r"C:\Program Files\SentinelOne"),
    ("McAfee", r"C:\Program Files\McAfee"),
    ("Norton", r"C:\Program Files\Norton"),
];

pub fn run() -> i32 {
    println!("qx {} — environment check\n", env!("CARGO_PKG_VERSION"));

    // 1. Spawn latency: the single best indicator of machine health.
    let mut samples = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/d", "/c", "exit 0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::exec::prepare(&mut cmd);
        let _ = cmd.status();
        samples.push(t.elapsed());
    }
    let avg = samples.iter().sum::<Duration>() / samples.len() as u32;
    let ms = avg.as_millis();

    let (tag, verdict) = if ms < 150 {
        ("OK", "process creation is healthy")
    } else if ms < 600 {
        ("SLOW", "process creation is slow; commands will feel laggy")
    } else {
        (
            "CRITICAL",
            "process creation is severely degraded (normal is 20-80 ms)",
        )
    };
    println!("[{tag}] cold spawn (cmd /d /c exit): {} ms avg", ms);
    println!("       {verdict}");

    // 2. Real-time scanners.
    let mut avs: Vec<&str> = Vec::new();
    for (name, dir) in AV_DIRS {
        if Path::new(dir).exists() {
            avs.push(name);
        }
    }
    if !avs.is_empty() {
        println!("\n[!] antivirus installs detected: {}", avs.join(", "));
        println!("    Windows Defender is installed on every Windows machine.");
        println!("    More than one real-time scanner multiplies process-creation cost.");
    } else {
        println!("\n[ok] no third-party antivirus directories found");
    }

    // 3. Shells. WSL bash + login profiles are the classic one-minute command.
    println!("\n[shells]");
    match crate::resolve::git_bash() {
        Some(p) => println!("  git bash : {}  (use this, non-login)", p.display()),
        None => println!("  git bash : not found"),
    }
    let wsl = Path::new(r"C:\Windows\System32\bash.exe");
    if wsl.exists() {
        println!("  wsl bash : present — avoid `bash -l`/`bash -lc` (profile startup)");
    }
    if let Some(n) = crate::resolve::find_on_path("node") {
        println!("  node     : {}", n.display());
    }

    // 4. Shim bypass status.
    println!("\n[resolution]");
    for name in ["git", "npm", "npx", "pnpm", "yarn"] {
        match crate::resolve::resolve(name) {
            crate::resolve::Target::Direct { exe, prefix } => {
                let extra = if prefix.is_empty() {
                    String::new()
                } else {
                    format!(" + {}", prefix.join(" "))
                };
                println!("  {name:<5} -> {}{}", exe.display(), extra);
            }
            crate::resolve::Target::Shim(p) => {
                println!("  {name:<5} -> {} [shim, still needs cmd.exe]", p.display())
            }
            crate::resolve::Target::Ps1(p) => {
                println!("  {name:<5} -> {} [PowerShell script]", p.display())
            }
            crate::resolve::Target::Missing => println!("  {name:<5} -> not found"),
        }
    }

    println!(
        "\n[builtins] 31 commands run in-process (ls, cat, head, tail, wc, mkdir, rm, cp, mv, touch, realpath, whoami, hostname, seq, ...)"
    );
    let guard_mode = match std::env::var("QX_GUARD").ok().as_deref() {
        Some("off") => "off",
        Some("strict") => "strict",
        _ => "safe",
    };
    println!("[guard] {guard_mode} — deny catastrophic, warn risky, audit to %LOCALAPPDATA%\\qx\\audit.jsonl");
    println!("[redact] on for captured/MCP output (QX_REDACT=off disables)");

    // 5. Self test.
    let t = Instant::now();
    if let Ok(cmd) = exec::build_command("git", &["--version".to_string()]) {
        let mut c = cmd;
        let _ = c.stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    println!("\n[qx] git --version via direct exec: {} ms", t.elapsed().as_millis());

    if ms >= 600 {
        println!("\nWhat to do:");
        println!("  1. Keep ONE real-time antivirus. Remove or fully disable the others.");
        println!("  2. Add folder exclusions for your code dirs, ~/.cargo, and node_modules.");
        println!("  3. Never use `bash -lc` — the login profile is what turns 7 s into 60 s.");
        println!("  4. Route commands through `qx` or the `qx mcp` server, not a shell wrapper.");
    }
    0
}
