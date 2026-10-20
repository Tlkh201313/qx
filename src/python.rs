//! Python: run any console-script launcher and any venv interpreter in one process.
//!
//! `pip`, `uv`, `poetry` and `pdm` all ship the same kind of launcher: a small
//! `.exe` stub with a stored zip appended (pip/distlib) or embedded (uv) whose
//! `__main__.py` starts with its shebang line. `python.exe <launcher.exe> …` is
//! exactly what the launcher does after one extra process creation, so qx runs
//! that instead. A venv's `Scripts\python.exe` is likewise a redirector: the base
//! interpreter with `__PYVENV_LAUNCHER__` set behaves identically, one process
//! fewer. Every path here fails closed to the old behavior.

use std::path::{Path, PathBuf};

/// A console-script launcher is a stub plus a zip; anything bigger is not one.
const MAX_LAUNCHER_BYTES: u64 = 2 * 1024 * 1024;

/// The absolute interpreter a pip/uv launcher would run, when that file exists.
pub fn launcher_interpreter(exe: &Path) -> Option<PathBuf> {
    // Cheap guard first: only console scripts live in a `Scripts` directory, so the
    // rest of the resolution path never pays for reading a binary.
    if !exe.parent()?.file_name()?.to_str()?.eq_ignore_ascii_case("Scripts") {
        return None;
    }
    let meta = std::fs::metadata(exe).ok()?;
    if !meta.is_file() || meta.len() > MAX_LAUNCHER_BYTES {
        return None;
    }
    let bytes = std::fs::read(exe).ok()?;
    let path = PathBuf::from(shebang_interpreter(&bytes)?);
    // A stale venv names an interpreter that is gone. Keep the launcher: its own error
    // names the missing path, which is more useful than qx guessing.
    path.is_file().then_some(path)
}

/// The interpreter named in the stored zip's shebang line, unvalidated.
fn shebang_interpreter(bytes: &[u8]) -> Option<String> {
    let (zip_start, data_start, data_len) = stored_main(bytes)?;
    let data = bytes.get(data_start..data_start.checked_add(data_len as usize)?)?;
    // uv puts the shebang inside `__main__.py`; pip/distlib put it just before the zip.
    let line = if data.starts_with(b"#!") {
        data.split(|b| *b == b'\n').next()?
    } else {
        let window = bytes.get(zip_start.saturating_sub(1024)..zip_start)?;
        // pip appends the shebang straight after the PE stub, so it is not at a line
        // start: take the last `#!` whose line runs to the zip's first byte.
        let end = window
            .iter()
            .rposition(|b| !matches!(b, b'\r' | b'\n'))
            .map_or(0, |i| i + 1);
        let start = window[..end].windows(2).rposition(|w| w == b"#!")?;
        let line = window.get(start..end)?;
        line.iter().all(|b| *b != b'\n').then_some(line)?
    };
    parse_shebang(std::str::from_utf8(line).ok()?)
}

fn parse_shebang(line: &str) -> Option<String> {
    let mut s = line.trim_end_matches(['\r', '\n']).trim();
    s = s.strip_prefix("#!")?.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s = &s[1..s.len() - 1];
    }
    let b = s.as_bytes();
    // Only a drive-absolute path can be a Windows interpreter; a bare `python.exe`
    // would resolve against whatever cwd the agent happens to be in.
    if b.len() < 3 || b[1] != b':' || !matches!(b[2], b'\\' | b'/') {
        return None;
    }
    let name = Path::new(s).file_name()?.to_str()?;
    name.eq_ignore_ascii_case("python.exe").then(|| s.to_string())
}

/// `(zip start, data start, data length)` for a single stored `__main__.py`.
fn stored_main(bytes: &[u8]) -> Option<(usize, usize, u32)> {
    let eocd = rfind(bytes, b"PK\x05\x06")?;
    // One entry only: a launcher embeds exactly one file.
    if u16_at(bytes, eocd + 10)? != 1 {
        return None;
    }
    let cd_size = u32_at(bytes, eocd + 12)? as usize;
    let cd_offset = u32_at(bytes, eocd + 16)? as usize;
    let zip_start = eocd.checked_sub(cd_size)?.checked_sub(cd_offset)?;
    if bytes.get(zip_start..zip_start + 4)? != b"PK\x03\x04" {
        return None;
    }
    // Only stored (method 0) data has its size and offset in fixed places we can trust.
    if u16_at(bytes, zip_start + 8)? != 0 {
        return None;
    }
    let data_len = u32_at(bytes, zip_start + 18)?;
    let name_len = u16_at(bytes, zip_start + 26)? as usize;
    let extra_len = u16_at(bytes, zip_start + 28)? as usize;
    let name_start = zip_start + 30;
    if bytes.get(name_start..name_start + name_len)? != b"__main__.py" {
        return None;
    }
    let data_start = name_start + name_len + extra_len;
    (data_start <= bytes.len()).then_some((zip_start, data_start, data_len))
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len())
        .rev()
        .find(|&i| &hay[i..i + needle.len()] == needle)
}

/// A venv interpreter, replaced by the base interpreter it would have started.
pub struct VenvRedirect {
    pub base: PathBuf,
    pub launcher: PathBuf,
}

/// The one-process equivalent of `<venv>\Scripts\python.exe`, when that is safe.
pub fn venv_redirect(python: &Path) -> Option<VenvRedirect> {
    venv_redirect_with(python, |k| std::env::var(k).ok())
}

fn venv_redirect_with(python: &Path, env: impl Fn(&str) -> Option<String>) -> Option<VenvRedirect> {
    if env("QX_PYTHON_VENV").is_some_and(|v| v.trim().eq_ignore_ascii_case("off")) {
        return None;
    }
    // Both of these change what the redirector would do, so leave it alone.
    if env("PYTHONHOME").is_some() || env("__PYVENV_LAUNCHER__").is_some() {
        return None;
    }
    if !python.file_name()?.to_str()?.eq_ignore_ascii_case("python.exe") {
        return None;
    }
    let scripts = python.parent()?;
    if !scripts.file_name()?.to_str()?.eq_ignore_ascii_case("Scripts") {
        return None;
    }
    let config = std::fs::read_to_string(scripts.parent()?.join("pyvenv.cfg")).ok()?;
    let (home, (major, minor)) = parse_pyvenv(&config)?;
    // The `__PYVENV_LAUNCHER__` redirect is the documented behavior from 3.11 on.
    if major < 3 || (major == 3 && minor < 11) {
        return None;
    }
    let base = home.join("python.exe");
    base.is_file().then(|| VenvRedirect {
        base,
        launcher: python.to_path_buf(),
    })
}

/// `(home, (major, minor))` from a `pyvenv.cfg`.
fn parse_pyvenv(text: &str) -> Option<(PathBuf, (u32, u32))> {
    let mut home = None;
    let mut version = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "home" => home = Some(PathBuf::from(value)),
            // uv writes `uv`/`version_info`; the stdlib writes `version` too.
            "version_info" | "version" => {
                if version.is_none() {
                    let mut parts = value.split('.');
                    let major = parts.next()?.parse().ok()?;
                    let minor = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
                    version = Some((major, minor));
                }
            }
            _ => {}
        }
    }
    Some((home?, version?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("qx-python-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// A single-entry stored zip: local header, data, central directory, EOCD.
    fn stored_zip(name: &str, data: &[u8], method: u16, entries: u16) -> Vec<u8> {
        let (name_len, size) = (name.len() as u16, data.len() as u32);
        let mut out = Vec::new();
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]); // time, date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc
        out.extend_from_slice(&size.to_le_bytes()); // compressed size
        out.extend_from_slice(&size.to_le_bytes()); // uncompressed size
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra length
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        let cd_offset = out.len() as u32;
        out.extend_from_slice(b"PK\x01\x02");
        out.extend_from_slice(&[20u8, 0, 20, 0, 0, 0]); // versions, flags
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]); // time, date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&[0u8; 8]); // extra, comment, disk, internal attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // local header offset
        out.extend_from_slice(name.as_bytes());
        let cd_size = out.len() as u32 - cd_offset;

        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0u8; 4]); // disk, cd disk
        out.extend_from_slice(&entries.to_le_bytes()); // entries this disk
        out.extend_from_slice(&entries.to_le_bytes()); // entries total
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment length
        out
    }

    fn launcher(name: &str, shebang: Option<&str>, data: &[u8], method: u16, entries: u16) -> Vec<u8> {
        let mut f = b"MZstub".to_vec();
        if let Some(line) = shebang {
            f.extend_from_slice(line.as_bytes());
            f.extend_from_slice(b"\r\n");
        }
        f.extend(stored_zip(name, data, method, entries));
        f
    }

    #[test]
    fn pip_layout() {
        let f = launcher(
            "__main__.py",
            Some("#!C:\\Py312\\python.exe"),
            b"import sys\nfrom pytest import console_main\n",
            0,
            1,
        );
        assert_eq!(shebang_interpreter(&f).as_deref(), Some("C:\\Py312\\python.exe"));
    }

    #[test]
    fn uv_layout_with_trailing_resources() {
        let mut f = launcher(
            "__main__.py",
            None,
            b"#!C:\\v\\Scripts\\python.exe\n# -*- coding: utf-8 -*-\nimport sys\n",
            0,
            1,
        );
        f.extend(b"<?xml version=\"1.0\"?><assembly/>PADDINGXXPADDING");
        assert_eq!(shebang_interpreter(&f).as_deref(), Some("C:\\v\\Scripts\\python.exe"));
    }

    #[test]
    fn quoted_shebang_is_unquoted() {
        let f = launcher(
            "__main__.py",
            Some("#!\"C:\\Program Files\\Py\\python.exe\""),
            b"import sys\n",
            0,
            1,
        );
        assert_eq!(
            shebang_interpreter(&f).as_deref(),
            Some("C:\\Program Files\\Py\\python.exe")
        );
    }

    #[test]
    fn rejects_unknown_shapes() {
        let good = b"import sys\n".to_vec();
        // two entries: not a console-script launcher
        assert!(shebang_interpreter(&launcher("__main__.py", Some("#!C:\\Py\\python.exe"), &good, 0, 2)).is_none());
        // deflated data: sizes and offsets are not what we read
        assert!(shebang_interpreter(&launcher("__main__.py", Some("#!C:\\Py\\python.exe"), &good, 8, 1)).is_none());
        // the entry has to be __main__.py
        assert!(shebang_interpreter(&launcher("x.py", Some("#!C:\\Py\\python.exe"), &good, 0, 1)).is_none());
        // never pythonw.exe: it detaches from the console
        assert!(shebang_interpreter(&launcher("__main__.py", Some("#!C:\\Py\\pythonw.exe"), &good, 0, 1)).is_none());
        // a bare name would resolve against an unknown cwd
        assert!(shebang_interpreter(&launcher("__main__.py", Some("#!python.exe"), &good, 0, 1)).is_none());
        assert!(shebang_interpreter(b"MZstub only").is_none());
    }

    #[test]
    fn stale_shebang_keeps_the_launcher() {
        let d = tmp("stale");
        fs::create_dir_all(d.join("Scripts")).unwrap();
        let tool = d.join("Scripts").join("tool.exe");
        let mut bytes = b"MZstub".to_vec();
        let gone = d.join("gone").join("python.exe");
        bytes.extend_from_slice(format!("#!{}\r\n", gone.display()).as_bytes());
        bytes.extend(stored_zip("__main__.py", b"import sys\n", 0, 1));
        fs::write(&tool, bytes).unwrap();

        assert!(launcher_interpreter(&tool).is_none());
        match crate::resolve::resolve(tool.to_str().unwrap(), None) {
            crate::resolve::Target::Direct { exe, prefix } => {
                assert_eq!(exe, tool);
                assert!(prefix.is_empty());
            }
            _ => panic!("a stale launcher must keep running itself"),
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn parses_pyvenv_variants() {
        assert_eq!(
            parse_pyvenv("home = C:\\H\nversion_info = 3.12.1.final.0\n"),
            Some((PathBuf::from("C:\\H"), (3, 12)))
        );
        assert_eq!(
            parse_pyvenv("home = C:\\H\nuv = 0.11.21\nversion_info = 3.11.9\n")
                .map(|v| v.1),
            Some((3, 11))
        );
        assert_eq!(
            parse_pyvenv("home = C:\\H\nversion = 3.10.4\n").map(|v| v.1),
            Some((3, 10))
        );
        assert!(parse_pyvenv("version_info = 3.12.1\n").is_none());
    }

    #[test]
    fn venv_redirect_skips_when_pythonhome_or_launcher_env_set() {
        let d = tmp("venv-env");
        let v = d.join("v");
        fs::create_dir_all(v.join("Scripts")).unwrap();
        fs::write(d.join("h").join("placeholder"), "").ok();
        fs::create_dir_all(d.join("h")).unwrap();
        fs::write(d.join("h").join("python.exe"), "").unwrap();
        fs::write(
            v.join("pyvenv.cfg"),
            format!("home = {}\nversion_info = 3.12.1\n", d.join("h").display()),
        )
        .unwrap();
        let python = v.join("Scripts").join("python.exe");
        fs::write(&python, "").unwrap();

        let none = |_: &str| None;
        let hit = venv_redirect_with(&python, none).expect("a 3.12 venv redirects");
        assert_eq!(hit.base, d.join("h").join("python.exe"));
        assert_eq!(hit.launcher, python);

        let set = |key: &'static str| move |k: &str| (k == key).then(|| "x".to_string());
        assert!(venv_redirect_with(&python, set("PYTHONHOME")).is_none());
        assert!(venv_redirect_with(&python, set("__PYVENV_LAUNCHER__")).is_none());
        assert!(venv_redirect_with(&python, |k| (k == "QX_PYTHON_VENV").then(|| "off".into())).is_none());

        fs::write(
            v.join("pyvenv.cfg"),
            format!("home = {}\nversion_info = 3.10.4\n", d.join("h").display()),
        )
        .unwrap();
        assert!(venv_redirect_with(&python, none).is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn venv_redirect_matches_the_redirector() {
        let d = tmp("venv-real");
        let v = d.join("v");
        let made = std::process::Command::new("python")
            .args(["-m", "venv", "--without-pip"])
            .arg(&v)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            println!("skip: cannot create a venv here");
            return;
        }
        let script_python = v.join("Scripts").join("python.exe");
        const PROBE: &str = "import sys;print(sys.prefix);print(sys.executable);print(sys.path)";
        let plain = std::process::Command::new(&script_python)
            .args(["-c", PROBE])
            .output()
            .unwrap();
        let mut cmd = crate::exec::build_command_at(
            script_python.to_str().unwrap(),
            &["-c".to_string(), PROBE.to_string()],
            None,
        )
        .unwrap();
        assert_ne!(
            Path::new(cmd.get_program()),
            script_python.as_path(),
            "the venv redirector was not bypassed"
        );
        let redirected = cmd.output().unwrap();
        // Compared line by line: qx's fast-python stub adds its own sys.path entry on purpose,
        // so only the two facts the redirect decides have to be identical.
        let lines = |out: &[u8]| {
            String::from_utf8_lossy(out)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let (plain, redirected) = (lines(&plain.stdout), lines(&redirected.stdout));
        assert_eq!(plain[0], redirected[0], "sys.prefix changed");
        assert_eq!(plain[1], redirected[1], "sys.executable changed");
        let site = v.join("Lib").join("site-packages").to_string_lossy().replace('\\', "\\\\");
        assert!(plain[2].contains(&site), "{}", plain[2]);
        assert!(redirected[2].contains(&site), "{}", redirected[2]);
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn a_real_pip_launcher_runs_as_one_process() {
        let Some(launcher) = crate::resolve::find_on_path("pytest") else {
            println!("skip: no pytest launcher on PATH");
            return;
        };
        if !launcher
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|n| n.eq_ignore_ascii_case("Scripts"))
        {
            println!("skip: pytest is not a Scripts launcher");
            return;
        }
        match crate::resolve::resolve("pytest", None) {
            crate::resolve::Target::Direct { exe, prefix } => {
                assert_eq!(prefix.len(), 1, "one extra argument: the launcher itself");
                assert_eq!(Path::new(&prefix[0]), launcher.as_path());
                assert!(
                    exe.file_name().unwrap().to_string_lossy().eq_ignore_ascii_case("python.exe"),
                    "{}",
                    exe.display()
                );
                let out = std::process::Command::new(&exe)
                    .args(&prefix)
                    .arg("--version")
                    .output()
                    .unwrap();
                assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            }
            _ => panic!("a pip launcher must resolve to python.exe plus the launcher"),
        }
    }
}
