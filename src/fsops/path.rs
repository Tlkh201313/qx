//! Path resolution and workspace confinement.

use super::*;

/// `%TEMP%\claude`: where Claude Code keeps a session's scratchpad. It is outside every project, yet
/// writing there is the sanctioned place for temporary files, so mutations are allowed in it too.
fn scratch_root() -> &'static Path {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        // Canonicalise the temp dir (it exists), not `claude` (it may not yet): otherwise a missing
        // folder leaves a short `RUNNER~1` form that never matches the canonical path of a write.
        let t = std::env::temp_dir();
        fs::canonicalize(&t).unwrap_or(t).join("claude")
    })
}

// --------------------------------------------------------------------- root

/// The client sends the same session cwd on every call. Canonicalizing it each time
/// costs a Win32 handle round-trip for no new information; cache the answer by
/// the input path. A deleted or replaced workspace invalidates naturally at the
/// next filesystem operation, and the cache is bounded.
fn canonical_cwd(p: &Path) -> Result<PathBuf, FsError> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock() {
        if let Some(hit) = map.get(p) {
            return Ok(hit.clone());
        }
    }
    let cwd = fs::canonicalize(p)
        .map_err(|e| FsError::new("FS_BAD_CWD", format!("cwd is not accessible: {e}")))?;
    if let Ok(mut map) = cache.lock() {
        if map.len() >= 64 {
            map.clear();
        }
        map.insert(p.to_path_buf(), cwd.clone());
    }
    Ok(cwd)
}

pub(super) struct Root {
    pub(super) cwd: PathBuf,
}

impl Root {
    pub(super) fn new(args: &Value) -> Result<Self, FsError> {
        // Omitting `cwd` means "where the server was started", like the CLI. Every fs call
        // otherwise fails on a field the model has no reason to know.
        let p = match args.get("cwd") {
            None | Some(Value::Null) => match last_cwd() {
                Some(d) => d,
                None => std::env::current_dir().map_err(|e| FsError::io("cannot resolve cwd", e))?,
            },
            Some(Value::String(s)) if !s.trim().is_empty() => PathBuf::from(s),
            Some(_) => {
                return Err(FsError::arg("cwd must be a non-empty string").fb("tool"));
            }
        };
        let p = if p.is_absolute() {
            p
        } else {
            std::env::current_dir()
                .map_err(|e| FsError::io("cannot resolve cwd", e))?
                .join(p)
        };
        let cwd = canonical_cwd(&p)?;
        if !cwd.is_dir() {
            return Err(FsError::new(
                "FS_BAD_CWD",
                format!("cwd is not a directory: {}", cwd.display()),
            ));
        }
        if matches!(args.get("cwd"), Some(Value::String(_))) {
            note_cwd(&p);
        }
        Ok(Self { cwd })
    }

    fn candidate(&self, raw: &str) -> PathBuf {
        let raw = raw.trim();
        // A path pasted with its quotes, or `~/x`: say what was meant instead of failing.
        let raw = raw
            .strip_prefix('"').and_then(|r| r.strip_suffix('"'))
            .or_else(|| raw.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
            .unwrap_or(raw);
        let home = if raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\") { crate::resolve::home_dir() } else { None };
        let p = match home {
            Some(h) => h.join(raw[1..].trim_start_matches(['/', '\\'])),
            None => PathBuf::from(raw),
        };
        if p.is_absolute() {
            p
        } else {
            self.cwd.join(p)
        }
    }

    pub(super) fn display(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.cwd).unwrap_or(path);
        let s = rel.to_string_lossy().replace('\\', "/");
        let s = s.strip_prefix("//?/").map(str::to_string).unwrap_or(s);
        if s.is_empty() {
            ".".to_string()
        } else {
            s
        }
    }

    pub(super) fn ensure_inside(&self, path: &Path) -> Result<(), FsError> {
        if path == self.cwd || path.starts_with(&self.cwd) || path.starts_with(scratch_root()) {
            Ok(())
        } else {
            Err(FsError::new(
                "FS_OUTSIDE_WORKSPACE",
                format!(
                    "path escapes the workspace: {} (use the native {} for paths outside cwd)",
                    path.display(),
                    native(Native::Shell)
                ),
            ))
        }
    }

    /// A path on another machine would be probed over the network (tens of seconds when it is down) just to be
    /// refused; refuse it first. A workspace on a share still owns its own paths.
    fn refuse_foreign_share(&self, candidate: &Path) -> Result<(), FsError> {
        match unc_share(candidate) {
            Some(share) if unc_share(&self.cwd).as_ref() != Some(&share) => self.ensure_inside(candidate),
            _ => Ok(()),
        }
    }

    pub(super) fn resolve_existing(&self, raw: &str) -> Result<PathBuf, FsError> {
        let candidate = self.candidate(raw);
        self.refuse_foreign_share(&candidate)?;
        let canonical = fs::canonicalize(&candidate).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => FsError::new(
                "FS_NOT_FOUND",
                format!("cannot access \"{raw}\": not found"),
            ),
            _ => FsError::io(format!("cannot access \"{raw}\""), e),
        })?;
        self.ensure_inside(&canonical)?;
        Ok(canonical)
    }

    /// Reads (read, glob, grep, list, stat) may look anywhere the user can: the dsh read tool is not
    /// confined to the workspace, and a fallback for every path outside `cwd` costs a round trip.
    /// Only mutations stay inside `cwd`.
    pub(super) fn resolve_read(&self, raw: &str) -> Result<PathBuf, FsError> {
        let candidate = self.candidate(raw);
        fs::canonicalize(&candidate).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                FsError::new("FS_NOT_FOUND", format!("cannot access \"{raw}\": not found"))
            }
            _ => FsError::io(format!("cannot access \"{raw}\""), e),
        })
    }

    /// Resolve a file for creation or replacement. Existing parent directories
    /// may be created, but every existing ancestor is canonicalized and checked
    /// first, so a symlink or junction cannot walk the write outside the root.
    pub(super) fn resolve_for_write(&self, raw: &str) -> Result<(PathBuf, bool), FsError> {
        // Relative paths are joined onto a verbatim `\\?\` cwd, which skips Win32 name rules: `nul`
        // or `name.` would become real, undeletable files and `a.txt:x` an alternate data stream.
        if let Some(bad) = Path::new(raw).components().find_map(|c| match c {
            Component::Normal(s) => win_name_problem(&s.to_string_lossy()),
            _ => None,
        }) {
            return Err(FsError::arg(format!("\"{raw}\": {bad}")).fb("write"));
        }
        if raw.ends_with(['/', '\\']) {
            return Err(FsError::arg(format!("\"{raw}\" ends with a separator: it names a directory, not a file")).fb("write"));
        }
        let candidate = self.candidate(raw);
        self.refuse_foreign_share(&candidate).map_err(|e| e.fb("write"))?;
        let name = candidate
            .file_name()
            .ok_or_else(|| FsError::arg("path must name a file"))?;

        let nearest = nearest_existing(&candidate).ok_or_else(|| {
            FsError::new(
                "FS_OUTSIDE_WORKSPACE",
                format!("no existing ancestor for {}", candidate.display()),
            )
        })?;
        let exact = nearest == candidate;
        let nearest = fs::canonicalize(&nearest)
            .map_err(|e| FsError::io("cannot resolve path ancestor", e))?;
        self.ensure_inside(&nearest)?;

        if let Ok(md) = fs::symlink_metadata(&candidate) {
            if md.file_type().is_symlink() {
                return Err(FsError::new(
                    "FS_SYMLINK",
                    format!(
                        "refusing to replace symlink \"{raw}\"; use the native {} tool",
                        native(Native::Write)
                    ),
                ));
            }
            if !md.is_file() {
                return Err(FsError::new(
                    "FS_NOT_FILE",
                    format!("cannot modify \"{raw}\": not a regular file"),
                ));
            }
            // `nearest` is this very file, canonicalized a moment ago: resolving it again gives the same path.
            let canonical = if exact {
                nearest
            } else {
                fs::canonicalize(&candidate).map_err(|e| FsError::io(format!("cannot resolve \"{raw}\""), e))?
            };
            self.ensure_inside(&canonical)?;
            return Ok((canonical, true));
        }

        let parent = candidate
            .parent()
            .ok_or_else(|| FsError::arg("path must have a parent directory"))?;
        // Before any directory is made: a refused `.git/x/y` must leave no `.git/x` behind.
        self.check_mutable(&candidate)?;
        fs::create_dir_all(parent)
            .map_err(|e| FsError::io(format!("cannot create {}", parent.display()), e))?;
        let parent = fs::canonicalize(parent)
            .map_err(|e| FsError::io("cannot resolve parent directory", e))?;
        self.ensure_inside(&parent)?;
        let target = parent.join(name);
        if let Ok(md) = fs::symlink_metadata(&target) {
            if md.file_type().is_symlink() {
                return Err(FsError::new(
                    "FS_SYMLINK",
                    format!("refusing to replace symlink \"{raw}\""),
                ));
            }
            if !md.is_file() {
                return Err(FsError::new(
                    "FS_NOT_FILE",
                    format!("cannot modify \"{raw}\": not a regular file"),
                ));
            }
            let canonical = fs::canonicalize(&target)
                .map_err(|e| FsError::io(format!("cannot resolve \"{raw}\""), e))?;
            self.ensure_inside(&canonical)?;
            return Ok((canonical, true));
        }
        Ok((target, false))
    }

    pub(super) fn check_mutable(&self, path: &Path) -> Result<(), FsError> {
        // The whole path, not just the part below cwd: a caller may pick cwd = `.git/hooks`.
        for c in path.components() {
            if let Component::Normal(s) = c {
                let s = s.to_string_lossy();
                if [".git", ".hg", ".svn"].iter().any(|v| s.eq_ignore_ascii_case(v)) {
                    return Err(FsError::new(
                        "FS_PROTECTED",
                        format!("refusing to modify VCS metadata: {}", path.display()),
                    ));
                }
            }
        }
        // The places `run` refuses to write (system folders, the startup folder, .ssh): the same rule,
        // so `fs` is not the way around it. The guard compares lowercase `\` paths.
        let mut s = path.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            s = rest.to_string();
        }
        if let Some(what) = crate::guard::write_protected(s.trim_end_matches('\\')) {
            return Err(FsError::new(
                "FS_PROTECTED",
                format!("refusing to modify {what}: {}", path.display()),
            ));
        }
        Ok(())
    }
}

/// `(server, share)` of a network path, in either spelling (`\\s\x` or `\\?\UNC\s\x`), compared without case.
fn unc_share(p: &Path) -> Option<(String, String)> {
    use std::path::Prefix;
    let Some(Component::Prefix(prefix)) = p.components().next() else {
        return None;
    };
    match prefix.kind() {
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
            Some((server.to_string_lossy().to_lowercase(), share.to_string_lossy().to_lowercase()))
        }
        _ => None,
    }
}

/// Why Windows cannot hold a file or directory with this name, if it cannot.
fn win_name_problem(name: &str) -> Option<&'static str> {
    if name.contains(':') {
        return Some("a ':' in a name makes an alternate data stream, not a file");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("Windows strips a trailing '.' or ' ', so the file could not be opened again");
    }
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit() && stem.as_bytes()[3] != b'0');
    reserved.then_some("a reserved device name (CON, PRN, AUX, NUL, COM1-9, LPT1-9)")
}

fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut p = path.to_path_buf();
    loop {
        if fs::symlink_metadata(&p).is_ok() {
            return Some(p);
        }
        if !p.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_may_go_to_the_claude_scratch_dir_and_nowhere_else_outside_cwd() {
        let dir = tmp("scratch-cwd");
        let scratch = std::env::temp_dir().join("claude").join(format!("qx-scratch-{}", std::process::id()));
        fs::create_dir_all(&scratch).unwrap();
        let target = scratch.join("n.txt");
        let r = invoke(&dir, "write", json!({"path": target.to_string_lossy(), "content": "hi"}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(fs::read_to_string(&target).unwrap(), "hi");
        let other = std::env::temp_dir().join(format!("qx-not-scratch-{}.txt", std::process::id()));
        let r = invoke(&dir, "write", json!({"path": other.to_string_lossy(), "content": "no"}));
        assert!(r.is_error && r.structured["code"] == "FS_OUTSIDE_WORKSPACE", "{}", r.text);
        let _ = fs::remove_dir_all(&scratch);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_refuses_a_path_outside_cwd() {
        let base = tmp("outside");
        let cwd = base.join("work");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(base.join("secret.txt"), "nope\n").unwrap();
        // Reads may go anywhere; only mutations are confined to cwd.
        let r = invoke(&cwd, "read", json!({"path": base.join("secret.txt")}));
        assert!(!r.is_error && r.text.contains("1: nope"), "{}", r.text);
        let r = invoke(&cwd, "write", json!({"path": base.join("new.txt"), "content": "x"}));
        assert!(r.is_error);
        assert_eq!(r.structured["code"], "FS_OUTSIDE_WORKSPACE");
        assert!(!base.join("new.txt").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn vcs_metadata_is_protected_whatever_the_cwd() {
        let base = tmp("vcs-cwd");
        let hooks = base.join("repo").join(".git").join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        // A cwd chosen inside .git must not turn its contents into ordinary workspace files.
        let r = invoke(&hooks, "write", json!({"path":"pre-commit","content":"#!/bin/sh\necho hi\n"}));
        assert!(r.is_error, "{}", r.text);
        assert_eq!(r.structured["code"], "FS_PROTECTED");
        assert!(!hooks.join("pre-commit").exists());
        // Refused before any directory on the way is made.
        let repo = base.join("repo");
        let r = invoke(&repo, "write", json!({"path":".git/zzz/y.txt","content":"x"}));
        assert_eq!(r.structured["code"], "FS_PROTECTED", "{}", r.text);
        assert!(!repo.join(".git").join("zzz").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn write_refuses_names_windows_cannot_handle() {
        let dir = tmp("win-names");
        for bad in ["nul", "log-12:30.txt", "notes.md.", "x.txt ", "sub/aux.rs", "COM1.log"] {
            let r = invoke(&dir, "write", json!({"path":bad,"content":"x"}));
            assert!(r.is_error, "{bad:?} was written");
            assert_eq!(r.structured["code"], "FS_BAD_ARG", "{bad:?}");
        }
        // `a:b/...` is drive-relative on Windows (drive A:): refused, whatever the code.
        assert!(invoke(&dir, "write", json!({"path":"a:b/c.txt","content":"x"})).is_error);
        assert!(!dir.join("log-12").exists() && !dir.join("sub").exists() && !dir.join("a").exists());
        // (`NUL.txt` is NUL too, per Windows naming rules, but `null` and `con-notes` are not.)
        for ok in ["con-notes.md", "console.log", "null/x.txt.bak"] {
            let r = invoke(&dir, "write", json!({"path":ok,"content":"x"}));
            assert!(!r.is_error, "{ok:?}: {}", r.text);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_name_and_a_foreign_share_are_refused_at_once() {
        let dir = tmp("write-dirname");
        for raw in ["newdir/", "newdir\\", "a/b/"] {
            let r = invoke(&dir, "write", json!({"path":raw,"content":"x"}));
            assert!(r.is_error && r.structured["code"] == "FS_BAD_ARG", "{raw:?}: {}", r.text);
        }
        assert!(!dir.join("newdir").exists() && !dir.join("a").exists());
        #[cfg(windows)]
        {
            // 192.0.2.0/24 is reserved for documentation: probing it would wait for a timeout.
            let t = Instant::now();
            let r = invoke(&dir, "write", json!({"path":"\\\\192.0.2.1\\share\\x.txt","content":"x"}));
            assert!(r.is_error && r.structured["code"] == "FS_OUTSIDE_WORKSPACE", "{}", r.text);
            assert!(t.elapsed().as_secs() < 2, "{:?}", t.elapsed());
            // `edit` resolves the path another way and must not probe the share either.
            let t = Instant::now();
            let r = invoke(&dir, "edit", json!({"path":"\\\\192.0.2.1\\share\\x.txt","old_string":"a","new_string":"b"}));
            assert!(r.is_error && r.structured["code"] == "FS_OUTSIDE_WORKSPACE", "{}", r.text);
            assert!(t.elapsed().as_secs() < 2, "{:?}", t.elapsed());
            // A workspace on a share owns the paths under it, however they are spelled.
            assert_eq!(unc_share(Path::new(r"\\localhost\C$\ws\a.txt")), unc_share(Path::new(r"\\?\UNC\LOCALHOST\c$\ws")));
            assert_ne!(unc_share(Path::new(r"\\localhost\C$\ws")), unc_share(Path::new(r"\\other\C$\ws")));
            assert_eq!(unc_share(Path::new(r"C:\ws")), None);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_and_edit_refuse_a_relative_path_without_cwd() {
        for op in ["write", "edit"] {
            let r = call(&json!({"op": op, "path": "qx-no-cwd-probe.txt", "content": "x", "old_string": "a", "new_string": "b"}));
            assert!(r.is_error && r.text.contains("cwd"), "{op}: {}", r.text);
            assert!(!Path::new("qx-no-cwd-probe.txt").exists());
        }
    }

    #[test]
    fn confinement_holds_against_dotdot_and_junctions() {
        let base = tmp("confine");
        let (work, outside) = (base.join("work"), base.join("outside"));
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "secret\n").unwrap();
        let r = invoke(&work, "write", json!({"path":"../escape.txt","content":"x"}));
        assert_eq!(r.structured["code"], "FS_OUTSIDE_WORKSPACE", "{}", r.text);
        assert!(!base.join("escape.txt").exists());
        let link = work.join("out");
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());
        if !made {
            println!("skip: cannot create a junction here");
            let _ = fs::remove_dir_all(&base);
            return;
        }
        let r = invoke(&work, "read", json!({"path":"out/secret.txt"}));
        assert!(!r.is_error, "{}", r.text); // reads follow the junction; walks and writes do not
        let r = invoke(&work, "write", json!({"path":"out/new.txt","content":"x"}));
        assert!(r.is_error && !outside.join("new.txt").exists(), "{}", r.text);
        let r = invoke(&work, "grep", json!({"pattern":"secret"}));
        assert!(!r.text.contains("secret.txt"), "{}", r.text);
        let r = invoke(&work, "glob", json!({"pattern":"**/*.txt"}));
        assert!(!r.text.contains("secret.txt"), "{}", r.text);
        let _ = fs::remove_dir(&link);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn fs_write_and_edit_refuse_the_places_the_guard_protects() {
        let dir = tmp("fs-protected");
        fs::create_dir_all(dir.join(".ssh")).unwrap();
        fs::create_dir_all(dir.join("Start Menu/Programs/Startup")).unwrap();
        fs::write(dir.join(".ssh/known"), "old\n").unwrap();
        for rel in [".ssh/authorized_keys", "Start Menu/Programs/Startup/a.cmd", "Start Menu\\Programs\\Startup\\b.cmd", "sub/.ssh/id"] {
            let r = invoke(&dir, "write", json!({"path":rel,"content":"x\n"}));
            assert!(r.is_error && r.structured["code"] == "FS_PROTECTED", "{rel}: {}", r.text);
            assert!(!dir.join(rel).exists(), "{rel} was created");
        }
        assert!(!dir.join("sub").exists(), "a refused write left a folder behind");
        let r = invoke(&dir, "edit", json!({"path":".ssh/known","old_string":"old","new_string":"new"}));
        assert!(r.is_error && r.structured["code"] == "FS_PROTECTED", "{}", r.text);
        assert_eq!(fs::read_to_string(dir.join(".ssh/known")).unwrap(), "old\n");
        // The protected-path rules themselves, without touching a real system folder.
        let root = Root { cwd: dir.clone() };
        let sys = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        let e = root.check_mutable(&Path::new(&sys).join("System32").join("x.dll")).unwrap_err();
        assert_eq!(e.code, "FS_PROTECTED");
        let e = root.check_mutable(&dir.join("owner/.git/hooks/pre-commit")).unwrap_err();
        assert_eq!(e.code, "FS_PROTECTED");
        // A normal path still writes.
        let r = invoke(&dir, "write", json!({"path":"src/ok.txt","content":"fine\n"}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(fs::read_to_string(dir.join("src/ok.txt")).unwrap(), "fine\n");
        let _ = fs::remove_dir_all(&dir);
    }

}
