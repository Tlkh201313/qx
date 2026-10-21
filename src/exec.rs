//! Process execution: direct spawn, shell fallback, capture with timeout.

use crate::resolve::{self, Target};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Hand children a clean, bounded environment.
///
/// Windows caps the environment block it will pass to a child. This machine's
/// PATH is thousands of characters of duplicated entries; when the block is
/// truncated, `PATHEXT` is lost and `cmd /c <tool>` starts answering
/// "not recognized". Deduplicating PATH keeps the block well under the limit,
/// and rebuilding it prevents a malformed tail from leaking into children.
pub fn prepare(cmd: &mut Command) {
    const ESSENTIAL: &[&str] = &[
        "SystemRoot",
        "windir",
        "SystemDrive",
        "ComSpec",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "APPDATA",
        "LOCALAPPDATA",
        "PROGRAMDATA",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
        "OS",
    ];

    let inherited: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    cmd.env_clear();

    let dirs = resolve::path_dirs();
    let path = std::env::join_paths(&dirs).unwrap_or_else(|_| {
        OsString::from(if cfg!(windows) {
            r"C:\Windows\System32;C:\Windows"
        } else {
            "/usr/local/bin:/usr/bin:/bin"
        })
    });
    cmd.env("PATH", path);
    cmd.env("PATHEXT", resolve::pathext_string());
    for key in ESSENTIAL {
        if let Some(v) = std::env::var_os(key) {
            cmd.env(key, v);
        }
    }
    for (key, value) in inherited {
        let name = key.to_string_lossy();
        if name.is_empty()
            || name.starts_with('=')
            || name.eq_ignore_ascii_case("PATH")
            || name.eq_ignore_ascii_case("PATHEXT")
            || ESSENTIAL.iter().any(|e| name.eq_ignore_ascii_case(e))
        {
            continue;
        }
        cmd.env(key, value);
    }
}

/// Node 22+ can reuse compiled bytecode across runs. Keep it in qx's own dir
/// and never override a cache the caller already chose.
pub fn apply_node_cache(cmd: &mut Command) {
    if std::env::var_os("NODE_COMPILE_CACHE").is_some() {
        return;
    }
    if std::env::var("QX_NODE_CACHE").ok().as_deref() == Some("off") {
        return;
    }
    if let Some(base) = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("TEMP")) {
        cmd.env(
            "NODE_COMPILE_CACHE",
            PathBuf::from(base).join("qx").join("node-cache"),
        );
    }
}

/// Skip startup hooks that import all of pip and importlib.resources on every
/// Python start. Default `tls` keeps system-cert injection through a lazy
/// shim; `1` skips it (max speed); `off`/`0` runs the hooks untouched.
pub fn apply_python_fast(cmd: &mut Command) {
    let mode = std::env::var("QX_PYTHON_FAST").ok();
    let (dir_name, certs) = match mode.as_deref() {
        Some("off" | "0") => return,
        Some("1") => (PYTHON_FAST_DIR, CERTS_STUB),
        _ => (PYTHON_FAST_TLS_DIR, CERTS_TLS_STUB),
    };
    let Some(base) =
        std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("TEMP"))
    else {
        return;
    };
    let stub = PathBuf::from(base).join("qx").join(dir_name);
    if ensure_python_stub(&stub, certs).is_err() {
        return;
    }
    let mut dirs = vec![stub];
    if let Some(existing) = std::env::var_os("PYTHONPATH") {
        dirs.extend(std::env::split_paths(&existing));
    }
    if let Ok(path) = std::env::join_paths(dirs) {
        cmd.env("PYTHONPATH", path);
    }
}

/// Bump the directory name when any stub changes. A fresh directory is never
/// rewritten while another qx process may be importing from it.
const PYTHON_FAST_DIR: &str = "python-fast-v2";
const PYTHON_FAST_TLS_DIR: &str = "python-fast-tls-v6";

const CERTS_STUB: &str = "def bootstrap():\n    return None\n";

/// Keeps the system trust store for requests/urllib3, but injects it directly
/// instead of importing all of pip. pip's `--trusted-host` adapter patch runs
/// lazily when pip's session module is imported.
const CERTS_TLS_STUB: &str = r#"_bootstrapped = False


def bootstrap():
    global _bootstrapped
    if _bootstrapped:
        return
    _bootstrapped = True
    try:
        _done = False

        def _inject():
            nonlocal _done
            if _done:
                return
            _done = True
            try:
                import ssl
                import sys

                # pip's own copy first: a second truststore patched over ssl
                # makes pip's copy subclass the patched class and recurse.
                try:
                    from pip._vendor import truststore
                except ImportError:
                    try:
                        import truststore
                    except ImportError:
                        return

                ssl.SSLContext = truststore.SSLContext
                for name in ("urllib3.util.ssl_", "requests.adapters"):
                    module = sys.modules.get(name)
                    if module is not None:
                        module.SSLContext = truststore.SSLContext
                adapters = sys.modules.get("requests.adapters")
                if adapters is not None and getattr(adapters, "_preloaded_ssl_context", None) is not None:
                    adapters._preloaded_ssl_context = truststore.SSLContext(ssl.PROTOCOL_TLS_CLIENT)

                def _patch_pip_adapters(module):
                    try:
                        insecure_cc = getattr(module, "InsecureCacheControlAdapter", None)
                        insecure_http = getattr(module, "InsecureHTTPAdapter", None)
                        if insecure_cc is None or insecure_http is None:
                            return
                        if getattr(insecure_cc, "_pip_system_certs_patched", False):
                            return
                        base_method = getattr(insecure_http, "get_connection_with_tls_context", None)
                        if base_method is None:
                            return
                        import inspect

                        expected = {"self", "request", "verify", "proxies", "cert"}
                        if set(inspect.signature(base_method).parameters) != expected:
                            return

                        def insecure_get_connection_with_tls_context(
                            self, request, verify, proxies=None, cert=None
                        ):
                            return super(self.__class__, self).get_connection_with_tls_context(
                                request, verify=False, proxies=proxies, cert=cert
                            )

                        insecure_cc.get_connection_with_tls_context = (
                            insecure_get_connection_with_tls_context
                        )
                        insecure_cc._pip_system_certs_patched = True
                        insecure_http.get_connection_with_tls_context = (
                            insecure_get_connection_with_tls_context
                        )
                        insecure_http._pip_system_certs_patched = True
                    except Exception:
                        pass

                class PipSessionFinder:
                    def find_spec(self, name, path=None, target=None):
                        if name != "pip._internal.network.session":
                            return None
                        try:
                            from importlib.machinery import PathFinder

                            spec = PathFinder.find_spec(name, path)
                            if spec is None or spec.loader is None:
                                return None
                            loader = spec.loader
                            original = loader.exec_module

                            def exec_module(module, _original=original):
                                _original(module)
                                _patch_pip_adapters(module)

                            loader.exec_module = exec_module
                            return spec
                        except Exception:
                            return None

                sys.meta_path.insert(0, PipSessionFinder())
            except Exception:
                pass

        class SslFinder:
            # Only processes that import ssl pay for truststore; a script that
            # never touches TLS starts without it.
            armed = True

            def find_spec(self, name, path=None, target=None):
                if name != "ssl" or not self.armed:
                    return None
                self.armed = False
                try:
                    from importlib.machinery import PathFinder

                    spec = PathFinder.find_spec(name, path)
                    if spec is None or spec.loader is None:
                        return None
                    original = spec.loader.exec_module

                    def exec_module(module, _original=original):
                        _original(module)
                        _inject()

                    spec.loader.exec_module = exec_module
                    return spec
                except Exception:
                    return None

        import sys

        sys.meta_path.insert(0, SslFinder())
    except Exception:
        pass
"#;

const PYNVML_STUB: &str = r#"# qx fast Python: behavior-equivalent replacement for NVIDIA's
# _pynvml_redirector. The original imports importlib.abc (and through it
# importlib.resources, pathlib, inspect, shutil) only to install a finder that
# emits deprecation warnings. This version emits the same warnings without
# those imports. If pynvml ever adds real finder logic here, disable
# QX_PYTHON_FAST=off.
import sys
import warnings

PYNVML_MSG = (
    "The pynvml package is deprecated. Please install nvidia-ml-py instead. "
    "If you did not install pynvml directly, please report this to the maintainers of "
    "the package that installed pynvml for you."
)

PYNVML_UTILS_MSG = (
    "The pynvml_utils module is deprecated. The source will remain accessible on "
    "Github at https://github.com/gpuopenanalytics/pynvml/."
)


class PynvmlFinder:
    def __init__(self):
        self.has_warned_pynvml = False
        self.has_warned_pynvml_utils = False

    def find_spec(self, name, path=None, target=None):
        if name.startswith("pynvml") and not self.has_warned_pynvml:
            warnings.warn(PYNVML_MSG, FutureWarning, stacklevel=2)
            self.has_warned_pynvml = True

        if name.startswith("pynvml_utils") and not self.has_warned_pynvml_utils:
            warnings.warn(PYNVML_UTILS_MSG, FutureWarning, stacklevel=2)
            self.has_warned_pynvml_utils = True
        return None


finder = PynvmlFinder()
sys.meta_path.insert(0, finder)
"#;

fn ensure_python_stub(dir: &Path, certs: &str) -> std::io::Result<()> {
    let pkg = dir.join("pip_system_certs");
    let ok = dir.join(".ok");
    if ok.is_file()
        && pkg.join("bootstrap.py").is_file()
        && dir.join("_pynvml_redirector.py").is_file()
    {
        return Ok(());
    }
    std::fs::create_dir_all(&pkg)?;
    write_atomic(&pkg.join("__init__.py"), "")?;
    write_atomic(&pkg.join("bootstrap.py"), certs)?;
    write_atomic(&dir.join("_pynvml_redirector.py"), PYNVML_STUB)?;
    write_atomic(&ok, "")
}

/// Two qx processes starting together must never import a half-written stub.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn is_python(program: &str, exe: &Path) -> bool {
    let stem = |p: &Path| {
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let lower = name.to_ascii_lowercase();
        lower
            .strip_suffix(".exe")
            .unwrap_or(&lower)
            .to_string()
    };
    let name = stem(Path::new(program));
    name == "py" || name.starts_with("python") || stem(exe).starts_with("python")
}

pub struct Outcome {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
    pub timed_out: bool,
    /// The 16 MiB capture limit dropped output.
    pub cut: bool,
}

impl From<crate::builtin::BuiltinOut> for Outcome {
    fn from(b: crate::builtin::BuiltinOut) -> Self {
        Outcome { code: b.code, stdout: b.stdout, stderr: b.stderr, elapsed: b.elapsed, timed_out: false, cut: false }
    }
}

/// Child-only defaults for agent work. Never set on qx itself: `npm` chooses
/// its fast path by reading qx's environment, and a user's explicit value wins.
pub(crate) fn apply_child_defaults(cmd: &mut Command, npm: bool) {
    const COMMON: &[(&str, &str)] = &[
        ("CARGO_TERM_COLOR", "never"),
        ("CARGO_TERM_PROGRESS_WHEN", "never"),
        // An agent has no terminal, so a pager or editor prompt is a hang waiting to happen.
        ("GIT_EDITOR", "true"),
        ("GIT_SEQUENCE_EDITOR", "true"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
        ("PYTHONIOENCODING", "utf-8"),
    ];
    const NPM: &[(&str, &str)] = &[
        ("NPM_CONFIG_FUND", "false"),
        ("NPM_CONFIG_UPDATE_NOTIFIER", "false"),
    ];
    for (key, value) in COMMON {
        if std::env::var_os(key).is_none() {
            cmd.env(key, value);
        }
    }
    if npm {
        for (key, value) in NPM {
            if std::env::var_os(key).is_none() {
                cmd.env(key, value);
            }
        }
    }
}

fn prepend_path(cmd: &mut Command, dir: &std::path::Path) {
    let key = dir
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_ascii_lowercase();
    let mut dirs = vec![dir.to_path_buf()];
    for d in resolve::path_dirs() {
        let k = d
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase();
        if k != key {
            dirs.push(d);
        }
    }
    if let Ok(path) = std::env::join_paths(dirs) {
        cmd.env("PATH", path);
    }
}

pub fn build_command(program: &str, args: &[String]) -> Result<Command, String> {
    build_command_at(program, args, None)
}

pub fn build_command_at(
    program: &str,
    args: &[String],
    cwd: Option<&std::path::Path>,
) -> Result<Command, String> {
    build_command_env(program, args, cwd, &[])
}

/// `build_command_at` for a call that will also get `envs` (`NAME=value` prefixes): a toolchain
/// variable among them must reach rustup's proxy, so the direct-toolchain shortcut stands aside.
pub fn build_command_env(
    program: &str,
    args: &[String],
    cwd: Option<&std::path::Path>,
    envs: &[(String, String)],
) -> Result<Command, String> {
    // A cmdlet is not a CreateProcess target; a `.ps1` is handled by `Target::Ps1`.
    if verb_noun(program) {
        return powershell_call(program, args);
    }
    let npmish = npmish(program);
    if let Some(r) = resolve::rustup_direct(program, args, cwd, envs) {
        let mut c = Command::new(r.exe);
        c.args(args);
        prepare(&mut c);
        prepend_path(&mut c, &r.bin);
        c.env("RUSTUP_TOOLCHAIN", &r.toolchain);
        apply_child_defaults(&mut c, npmish);
        return Ok(c);
    }
    // `dir /b`, `del /q`: a cmd.exe internal, or a coreutils twin that reads `/b` as a path.
    if cmd_internal(program)
        && (args.iter().any(|a| a.starts_with('/')) || matches!(resolve::resolve(program, cwd), Target::Missing))
    {
        let mut line = program.to_string();
        for a in args {
            line.push(' ');
            line.push_str(&cmd_quote(a));
        }
        let mut c = cmd_exe(&line);
        prepare(&mut c);
        apply_child_defaults(&mut c, false);
        return Ok(c);
    }
    command_for(program, resolve::resolve(program, cwd), args)
}

fn npmish(program: &str) -> bool {
    crate::npm::is_npm(program) || crate::npm::is_npx(program)
}

/// One resolved target to the command that runs it. `build_command_at` and the npm fast
/// path share this, so a script step and a direct call cannot drift apart.
pub fn command_for(program: &str, target: Target, args: &[String]) -> Result<Command, String> {
    let npmish = npmish(program);
    match target {
        Target::Direct { exe, prefix } => {
            let node = exe
                .file_name()
                .map(|n| n.to_string_lossy().eq_ignore_ascii_case("node.exe"))
                .unwrap_or(false);
            let python = is_python(program, &exe);
            let skip_profile = exe
                .file_stem()
                .is_some_and(|s| s.eq_ignore_ascii_case("powershell"))
                && std::env::var("QX_PS_PROFILE").ok().as_deref() != Some("1")
                && !args.iter().any(|a| a.to_ascii_lowercase().starts_with("-nop"));
            // A venv's `python.exe` is a redirector: it re-execs the base interpreter. Running
            // the base directly with `__PYVENV_LAUNCHER__` set is the same session, one process
            // fewer.
            let venv = crate::python::venv_redirect(&exe);
            let program = match &venv {
                Some(v) => v.base.as_path(),
                None => exe.as_path(),
            };
            let mut c = Command::new(program);
            c.args(prefix);
            if skip_profile {
                // A profile adds 300+ ms and can print into the output.
                c.arg("-NoProfile");
            }
            match cmd_c_line(&exe, args) {
                // The text after `/c` is a command line: raw, as `cmd_exe` passes it.
                #[cfg(windows)]
                Some(raw) => {
                    use std::os::windows::process::CommandExt;
                    c.raw_arg(raw);
                }
                _ => {
                    c.args(args);
                }
            }
            prepare(&mut c);
            if let Some(v) = &venv {
                // After `prepare`, which clears the inherited environment: this is what makes
                // the base interpreter report the venv's `sys.prefix` and `sys.executable`.
                c.env("__PYVENV_LAUNCHER__", &v.launcher);
            }
            if node {
                apply_node_cache(&mut c);
            }
            if python {
                apply_python_fast(&mut c);
            }
            if let Some(git) = git_root(&c) {
                // The mingw64 binary was reached past Git\cmd, which would have set this PATH.
                prepend_path(&mut c, &git.join("mingw64").join("bin"));
                prepend_path(&mut c, &git.join("usr").join("bin"));
            }
            apply_child_defaults(&mut c, npmish);
            Ok(c)
        }
        Target::Shim(p) => {
            let mut line = cmd_quote(&p.to_string_lossy());
            for a in args {
                line.push(' ');
                line.push_str(&cmd_quote(a));
            }
            let mut c = cmd_exe(&line);
            prepare(&mut c);
            apply_python_fast(&mut c);
            apply_child_defaults(&mut c, npmish);
            Ok(c)
        }
        Target::Ps1(p) => {
            let mut c = Command::new("powershell.exe");
            let path = p.to_string_lossy().into_owned();
            c.args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                path.as_str(),
            ]);
            c.args(args);
            prepare(&mut c);
            apply_python_fast(&mut c);
            apply_child_defaults(&mut c, npmish);
            Ok(c)
        }
        Target::Missing => Err(not_found(program)),
    }
}

/// `cmd [/d] [/s] [/q] /c|/k <text>`: the raw argument string cmd.exe needs. Rust's own quoting turns
/// every inner quote into `\"`, which cmd prints literally. A single text argument is the command line
/// as written; several are joined, with all but the first quoted when they need it.
#[cfg_attr(not(windows), allow(dead_code))]
fn cmd_c_line(exe: &Path, args: &[String]) -> Option<String> {
    if !exe.file_stem().is_some_and(|s| s.eq_ignore_ascii_case("cmd")) {
        return None;
    }
    let mut i = 0;
    while args.get(i).is_some_and(|a| ["/d", "/s", "/q"].iter().any(|f| a.eq_ignore_ascii_case(f))) {
        i += 1;
    }
    let mode = args.get(i).filter(|a| a.eq_ignore_ascii_case("/c") || a.eq_ignore_ascii_case("/k"))?;
    let rest = &args[i + 1..];
    if rest.is_empty() || rest.iter().any(|a| a.contains(['\n', '\r', '\0'])) {
        return None;
    }
    let text = match rest {
        [one] => one.clone(),
        many => many
            .iter()
            .enumerate()
            .map(|(n, a)| if n == 0 { a.clone() } else { cmd_quote(a) })
            .collect::<Vec<_>>()
            .join(" "),
    };
    Some(format!("/d /s {mode} \"{text}\""))
}

/// The message for a program that could not be resolved. A built-in that declined its
/// arguments is not a typo, so the message says so and names where qx looked.
pub fn not_found(program: &str) -> String {
    if crate::builtin::is_builtin(program) {
        format!(
            "command not found: {program} (the qx built-in does not support these arguments, and no {program}.exe is on PATH or in Git for Windows usr\\bin)"
        )
    } else {
        format!("command not found: {program}")
    }
}

fn cmd_internal(program: &str) -> bool {
    ["dir", "type", "del", "erase", "copy", "move"].iter().any(|n| program.eq_ignore_ascii_case(n))
}

/// The Git for Windows root when `cmd` runs a binary from its `mingw64\bin` or `usr\bin`.
fn git_root(cmd: &Command) -> Option<PathBuf> {
    let bin = Path::new(cmd.get_program()).parent()?;
    let named = |p: &Path, n: &str| p.file_name().is_some_and(|f| f.eq_ignore_ascii_case(n));
    if named(bin, "bin") {
        // The coreutils fallback runs <root>\usr\bin\x.exe, which needs the same PATH.
        if let Some(usr) = bin.parent().filter(|p| named(p, "usr")) {
            return usr.parent().map(Path::to_path_buf);
        }
    }
    let mingw = bin.parent()?;
    (named(bin, "bin") && named(mingw, "mingw64")).then(|| mingw.parent().map(Path::to_path_buf))?
}

/// `cmd.exe` folds a line over ~8 KiB into "The command line is too long."
const CMD_LINE_CAP: usize = 8_000;
/// `-EncodedCommand` carries the script in argv as base64 (~2.7 bytes per character).
const PS_SCRIPT_CAP: usize = 12_000;

pub fn build_shell(command: &str, shell: &str) -> Result<Command, String> {
    let choice = if shell == "auto" { pick_shell(command) } else { shell };
    match choice {
        "bash" => {
            // `ls | head` has no cmd.exe equivalent: bash is the only shell that runs it.
            let b = resolve::git_bash().ok_or_else(|| {
                "this line needs a POSIX utility (ls/cat/grep/head…), but Git Bash was not found; \
                 pass `program` + `args`, or install Git for Windows"
                    .to_string()
            })?;
            let mut c = Command::new(b);
            c.args(["-c", command]);
            prepare(&mut c);
            apply_python_fast(&mut c);
            apply_child_defaults(&mut c, false);
            Ok(c)
        }
        "cmd" => {
            if command.len() > CMD_LINE_CAP {
                return Err(format!(
                    "command line is {} characters; cmd.exe stops near {CMD_LINE_CAP}. \
                     Pass `program` + `args` instead of one long line",
                    command.len()
                ));
            }
            let mut c = cmd_exe(command);
            prepare(&mut c);
            apply_python_fast(&mut c);
            apply_child_defaults(&mut c, false);
            Ok(c)
        }
        "powershell" => {
            if command.len() > PS_SCRIPT_CAP {
                return Err(format!(
                    "script is {} characters; `-EncodedCommand` carries it in one argv \
                     (limit ~{PS_SCRIPT_CAP}). Split it, or pass `program` + `args`",
                    command.len()
                ));
            }
            let mut c = Command::new("powershell.exe");
            // The user's profile costs 300+ ms and changes output; the prelude keeps
            // piped output UTF-8 and drops progress-bar noise. `-NonInteractive` turns a
            // prompt into an error instead of a hang, and `-EncodedCommand` deletes the
            // quoting layer that cmd.exe and `Command` used to mangle.
            c.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-EncodedCommand"]);
            c.arg(encoded_powershell(command));
            prepare(&mut c);
            apply_python_fast(&mut c);
            apply_child_defaults(&mut c, false);
            Ok(c)
        }
        "none" => Err("shell=none needs program+args".to_string()),
        other => Err(format!("unknown shell: {other}")),
    }
}

/// The script behind `-EncodedCommand`: the prelude keeps its own line so a parse error
/// points at the command, and the trailer hands the native exit code back
/// (`-Command` folds every failing native call to 1).
fn encoded_powershell(command: &str) -> String {
    let script = format!(
        "$ProgressPreference='SilentlyContinue';try{{[Console]::OutputEncoding=[Text.Encoding]::UTF8}}catch{{}}\n{command}\n$qxok=$?;if($LASTEXITCODE){{exit $LASTEXITCODE}};if(!$qxok){{exit 1}}"
    );
    base64_utf16le(&script)
}

/// PowerShell decodes `-EncodedCommand` as base64 over UTF-16LE; one alphabet, no dependency.
fn base64_utf16le(s: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The one way qx starts cmd.exe. The line goes in as raw command-line text: `Command`'s own
/// quoting turns inner quotes into `\"`, which cmd hands to node/python as literal backslashes.
/// `/s` plus the outer quotes make cmd strip exactly that outer pair, so a quoted
/// `"C:\Program Files\x.exe" "a b"` survives.
fn cmd_exe(line: &str) -> Command {
    let mut c = Command::new("cmd.exe");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.raw_arg(format!("/d /s /c \"{line}\""));
    }
    #[cfg(not(windows))]
    c.args(["/d", "/s", "/c", line]);
    c
}

/// `auto` picks the shell that can parse the line: PowerShell syntax, else a POSIX
/// utility cmd.exe cannot run, else cmd. Getting this wrong is the usual cause of a
/// wasted "'$x' is not recognized" round trip.
fn pick_shell(s: &str) -> &'static str {
    if powershell_syntax(s) {
        "powershell"
    } else if posix_syntax(s) {
        "bash"
    } else {
        "cmd"
    }
}

/// Command positions: `;`, `&&`, `||`, a newline, a pipe, or the brace/paren that opens a
/// script block. A lone `&` is not a separator — it is the PowerShell call operator.
fn segments(s: &str) -> Vec<&str> {
    s.split(['|', ';', '\n', '{', '}', '(', ')'])
        .flat_map(|piece| piece.split("&&"))
        .flat_map(|piece| piece.split("||"))
        .collect()
}

fn first_word(seg: &str) -> &str {
    seg.split_whitespace().next().unwrap_or("")
}

/// PowerShell-only syntax. Quoted text never starts a command segment, so
/// `git commit -m "Add Get-Thing support"` stays on cmd.
fn powershell_syntax(s: &str) -> bool {
    const ANYWHERE: &[&str] = &[
        "$env:",
        "$LASTEXITCODE",
        "$null",
        "$true",
        "$false",
        "$_",
        "]::",
        "@'",
        "@\"",
        "2>$null",
        " -eq ",
        " -ne ",
        " -gt ",
        " -ge ",
        " -lt ",
        " -le ",
        " -like ",
        " -match ",
        " -contains ",
    ];
    if ANYWHERE.iter().any(|m| s.contains(m)) {
        return true;
    }
    segments(s).into_iter().any(|seg| {
        let w = first_word(seg);
        // `$(…)` and `${…}` are bash command substitution, not a PS variable.
        if w.is_empty() || w.starts_with("$(") || w.starts_with("${") {
            return false;
        }
        // A cmdlet, a `$variable` in command position (`$x = 1`, `$p.Foo()`), the call
        // operator (`& $exe`), a script block, or a block keyword.
        verb_noun(w)
            || w.starts_with('$')
            || w == "&"
            || w.starts_with("%{")
            || w.starts_with("?{")
            || w.starts_with("@(")
            || w.starts_with("@{")
            || matches!(w, "try" | "catch" | "finally" | "foreach" | "trap")
    })
}

/// `Get-Item`, `Invoke-CimMethod`: a cmdlet is `Verb-Noun` with both halves capitalised.
/// `docker-compose`, `git-log` and `rustc-workspace` are not cmdlets.
fn verb_noun(w: &str) -> bool {
    let Some((verb, noun)) = w.split_once('-') else {
        return false;
    };
    verb.len() > 2
        && verb.starts_with(|c: char| c.is_ascii_uppercase())
        && verb.chars().all(|c| c.is_ascii_alphabetic())
        && noun.starts_with(|c: char| c.is_ascii_uppercase())
        && noun.chars().all(|c: char| c.is_ascii_alphanumeric())
}

/// Utilities cmd.exe has no built-in for; a pipeline that runs one is a POSIX pipeline.
const BASH_ONLY: &[&str] = &[
    "awk", "basename", "cat", "chmod", "cp", "cut", "date", "df", "diff", "dirname", "du",
    "env", "find", "grep", "head", "ln", "ls", "md5sum", "mkdir", "mv", "nl", "printf",
    "pwd", "readlink", "realpath", "rev", "rm", "sed", "seq", "sha256sum", "shasum",
    "sleep", "sort", "stat", "tac", "tail", "tee", "test", "touch", "tr", "uniq", "uname",
    "wc", "which", "xargs",
];

fn posix_syntax(s: &str) -> bool {
    // Only markers cmd.exe cannot handle; plain `&&`, `|` and `2>&1` stay on cmd.
    const MARKERS: &[&str] = &[
        "$(", "${", "2>/dev/null", "|&", "<<", "/c/", "/mnt/", "`", "source ", "export ",
        "~/",
    ];
    // A Windows path in the line means the caller is not writing POSIX.
    if !s.contains('\\') && MARKERS.iter().any(|m| s.contains(m)) {
        return true;
    }
    let chained = s.contains('|') || s.contains('>') || s.contains(';');
    chained && segments(s).into_iter().any(|seg| BASH_ONLY.contains(&first_word(seg)))
}

/// Run a cmdlet given as `program`. Arguments become single-quoted literals, so `$var`,
/// quotes and redirection inside them are never re-evaluated.
fn powershell_call(program: &str, args: &[String]) -> Result<Command, String> {
    let mut script = program.to_string();
    for a in args {
        script.push(' ');
        script.push_str(&ps_quote(a));
    }
    if script.len() > PS_SCRIPT_CAP {
        return Err(format!(
            "script is {} characters; `-EncodedCommand` carries it in one argv (limit ~{PS_SCRIPT_CAP})",
            script.len()
        ));
    }
    let mut c = Command::new("powershell.exe");
    c.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-EncodedCommand"]);
    c.arg(encoded_powershell(&script));
    prepare(&mut c);
    apply_python_fast(&mut c);
    apply_child_defaults(&mut c, false);
    Ok(c)
}

/// Bare words stay bare (`-Path`, `src`); anything else is single-quoted, the one form
/// PowerShell never expands.
fn ps_quote(s: &str) -> String {
    let bare = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./\\:@".contains(c));
    if bare {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

fn cmd_quote(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".to_string();
    }
    // A bare `"` must be quoted too: left alone it flips cmd's quote state and a later `&` runs.
    if s.chars()
        .any(|c| c.is_whitespace() || "&|<>^()\"".contains(c))
    {
        // shortcut: a `%VAR%` inside a quoted arg is still expanded by cmd.exe; no escape exists there.
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        // `%^PATH%` names no variable, so cmd leaves it alone, then drops the caret.
        s.replace('%', "%^")
    }
}

pub(crate) const CAPTURE_CAP: usize = 16 * 1024 * 1024;

pub(crate) struct Capture {
    data: std::sync::Mutex<Vec<u8>>,
    truncated: std::sync::atomic::AtomicBool,
    /// Set by `snapshot`: a daemon still writing to the pipe is drained and discarded, so it
    /// cannot grow a hidden buffer after the call has returned.
    closed: std::sync::atomic::AtomicBool,
}

fn capture_from_cap<R: Read + Send + 'static>(
    mut r: R,
    cap: usize,
) -> (std::sync::Arc<Capture>, mpsc::Receiver<()>) {
    let sink = std::sync::Arc::new(Capture {
        data: std::sync::Mutex::new(Vec::new()),
        truncated: std::sync::atomic::AtomicBool::new(false),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let shared = sink.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 64 * 1024];
        loop {
            match r.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let mut data = match shared.data.lock() {
                        Ok(d) => d,
                        Err(_) => break,
                    };
                    if shared.closed.load(std::sync::atomic::Ordering::Relaxed) {
                        continue; // keep draining so the writer never blocks, but keep nothing
                    }
                    if data.len() < cap {
                        let take = n.min(cap - data.len());
                        data.extend_from_slice(&chunk[..take]);
                        if take < n {
                            shared.truncated.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    } else {
                        shared.truncated.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(());
    });
    (sink, rx)
}

fn snapshot(c: &Capture) -> (Vec<u8>, bool) {
    let data = match c.data.lock() {
        Ok(mut d) => {
            c.closed.store(true, std::sync::atomic::Ordering::Relaxed);
            std::mem::take(&mut *d)
        }
        Err(_) => Vec::new(),
    };
    (
        data,
        c.truncated.load(std::sync::atomic::Ordering::Relaxed),
    )
}

#[cfg(windows)]
mod job {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    type Handle = *mut c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> Handle;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, code: u32) -> i32;
        fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, len: u32) -> i32;
        fn CloseHandle(h: Handle) -> i32;
    }

    const EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const KILL_ON_JOB_CLOSE: u32 = 0x2000;

    /// JOBOBJECT_EXTENDED_LIMIT_INFORMATION; only `limit_flags` is ever set.
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimits {
        per_process_time: i64,
        per_job_time: i64,
        limit_flags: u32,
        min_working_set: usize,
        max_working_set: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
        io: [u64; 6],
        process_memory: usize,
        job_memory: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }

    /// Best-effort process-tree container. If qx itself dies mid-command (client closed, killed),
    /// closing the job kills the tree; `release` lifts that once the command ended on its own, so a
    /// background process the command left running on purpose is not disturbed.
    pub struct Job(Handle);

    impl Job {
        pub fn assign(child: &std::process::Child) -> Option<Job> {
            unsafe {
                let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                if AssignProcessToJobObject(job, child.as_raw_handle() as Handle) == 0 {
                    CloseHandle(job);
                    return None;
                }
                let job = Job(job);
                job.set_flags(KILL_ON_JOB_CLOSE);
                Some(job)
            }
        }

        fn set_flags(&self, flags: u32) {
            let info = ExtendedLimits { limit_flags: flags, ..Default::default() };
            unsafe {
                SetInformationJobObject(
                    self.0,
                    EXTENDED_LIMIT_INFORMATION,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<ExtendedLimits>() as u32,
                );
            }
        }

        pub fn release(&self) {
            self.set_flags(0);
        }

        pub fn terminate(&self, code: u32) {
            unsafe {
                TerminateJobObject(self.0, code);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Block until the child exits or the remaining timeout elapses.
///
/// Polling `try_wait` every 15 ms made short commands (`npm run` scripts,
/// `cmd /c exit`) land on 15 ms boundaries. Waiting on the process handle
/// removes that quantization and wakes as soon as the child is gone.
#[cfg(windows)]
fn wait_until_exit(child: &std::process::Child, timeout: Option<Duration>) {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    #[link(name = "kernel32")]
    extern "system" {
        fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
    }
    const INFINITE: u32 = u32::MAX;
    let millis = match timeout {
        Some(d) => d.as_millis().min(INFINITE as u128 - 1) as u32,
        None => INFINITE,
    };
    let rc = unsafe { WaitForSingleObject(child.as_raw_handle(), millis) };
    if rc == u32::MAX {
        // WAIT_FAILED: fall back to a short sleep instead of spinning.
        let nap = timeout
            .map(|t| t.min(Duration::from_millis(15)))
            .unwrap_or(Duration::from_millis(15));
        std::thread::sleep(nap);
    }
}

#[cfg(not(windows))]
fn wait_until_exit(_child: &std::process::Child, timeout: Option<Duration>) {
    let nap = timeout
        .map(|t| t.min(Duration::from_millis(50)))
        .unwrap_or(Duration::from_millis(50));
    std::thread::sleep(nap);
}

/// Set once by the CLI when its stdin is a pipe or file rather than a terminal.
pub static INHERIT_STDIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

thread_local! {
    /// Set by the MCP worker running this thread's command; `notifications/cancelled` raises it.
    static CANCEL: std::cell::RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>> = const { std::cell::RefCell::new(None) };
}

pub fn set_cancel(flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>) {
    CANCEL.with(|c| *c.borrow_mut() = flag);
}

/// Wait for the child; past `timeout` (or once cancelled) kill its whole tree. Returns (exit code, timed out).
fn wait_or_kill(child: &mut std::process::Child, timeout: Option<Duration>) -> std::io::Result<(i32, bool)> {
    let start = Instant::now();
    #[cfg(windows)]
    let job = job::Job::assign(child);
    loop {
        if let Some(status) = child.try_wait()? {
            #[cfg(windows)]
            if let Some(j) = &job {
                j.release();
            }
            return Ok((status.code().unwrap_or(1), false));
        }
        let remaining = timeout.map(|t| t.saturating_sub(start.elapsed()));
        let cancel = CANCEL.with(|c| c.borrow().clone());
        let cancelled = cancel.as_ref().is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed));
        if cancelled || remaining.is_some_and(|r| r.is_zero()) {
            let pid = child.id();
            // Enumerate the tree while the root is still alive:
            // after the root exits, taskkill can no longer find
            // children such as `ping`.
            let taskkill = std::env::var_os("SystemRoot")
                .map(PathBuf::from)
                .map(|p| p.join("System32").join("taskkill.exe"))
                .filter(|p| p.is_file())
                .unwrap_or_else(|| PathBuf::from("taskkill"));
            let mut tk = Command::new(taskkill);
            tk.args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            prepare(&mut tk);
            let _ = tk.status();
            #[cfg(windows)]
            if let Some(j) = &job {
                // Catches anything that was created after
                // taskkill took its snapshot.
                j.terminate(124);
            }
            let _ = child.kill();
            // Bounded wait: if a grandchild still holds handles,
            // do not hang the whole qx process.
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
            return Ok((124, true));
        }
        // A cancel cannot wake the wait, so a cancellable command is polled.
        let nap = Duration::from_millis(100);
        wait_until_exit(child, if cancel.is_some() { Some(remaining.map_or(nap, |r| r.min(nap))) } else { remaining });
    }
}

/// Run with captured stdout/stderr. Necessary for compaction and MCP replies.
pub fn run_captured(mut cmd: Command, timeout: Option<Duration>) -> std::io::Result<Outcome> {
    // The CLI forwards piped input (`echo x | qx grep x`); an MCP server has none to give.
    cmd.stdin(if INHERIT_STDIN.load(std::sync::atomic::Ordering::Relaxed) { Stdio::inherit() } else { Stdio::null() });
    let start = Instant::now();
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;

    let (out_sink, out_done) = capture_from_cap(child.stdout.take().expect("stdout piped"), CAPTURE_CAP);
    let (err_sink, err_done) = capture_from_cap(child.stderr.take().expect("stderr piped"), CAPTURE_CAP);

    let (code, timed_out) = wait_or_kill(&mut child, timeout)?;

    // Grandchildren can hold the pipe open; don't block forever, but keep any
    // partial output the readers already captured.
    let drain = Instant::now() + Duration::from_millis(250);
    let _ = out_done.recv_timeout(drain.saturating_duration_since(Instant::now()));
    let _ = err_done.recv_timeout(drain.saturating_duration_since(Instant::now()));
    let (so, so_cut) = snapshot(&out_sink);
    let (se, se_cut) = snapshot(&err_sink);

    let mut stdout = decode_owned(so);
    let mut stderr = decode_owned(se);
    if so_cut {
        stdout.push_str("\n[qx] stdout truncated at 16 MiB\n");
    }
    if se_cut {
        stderr.push_str("\n[qx] stderr truncated at 16 MiB\n");
    }

    Ok(Outcome {
        code,
        stdout,
        stderr,
        elapsed: start.elapsed(),
        timed_out,
        cut: so_cut || se_cut,
    })
}

/// Run one `command` through `shell`, with env prefixes, a cwd and one retry for a startup
/// flake that is not the command's fault.
pub fn run_shell(
    command: &str,
    shell: &str,
    envs: &[(String, String)],
    cwd: &Option<PathBuf>,
    timeout: Option<Duration>,
) -> Result<Outcome, String> {
    for attempt in 0..2 {
        let mut cmd = build_shell(command, shell)?;
        apply_envs(&mut cmd, envs);
        apply_cwd(&mut cmd, cwd);
        let out = run_captured(cmd, timeout).map_err(|e| format!("failed to start: {e}"))?;
        if attempt == 0 && msys_startup_flake(&out) {
            continue;
        }
        return Ok(out);
    }
    unreachable!("the loop returns on the second attempt")
}

/// msys bash sometimes cannot create its shared-memory object under a restricted token and
/// dies with 0xC0000142 before running the command. Startup is ~50 ms, so retry once
/// instead of handing the model a cryptic NTSTATUS.
fn msys_startup_flake(o: &Outcome) -> bool {
    const STATUS_DLL_INIT_FAILED: i32 = -1073741502;
    o.code == STATUS_DLL_INIT_FAILED && o.stderr.to_ascii_lowercase().contains("msys")
}

/// Run commands in order and stop at the first failure, like npm's
/// pre/main/post scripts. Output is concatenated; the code is the last one run.
pub fn run_chain(cmds: Vec<Command>, timeout: Option<Duration>) -> std::io::Result<Outcome> {
    let start = Instant::now();
    let mut all = Outcome {
        code: 0,
        stdout: String::new(),
        stderr: String::new(),
        elapsed: Duration::ZERO,
        timed_out: false,
        cut: false,
    };
    for cmd in cmds {
        let o = run_captured(cmd, timeout.map(|t| t.saturating_sub(start.elapsed())))?;
        all.stdout.push_str(&o.stdout);
        all.stderr.push_str(&o.stderr);
        all.code = o.code;
        all.timed_out = o.timed_out;
        all.cut |= o.cut;
        if o.code != 0 {
            break;
        }
    }
    all.elapsed = start.elapsed();
    Ok(all)
}

pub fn run_stream(cmd: &mut Command, timeout: Option<Duration>) -> std::io::Result<i32> {
    let mut child = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(wait_or_kill(&mut child, timeout)?.0)
}

pub(crate) fn decode(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    let s = String::from_utf8_lossy(bytes);
    if s.contains('\0') { s.replace('\0', "") } else { s.into_owned() }
}

/// `decode` for a buffer the caller owns: valid UTF-8 (the usual case) is validated in place instead of
/// being copied. Same result as `decode`.
pub(crate) fn decode_owned(bytes: Vec<u8>) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return decode(&bytes);
    }
    let mut s = String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    if s.contains('\0') {
        s.retain(|c| c != '\0');
    }
    s
}

/// Give a child the `NAME=value` prefix of its command line. qx's own environment is never touched.
pub fn apply_envs(cmd: &mut Command, envs: &[(String, String)]) {
    cmd.envs(envs.iter().map(|(k, v)| (k, v)));
}

/// `npm::direct` reads npm_* settings from the process env, so a prefix that sets one goes to real npm.
pub fn sets_npm_config(envs: &[(String, String)]) -> bool {
    envs.iter().any(|(k, _)| k.to_ascii_lowercase().starts_with("npm_"))
}

pub fn apply_cwd(cmd: &mut Command, cwd: &Option<PathBuf>) {
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
}

/// Leading `NAME=value` words, read the way POSIX shells do (`CI=1 npm test`),
/// which cmd.exe rejects. Only plain values are peeled: a `$`, `%`, backtick or
/// shell operator in the value stays with the shell. All-assignment input is
/// left whole. shortcut: the env reaches the whole line, not just its first command.
pub fn peel_env(s: &str) -> (Vec<(String, String)>, &str) {
    let mut envs = Vec::new();
    let mut rest = s.trim_start();
    while let Some(eq) = rest.find('=') {
        let name = &rest[..eq];
        if name.is_empty()
            || name.starts_with(|c: char| c.is_ascii_digit())
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            break;
        }
        let after = &rest[eq + 1..];
        let (val, tail) = match after.chars().next() {
            Some(q @ ('"' | '\'')) => match after[1..].find(q) {
                Some(e) => (&after[1..=e], &after[e + 2..]),
                None => break,
            },
            _ => after.split_at(after.find(char::is_whitespace).unwrap_or(after.len())),
        };
        let bare = !after.starts_with(['"', '\'']);
        let bad = |c: char| "$%`&|<>;()\n\r".contains(c) || (bare && "*?\"'".contains(c));
        if val.contains(bad) || !(tail.is_empty() || tail.starts_with(char::is_whitespace)) {
            break;
        }
        envs.push((name.to_string(), val.to_string()));
        rest = tail.trim_start();
    }
    if rest.is_empty() {
        return (Vec::new(), s);
    }
    (envs, rest)
}

/// Tokenize a command line only when it needs no shell at all.
/// Returns None for quotes, pipes, redirects, globs, variables, or built-ins
/// that only a shell can implement (cd).
pub fn split_simple(s: &str) -> Option<(String, Vec<String>)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    const SHELL_ONLY: &[char] = &[
        '"', '\'', '|', '&', '>', '<', ';', '$', '`', '(', ')', '*', '?', '%', '\n', '\r',
    ];
    if s.chars().any(|c| SHELL_ONLY.contains(&c)) {
        return None;
    }
    // Escaped whitespace means shell quoting rules; don't guess and split it.
    if s.contains("\\ ") || s.contains("\\'") || s.contains("\\\"") {
        return None;
    }
    let mut parts = s.split_whitespace();
    let prog = parts.next()?.to_string();
    if prog.contains('=')
        || prog.starts_with('~')
        || prog.eq_ignore_ascii_case("cd")
    {
        return None;
    }
    let args: Vec<String> = parts.map(|p| p.to_string()).collect();
    if args.iter().any(|a| a.starts_with('~')) {
        return None;
    }
    Some((prog, args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_owned_matches_decode() {
        let utf16: &[u8] = b"\xff\xfeh\0i\0";
        for b in [&b"plain\n"[..], b"caf\xe9 x\0y", b"a\0b\xc3\xa9", utf16, b"\xfe\xff\0h", b""] {
            assert_eq!(decode_owned(b.to_vec()), decode(b), "{b:?}");
        }
    }

    #[test]
    fn splits_plain_commands() {
        let (p, a) = split_simple("git status --short").unwrap();
        assert_eq!(p, "git");
        assert_eq!(a, ["status", "--short"]);
    }

    #[test]
    fn routes_each_shell_only_syntax_to_a_shell_that_parses_it() {
        // Each PowerShell line below used to run in cmd.exe and die with
        // "'$x' is not recognized"; `ls | head` used to run in cmd.exe and exit 255.
        for (line, want) in [
            ("$x = 1 + 2; $x", "powershell"),
            ("& $env:ComSpec /c echo hi", "powershell"),
            ("& \"C:\\Program Files\\nodejs\\node.exe\" x.js", "powershell"),
            ("[System.IO.Path]::GetFileName(\"C:\\a\\b.txt\")", "powershell"),
            ("$p = \"abc\"; $p.ToUpper()", "powershell"),
            ("try { throw \"boom\" } catch { $_.Exception.Message }", "powershell"),
            ("foreach ($f in $list) { $f }", "powershell"),
            ("$env:USERPROFILE", "powershell"),
            ("if ($a -eq 1) { 'y' }", "powershell"),
            ("Get-CimInstance Win32_Process | Measure-Object", "powershell"),
            ("Get-Content x.log -Tail 5", "powershell"),
            ("$js = @'\nconsole.log(1)\n'@; node x.mjs", "powershell"),
            ("ls src | head -3", "bash"),
            ("cat Cargo.toml | head -5", "bash"),
            ("grep -n \"fn main\" src/main.rs | head -3", "bash"),
            ("git log --oneline | head -3", "bash"),
            ("find src -name \"*.rs\" | wc -l", "bash"),
            ("echo $(date)", "bash"),
            ("rg -n fn src | wc -l", "bash"),
            ("cargo build --release", "cmd"),
            ("git status --short", "cmd"),
            ("echo one && echo two", "cmd"),
            ("dir /b src | findstr /i main", "cmd"),
            ("echo hello > out.txt && type out.txt", "cmd"),
            ("git commit -m \"Add Get-Thing support\"", "cmd"),
            ("docker-compose up", "cmd"),
            ("curl -s -w \"%{http_code}\" http://127.0.0.1:9360/", "cmd"),
            ("echo %USERPROFILE%", "cmd"),
            ("powershell -NoProfile -Command \"1+1\"", "cmd"),
            ("node -e \"console.log(1+1)\"", "cmd"),
            ("python -c \"print(1+1)\"", "cmd"),
        ] {
            assert_eq!(pick_shell(line), want, "{line}");
        }
    }

    #[test]
    fn encodes_powershell_scripts_as_utf16le_base64() {
        // Vectors from [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes(x)).
        assert_eq!(base64_utf16le(""), "");
        assert_eq!(base64_utf16le("A"), "QQA=");
        assert_eq!(base64_utf16le("AB"), "QQBCAA==");
        assert_eq!(base64_utf16le("ABC"), "QQBCAEMA");
    }

    #[test]
    fn runs_a_cmdlet_given_as_program_through_powershell() {
        let c = build_command_at("Get-ChildItem", &["src".into(), "a b".into()], None).unwrap();
        assert!(c
            .get_program()
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("powershell"));
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args[args.len() - 2], "-EncodedCommand");
        let script = args.last().unwrap();
        assert!(script.len() > 40);
        assert!(script.chars().all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c)));
        assert_eq!(ps_quote("-Path"), "-Path");
        assert_eq!(ps_quote("a b"), "'a b'");
        assert_eq!(ps_quote("it's"), "'it''s'");
        // A normal executable is not hijacked by the cmdlet rule.
        let git = build_command_at("git", &["status".into()], None).unwrap();
        assert!(!git
            .get_program()
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("powershell"));
    }

    #[test]
    fn peels_plain_env_assignments_only() {
        let kv = |s: &str| peel_env(s).0;
        let (e, r) = peel_env("RUST_BACKTRACE=1 CI=true cargo test -- --x=1");
        assert_eq!(e, [("RUST_BACKTRACE".into(), "1".into()), ("CI".into(), "true".into())]);
        assert_eq!(r, "cargo test -- --x=1");
        assert_eq!(peel_env("A=\"x y\" B='z' make").0.len(), 2);
        assert_eq!(peel_env("A=\"x y\" B='z' make").1, "make");
        assert_eq!(kv("PYTHONPATH=C:\\src pytest")[0].1, "C:\\src");
        // left to the shell: expansion, operators, no command, not a name
        for s in ["A=$HOME ls", "A=%X% ls", "A=`x` ls", "A=1", "A=1 B=2", "1A=2 ls", "A=\"x\"y ls", "A=\"x ls", "A=*.rs ls", "git commit -m \"a=b\"", "npm test -- --x=1", "é=1 ls"] {
            let (e, r) = peel_env(s);
            assert!(e.is_empty() && r == s, "{s}");
        }
        assert_eq!(peel_env("A=é ls"), (vec![("A".into(), "é".into())], "ls"));
    }

    #[test]
    fn rejects_shell_syntax() {
        assert!(split_simple("echo hi && echo bye").is_none());
        assert!(split_simple("cat x | tail -5").is_none());
        assert!(split_simple("echo \"hi there\"").is_none());
        assert!(split_simple("ls *.rs").is_none());
        assert!(split_simple("cd ..").is_none());
        assert!(split_simple("FOO=1 git status").is_none());
        assert!(split_simple("rm a\\ b").is_none());
    }

    #[test]
    fn capture_is_capped_and_keeps_partial_data() {
        let input = std::io::repeat(b'x').take(100);
        let (sink, done) = capture_from_cap(input, 10);
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        let (data, cut) = snapshot(&sink);
        assert_eq!(data.len(), 10);
        assert!(cut);
    }

    #[test]
    fn capture_keeps_small_output() {
        let input = std::io::Cursor::new(b"hello".to_vec());
        let (sink, done) = capture_from_cap(input, 10);
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        let (data, cut) = snapshot(&sink);
        assert_eq!(data, b"hello");
        assert!(!cut);
    }

    #[cfg(windows)]
    #[test]
    fn child_environment_keeps_pathext_resolvable() {
        let cmd = build_command(
            "cmd",
            &["/c".to_string(), "where ping".to_string()],
        )
        .unwrap();
        let out = run_captured(cmd, Some(Duration::from_secs(10))).unwrap();
        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        assert!(
            out.stdout.to_ascii_lowercase().contains("ping.exe"),
            "stdout: {}",
            out.stdout
        );
    }

    #[test]
    fn python_fast_stub_is_written_once() {
        let d = std::env::temp_dir().join(format!("qx-pystub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        ensure_python_stub(&d, CERTS_STUB).unwrap();
        let pkg = d.join("pip_system_certs");
        let bootstrap = pkg.join("bootstrap.py");
        let pynvml = d.join("_pynvml_redirector.py");
        assert!(bootstrap.is_file());
        assert!(pkg.join("__init__.py").is_file());
        assert!(pynvml.is_file());
        assert!(d.join(".ok").is_file());
        assert_eq!(std::fs::read_to_string(&bootstrap).unwrap(), CERTS_STUB);
        let before = std::fs::read_to_string(&pynvml).unwrap();
        assert!(before.contains("class PynvmlFinder"));
        assert!(!before.contains("import importlib"));
        ensure_python_stub(&d, CERTS_STUB).unwrap();
        assert_eq!(std::fs::read_to_string(&pynvml).unwrap(), before);
        std::fs::remove_file(&pynvml).unwrap();
        ensure_python_stub(&d, CERTS_STUB).unwrap();
        assert!(pynvml.is_file());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn python_fast_tls_stub_keeps_truststore() {
        let d = std::env::temp_dir().join(format!("qx-pystub-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        ensure_python_stub(&d, CERTS_TLS_STUB).unwrap();
        let bootstrap = std::fs::read_to_string(
            d.join("pip_system_certs").join("bootstrap.py"),
        )
        .unwrap();
        assert!(bootstrap.contains("truststore"));
        assert!(bootstrap.contains("ssl.SSLContext"));
        assert!(bootstrap.contains("PipSessionFinder"));
        // Lazy: nothing imports ssl/truststore until the program imports ssl.
        assert!(bootstrap.contains("SslFinder") && !bootstrap.contains("execsitecustomize"));
        // pip's vendored copy must win: a second truststore patched over ssl makes
        // pip's copy recurse forever in verify_mode on the first network call.
        let pip_copy = bootstrap.find("from pip._vendor import truststore").unwrap();
        let standalone = bootstrap.find("\n                        import truststore").unwrap();
        assert!(pip_copy < standalone);
        assert!(d.join("_pynvml_redirector.py").is_file());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detects_python_programs() {
        assert!(is_python("python", Path::new(r"C:\x\python.exe")));
        assert!(is_python("py", Path::new(r"C:\x\py.exe")));
        assert!(is_python("python3.12", Path::new(r"C:\x\python3.12.exe")));
        assert!(is_python("pip", Path::new(r"C:\x\python.exe")));
        assert!(!is_python("node", Path::new(r"C:\x\node.exe")));
        assert!(!is_python("git", Path::new(r"C:\x\git.exe")));
    }

    #[cfg(windows)]
    #[test]
    fn reaps_fast_child_without_timeout() {
        let cmd = build_command(
            "cmd",
            &["/c".to_string(), "exit 0".to_string()],
        )
        .unwrap();
        let out = run_captured(cmd, None).unwrap();
        assert_eq!(out.code, 0);
    }

    #[cfg(windows)]
    #[test]
    fn timeout_kills_the_process_tree() {
        let cmd = build_command(
            "cmd",
            &["/c".to_string(), "ping -n 10 127.0.0.1".to_string()],
        )
        .unwrap();
        let start = Instant::now();
        let out = run_captured(cmd, Some(Duration::from_millis(500))).unwrap();
        assert!(out.timed_out);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_bypassed_git_binary_gets_the_git_root_for_its_path() {
        let c = Command::new(r"C:\Program Files\Git\mingw64\bin\git.exe");
        assert_eq!(git_root(&c).as_deref(), Some(Path::new(r"C:\Program Files\Git")));
        assert!(git_root(&Command::new(r"C:\tools\bin\git.exe")).is_none());
        // The coreutils fallback runs from usr\bin and needs the same PATH prepend.
        let wc = Command::new(r"C:\Program Files\Git\usr\bin\wc.exe");
        assert_eq!(git_root(&wc).as_deref(), Some(Path::new(r"C:\Program Files\Git")));
    }

    #[test]
    fn not_found_names_the_builtin_limit() {
        // A declined built-in is not a typo: say which built-in declined and where qx looked.
        let wc = not_found("wc");
        assert!(wc.contains("qx built-in"), "{wc}");
        assert!(wc.contains("usr\\bin"), "{wc}");
        assert_eq!(not_found("zzz-nope"), "command not found: zzz-nope");
    }

    #[test]
    fn cmd_internals_run_through_cmd_not_as_missing_programs() {
        assert!(cmd_internal("DIR") && cmd_internal("del") && !cmd_internal("ls"));
        assert!(build_command_at("qx-no-such-program", &[], None).is_err());
        let c = build_command_at("dir", &["/b".to_string()], None).unwrap();
        assert!(c.get_program().to_string_lossy().eq_ignore_ascii_case("cmd.exe"));
    }

    #[test]
    fn an_env_prefix_reaches_the_child_and_never_qx_itself() {
        let (envs, run) = peel_env("QX_TEST_PEEL=1 echo hi");
        assert_eq!((envs.len(), run), (1, "echo hi"));
        let mut c = Command::new("cmd.exe");
        apply_envs(&mut c, &envs);
        assert!(c.get_envs().any(|(k, v)| k == "QX_TEST_PEEL" && v.is_some_and(|v| v == "1")));
        assert!(std::env::var_os("QX_TEST_PEEL").is_none());
        assert!(sets_npm_config(&[("NPM_CONFIG_PREFIX".into(), "x".into())]));
        assert!(!sets_npm_config(&envs));
    }

    #[test]
    fn percent_in_a_cmd_arg_cannot_expand_a_variable() {
        assert_eq!(cmd_quote("%PATH%"), "%^PATH%^");
        assert_eq!(cmd_quote("a b"), "\"a b\"");
        assert_eq!(cmd_quote(""), "\"\"");
    }

    #[test]
    fn write_atomic_replaces_the_file_whole() {
        let d = std::env::temp_dir().join(format!("qx-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("stub.py");
        write_atomic(&f, "one").unwrap();
        write_atomic(&f, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "two");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "no temp file left behind");
    }

    #[test]
    fn a_cut_stream_is_flagged_and_bounded() {
        let (sink, done) = capture_from_cap(std::io::Cursor::new(vec![b'x'; 100]), 10);
        done.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(sink.truncated.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(sink.data.lock().unwrap().len(), 10);
    }

    #[test]
    fn powershell_hands_back_the_real_exit_code() {
        for (cmd, want) in [("cmd /c exit 7", 7), ("Get-Item C:\\qx-no-such-zz -ErrorAction Stop", 1), ("Write-Output ok", 0)] {
            let o = run_captured(build_shell(cmd, "powershell").unwrap(), Some(Duration::from_secs(60))).unwrap();
            assert_eq!(o.code, want, "{cmd}");
        }
    }

    #[test]
    fn a_grandchild_holding_the_pipe_cannot_stall_the_reply() {
        let t = std::time::Instant::now();
        let o = run_captured(build_shell("start /b ping -n 6 127.0.0.1", "cmd").unwrap(), Some(Duration::from_secs(30))).unwrap();
        assert_eq!(o.code, 0);
        assert!(t.elapsed() < Duration::from_secs(4), "waited {:?} for the background ping", t.elapsed());
    }

    #[test]
    fn a_stream_timeout_kills_the_child() {
        let args = ["-n".to_string(), "20".to_string(), "127.0.0.1".to_string()];
        let mut c = build_command("ping", &args).unwrap();
        let t = std::time::Instant::now();
        assert_eq!(run_stream(&mut c, Some(Duration::from_secs(1))).unwrap(), 124);
        assert!(t.elapsed() < Duration::from_secs(8));
    }

    #[test]
    fn cmd_quote_neutralises_a_bare_quote() {
        // An odd `"` flips cmd's quote state, and the `&` after it runs as a second command.
        assert_eq!(cmd_quote("a\"b"), "\"a\"\"b\"");
        assert_eq!(cmd_quote("plain"), "plain");
        assert_eq!(cmd_quote("a b"), "\"a b\"");
    }

    #[cfg(windows)]
    #[test]
    fn a_bare_quote_in_an_argument_cannot_start_a_second_command() {
        let args = ["/b".to_string(), "a\"b".to_string(), "c&echo PWNED".to_string()];
        let o = run_captured(build_command("dir", &args).unwrap(), Some(Duration::from_secs(20))).unwrap();
        assert!(
            !o.stdout.contains("PWNED") && !o.stderr.contains("PWNED"),
            "the second command ran: stdout={:?} stderr={:?}",
            o.stdout,
            o.stderr
        );
    }

    #[cfg(windows)]
    #[test]
    fn cmd_c_receives_its_text_unmangled() {
        let args = ["/c".to_string(), "echo \"a b\"".to_string()];
        let o = run_captured(build_command("cmd", &args).unwrap(), Some(Duration::from_secs(20))).unwrap();
        assert_eq!(o.stdout.trim(), "\"a b\"", "stderr: {}", o.stderr);
    }

    #[test]
    fn a_snapshot_closes_the_capture_to_later_bytes() {
        struct Fed(mpsc::Receiver<Vec<u8>>);
        impl Read for Fed {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.0.recv() {
                    Ok(chunk) => {
                        buf[..chunk.len()].copy_from_slice(&chunk);
                        Ok(chunk.len())
                    }
                    Err(_) => Ok(0),
                }
            }
        }
        let (tx, rx) = mpsc::channel();
        let (sink, done) = capture_from_cap(Fed(rx), 1024);
        tx.send(b"early".to_vec()).unwrap();
        let t = Instant::now();
        while sink.data.lock().unwrap().len() < 5 && t.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let (data, _) = snapshot(&sink);
        assert_eq!(data, b"early");
        // a daemon that keeps writing after the call returned must not grow a hidden buffer
        tx.send(b"late".to_vec()).unwrap();
        drop(tx);
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(sink.data.lock().unwrap().is_empty(), "bytes after the snapshot were buffered");
    }

    #[test]
    fn a_captured_child_gets_no_stdin_by_default() {
        let c = build_command("findstr", &["^".to_string()]).unwrap();
        let o = run_captured(c, Some(Duration::from_secs(10))).unwrap();
        assert_eq!((o.code, o.stdout.as_str()), (1, ""));
    }
}
