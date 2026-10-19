//! Process execution: direct spawn, shell fallback, capture with timeout.

use crate::resolve::{self, Target};
use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
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

pub struct Outcome {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
    pub timed_out: bool,
}

pub fn build_command(program: &str, args: &[String]) -> Result<Command, String> {
    match resolve::resolve(program) {
        Target::Direct { exe, prefix } => {
            let node = exe
                .file_name()
                .map(|n| n.to_string_lossy().eq_ignore_ascii_case("node.exe"))
                .unwrap_or(false);
            let mut c = Command::new(exe);
            c.args(prefix);
            c.args(args);
            prepare(&mut c);
            if node {
                apply_node_cache(&mut c);
            }
            Ok(c)
        }
        Target::Shim(p) => {
            let mut line = cmd_quote(&p.to_string_lossy());
            for a in args {
                line.push(' ');
                line.push_str(&cmd_quote(a));
            }
            let mut c = Command::new("cmd.exe");
            c.args(["/d", "/s", "/c", line.as_str()]);
            prepare(&mut c);
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
            Ok(c)
        }
        Target::Missing => Err(format!("command not found: {program}")),
    }
}

pub fn build_shell(command: &str, shell: &str) -> Result<Command, String> {
    let choice = match shell {
        "auto" => {
            if looks_posix(command) {
                "bash"
            } else {
                "cmd"
            }
        }
        other => other,
    };
    match choice {
        "bash" => {
            let b = resolve::git_bash().ok_or("Git Bash not found")?;
            let mut c = Command::new(b);
            c.args(["-c", command]);
            prepare(&mut c);
            Ok(c)
        }
        "cmd" => {
            let mut c = Command::new("cmd.exe");
            c.args(["/d", "/c"]);
            // Pass the command as raw command-line text. Letting Command quote
            // it turns inner quotes into `\"`, which cmd hands to node/python as
            // literal backslashes (`node -e "..."` silently printed nothing).
            // `/s` is deliberately absent: it strips the first quote of a quoted
            // executable path, turning `"C:\Program Files\..."` into `C:\Program`.
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                c.raw_arg(command);
            }
            #[cfg(not(windows))]
            c.arg(command);
            prepare(&mut c);
            Ok(c)
        }
        "none" => Err("shell=none needs program+args".to_string()),
        other => Err(format!("unknown shell: {other}")),
    }
}

fn looks_posix(s: &str) -> bool {
    // Only markers cmd.exe cannot handle; plain `&&`, `|` and `2>&1` stay on cmd.
    const MARKERS: &[&str] = &[
        "$(", "${", "2>/dev/null", "|&", "<<'", "/c/", "/mnt/", "`", "source ", "export ",
    ];
    MARKERS.iter().any(|m| s.contains(m)) && !s.contains("\\")
}

fn cmd_quote(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".to_string();
    }
    if s.chars()
        .any(|c| c.is_whitespace() || "&|<>^()".contains(c))
    {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn reader_thread<R: Read + Send + 'static>(r: R) -> (std::sync::Arc<Capture>, mpsc::Receiver<()>) {
    capture_from_cap(r, CAPTURE_CAP)
}

pub(crate) const CAPTURE_CAP: usize = 16 * 1024 * 1024;

pub(crate) struct Capture {
    data: std::sync::Mutex<Vec<u8>>,
    truncated: std::sync::atomic::AtomicBool,
}

fn capture_from_cap<R: Read + Send + 'static>(
    mut r: R,
    cap: usize,
) -> (std::sync::Arc<Capture>, mpsc::Receiver<()>) {
    let sink = std::sync::Arc::new(Capture {
        data: std::sync::Mutex::new(Vec::new()),
        truncated: std::sync::atomic::AtomicBool::new(false),
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
        Ok(d) => d.clone(),
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
        fn CloseHandle(h: Handle) -> i32;
    }

    /// Best-effort process-tree container. Created without kill-on-close so a
    /// command that intentionally leaves a background process running is not
    /// disturbed on normal exit; only an explicit timeout terminates the tree.
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
                Some(Job(job))
            }
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

/// Run with captured stdout/stderr. Necessary for compaction and MCP replies.
pub fn run_captured(mut cmd: Command, timeout: Option<Duration>) -> std::io::Result<Outcome> {
    cmd.stdin(Stdio::null());
    let start = Instant::now();
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;

    #[cfg(windows)]
    let job = job::Job::assign(&child);

    let (out_sink, out_done) = reader_thread(child.stdout.take().expect("stdout piped"));
    let (err_sink, err_done) = reader_thread(child.stderr.take().expect("stderr piped"));

    let mut timed_out = false;
    let code: i32;
    loop {
        match child.try_wait()? {
            Some(status) => {
                code = status.code().unwrap_or(1);
                break;
            }
            None => {
                if let Some(t) = timeout {
                    if start.elapsed() > t {
                        timed_out = true;
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
                        code = 124;
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        }
    }

    // Grandchildren can hold the pipe open; don't block forever, but keep any
    // partial output the readers already captured.
    let _ = out_done.recv_timeout(Duration::from_secs(2));
    let _ = err_done.recv_timeout(Duration::from_secs(2));
    let (so, so_cut) = snapshot(&out_sink);
    let (se, se_cut) = snapshot(&err_sink);

    let mut stdout = decode(&so);
    let mut stderr = decode(&se);
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
    })
}

pub fn run_stream(cmd: &mut Command) -> std::io::Result<i32> {
    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    Ok(status.code().unwrap_or(1))
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
    String::from_utf8_lossy(bytes).replace('\0', "")
}

pub fn apply_cwd(cmd: &mut Command, cwd: &Option<PathBuf>) {
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
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
    fn splits_plain_commands() {
        let (p, a) = split_simple("git status --short").unwrap();
        assert_eq!(p, "git");
        assert_eq!(a, ["status", "--short"]);
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
}
