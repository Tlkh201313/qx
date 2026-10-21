//! Environment diagnostics: why is a command slow on this machine?

use crate::exec;
use std::path::{Path, PathBuf};
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
    // The first spawn is cold (loader, AV scan); the median is the steady state.
    samples.sort();
    let ms = samples[samples.len() / 2].as_millis();

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
    println!("[{tag}] cold spawn (cmd /d /c exit): {} ms median", ms);
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
        match crate::resolve::resolve(name, None) {
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
        "\n[builtins] {} commands run in-process (ls, cat, head, tail, wc, grep, mkdir, rm, cp, mv, touch, realpath, whoami, hostname, seq, ...)",
        crate::builtin::NAMES.len()
    );
    let guard_mode = match std::env::var("QX_GUARD").ok().as_deref() {
        Some("off") => "off",
        Some("strict") => "strict",
        _ => "safe",
    };
    println!("[guard] {guard_mode} — deny catastrophic, warn risky, audit to %LOCALAPPDATA%\\qx\\audit.jsonl");
    println!("[redact] on for captured/MCP output (QX_REDACT=off disables)");

    // 5. Python startup. Site-packages `.pth` hooks run on every launch.
    python_startup();

    // 6. Self test.
    let t = Instant::now();
    if let Ok(cmd) = exec::build_command("git", &["--version".to_string()]) {
        let mut c = cmd;
        let _ = c.stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    println!("\n[qx] git --version via direct exec: {} ms", t.elapsed().as_millis());

    install_section();
    clients_section();

    if ms >= 600 {
        println!("\nWhat to do:");
        println!("  1. Keep ONE real-time antivirus. Remove or fully disable the others.");
        println!("  2. Add folder exclusions for your code dirs, ~/.cargo, and node_modules.");
        println!("  3. Never use `bash -lc` — the login profile is what turns 7 s into 60 s.");
        println!("  4. Route commands through `qx` or the `qx mcp` server, not a shell wrapper.");
    }
    0
}

/// Canonical binary plus the stale copies an old install style left behind.
fn install_section() {
    let paths = crate::setup::Paths::from_env();
    let Some(canonical) = crate::deploy::canonical() else {
        println!("\n[install] no cargo bin directory (set CARGO_HOME or USERPROFILE)");
        return;
    };
    let running = std::env::current_exe().ok();
    let state = match (&running, canonical.exists()) {
        (Some(exe), true) => match crate::deploy::same_bytes(exe, &canonical) {
            Ok(true) => "ok".to_string(),
            Ok(false) => "STALE (run `qx deploy`)".to_string(),
            Err(e) => format!("cannot compare: {e}"),
        },
        (Some(_), false) => "MISSING (run `qx deploy`)".to_string(),
        (None, _) => "unknown".to_string(),
    };
    println!("\n[install] canonical: {} [{state}]", canonical.display());
    if let Some(exe) = &running {
        if exe != &canonical {
            println!("          running from: {}", exe.display());
        }
    }
    let stale = stale_dsh_binaries(&paths);
    if !stale.is_empty() {
        println!(
            "          {} stale copy/copies in {} (one canonical qx.exe is enough)",
            stale.len(),
            paths.dsh_home.join("bin").display()
        );
    }
}

/// `dsh_home\bin\qx*.exe` files. dsh used to need a versioned copy per build; deploy makes
/// them unnecessary, and each stale copy is a different qx behavior under a client.
fn stale_dsh_binaries(p: &crate::setup::Paths) -> Vec<PathBuf> {
    let dir = p.dsh_home.join("bin");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            path.is_file() && name.starts_with("qx") && name.ends_with(".exe")
        })
        .collect()
}

/// One line per client qx can register: where the config is and which binary it points at.
fn clients_section() {
    let paths = crate::setup::Paths::from_env();
    let canonical = crate::deploy::canonical();
    let mut drift = 0;
    println!("\n[clients]");
    for client in crate::client::Client::ALL {
        let status = crate::setup::status(client, &paths);
        let file = status
            .file
            .as_ref()
            .map(|f| f.display().to_string())
            .unwrap_or_else(|| "-".to_string());
        let command = status.command.clone();
        let same = match (&command, &canonical) {
            (Some(c), Some(expected)) => Path::new(c) == expected.as_path(),
            (Some(_), None) => true,
            (None, _) => false,
        };
        let state = if command.is_none() {
            "not registered"
        } else if same {
            "ok"
        } else {
            drift += 1;
            "points at another binary"
        };
        println!("  {:<12} {:<11} {}", status.client.key(), state, file);
        for note in &status.notes {
            println!("               note: {note}");
        }
    }
    if drift > 0 {
        println!("  {drift} client(s) point at a binary that is not the canonical one: run `qx setup all`");
    }
}

struct ImportTime {
    indent: usize,
    name: String,
    cum: u64,
}

fn python_startup() {
    let args = vec![
        "-X".to_string(),
        "importtime".to_string(),
        "-c".to_string(),
        "pass".to_string(),
    ];
    let Ok(mut cmd) = exec::build_command("python", &args) else {
        println!("\n[python] not found");
        return;
    };
    // Measure the real startup cost even when QX_PYTHON_FAST=1 would skip the
    // hook being diagnosed.
    match std::env::var_os("PYTHONPATH") {
        Some(v) => cmd.env("PYTHONPATH", v),
        None => cmd.env_remove("PYTHONPATH"),
    };
    let out = match exec::run_captured(cmd, Some(Duration::from_secs(20))) {
        Ok(o) => o,
        Err(e) => {
            println!("\n[python] could not run importtime: {e}");
            return;
        }
    };
    let text = if out.stderr.contains("import time:") {
        &out.stderr
    } else {
        &out.stdout
    };
    let imports = parse_importtime(text);
    let Some(site) = imports.iter().find(|i| i.name == "site").map(|i| i.cum) else {
        println!("\n[python] no importtime data");
        return;
    };
    println!("\n[python] site import: {:.0} ms", site as f64 / 1000.0);
    if site < 100_000 {
        println!("  below the 100 ms threshold; no .pth action needed");
        return;
    }

    let dirs = site_packages_dirs();
    let pths = pth_files(&dirs);
    if pths.is_empty() {
        println!("  no site-packages .pth files with import lines");
    } else {
        println!("  .pth files with import lines:");
        for (name, modules) in pths.iter().take(12) {
            if modules.is_empty() {
                println!("    {name}: (imports via exec)");
            } else {
                println!("    {name}: {}", modules.join(", "));
            }
        }
        if pths.len() > 12 {
            println!("    ... {} more", pths.len() - 12);
        }
    }

    if let Some((us, chain)) = heaviest_chain(&imports) {
        let shown = if chain.len() > 5 {
            &chain[chain.len() - 5..]
        } else {
            &chain[..]
        };
        println!(
            "  heaviest chain: {} ({:.1} ms)",
            shown.join(" -> "),
            us as f64 / 1000.0
        );
    }
    let mut top: Vec<&ImportTime> = imports.iter().filter(|i| i.name != "site").collect();
    top.sort_by_key(|i| std::cmp::Reverse(i.cum));
    if !top.is_empty() {
        let names: Vec<String> = top
            .iter()
            .take(4)
            .map(|i| format!("{} {:.1} ms", i.name, i.cum as f64 / 1000.0))
            .collect();
        println!("  top imports: {}", names.join(", "));
    }
    if let Some(i) = imports
        .iter()
        .filter(|i| i.name.starts_with("__editable__"))
        .max_by_key(|i| i.cum)
    {
        println!(
            "  editable install finder: {} ({:.1} ms; functional, fast mode keeps it)",
            i.name,
            i.cum as f64 / 1000.0
        );
    }
    println!("  (diagnostic only; qx never edits or deletes site-packages)");
    match std::env::var("QX_PYTHON_FAST").ok().as_deref() {
        Some("1") => println!(
            "  QX_PYTHON_FAST=1: max speed; qx-launched Python skips the pip-system-certs and pynvml hooks, so system-cert injection for requests/urllib3 is disabled."
        ),
        Some("tls") => println!(
            "  QX_PYTHON_FAST=tls: qx-launched Python skips the pip-system-certs and pynvml startup hooks but keeps system-cert injection; pip's --trusted-host adapters are patched lazily."
        ),
        Some("off" | "0") => println!(
            "  QX_PYTHON_FAST=off: hooks run untouched. Unset it to get the default tls mode (startup hooks skipped, system certs kept)."
        ),
        _ => println!(
            "  default (tls): qx-launched Python skips the heavy startup hooks and keeps system certs. QX_PYTHON_FAST=1 is faster but drops the cert hook; =off disables."
        ),
    }

    match pip_version() {
        // pip's truststore needs Python 3.10+, whatever the pip version says.
        Some((v, py)) => match pip_uses_system_store(&v).map(|s| s && py.is_none_or(|p| p >= (3, 10))) {
            Some(true) => {
                println!("  pip {v}: uses the system trust store by default (>= 24.2)");
                println!(
                    "  recommendation: pip-system-certs is redundant for pip itself. Do not remove it blindly: it also patches requests/urllib3 for other tools, and removal can break TLS behind a proxy or corporate AV. Test in a copy first."
                );
            }
            Some(false) => {
                println!("  pip {v}: does not use the system trust store by default");
                println!(
                    "  recommendation: leave pip-system-certs alone; removing it may break TLS behind a proxy or corporate AV."
                );
            }
            None => {
                println!(
                    "  recommendation: verify pip uses the system trust store before removing pip-system-certs; removal may break TLS behind a proxy or corporate AV."
                );
            }
        },
        None => println!(
            "  recommendation: pip not detected; removing pip-system-certs may break TLS behind a proxy or corporate AV."
        ),
    }
}

fn parse_importtime(text: &str) -> Vec<ImportTime> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim_end().strip_prefix("import time:") else {
            continue;
        };
        let parts: Vec<&str> = rest.split('|').collect();
        if parts.len() < 3 {
            continue;
        }
        let Ok(cum) = parts[1].trim().parse::<u64>() else {
            continue;
        };
        let raw = parts[2];
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        out.push(ImportTime {
            indent: raw.len() - raw.trim_start().len(),
            name: name.to_string(),
            cum,
        });
    }
    out
}

fn heaviest_chain(imports: &[ImportTime]) -> Option<(u64, Vec<String>)> {
    let site = imports.iter().find(|i| i.name == "site").map(|i| i.cum);
    let best = imports
        .iter()
        .filter(|i| i.name != "site")
        .filter(|i| i.indent > 1 || site.is_none())
        .filter(|i| site.map(|s| i.cum <= s).unwrap_or(true))
        .max_by_key(|i| i.cum)?;
    let mut chain = vec!["site".to_string()];
    let parts: Vec<&str> = best.name.split('.').collect();
    for n in 1..=parts.len() {
        chain.push(parts[..n].join("."));
    }
    Some((best.cum, chain))
}

fn site_packages_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(python) = crate::resolve::find_on_path("python") else {
        return out;
    };
    let Some(parent) = python.parent() else {
        return out;
    };
    let bases = [parent, parent.parent().unwrap_or(parent)];
    for base in bases {
        for lib in ["Lib", "lib"] {
            let p = base.join(lib).join("site-packages");
            let key = p.to_string_lossy().to_ascii_lowercase();
            if p.is_dir()
                && !out
                    .iter()
                    .any(|x: &PathBuf| x.to_string_lossy().to_ascii_lowercase() == key)
            {
                out.push(p);
            }
        }
    }
    out
}

fn pth_files(dirs: &[PathBuf]) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("pth"))
                .unwrap_or(false)
            {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if !text.lines().any(|line| {
                let line = line.trim_start();
                line.starts_with("import ") || line.contains("__import__(")
            }) {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            out.push((name, imports_in_pth(&text)));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn imports_in_pth(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("import ") {
            let head = rest.split(';').next().unwrap_or("");
            for part in head.split(',') {
                let name = part.split_whitespace().next().unwrap_or("");
                if valid_module(name) {
                    out.push(name.to_string());
                }
            }
        }
        let mut rest = line;
        while let Some(i) = rest.find("__import__(") {
            rest = rest[i + 11..].trim_start();
            let Some(quote) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') else {
                break;
            };
            let after = &rest[1..];
            let Some(end) = after.find(quote) else {
                break;
            };
            let name = &after[..end];
            if valid_module(name) {
                out.push(name.to_string());
            }
            rest = &after[end + 1..];
        }
    }
    out.sort();
    out.dedup();
    out
}

fn valid_module(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// pip's version and the (major, minor) of the Python it runs on.
fn pip_version() -> Option<(String, Option<(u32, u32)>)> {
    let args = vec!["-m".to_string(), "pip".to_string(), "--version".to_string()];
    let cmd = exec::build_command("python", &args).ok()?;
    let out = exec::run_captured(cmd, Some(Duration::from_secs(20))).ok()?;
    if out.code != 0 {
        return None;
    }
    let version = out.stdout.split_whitespace().nth(1)?.to_string();
    let py = out.stdout.rsplit_once("(python ").and_then(|(_, v)| {
        let mut n = v.trim_end_matches([')', '\r', '\n', ' ']).split('.');
        Some((n.next()?.parse().ok()?, n.next()?.parse().ok()?))
    });
    Some((version, py))
}

/// pip >= 24.2 uses the OS certificate store in addition to certifi.
fn pip_uses_system_store(version: &str) -> Option<bool> {
    let mut parts = version.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor) >= (24, 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_importtime_and_pth_imports() {
        let text = "import time: 100 | 300000 | site\nimport time: 50 | 200000 |   pip.foo\n";
        let imports = parse_importtime(text);
        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].name, "site");
        assert_eq!(imports[0].cum, 300_000);
        let (cum, chain) = heaviest_chain(&imports).unwrap();
        assert_eq!(cum, 200_000);
        assert_eq!(chain.last().map(String::as_str), Some("pip.foo"));

        let pth = "import os, sys; __import__('pip_system_certs.bootstrap')";
        assert_eq!(
            imports_in_pth(pth),
            vec!["os", "pip_system_certs.bootstrap", "sys"]
        );
        assert_eq!(pip_uses_system_store("26.1.2"), Some(true));
        assert_eq!(pip_uses_system_store("24.1"), Some(false));
    }

    #[test]
    fn heaviest_chain_ignores_top_level_imports_and_pip_needs_python_310() {
        let imports = parse_importtime("import time: 10 | 900000 | os\nimport time: 10 | 5000 |   json\nimport time: 10 | 6000 | site\n");
        let (cum, chain) = heaviest_chain(&imports).unwrap();
        assert_eq!((cum, chain.last().map(String::as_str)), (5000, Some("json")));
        assert_eq!(pip_uses_system_store("24.2"), Some(true));
    }
}
