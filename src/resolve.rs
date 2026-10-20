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

pub fn resolve(program: &str, cwd: Option<&Path>) -> Target {
    if program.contains(['\\', '/']) || Path::new(program).is_absolute() {
        // Relative to the call's cwd, so the answer is not cacheable by name.
        return resolve_path(program, cwd);
    }
    let key = program.to_ascii_lowercase();
    if let Ok(guard) = cache().lock() {
        if let Some(hit) = guard.get(&key) {
            if hit.alive() {
                return hit.clone();
            }
        }
    }
    let found = find_on_path(program).map_or(Target::Missing, classify);
    if !matches!(found, Target::Missing) {
        if let Ok(mut guard) = cache().lock() {
            if guard.len() < 512 {
                guard.insert(key, found.clone());
            }
        }
    }
    found
}

/// An explicit path: join it onto the call's cwd, then trust only a runnable name. A bare
/// `node` means `node.exe`; the extensionless file beside it is a sh script (os error 193).
fn resolve_path(program: &str, cwd: Option<&Path>) -> Target {
    let mut p = PathBuf::from(program);
    if !p.is_absolute() {
        if let Some(c) = cwd {
            p = c.join(p);
        }
    }
    // cmd.exe reads `./bin/x.cmd` as the switch `/bin`.
    let p = PathBuf::from(p.to_string_lossy().replace('/', "\\"));
    let runnable = |f: &Path| {
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        f.is_file() && matches!(ext.as_str(), "exe" | "com" | "cmd" | "bat" | "ps1")
    };
    if runnable(&p) {
        return classify(p);
    }
    for ext in pathext() {
        let mut s = p.clone().into_os_string();
        s.push(&ext);
        let cand = PathBuf::from(s);
        if cand.is_file() {
            return classify(cand);
        }
    }
    Target::Missing
}

pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let exts = pathext();
    path_dirs().iter().find_map(|dir| find_in_dir(dir, name, &exts))
}

/// One PATH directory. On Windows a single listing answers every PATHEXT candidate at once (a miss would
/// otherwise cost one stat per extension per directory); anything it cannot answer falls back to stats.
fn find_in_dir(dir: &Path, name: &str, exts: &[String]) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        // Only plain names: the listing pattern is a DOS wildcard, and NTFS case folding / 8.3 aliases
        // are wider than ASCII.
        let plain = !name.is_empty()
            && !name.ends_with('.')
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
        if plain {
            if let Ok(entries) = list_prefix(dir, name) {
                return pick_listed(dir, name, exts, &entries);
            }
        }
    }
    stat_dir(dir, name, exts)
}

fn stat_dir(dir: &Path, name: &str, exts: &[String]) -> Option<PathBuf> {
    // Try real Windows executables first. Node ships an extensionless `npm`
    // shell script next to npm.cmd; the extensionless file is not runnable.
    for ext in exts {
        let candidate = dir.join(format!("{name}{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let direct = dir.join(name);
    if direct.is_file() && runnable_bare(&direct) {
        return Some(direct);
    }
    None
}

/// An extensionless-in-PATHEXT hit counts only when the file itself is a real `.exe`/`.com`.
fn runnable_bare(p: &Path) -> bool {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    ext == "exe" || ext == "com"
}

#[cfg(windows)]
fn pick_listed(dir: &Path, name: &str, exts: &[String], entries: &[(String, bool)]) -> Option<PathBuf> {
    // A reparse point (symlink, app-execution alias) is only a file if it resolves to one: ask, as is_file did.
    let listed = |file: &str, candidate: &Path| {
        let want = file.to_ascii_lowercase();
        entries.iter().any(|(n, reparse)| *n == want && (!*reparse || candidate.is_file()))
    };
    for ext in exts {
        let file = format!("{name}{ext}");
        let candidate = dir.join(&file);
        if listed(&file, &candidate) {
            return Some(candidate);
        }
    }
    let direct = dir.join(name);
    if runnable_bare(&direct) && listed(name, &direct) {
        return Some(direct);
    }
    None
}

/// Files in `dir` whose name starts with `name`, from one `FindFirstFileExW` listing: lowercased names, and
/// whether each is a reparse point. `Err` means the listing is not trustworthy (any error but "nothing there").
#[cfg(windows)]
fn list_prefix(dir: &Path, name: &str) -> Result<Vec<(String, bool)>, ()> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    #[repr(C)]
    struct FindData {
        attrs: u32,
        times: [u32; 6],
        size: [u32; 2],
        reserved: [u32; 2],
        file_name: [u16; 260],
        alt_name: [u16; 14],
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn FindFirstFileExW(
            name: *const u16,
            level: i32,
            data: *mut c_void,
            op: i32,
            filter: *const c_void,
            flags: u32,
        ) -> *mut c_void;
        fn FindNextFileW(h: *mut c_void, data: *mut c_void) -> i32;
        fn FindClose(h: *mut c_void) -> i32;
    }
    const FIND_EX_INFO_BASIC: i32 = 1;
    const FIND_EX_SEARCH_NAME_MATCH: i32 = 0;
    const FIND_FIRST_EX_LARGE_FETCH: u32 = 2;
    const ERRORS_FOR_NOTHING_HERE: [i32; 3] = [2, 3, 18]; // FILE_NOT_FOUND, PATH_NOT_FOUND, NO_MORE_FILES

    let pattern: Vec<u16> = dir
        .join(format!("{name}*"))
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: `data` is a zeroed repr(C) WIN32_FIND_DATAW (592 bytes) that outlives both calls, `pattern`
    // is NUL-terminated, and the handle is closed exactly once.
    unsafe {
        let mut data: FindData = std::mem::zeroed();
        let dp = &mut data as *mut FindData as *mut c_void;
        let h = FindFirstFileExW(
            pattern.as_ptr(),
            FIND_EX_INFO_BASIC,
            dp,
            FIND_EX_SEARCH_NAME_MATCH,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        );
        if h as isize == -1 {
            let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            return if ERRORS_FOR_NOTHING_HERE.contains(&e) { Ok(Vec::new()) } else { Err(()) };
        }
        let mut out = Vec::new();
        let end = loop {
            if data.attrs & 0x10 == 0 {
                let len = data.file_name.iter().position(|&c| c == 0).unwrap_or(data.file_name.len());
                let file = String::from_utf16_lossy(&data.file_name[..len]).to_ascii_lowercase();
                out.push((file, data.attrs & 0x400 != 0));
            }
            if FindNextFileW(h, dp) == 0 {
                break std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            }
        };
        FindClose(h);
        if end == 18 { Ok(out) } else { Err(()) }
    }
}

/// PATH entries, deduplicated case-insensitively and with the malformed tail
/// produced when an oversized environment block is truncated removed.
pub fn path_dirs() -> Vec<PathBuf> {
    // PATH is read once per process: nothing in qx edits its own environment (children get a copy).
    static DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    DIRS.get_or_init(compute_path_dirs).clone()
}

fn compute_path_dirs() -> Vec<PathBuf> {
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
            .trim_end_matches(['\\', '/'])
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

const RUSTUP_TOOLS: &[&str] = &[
    "cargo",
    "rustc",
    "rustdoc",
    "cargo-clippy",
    "cargo-fmt",
    "rustfmt",
    "clippy-driver",
    "rust-analyzer",
];

pub struct RustupTarget {
    pub exe: PathBuf,
    pub bin: PathBuf,
    pub toolchain: String,
}

struct RustupSettings {
    default_toolchain: Option<String>,
    overrides: Vec<String>,
}

/// Resolve a rustup proxy to the real toolchain binary.
///
/// Returns None whenever rustup would have made a context-dependent choice:
/// `+toolchain` arguments, a rust-toolchain file, a directory override, or a
/// missing toolchain. The caller then keeps the proxy, so behavior is unchanged.
pub fn rustup_direct(program: &str, args: &[String], cwd: Option<&Path>) -> Option<RustupTarget> {
    let tool = rustup_tool(program)?;
    if args.first().is_some_and(|a| a.starts_with('+')) {
        return None;
    }
    let Target::Direct { exe, prefix } = resolve(program, cwd) else {
        return None;
    };
    if !prefix.is_empty() || !is_rustup_proxy(&exe) {
        return None;
    }
    let cwd = cwd
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())?;
    if has_toolchain_file(&cwd) {
        return None;
    }
    let settings = read_settings();
    if under_override(&cwd, &settings.overrides) {
        return None;
    }

    let toolchain = std::env::var("RUSTUP_TOOLCHAIN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or(settings.default_toolchain)?;
    let root = if Path::new(&toolchain).is_absolute() {
        PathBuf::from(&toolchain)
    } else {
        rustup_home()?.join("toolchains").join(&toolchain)
    };
    let bin = root.join("bin");
    let exe = bin.join(format!("{tool}.exe"));
    if !exe.is_file() {
        return None;
    }
    Some(RustupTarget {
        exe,
        bin,
        toolchain,
    })
}

fn rustup_tool(program: &str) -> Option<&'static str> {
    let file = Path::new(program).file_name()?.to_string_lossy();
    let stem = file
        .strip_suffix(".exe")
        .or_else(|| file.strip_suffix(".EXE"))
        .unwrap_or(&file);
    RUSTUP_TOOLS
        .iter()
        .copied()
        .find(|t| stem.eq_ignore_ascii_case(t))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn rustup_home() -> Option<PathBuf> {
    std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".rustup")))
}

fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".cargo")))
}

fn is_rustup_proxy(exe: &Path) -> bool {
    let Some(bin) = cargo_home().map(|h| h.join("bin")) else {
        return false;
    };
    if exe.parent().map(norm_path) != Some(norm_path(&bin)) {
        return false;
    }
    bin.join("rustup.exe").is_file()
}

fn has_toolchain_file(cwd: &Path) -> bool {
    cwd.ancestors().any(|dir| {
        dir.join("rust-toolchain").is_file() || dir.join("rust-toolchain.toml").is_file()
    })
}

fn read_settings() -> RustupSettings {
    let Some(home) = rustup_home() else {
        return RustupSettings {
            default_toolchain: None,
            overrides: Vec::new(),
        };
    };
    let text = std::fs::read_to_string(home.join("settings.toml")).unwrap_or_default();
    parse_settings(&text)
}

fn parse_settings(text: &str) -> RustupSettings {
    let mut default_toolchain = None;
    let mut overrides = Vec::new();
    let mut in_overrides = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_overrides = line == "[overrides]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = unquote(key);
        if in_overrides {
            if !key.is_empty() {
                overrides.push(norm_path_str(&key));
            }
        } else if key == "default_toolchain" {
            let value = unquote(value);
            if !value.is_empty() {
                default_toolchain = Some(value);
            }
        }
    }
    RustupSettings {
        default_toolchain,
        overrides,
    }
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"'))
            || (s.starts_with('\'') && s.ends_with('\'')))
    {
        let inner = &s[1..s.len() - 1];
        inner.replace("\\\\", "\\").replace("\\\"", "\"")
    } else {
        s.to_string()
    }
}

fn norm_path(p: &Path) -> String {
    norm_path_str(&p.to_string_lossy())
}

fn norm_path_str(s: &str) -> String {
    let mut s = s.replace('\\', "/").to_ascii_lowercase();
    while s.contains("//") {
        s = s.replace("//", "/");
    }
    s.trim_end_matches('/').to_string()
}

fn under_override(cwd: &Path, overrides: &[String]) -> bool {
    let cwd = norm_path(cwd);
    overrides
        .iter()
        .any(|o| cwd == *o || cwd.starts_with(&format!("{o}/")))
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
    if !python.is_file() || python_minor(python.parent()?)? < 11 {
        return None; // PYTHONSAFEPATH (cwd off sys.path) exists from 3.11 only
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

/// Minor version of the CPython install in `dir`, read from its `python3NN.dll`.
fn python_minor(dir: &Path) -> Option<u32> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        name.strip_prefix("python3")?.strip_suffix(".dll")?.parse().ok()
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

    // npm/npx shims first try `<global prefix>\node_modules\npm\bin\*-cli.js`
    // and only then fall back to the copy bundled with Node. Skipping that
    // check silently runs a different npm version from the one the shell uses.
    let prefer_global = matches!(stem.as_str(), "npm" | "npx");
    let prefix = if prefer_global { global_prefix() } else { None };

    for rel in scripts {
        let bundled = dir.join(rel.replace('/', "\\"));
        if bundled.is_file() {
            let script = if prefer_global {
                pick_npm_script(bundled, prefix.as_deref(), rel)
            } else {
                bundled
            };
            return Some(Target::Direct {
                exe: node,
                prefix: vec![script.to_string_lossy().into_owned()],
            });
        }
    }
    None
}

/// The global prefix npm would use, in its precedence order: environment,
/// user `.npmrc`, then the Windows default.
fn global_prefix() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("NPM_CONFIG_PREFIX").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(v));
    }
    if let Some(home) = home_dir() {
        if let Some(p) = npmrc_prefix(&home.join(".npmrc")) {
            return Some(p);
        }
    }
    std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("npm"))
}

fn npmrc_prefix(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut prefix = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("prefix") {
            let value = unquote(value);
            if !value.is_empty() {
                prefix = Some(PathBuf::from(value));
            }
        }
    }
    prefix
}

fn pick_npm_script(bundled: PathBuf, prefix: Option<&Path>, rel: &str) -> PathBuf {
    let Some(prefix) = prefix else {
        return bundled;
    };
    let candidate = prefix.join(rel.replace('/', "\\"));
    if candidate.is_file() {
        candidate
    } else {
        bundled
    }
}

/// True only when every line is the stock cmd-shim boilerplate and exactly one line launches
/// `prog [script] %*`. Anything else (`set FOO_HOME=`, `cd`, `if`, node flags) is script logic
/// that unwrapping would drop, so that file runs through cmd.exe as written.
fn plain_shim(text: &str) -> bool {
    const BOILER: &[&str] = &[
        "@echo off", "echo off", "goto start", ":find_dp0", "set dp0=%~dp0", "exit /b", ":start",
        "setlocal", "call :find_dp0", "endlocal", "if exist \"%dp0%\\node.exe\" (",
        "set \"_prog=%dp0%\\node.exe\"", ") else (", "set \"_prog=node\"",
        "set pathext=%pathext:;.js;=;%", ")",
    ];
    let mut launches = 0;
    for raw in text.lines() {
        let l = raw.trim().to_ascii_lowercase();
        if l.is_empty() || l.starts_with("rem ") || l.starts_with("@rem") || l.starts_with("::") || BOILER.contains(&l.as_str()) {
            continue;
        }
        launches += 1;
        if !l.contains("%*") || !launch_shape(raw.trim()) {
            return false;
        }
    }
    launches == 1
}

/// `[endlocal & goto ... &] ["prog"|node] ["script"] %*`, nothing else.
fn launch_shape(line: &str) -> bool {
    let line = line.trim_start_matches('@');
    let lead = line.find('"').unwrap_or(line.len());
    let (pre, rest) = line.split_at(lead);
    let pre = pre.trim().to_ascii_lowercase();
    if !(pre.is_empty() || pre == "endlocal & goto #_undefined_# 2>nul || title %comspec% &" || pre == "node") {
        return false;
    }
    let mut toks: Vec<(String, bool)> = Vec::new();
    let (mut cur, mut was_quoted, mut quoted) = (String::new(), false, false);
    for ch in rest.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                was_quoted = true;
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() || was_quoted {
                    toks.push((std::mem::take(&mut cur), was_quoted));
                    was_quoted = false;
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() || was_quoted {
        toks.push((cur, was_quoted));
    }
    let n = toks.len();
    (1..=3).contains(&n)
        && toks[n - 1] == ("%*".to_string(), false)
        && toks[..n - 1].iter().all(|t| t.1)
        && (n > 1 || pre == "node")
}

/// Any npm/Scoop-style .cmd shim: read it, find the real script/executable it
/// launches, and skip cmd.exe. Covers every Node CLI, not just the ones above.
fn generic_shim(shim: &Path) -> Option<Target> {
    let text = std::fs::read_to_string(shim).ok()?;
    if text.len() > 64 * 1024 || !plain_shim(&text) {
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
        let cleaned = cleaned.trim_start_matches(['\\', '/']);
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

    #[cfg(windows)]
    #[test]
    fn directory_listing_finds_what_per_extension_stats_find() {
        let exts = pathext();
        let d = tmp("listing");
        for f in ["tool.exe", "tool.cmd", "only.CMD", "bare.exe", "script", "shy.com"] {
            fs::write(d.join(f), "x").unwrap();
        }
        fs::create_dir(d.join("dirname.exe")).unwrap();
        for name in ["tool", "TOOL", "only", "bare.exe", "script", "dirname", "shy", "missing"] {
            assert_eq!(find_in_dir(&d, name, &exts), stat_dir(&d, name, &exts), "{name}");
        }
        assert_eq!(find_in_dir(&d.join("nope"), "tool", &exts), None);
        let _ = fs::remove_dir_all(&d);
        // and on the real PATH, for names that exist on most machines and one that does not
        for name in ["cmd", "powershell", "where", "git", "cargo", "qx", "no-such-program-zz"] {
            for dir in path_dirs() {
                assert_eq!(find_in_dir(&dir, name, &exts), stat_dir(&dir, name, &exts), "{name} in {}", dir.display());
            }
        }
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
        let first = resolve("cmd", None);
        assert!(matches!(first, Target::Direct { .. }));
        let second = resolve("cmd", None);
        assert!(matches!(second, Target::Direct { .. }));
    }

    #[test]
    fn path_dirs_have_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for dir in path_dirs() {
            let key = dir
                .to_string_lossy()
                .trim_end_matches(['\\', '/'])
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
            "@echo off\n@rem \"%~dp0\\first.js\"\n\"%~dp0\\last.js\" %*\n",
        )
        .unwrap();
        match generic_shim(&shim) {
            Some(Target::Direct { prefix, .. }) => assert!(prefix[0].ends_with("last.js")),
            _ => panic!("not unwrapped"),
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn npmrc_prefix_is_read() {
        let d = tmp("npmrcprefix");
        let f = d.join(".npmrc");
        fs::write(&f, "strict-ssl=false\nprefix=C:\\Users\\me\\AppData\\Roaming\\npm\n").unwrap();
        assert_eq!(
            npmrc_prefix(&f).unwrap(),
            PathBuf::from(r"C:\Users\me\AppData\Roaming\npm")
        );
        fs::write(&f, "prefix=\"C:/tools/npm\"\n").unwrap();
        assert_eq!(npmrc_prefix(&f).unwrap(), PathBuf::from("C:/tools/npm"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn npm_prefers_global_prefix_cli() {
        let d = tmp("globalnpm");
        let rel = "node_modules/npm/bin/npm-cli.js";
        let bundled = d.join("nodejs").join(rel.replace('/', "\\"));
        fs::create_dir_all(bundled.parent().unwrap()).unwrap();
        fs::write(&bundled, "").unwrap();
        let prefix = d.join("roaming").join("npm");
        assert_eq!(
            pick_npm_script(bundled.clone(), Some(&prefix), rel),
            bundled
        );
        assert_eq!(pick_npm_script(bundled.clone(), None, rel), bundled);
        let global = prefix.join(rel.replace('/', "\\"));
        fs::create_dir_all(global.parent().unwrap()).unwrap();
        fs::write(&global, "").unwrap();
        assert_eq!(
            pick_npm_script(bundled, Some(&prefix), rel),
            global
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn parses_rustup_settings_and_overrides() {
        let text = r#"
version = "12"
default_toolchain = "stable-x86_64-pc-windows-msvc"
[overrides]
"C:\\work\\old" = "1.88-x86_64-pc-windows-msvc"
"#;
        let s = parse_settings(text);
        assert_eq!(
            s.default_toolchain.as_deref(),
            Some("stable-x86_64-pc-windows-msvc")
        );
        assert!(under_override(
            Path::new(r"C:\WORK\old\sub"),
            &s.overrides
        ));
        assert!(!under_override(
            Path::new(r"C:\work\older"),
            &s.overrides
        ));
        assert_eq!(
            rustup_tool(r"C:\Users\USER\.cargo\bin\cargo.exe"),
            Some("cargo")
        );
    }

    #[test]
    fn explicit_paths_resolve_against_the_call_cwd_and_pathext() {
        let d = tmp("explicit");
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(d.join("bin").join("x.cmd"), "@echo hi\r\n").unwrap();
        fs::write(d.join("bin").join("x"), "#!/bin/sh\n").unwrap();
        fs::write(d.join("node.exe"), "").unwrap();
        match resolve("./bin/x.cmd", Some(&d)) {
            Target::Shim(p) => assert!(!p.to_string_lossy().contains('/'), "{}", p.display()),
            _ => panic!("relative .cmd must resolve against the cwd"),
        }
        match resolve("bin/x", Some(&d)) {
            Target::Shim(p) => assert!(p.to_string_lossy().ends_with("x.cmd")),
            _ => panic!("PATHEXT comes before the bare name"),
        }
        let bare = d.join("node").to_string_lossy().into_owned();
        assert!(matches!(resolve(&bare, None), Target::Direct { .. }));
        fs::remove_file(d.join("bin").join("x.cmd")).unwrap();
        assert!(matches!(resolve("bin/x", Some(&d)), Target::Missing), "never the sh script");
    }

    #[test]
    fn a_shim_with_script_logic_is_not_unwrapped() {
        let d = tmp("logic");
        fs::write(d.join("foo.exe"), "").unwrap();
        let shim = d.join("foo.cmd");
        let set = |body: &str| fs::write(&shim, body).unwrap();
        set("@echo off\r\n\"%~dp0foo.exe\" %*\r\n");
        assert!(generic_shim(&shim).is_some());
        for logic in [
            "@echo off\r\nset FOO_HOME=%~dp0data\r\n\"%~dp0foo.exe\" %*\r\n",
            "@echo off\r\ncd /d %~dp0\r\n\"%~dp0foo.exe\" %*\r\n",
            "@echo off\r\n\"%~dp0foo.exe\" --flag %*\r\n",
            "@echo off\r\nif exist x (echo y)\r\n\"%~dp0foo.exe\" %*\r\n",
        ] {
            set(logic);
            assert!(generic_shim(&shim).is_none(), "{logic}");
        }
    }

    #[test]
    fn python_launchers_are_bypassed_only_where_pythonsafepath_exists() {
        let mk = |tag: &str, dll: &str| {
            let d = tmp(tag);
            fs::create_dir_all(d.join("Scripts")).unwrap();
            fs::create_dir_all(d.join("Lib").join("site-packages").join("pytest")).unwrap();
            fs::write(d.join("python.exe"), "").unwrap();
            fs::write(d.join(dll), "").unwrap();
            fs::write(d.join("Scripts").join("pytest.exe"), "").unwrap();
            d.join("Scripts").join("pytest.exe")
        };
        let new = mk("py311", "python311.dll");
        assert!(matches!(python_console_script(&new), Some(Target::Direct { prefix, .. }) if prefix == ["-m", "pytest"]));
        assert!(python_console_script(&mk("py310", "python310.dll")).is_none());
    }
}
