//! Process resolution: find the real executable and skip Windows shims.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Clone)]
pub enum Target {
    /// Executable plus any fixed leading args (e.g. node + script.js).
    Direct { exe: PathBuf, prefix: Vec<String> },
    /// A .cmd/.bat we could not unwrap; must go through cmd.exe.
    Shim(PathBuf),
    /// A PowerShell script.
    Ps1(PathBuf),
    Missing,
}

impl Target {
    /// Re-check the filesystem before trusting a cached resolution, so a
    /// mid-session upgrade/uninstall does not leave a stale path behind.
    fn alive(&self) -> bool {
        match self {
            Target::Direct { exe, prefix } => {
                exe.is_file()
                    && prefix.iter().all(|p| {
                        if p.contains('\\') || p.contains('/') {
                            Path::new(p).is_file()
                        } else {
                            true
                        }
                    })
            }
            Target::Shim(p) | Target::Ps1(p) => p.is_file(),
            Target::Missing => false,
        }
    }
}

fn cache() -> &'static Mutex<HashMap<String, Target>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Target>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn resolve(program: &str) -> Target {
    let key = program.to_ascii_lowercase();
    if let Ok(guard) = cache().lock() {
        if let Some(hit) = guard.get(&key) {
            if hit.alive() {
                return hit.clone();
            }
        }
    }
    let found = resolve_uncached(program);
    if !matches!(found, Target::Missing) {
        if let Ok(mut guard) = cache().lock() {
            if guard.len() < 512 {
                guard.insert(key, found.clone());
            }
        }
    }
    found
}

fn resolve_uncached(program: &str) -> Target {
    let p = Path::new(program);
    let has_sep = program.contains('\\') || program.contains('/');
    if has_sep || p.is_absolute() {
        if p.exists() {
            return classify(p.to_path_buf());
        }
        return Target::Missing;
    }
    match find_on_path(program) {
        Some(found) => classify(found),
        None => Target::Missing,
    }
}

pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let dirs = path_dirs();
    let exts = pathext();
    for dir in dirs {
        // Try real Windows executables first. Node ships an extensionless `npm`
        // shell script next to npm.cmd; the extensionless file is not runnable.
        for ext in &exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        let direct = dir.join(name);
        if direct.is_file() {
            let ext = direct
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ext == "exe" || ext == "com" {
                return Some(direct);
            }
        }
    }
    None
}

/// PATH entries, deduplicated case-insensitively and with the malformed tail
/// produced when an oversized environment block is truncated removed.
pub fn path_dirs() -> Vec<PathBuf> {
    let raw = std::env::var_os("PATH").unwrap_or_default();
    let raw = raw.to_string_lossy();
    let cut = raw
        .to_ascii_lowercase()
        .find("pathext=")
        .map(|i| &raw[..i])
        .unwrap_or(&raw);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for dir in std::env::split_paths(cut) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let key = dir
            .to_string_lossy()
            .trim_end_matches(|c| c == '\\' || c == '/')
            .to_ascii_lowercase();
        if seen.insert(key) {
            out.push(dir);
        }
    }
    out
}

pub fn pathext_string() -> String {
    match std::env::var("PATHEXT") {
        Ok(v) if !v.is_empty() && !v.to_ascii_lowercase().contains("path=") => v,
        _ => ".COM;.EXE;.BAT;.CMD".to_string(),
    }
}

fn pathext() -> Vec<String> {
    pathext_string()
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

fn classify(p: PathBuf) -> Target {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if ext == "ps1" {
        return Target::Ps1(p);
    }

    if ext == "cmd" || ext == "bat" {
        if let Some(t) = node_target(&p) {
            return t;
        }
        if let Some(t) = generic_shim(&p) {
            return t;
        }
        return Target::Shim(p);
    }

    if let Some(t) = de_trampoline(&p) {
        return t;
    }
    if let Some(t) = python_console_script(&p) {
        return t;
    }

    Target::Direct {
        exe: p,
        prefix: Vec::new(),
    }
}

/// Git for Windows ships cmd\*.exe launchers that re-exec the real binaries in
/// mingw64\bin. Going straight there removes one process creation.
fn de_trampoline(p: &Path) -> Option<Target> {
    let s = p.to_string_lossy().to_ascii_lowercase();
    if s.contains("\\git\\cmd\\") {
        let name = p.file_name()?;
        let real = p
            .parent()?
            .parent()?
            .join("mingw64")
            .join("bin")
            .join(name);
        if real.is_file() {
            return Some(Target::Direct {
                exe: real,
                prefix: Vec::new(),
            });
        }
    }
    None
}

/// Python console scripts (pip.exe, pytest.exe) are launchers that spawn
/// python.exe. Calling `python -m <module>` is one process instead of two.
fn python_console_script(p: &Path) -> Option<Target> {
    let stem = p.file_stem()?.to_str()?.to_ascii_lowercase();
    let module = match stem.as_str() {
        "pip" | "pip3" => "pip",
        s if s.starts_with("pip3.") => "pip",
        "pytest" | "py.test" => "pytest",
        "black" => "black",
        "uvicorn" => "uvicorn",
        "flake8" => "flake8",
        "django-admin" => "django",
        "mkdocs" => "mkdocs",
        "virtualenv" => "virtualenv",
        "poetry" => "poetry",
        "ipython" => "IPython",
        "jupyter" => "jupyter",
        "mypy" => "mypy",
        "pylint" => "pylint",
        _ => return None,
    };
    let python = p.parent()?.parent()?.join("python.exe");
    if !python.is_file() {
        return None;
    }
    // Only bypass the launcher when the module is actually installed beside
    // this interpreter; otherwise keep the launcher's own error behavior.
    let site = python.parent()?.join("Lib").join("site-packages");
    if !site.join(module).is_dir() && !site.join(format!("{module}.py")).is_file() {
        return None;
    }
    Some(Target::Direct {
        exe: python,
        prefix: vec!["-m".to_string(), module.to_string()],
    })
}

/// npm/npx/pnpm/yarn are .cmd trampolines that spawn cmd.exe then node.exe.
/// Run node + the JS entrypoint directly: one spawn instead of three.
fn node_target(shim: &Path) -> Option<Target> {
    let dir = shim.parent()?;
    let stem = shim.file_stem()?.to_str()?.to_ascii_lowercase();

    let mappings: &[(&str, &[&str])] = &[
        ("npm", &["node_modules/npm/bin/npm-cli.js"]),
        ("npx", &["node_modules/npm/bin/npx-cli.js"]),
        ("pnpm", &["node_modules/pnpm/bin/pnpm.cjs", "node_modules/corepack/dist/pnpm.js"]),
        ("yarn", &["node_modules/yarn/bin/yarn.js", "yarn.js"]),
        ("yarnpkg", &["node_modules/yarn/bin/yarn.js", "yarn.js"]),
        ("tsc", &["node_modules/typescript/bin/tsc"]),
        ("tsx", &["node_modules/tsx/dist/cli.mjs"]),
    ];

    let scripts = mappings.iter().find(|(n, _)| *n == stem).map(|(_, s)| *s)?;

    let node = if dir.join("node.exe").is_file() {
        dir.join("node.exe")
    } else {
        find_on_path("node")?
    };

    for rel in scripts {
        let script = dir.join(rel.replace('/', "\\"));
        if script.is_file() {
            return Some(Target::Direct {
                exe: node,
                prefix: vec![script.to_string_lossy().into_owned()],
            });
        }
    }
    None
}

/// Any npm/Scoop-style .cmd shim: read it, find the real script/executable it
/// launches, and skip cmd.exe. Covers every Node CLI, not just the ones above.
fn generic_shim(shim: &Path) -> Option<Target> {
    let text = std::fs::read_to_string(shim).ok()?;
    if text.len() > 64 * 1024 {
        return None;
    }
    let dir = shim.parent()?;
    let mut js: Option<PathBuf> = None;
    let mut exe: Option<PathBuf> = None;

    for token in text.split('"') {
        let raw = token.trim();
        if raw.is_empty() || raw.contains("%*") {
            continue;
        }
        if !(raw.contains("%~dp0") || raw.contains("%dp0%")) {
            continue;
        }
        let cleaned = raw.replace("%~dp0", "").replace("%dp0%", "");
        let cleaned = cleaned.trim_start_matches(|c| c == '\\' || c == '/');
        if cleaned.contains('%') {
            continue; // unresolved variable
        }
        let lower = cleaned.to_ascii_lowercase();
        let is_js = lower.ends_with(".js") || lower.ends_with(".cjs") || lower.ends_with(".mjs");
        let is_exe = lower.ends_with(".exe");
        if !is_js && !is_exe {
            continue;
        }
        let candidate = dir.join(cleaned.replace('/', "\\"));
        if !candidate.is_file() {
            continue;
        }
        if is_exe {
            // Skip the interpreter itself; we want the payload.
            if lower.ends_with("node.exe") || lower.ends_with("cmd.exe") {
                continue;
            }
            if exe.is_none() {
                exe = Some(candidate);
            }
        } else {
            // Last JS path in the file is the one actually executed.
            js = Some(candidate);
        }
    }

    if let Some(script) = js {
        let node = if dir.join("node.exe").is_file() {
            dir.join("node.exe")
        } else {
            find_on_path("node")?
        };
        return Some(Target::Direct {
            exe: node,
            prefix: vec![script.to_string_lossy().into_owned()],
        });
    }
    exe.map(|e| Target::Direct {
        exe: e,
        prefix: Vec::new(),
    })
}

/// Locate Git Bash without going through WSL's system32\bash.exe.
pub fn git_bash() -> Option<PathBuf> {
    const CANDIDATES: &[&str] = &[
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files\Git\usr\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
    ];
    for c in CANDIDATES {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("qx-resolve-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn parses_standard_npm_shim() {
        let d = tmp("shim");
        let script = d.join("node_modules").join("pkg").join("bin").join("cli.js");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::write(&script, "console.log(1)").unwrap();
        let shim = d.join("tool.cmd");
        fs::write(
            &shim,
            "@ECHO off\n\"%_prog%\"  \"%dp0%\\node_modules\\pkg\\bin\\cli.js\" %*\n",
        )
        .unwrap();

        match generic_shim(&shim) {
            Some(Target::Direct { exe, prefix }) => {
                assert!(exe.to_string_lossy().to_ascii_lowercase().ends_with("node.exe"));
                assert_eq!(prefix.len(), 1);
                assert!(prefix[0].ends_with("cli.js"));
            }
            _ => panic!("shim was not unwrapped"),
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn resolves_real_program_twice() {
        let first = resolve("cmd");
        assert!(matches!(first, Target::Direct { .. }));
        let second = resolve("cmd");
        assert!(matches!(second, Target::Direct { .. }));
    }

    #[test]
    fn path_dirs_have_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for dir in path_dirs() {
            let key = dir
                .to_string_lossy()
                .trim_end_matches(|c| c == '\\' || c == '/')
                .to_ascii_lowercase();
            assert!(seen.insert(key), "duplicate path dir: {}", dir.display());
        }
    }

    #[test]
    fn prefers_last_js_target() {
        let d = tmp("lastjs");
        fs::create_dir_all(d.join("node_modules").join("pkg").join("bin")).unwrap();
        fs::write(d.join("first.js"), "").unwrap();
        fs::write(d.join("last.js"), "").unwrap();
        let shim = d.join("tool.cmd");
        fs::write(
            &shim,
            "@echo off\n\"%~dp0\\first.js\"\n\"%~dp0\\last.js\" %*\n",
        )
        .unwrap();
        match generic_shim(&shim) {
            Some(Target::Direct { prefix, .. }) => assert!(prefix[0].ends_with("last.js")),
            _ => panic!("not unwrapped"),
        }
        let _ = fs::remove_dir_all(&d);
    }
}
