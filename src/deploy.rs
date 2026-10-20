//! `qx deploy`: keep one canonical `qx.exe` that every client points at.
//!
//! Every MCP client holds its own copy of the path it was started from, and on Windows
//! a running image cannot be overwritten. Clients also drift: this machine had five
//! different qx builds with three of them stale. `deploy` copies the running binary to
//! the canonical path, renaming the old one aside first (Windows allows renaming a
//! running exe), so a rebuild can replace the file while servers are still using it.

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub enum Installed {
    Unchanged,
    Replaced { aside: Option<PathBuf> },
}

/// `%CARGO_HOME%\bin\qx.exe`, else `%USERPROFILE%\.cargo\bin\qx.exe`.
pub fn canonical() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CARGO_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(home).join("bin").join("qx.exe"));
    }
    std::env::var_os("USERPROFILE")
        .filter(|h| !h.is_empty())
        .map(|home| PathBuf::from(home).join(".cargo").join("bin").join("qx.exe"))
}

fn file_name(dst: &Path) -> std::io::Result<String> {
    dst.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "destination has no file name"))
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Whether two files hold the same bytes, streamed so a large binary is never loaded.
pub fn same_bytes(a: &Path, b: &Path) -> std::io::Result<bool> {
    if std::fs::metadata(a)?.len() != std::fs::metadata(b)?.len() {
        return Ok(false);
    }
    let mut a = std::io::BufReader::new(std::fs::File::open(a)?);
    let mut b = std::io::BufReader::new(std::fs::File::open(b)?);
    let mut ba = [0u8; 64 * 1024];
    let mut bb = [0u8; 64 * 1024];
    loop {
        let (na, nb) = (read_full(&mut a, &mut ba)?, read_full(&mut b, &mut bb)?);
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

fn read_full(r: &mut impl std::io::Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Copy `src` over `dst`, keeping the previous `dst` as a sibling until clients exit.
pub fn install(src: &Path, dst: &Path) -> std::io::Result<Installed> {
    if same_path(src, dst) {
        return Ok(Installed::Unchanged);
    }
    let exists = dst.is_file();
    if exists && same_bytes(src, dst)? {
        return Ok(Installed::Unchanged);
    }
    let parent = dst.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "destination has no parent directory")
    })?;
    std::fs::create_dir_all(parent)?;
    let name = file_name(dst)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let new = parent.join(format!("{name}.new-{}", std::process::id()));
    // Copy first, so a half-written file can never take the canonical name.
    std::fs::copy(src, &new)?;
    let aside = if exists {
        let aside = parent.join(format!("{name}.old-{stamp}"));
        if let Err(e) = std::fs::rename(dst, &aside) {
            let _ = std::fs::remove_file(&new);
            return Err(e);
        }
        Some(aside)
    } else {
        None
    };
    if let Err(e) = std::fs::rename(&new, dst) {
        if let Some(aside) = &aside {
            let _ = std::fs::rename(aside, dst);
        }
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    Ok(Installed::Replaced { aside })
}

/// Delete the `<name>.old-*` siblings a previous deploy left behind.
pub fn sweep_aside(dst: &Path) -> usize {
    let (Some(parent), Ok(name)) = (dst.parent(), file_name(dst)) else {
        return 0;
    };
    let prefix = format!("{name}.old-");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix)
            && std::fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

pub fn run(args: &[String]) -> i32 {
    let mut to: Option<PathBuf> = None;
    let mut dry = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--to" => {
                i += 1;
                match args.get(i) {
                    Some(p) => to = Some(PathBuf::from(p)),
                    None => {
                        eprintln!("qx deploy: --to needs a path");
                        return 2;
                    }
                }
            }
            "--dry-run" => dry = true,
            other if other.starts_with('-') => {
                eprintln!("qx deploy: unknown flag {other}");
                return 2;
            }
            other => {
                eprintln!("qx deploy: unexpected argument {other}");
                return 2;
            }
        }
        i += 1;
    }
    let Some(dst) = to.or_else(canonical) else {
        eprintln!("qx deploy: no cargo bin directory (set CARGO_HOME or USERPROFILE)");
        return 2;
    };
    let Ok(src) = std::env::current_exe() else {
        eprintln!("qx deploy: cannot locate the running executable");
        return 2;
    };
    if dry {
        let up_to_date = same_bytes(&src, &dst).unwrap_or(false);
        if up_to_date {
            println!("qx deploy: up to date ({})", dst.display());
        } else {
            println!("qx deploy: would install {} from {}", dst.display(), src.display());
        }
        return 0;
    }
    match install(&src, &dst) {
        Ok(Installed::Unchanged) => {
            println!("qx deploy: up to date ({})", dst.display());
            0
        }
        Ok(Installed::Replaced { aside: Some(aside) }) => {
            println!(
                "qx deploy: installed {} (previous copy kept as {} until running clients exit)",
                dst.display(),
                aside.display()
            );
            0
        }
        Ok(Installed::Replaced { aside: None }) => {
            println!("qx deploy: installed {}", dst.display());
            0
        }
        Err(e) => {
            eprintln!("qx deploy: cannot install {}: {e}", dst.display());
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("qx-deploy-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn install_copies_then_is_idempotent() {
        let dir = tmp("copy");
        let src = dir.join("src.exe");
        std::fs::write(&src, "A").unwrap();
        let dst = dir.join("bin").join("qx.exe");

        assert_eq!(install(&src, &dst).unwrap(), Installed::Replaced { aside: None });
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "A");
        assert_eq!(install(&src, &dst).unwrap(), Installed::Unchanged);

        std::fs::write(&src, "B").unwrap();
        match install(&src, &dst).unwrap() {
            Installed::Replaced { aside: Some(aside) } => {
                assert_eq!(std::fs::read_to_string(&aside).unwrap(), "A");
            }
            other => panic!("expected a replaced install, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "B");
        assert_eq!(sweep_aside(&dst), 1);
        assert_eq!(sweep_aside(&dst), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_bytes_compares_content_not_names() {
        let dir = tmp("bytes");
        let a = dir.join("a");
        std::fs::write(&a, vec![7u8; 100_000]).unwrap();
        let b = dir.join("b");
        std::fs::write(&b, vec![7u8; 100_000]).unwrap();
        assert!(same_bytes(&a, &b).unwrap());
        std::fs::write(&b, vec![8u8; 100_000]).unwrap();
        assert!(!same_bytes(&a, &b).unwrap());
        std::fs::write(&b, vec![7u8; 99_999]).unwrap();
        assert!(!same_bytes(&a, &b).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = tmp("dry");
        let dst = dir.join("bin").join("qx.exe");
        let code = run(&["--to".into(), dst.to_string_lossy().into_owned(), "--dry-run".into()]);
        assert_eq!(code, 0);
        assert!(!dst.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn install_replaces_a_running_exe() {
        let sys = Path::new(r"C:\Windows\System32");
        let (ping, whoami) = (sys.join("PING.EXE"), sys.join("whoami.exe"));
        if !ping.is_file() || !whoami.is_file() {
            println!("skip: the system tools this test needs are absent");
            return;
        }
        let dir = tmp("running");
        let dst = dir.join("qx.exe");
        std::fs::copy(&ping, &dst).unwrap();
        let mut child = std::process::Command::new(&dst)
            .args(["-n", "6", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        let src = dir.join("whoami.exe");
        std::fs::copy(&whoami, &src).unwrap();
        let aside = match install(&src, &dst).unwrap() {
            Installed::Replaced { aside: Some(aside) } => aside,
            other => panic!("a running exe must be replaced by renaming it aside: {other:?}"),
        };
        assert!(aside.is_file());
        assert_eq!(child.try_wait().unwrap(), None, "the running client must survive");
        child.kill().unwrap();
        child.wait().unwrap();

        assert_eq!(sweep_aside(&dst), 1);
        assert!(!aside.exists());
        assert!(same_bytes(&src, &dst).unwrap(), "the new binary must be in place");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
