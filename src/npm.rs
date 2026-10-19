//! Ultra-fast `npm run/test/start`: read package.json, run the script directly.
//!
//! npm's CLI costs ~100 ms before the script even starts. This path skips it and
//! reproduces the parts scripts actually depend on: package root, local bins on
//! PATH, lifecycle env, and `--` args. Anything unusual returns `None` and the
//! real npm handles it, so behavior never silently drifts.

use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Fast {
    pub command: String,
    pub cwd: PathBuf,
    pub path: OsString,
    pub env: Vec<(&'static str, String)>,
}

pub struct Npx {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub path: OsString,
}

pub fn is_npm(program: &str) -> bool {
    let name = basename(program);
    name.eq_ignore_ascii_case("npm")
        || name.eq_ignore_ascii_case("npm.cmd")
        || name.eq_ignore_ascii_case("npm.exe")
}

pub fn is_npx(program: &str) -> bool {
    let name = basename(program);
    name.eq_ignore_ascii_case("npx")
        || name.eq_ignore_ascii_case("npx.cmd")
        || name.eq_ignore_ascii_case("npx.exe")
}

fn basename(program: &str) -> &str {
    program
        .rsplit(|c| c == '\\' || c == '/')
        .next()
        .unwrap_or(program)
}

pub fn command(f: &Fast) -> Result<Command, String> {
    let mut c = crate::exec::build_shell(&f.command, "cmd")?;
    c.current_dir(&f.cwd);
    c.env("PATH", &f.path);
    for (k, v) in &f.env {
        c.env(k, v);
    }
    crate::exec::apply_node_cache(&mut c);
    Ok(c)
}

pub fn fast_command(program: &str, args: &[String], cwd: Option<&Path>) -> Option<Command> {
    if !is_npm(program) {
        return None;
    }
    command(&fast(args, cwd)?).ok()
}

pub fn fast_npx_command(program: &str, args: &[String], cwd: Option<&Path>) -> Option<Command> {
    if !is_npx(program) {
        return None;
    }
    let n = fast_npx(args, cwd)?;
    let mut c = crate::exec::build_command(&n.program.to_string_lossy(), &n.args).ok()?;
    c.current_dir(&n.cwd);
    c.env("PATH", &n.path);
    crate::exec::apply_node_cache(&mut c);
    Some(c)
}

/// `npx <local-bin>`: run the local package bin directly instead of paying for
/// npx's CLI and a second process. Anything that might download falls back.
pub fn fast_npx(args: &[String], cwd: Option<&Path>) -> Option<Npx> {
    if std::env::var("QX_NPM_FAST").ok().as_deref() == Some("off") {
        return None;
    }
    let mut i = 0usize;
    let no_install = args.get(i).map(String::as_str) == Some("--no-install");
    if no_install {
        i += 1;
    }
    let name = args.get(i)?;
    if name.starts_with('-') || name.contains('@') {
        return None;
    }
    let start = cwd
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())?;
    if !start.is_dir() {
        return None;
    }
    if !no_install && !declared_dependency(&start, name) {
        // npx may download a package with this name; let it own that decision.
        return None;
    }
    let program = find_bin(&start, name)?;
    Some(Npx {
        program,
        args: args[i + 1..].to_vec(),
        cwd: start.clone(),
        path: bin_path(&start),
    })
}

fn declared_dependency(start: &Path, bin: &str) -> bool {
    let alias = match bin {
        "tsc" | "tsserver" => "typescript",
        other => other,
    };
    let Some((_, pkg)) = find_package(start) else {
        return false;
    };
    ["dependencies", "devDependencies", "optionalDependencies", "peerDependencies"]
        .iter()
        .any(|k| {
            pkg.get(k)
                .and_then(Value::as_object)
                .map(|d| d.contains_key(alias))
                .unwrap_or(false)
        })
}

fn find_bin(start: &Path, name: &str) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let bin = dir.join("node_modules").join(".bin");
        for ext in [".cmd", ".exe", ".bat", ".ps1", ""] {
            let p = bin.join(format!("{name}{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub fn fast(args: &[String], cwd: Option<&Path>) -> Option<Fast> {
    if std::env::var("QX_NPM_FAST").ok().as_deref() == Some("off") {
        return None;
    }
    for k in [
        "npm_config_script_shell",
        "npm_config_workspace",
        "npm_config_workspaces",
        "npm_config_ignore_scripts",
        "npm_config_prefix",
        "npm_config_userconfig",
        "npm_config_globalconfig",
        "NPM_CONFIG_SCRIPT_SHELL",
        "NPM_CONFIG_WORKSPACE",
        "NPM_CONFIG_WORKSPACES",
        "NPM_CONFIG_IGNORE_SCRIPTS",
        "NPM_CONFIG_PREFIX",
        "NPM_CONFIG_USERCONFIG",
        "NPM_CONFIG_GLOBALCONFIG",
    ] {
        if std::env::var_os(k).is_some() {
            return None;
        }
    }

    let (name, extra, kind) = parse(args)?;
    let start = cwd
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())?;
    if !start.is_dir() {
        return None;
    }
    let (root, pkg) = find_package(&start)?;

    if pkg.get("workspaces").is_some() {
        return None;
    }
    if let Some(pm) = pkg.get("packageManager").and_then(Value::as_str) {
        if !pm.starts_with("npm@") {
            return None;
        }
    }

    let scripts = pkg.get("scripts")?.as_object()?;
    let script = scripts.get(&name)?.as_str()?.trim();
    if script.is_empty() || script.contains("npm_") {
        return None;
    }
    // Pre/post hooks get their own lifecycle env; let npm own that shape.
    if scripts.contains_key(&format!("pre{name}")) || scripts.contains_key(&format!("post{name}")) {
        return None;
    }
    if npmrc_overrides(&root) {
        return None;
    }

    let mut command = script.to_string();
    for a in &extra {
        command.push(' ');
        command.push_str(a);
    }

    let mut env = vec![
        ("npm_lifecycle_event", name.clone()),
        ("npm_lifecycle_script", script.to_string()),
        ("npm_command", kind.to_string()),
        ("INIT_CWD", start.to_string_lossy().into_owned()),
        (
            "npm_config_local_prefix",
            root.to_string_lossy().into_owned(),
        ),
        (
            "npm_package_json",
            root.join("package.json").to_string_lossy().into_owned(),
        ),
    ];
    if let Some(n) = pkg.get("name").and_then(Value::as_str) {
        env.push(("npm_package_name", n.to_string()));
    }
    if let Some(v) = pkg.get("version").and_then(Value::as_str) {
        env.push(("npm_package_version", v.to_string()));
    }
    Some(Fast {
        command,
        cwd: root.clone(),
        path: bin_path(&root),
        env,
    })
}

fn parse(args: &[String]) -> Option<(String, Vec<String>, &'static str)> {
    let (name, rest, kind) = match args {
        [cmd, name, rest @ ..] if cmd == "run" || cmd == "run-script" => {
            (name.clone(), rest, "run-script")
        }
        [cmd, rest @ ..] if cmd == "test" || cmd == "start" => (cmd.clone(), rest, "run-script"),
        _ => return None,
    };
    if name.is_empty() || name.starts_with('-') || name == "env" {
        return None;
    }
    let mut extra = Vec::new();
    if !rest.is_empty() {
        if rest[0] != "--" {
            return None;
        }
        for a in &rest[1..] {
            if !safe_arg(a) {
                return None;
            }
            extra.push(a.clone());
        }
    }
    Some((name, extra, kind))
}

fn safe_arg(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || "-_.:/=@+,~\\".contains(c)
        })
}

fn find_package(start: &Path) -> Option<(PathBuf, Value)> {
    let mut dir = start.to_path_buf();
    loop {
        let file = dir.join("package.json");
        if file.is_file() {
            let text = std::fs::read_to_string(&file).ok()?;
            if text.len() > 2 * 1024 * 1024 {
                return None;
            }
            let text = text.trim_start_matches('\u{feff}');
            let pkg: Value = serde_json::from_str(text).ok()?;
            return Some((dir, pkg));
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn npmrc_overrides(root: &Path) -> bool {
    let mut files = vec![root.join(".npmrc")];
    if let Some(home) = std::env::var_os("USERPROFILE") {
        files.push(PathBuf::from(home).join(".npmrc"));
    }
    for f in files {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        for line in text.lines() {
            let l = line.trim();
            if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
                continue;
            }
            let key = l
                .split('=')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if matches!(
                key.as_str(),
                "script-shell"
                    | "shell"
                    | "node-options"
                    | "ignore-scripts"
                    | "workspace"
                    | "workspaces"
                    | "include-workspace-root"
            ) {
                return true;
            }
        }
    }
    false
}

fn bin_path(root: &Path) -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut cur = Some(root);
    while let Some(p) = cur {
        let bin = p.join("node_modules").join(".bin");
        if bin.is_dir() {
            dirs.push(bin);
        }
        cur = p.parent();
    }
    for p in crate::resolve::path_dirs() {
        let lower = p.to_string_lossy().to_ascii_lowercase();
        if !dirs
            .iter()
            .any(|d| d.to_string_lossy().to_ascii_lowercase() == lower)
        {
            dirs.push(p);
        }
    }
    std::env::join_paths(&dirs).unwrap_or_else(|_| {
        OsString::from(
            dirs.iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>()
                .join(";"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_run_and_args() {
        let (n, a, k) = parse(&[
            "run".into(),
            "build".into(),
            "--".into(),
            "--watch".into(),
        ])
        .unwrap();
        assert_eq!(n, "build");
        assert_eq!(a, vec!["--watch"]);
        assert_eq!(k, "run-script");
    }

    #[test]
    fn rejects_flags_and_unsafe_args() {
        assert!(parse(&["run".into(), "build".into(), "--silent".into()]).is_none());
        assert!(parse(&["run".into(), "build".into(), "--".into(), "a b".into()]).is_none());
        assert!(parse(&["run".into()]).is_none());
        assert!(parse(&["run".into(), "env".into()]).is_none());
    }

    #[test]
    fn npx_runs_a_local_bin_only() {
        let d = std::env::temp_dir().join(format!("qx-npx-{}", std::process::id()));
        let bin = d.join("node_modules").join(".bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("tool.cmd"), "@echo off\n").unwrap();
        std::fs::write(d.join("package.json"), r#"{"devDependencies":{"tool":"1"}}"#).unwrap();
        let args = vec!["--no-install".to_string(), "tool".to_string(), "-v".to_string()];
        let n = fast_npx(&args, Some(&d)).unwrap();
        assert!(n.program.ends_with("tool.cmd"));
        assert_eq!(n.args, vec!["-v"]);
        assert!(fast_npx(&["tool".to_string()], Some(&d)).is_some());
        assert!(fast_npx(&["other".to_string()], Some(&d)).is_none());
        assert!(fast_npx(&["tool@latest".to_string()], Some(&d)).is_none());
        assert!(fast_npx(&["-y".to_string(), "tool".to_string()], Some(&d)).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn npmrc_ignore_scripts_falls_back() {
        let d = std::env::temp_dir().join(format!("qx-npmrc-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("package.json"),
            r#"{"scripts":{"x":"echo x"}}"#,
        )
        .unwrap();
        assert!(fast(&["run".into(), "x".into()], Some(&d)).is_some());
        std::fs::write(d.join(".npmrc"), "ignore-scripts=true\n").unwrap();
        assert!(fast(&["run".into(), "x".into()], Some(&d)).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn finds_nearest_package() {
        let d = std::env::temp_dir().join(format!("qx-npm-{}", std::process::id()));
        let sub = d.join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(d.join("package.json"), r#"{"name":"x"}"#).unwrap();
        let (root, pkg) = find_package(&sub).unwrap();
        assert_eq!(root, d);
        assert_eq!(pkg["name"], "x");
        let _ = std::fs::remove_dir_all(&d);
    }
}
