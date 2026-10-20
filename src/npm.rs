//! Ultra-fast `npm run/test/start`: read package.json, run the script directly.
//!
//! npm's CLI costs ~100 ms before the script even starts. This path skips it and
//! reproduces the parts scripts actually depend on: package root, local bins on
//! PATH, lifecycle env, pre/post hooks and `--` args. Anything unusual returns
//! `None` and the real npm handles it. Known drift: the resolved `npm_config_*`
//! values (cache, prefix, registry...) and COLOR are not exported.

use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One lifecycle script: npm runs `pre<name>`, `<name>`, `post<name>` in turn,
/// each with its own `npm_lifecycle_*` env.
pub struct Step {
    pub event: String,
    pub script: String,
    pub command: String,
}

pub struct Fast {
    pub steps: Vec<Step>,
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
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(program)
}

pub fn commands(f: &Fast) -> Result<Vec<Command>, String> {
    f.steps
        .iter()
        .map(|s| match direct_step(f, s) {
            Some(c) => Ok(c),
            None => {
                // Anything a plain exec cannot reproduce keeps npm's own shell.
                let mut c = crate::exec::build_shell(&s.command, "cmd")?;
                decorate(&mut c, f, s);
                Ok(c)
            }
        })
        .collect()
}

/// Names `cmd.exe` answers itself. A script naming one of these can never be exec'd.
const CMD_INTERNALS: &[&str] = &[
    "assoc", "break", "call", "cd", "chcp", "chdir", "cls", "color", "copy", "date", "del", "dir",
    "echo", "endlocal", "erase", "exit", "for", "ftype", "goto", "if", "md", "mkdir", "mklink",
    "move", "path", "pause", "popd", "prompt", "pushd", "rd", "rem", "ren", "rename", "rmdir",
    "set", "setlocal", "shift", "start", "time", "title", "type", "ver", "verify", "vol",
];

/// The `node_modules\.bin` dirs from `root` upward, nearest first.
fn local_bins(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut cur = Some(root);
    while let Some(p) = cur {
        let bin = p.join("node_modules").join(".bin");
        if bin.is_dir() {
            dirs.push(bin);
        }
        cur = p.parent();
    }
    dirs
}

/// A plain script (no shell syntax, no cmd internal) whose program is a real executable:
/// `cmd.exe` would only add a process.
fn direct_step(f: &Fast, s: &Step) -> Option<Command> {
    let (program, args) = crate::exec::split_simple(&s.command)?;
    if CMD_INTERNALS.iter().any(|n| n.eq_ignore_ascii_case(&program)) {
        return None;
    }
    let target = crate::resolve::resolve_in(&program, &local_bins(&f.cwd), Some(&f.cwd));
    if !matches!(target, crate::resolve::Target::Direct { .. }) {
        return None;
    }
    let mut c = crate::exec::command_for(&program, target, &args).ok()?;
    decorate(&mut c, f, s);
    Some(c)
}

/// Everything a script expects from npm, applied the same way to both kinds of command.
fn decorate(c: &mut Command, f: &Fast, s: &Step) {
    c.current_dir(&f.cwd);
    c.env("PATH", &f.path);
    for (k, v) in &f.env {
        c.env(k, v);
    }
    c.env("npm_lifecycle_event", &s.event);
    c.env("npm_lifecycle_script", &s.script);
    crate::exec::apply_child_defaults(c, true);
    crate::exec::apply_node_cache(c);
}

/// Commands to run in order (stop at the first failure) for `npm run/test/start`
/// or a local-bin `npx`; None means "let the real tool handle it".
pub fn direct(program: &str, args: &[String], cwd: Option<&Path>) -> Option<Vec<Command>> {
    if is_npm(program) {
        return commands(&fast(args, cwd)?).ok();
    }
    if !is_npx(program) {
        return None;
    }
    let n = fast_npx(args, cwd)?;
    let mut c = crate::exec::build_command(&n.program.to_string_lossy(), &n.args).ok()?;
    c.current_dir(&n.cwd);
    c.env("PATH", &n.path);
    crate::exec::apply_node_cache(&mut c);
    Some(vec![c])
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
    if npmrc_overrides(&root) {
        return None;
    }
    // npm runs pre<name>, <name>, post<name>; only <name> gets the `--` args.
    let mut steps = Vec::new();
    for event in [format!("pre{name}"), name.clone(), format!("post{name}")] {
        let script = match scripts.get(&event) {
            Some(v) => v.as_str()?.trim(),
            None if event == name => return None,
            None => continue,
        };
        if script.is_empty() || script.contains("npm_") {
            return None;
        }
        let mut command = script.to_string();
        if event == name {
            for a in &extra {
                command.push(' ');
                command.push_str(a);
            }
        }
        steps.push(Step {
            event,
            script: script.to_string(),
            command,
        });
    }

    let mut env = vec![
        ("npm_command", kind),
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
    env.extend(identity());
    Some(Fast {
        steps,
        cwd: root.clone(),
        path: bin_path(&root),
        env,
    })
}

/// Env real npm exports that tools sniff to detect the package manager
/// (`npm_config_user_agent`) or to re-invoke npm/node. Omitted when npm or node
/// cannot be located, never guessed.
fn identity() -> Vec<(&'static str, String)> {
    use crate::resolve::Target;
    let Target::Direct { exe, prefix } = crate::resolve::resolve("npm", None) else {
        return Vec::new();
    };
    let Some(cli) = prefix.first().map(PathBuf::from) else {
        return Vec::new();
    };
    let node = exe.to_string_lossy().into_owned();
    let mut env = vec![
        ("npm_execpath", cli.to_string_lossy().into_owned()),
        ("npm_node_execpath", node.clone()),
        ("NODE", node),
    ];
    let version = cli
        .parent()
        .and_then(Path::parent)
        .and_then(|d| std::fs::read_to_string(d.join("package.json")).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|p| p.get("version")?.as_str().map(str::to_string));
    if let (Some(npm), Some(node)) = (version, node_version(&exe)) {
        env.push((
            "npm_config_user_agent",
            format!("npm/{npm} node/{node} win32 x64 workspaces/false"),
        ));
    }
    env
}

/// `node -v` costs ~50 ms, so keep the answer keyed by node.exe's identity.
fn node_version(node: &Path) -> Option<String> {
    let meta = std::fs::metadata(node).ok()?;
    let secs = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let stamp = format!("{}|{}|{secs}", node.display(), meta.len());
    let file = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("qx")
        .join("node-version");
    if let Some((s, v)) = std::fs::read_to_string(&file)
        .ok()
        .as_deref()
        .and_then(|t| t.trim().split_once('\n'))
    {
        if s == stamp {
            return Some(v.to_string());
        }
    }
    let mut c = Command::new(node);
    c.arg("-v");
    crate::exec::prepare(&mut c);
    let out = c.output().ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || !v.starts_with('v') {
        return None;
    }
    let _ = std::fs::create_dir_all(file.parent()?);
    let _ = std::fs::write(&file, format!("{stamp}\n{v}"));
    Some(v)
}

fn parse(args: &[String]) -> Option<(String, Vec<String>, String)> {
    // `kind` is exported as npm_command; npm reports the `run-script` alias as `run`.
    let (name, rest, kind) = match args {
        [cmd, name, rest @ ..] if cmd == "run" || cmd == "run-script" => {
            (name.clone(), rest, "run".to_string())
        }
        [cmd, rest @ ..] if cmd == "test" || cmd == "start" => (cmd.clone(), rest, cmd.clone()),
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
    let mut dirs: Vec<PathBuf> = local_bins(root);
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
        assert_eq!(k, "run");
        assert_eq!(parse(&["test".into()]).unwrap().2, "test");
    }

    #[test]
    fn pre_and_post_run_around_the_script() {
        let d = std::env::temp_dir().join(format!("qx-npmchain-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("package.json"),
            r#"{"scripts":{"prebuild":"echo pre","build":"echo main","postbuild":"echo post","solo":"echo solo","prebad":"echo x","bad":"echo npm_foo"}}"#,
        )
        .unwrap();
        let f = fast(&["run".into(), "build".into(), "--".into(), "--x".into()], Some(&d)).unwrap();
        let got: Vec<_> = f.steps.iter().map(|s| (s.event.as_str(), s.command.as_str())).collect();
        assert_eq!(
            got,
            [("prebuild", "echo pre"), ("build", "echo main --x"), ("postbuild", "echo post")]
        );
        assert_eq!(fast(&["run".into(), "solo".into()], Some(&d)).unwrap().steps.len(), 1);
        // A script that reads npm_* env falls back, and so does a missing main script.
        assert!(fast(&["run".into(), "bad".into()], Some(&d)).is_none());
        assert!(fast(&["run".into(), "pre".into()], Some(&d)).is_none());
        let _ = std::fs::remove_dir_all(&d);
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

    /// The `.cmd` npm itself writes for a package bin: a stock shim that only forwards to
    /// node, which is what makes it safe to unwrap.
    const STOCK_SHIM: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\..\\tool\\cli.js\" %*\r\n";

    #[test]
    fn a_plain_script_runs_its_local_bin_without_cmd() {
        if crate::resolve::find_on_path("node").is_none() {
            println!("skip: node is not on PATH");
            return;
        }
        let root = std::env::temp_dir().join(format!("qx-npmplain-{}", std::process::id()));
        let bin = root.join("node_modules").join(".bin");
        std::fs::create_dir_all(root.join("node_modules").join("tool")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(root.join("package.json"), r#"{"scripts":{"build":"tool --flag"}}"#).unwrap();
        std::fs::write(bin.join("tool.cmd"), STOCK_SHIM).unwrap();
        std::fs::write(root.join("node_modules").join("tool").join("cli.js"), "").unwrap();

        let f = fast(&["run".into(), "build".into()], Some(&root)).unwrap();
        let c = &commands(&f).unwrap()[0];
        assert_eq!(
            Path::new(c.get_program()).file_name().unwrap().to_string_lossy(),
            "node.exe"
        );
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a.ends_with("cli.js")), "{args:?}");
        assert!(args.iter().any(|a| a == "--flag"), "{args:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn shell_syntax_cmd_internals_and_logic_shims_keep_cmd() {
        let root = std::env::temp_dir().join(format!("qx-npmkeep-{}", std::process::id()));
        let bin = root.join("node_modules").join(".bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts":{"a":"tool && tool","b":"echo hi","c":"no-such-tool-zz x","d":"logic"}}"#,
        )
        .unwrap();
        std::fs::write(bin.join("tool.cmd"), STOCK_SHIM).unwrap();
        // A shim that does real work cannot be unwrapped into `node`.
        std::fs::write(bin.join("logic.cmd"), "@echo off\r\nset FOO=1\r\nnode \"%dp0%\\..\\tool\\cli.js\" %*\r\n").unwrap();
        for script in ["a", "b", "c", "d"] {
            let f = fast(&["run".into(), script.into()], Some(&root)).unwrap();
            let cmd = &commands(&f).unwrap()[0];
            assert_eq!(
                Path::new(cmd.get_program()).file_name().unwrap().to_string_lossy().to_ascii_lowercase(),
                "cmd.exe",
                "script {script} must keep cmd.exe"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
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
