//! Observations, locking, atomic writes, and the `write` and `edit` operations.

use super::*;

// --------------------------------------------------------------- observation

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Version {
    pub(super) len: u64,
    modified: Option<SystemTime>,
    pub(super) hash: u64,
}

impl Version {
    fn of(bytes: &[u8], modified: Option<SystemTime>) -> Self {
        Self {
            len: bytes.len() as u64,
            modified,
            hash: hash_bytes(bytes),
        }
    }
}

/// A content fingerprint over every byte, so a same-size edit anywhere in the file is seen (the clock is
/// not compared: a `touch` is no conflict). Eight bytes a step (the multiply is odd, so each step is a
/// bijection and one changed word always changes the hash); it only ever meets hashes made by this
/// same function, always of the whole file.
fn hash_bytes(bytes: &[u8]) -> u64 {
    const K: u64 = 0x517c_c1b7_2722_0a95;
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    let mut words = bytes.chunks_exact(8);
    for w in &mut words {
        h = (h.rotate_left(5) ^ u64::from_le_bytes(w.try_into().unwrap_or_default())).wrapping_mul(K);
    }
    let rest = words.remainder();
    if !rest.is_empty() {
        let mut last = [0u8; 8];
        last[..rest.len()].copy_from_slice(rest);
        h = (h.rotate_left(5) ^ u64::from_le_bytes(last)).wrapping_mul(K);
    }
    h
}

/// Keyed by the file alone: the same file reached through another `cwd` is the same file. Each
/// entry keeps when it was last read, so past `OBSERVED_MAX` files the stalest one makes room.
fn observations() -> &'static Mutex<HashMap<PathBuf, (Version, Instant)>> {
    static MAP: OnceLock<Mutex<HashMap<PathBuf, (Version, Instant)>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

const OBSERVED_MAX: usize = 4096;

pub(super) fn remember(path: &Path, version: Version) {
    observe_in(&mut observations().lock().unwrap_or_else(|e| e.into_inner()), path, version);
}

fn observe_in(map: &mut HashMap<PathBuf, (Version, Instant)>, path: &Path, version: Version) {
    if map.len() >= OBSERVED_MAX && !map.contains_key(path) {
        if let Some(oldest) = map.iter().min_by_key(|(_, (_, at))| *at).map(|(p, _)| p.clone()) {
            map.remove(&oldest);
        }
    }
    map.insert(path.to_path_buf(), (version, Instant::now()));
}

fn check_observed(path: &Path, current: Version, display: &str) -> Result<(), FsError> {
    let map = observations().lock().unwrap_or_else(|e| e.into_inner());
    match map.get(path).map(|(v, _)| v) {
        None => Err(FsError::new(
            "FS_NOT_OBSERVED",
            format!(
                "cannot modify \"{display}\": read it first (op=read; a partial read counts), then write; or use the native {} tool",
                native(Native::Write)
            ),
        )),
        // Content decides, not the clock: a `touch` or a save without changes is not a conflict.
        Some(v) if v.len != current.len || v.hash != current.hash => Err(FsError::new(
            "FS_STALE",
            format!(
                "cannot modify \"{display}\": file changed since it was read — re-read it, or use the native {} tool",
                native(Native::Write)
            ),
        )),
        Some(_) => Ok(()),
    }
}

fn mutation_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Held from the check that a file is as it was read until the new content is in place. Other qx
/// servers (another session, another agent) edit the same tree, so on Windows the lock is a named
/// mutex the system also releases if its owner dies; past `MUTATION_WAIT_MS` the call goes on
/// without it rather than hang.
struct MutationGuard {
    _local: std::sync::MutexGuard<'static, ()>,
    #[cfg(windows)]
    held: bool,
}

const MUTATION_WAIT_MS: u32 = 5000;

fn lock_mutation() -> MutationGuard {
    let local = mutation_lock().lock().unwrap_or_else(|e| e.into_inner());
    MutationGuard {
        _local: local,
        #[cfg(windows)]
        held: os_mutex::acquire(MUTATION_WAIT_MS),
    }
}

impl Drop for MutationGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.held {
            os_mutex::release();
        }
    }
}

#[cfg(windows)]
mod os_mutex {
    use std::ffi::c_void;
    use std::sync::OnceLock;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(attributes: *mut c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
        fn ReleaseMutex(handle: *mut c_void) -> i32;
    }
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_ABANDONED: u32 = 0x80;

    /// One handle for the life of the process (null when the system refused one); kept as an
    /// address because a raw pointer cannot sit in a static.
    fn handle() -> *mut c_void {
        static HANDLE: OnceLock<usize> = OnceLock::new();
        *HANDLE.get_or_init(|| {
            let name: Vec<u16> = "Local\\qx-fs-mutation\0".encode_utf16().collect();
            // SAFETY: a NUL-terminated name and default security.
            unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) as usize }
        }) as *mut c_void
    }

    /// True when this thread now owns the mutex (an abandoned one counts: its owner died).
    pub fn acquire(millis: u32) -> bool {
        let h = handle();
        // SAFETY: `h` is a live handle for the whole process.
        !h.is_null() && matches!(unsafe { WaitForSingleObject(h, millis) }, WAIT_OBJECT_0 | WAIT_ABANDONED)
    }

    pub fn release() {
        // SAFETY: only called by the thread that acquired it.
        unsafe { ReleaseMutex(handle()) };
    }
}

// ---------------------------------------------------------------------- I/O

pub(super) fn read_file(path: &Path, fallback: &'static str) -> Result<(Vec<u8>, Version), FsError> {
    let md = fs::metadata(path)
        .map_err(|e| FsError::io(format!("cannot read \"{}\"", path.display()), e).fb(fallback))?;
    if !md.is_file() {
        return Err(FsError::new(
            "FS_NOT_FILE",
            format!("cannot read \"{}\": not a regular file", path.display()),
        )
        .fb(fallback));
    }
    if md.len() > MAX_READ_BYTES {
        return Err(FsError::new(
            "FS_TOO_LARGE",
            format!(
                "cannot read \"{}\": file is {} bytes (qx cap {}); use the native {} tool",
                path.display(),
                md.len(),
                MAX_READ_BYTES,
                native(Native::Read)
            ),
        )
        .fb(fallback));
    }
    let bytes = fs::read(path)
        .map_err(|e| FsError::io(format!("cannot read \"{}\"", path.display()), e).fb(fallback))?;
    if bytes.len() as u64 > MAX_READ_BYTES {
        return Err(FsError::new(
            "FS_TOO_LARGE",
            format!(
                "cannot read \"{}\": file grew past the qx cap",
                path.display()
            ),
        )
        .fb(fallback));
    }
    let version = Version::of(&bytes, md.modified().ok());
    Ok((bytes, version))
}

fn version_after(path: &Path, bytes: &[u8]) -> Result<Version, FsError> {
    let md = fs::metadata(path)
        .map_err(|e| FsError::io(format!("cannot stat \"{}\"", path.display()), e))?;
    Ok(Version::of(bytes, md.modified().ok()))
}

/// A killed write leaves its `.qx-tmp-*` beside the target. The first write into a folder (per process)
/// removes those older than an hour.
fn sweep_stale_temps(parent: &Path) {
    static SWEPT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    {
        let mut swept = SWEPT.lock().unwrap_or_else(|e| e.into_inner());
        if swept.iter().any(|p| p == parent) {
            return;
        }
        if swept.len() >= 64 {
            swept.clear();
        }
        swept.push(parent.to_path_buf());
    }
    let Ok(rd) = fs::read_dir(parent) else { return };
    let hour = std::time::Duration::from_secs(3600);
    for e in rd.flatten() {
        if !e.file_name().to_string_lossy().starts_with(".qx-tmp-") {
            continue;
        }
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age >= hour);
        if old {
            let _ = fs::remove_file(e.path());
        }
    }
}

fn write_temp(parent: &Path, bytes: &[u8]) -> Result<Temp, FsError> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    sweep_stale_temps(parent);
    for _ in 0..100 {
        let path = parent.join(format!(
            ".qx-tmp-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(bytes).and_then(|_| f.sync_all()) {
                    let _ = fs::remove_file(&path);
                    return Err(FsError::io("cannot write temporary file", e));
                }
                return Ok(Temp { path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(FsError::io("cannot create temporary file", e)),
        }
    }
    Err(FsError::new(
        "FS_IO",
        "cannot allocate a temporary file after 100 attempts",
    ))
}

struct Temp {
    path: PathBuf,
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// `ReplaceFileW` keeps what belongs to the file rather than its bytes: alternate data streams
/// (a download's `Zone.Identifier`), ACLs, attributes and creation time. Where it cannot (some
/// file systems and shares), a plain replacing move still lands the new content.
#[cfg(windows)]
fn replace_file(temp: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn ReplaceFileW(
            replaced: *const u16,
            replacement: *const u16,
            backup: *const u16,
            flags: u32,
            exclude: *mut std::ffi::c_void,
            reserved: *mut std::ffi::c_void,
        ) -> i32;
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    const REPLACEFILE_IGNORE_MERGE_ERRORS: u32 = 0x2;
    const REPLACEFILE_IGNORE_ACL_ERRORS: u32 = 0x4;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let wide = |p: &Path| p.as_os_str().encode_wide().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (old, new) = (wide(temp), wide(target));
    let null = std::ptr::null_mut();
    let flags = REPLACEFILE_IGNORE_MERGE_ERRORS | REPLACEFILE_IGNORE_ACL_ERRORS;
    // SAFETY: NUL-terminated paths; no backup file, no exclusions.
    if unsafe { ReplaceFileW(new.as_ptr(), old.as_ptr(), std::ptr::null(), flags, null, null) } != 0 {
        return Ok(());
    }
    // Every failure leaves the replacement under its own name, and the target either as it was or
    // gone; the move puts the new content in place either way.
    // SAFETY: as above.
    if unsafe { MoveFileExW(old.as_ptr(), new.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(temp: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(temp, target)
}

/// The folder a mutation lands in, held open from its last check until the new file is in place.
/// The path was checked by name, and a folder on it can be swapped for a junction to anywhere before
/// the write; so the write goes through the folder's final path, checked against the root again, and
/// the handle (no delete sharing) keeps that folder from being renamed or removed meanwhile. NTFS
/// also refuses to rename a folder with an open handle below it, so the folders above stay put too.
struct Pinned {
    dir: PathBuf,
    #[cfg(windows)]
    _handle: DirHandle,
}

/// Pins the parent of `path`, returning it and the path to write through.
fn pin_parent(root: &Root, path: &Path) -> Result<(Pinned, PathBuf), FsError> {
    let (parent, name) = match (path.parent(), path.file_name()) {
        (Some(p), Some(n)) => (p, n),
        _ => return Err(FsError::arg("path must have a parent directory")),
    };
    let pinned = open_dir_final(parent).map_err(|e| FsError::io(format!("cannot open {}", parent.display()), e))?;
    root.ensure_inside(&pinned.dir)?;
    let target = pinned.dir.join(name);
    Ok((pinned, target))
}

#[cfg(windows)]
struct DirHandle(*mut std::ffi::c_void);

#[cfg(windows)]
impl Drop for DirHandle {
    fn drop(&mut self) {
        #[link(name = "kernel32")]
        extern "system" {
            fn CloseHandle(h: *mut std::ffi::c_void) -> i32;
        }
        // SAFETY: a handle this process opened and closes once.
        unsafe { CloseHandle(self.0) };
    }
}

#[cfg(windows)]
fn open_dir_final(dir: &Path) -> std::io::Result<Pinned> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(name: *const u16, access: u32, share: u32, sa: *mut std::ffi::c_void, disposition: u32, flags: u32, template: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
        fn GetFinalPathNameByHandleW(h: *mut std::ffi::c_void, buf: *mut u16, len: u32, flags: u32) -> u32;
    }
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_SHARE_READ_WRITE: u32 = 0x3;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let null = std::ptr::null_mut();
    // SAFETY: a NUL-terminated path, default security, no template.
    let h = unsafe { CreateFileW(wide.as_ptr(), FILE_READ_ATTRIBUTES, FILE_SHARE_READ_WRITE, null, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, null) };
    if h as isize == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let handle = DirHandle(h);
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: `buf` holds `buf.len()` u16s; flags 0 = the DOS name, as `canonicalize` gives it.
        let n = unsafe { GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, 0) } as usize;
        if n == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            break;
        }
        buf.resize(n, 0);
    }
    Ok(Pinned { dir: PathBuf::from(std::ffi::OsString::from_wide(&buf)), _handle: handle })
}

// shortcut: no handle is held off Windows, so a swap between this check and the rename still escapes.
#[cfg(not(windows))]
fn open_dir_final(dir: &Path) -> std::io::Result<Pinned> {
    Ok(Pinned { dir: fs::canonicalize(dir)? })
}

fn atomic_replace(root: &Root, path: &Path, bytes: &[u8], expected: Version) -> Result<(), FsError> {
    let (pinned, target) = pin_parent(root, path)?;
    let temp = write_temp(&pinned.dir, bytes)?;

    // Re-check immediately before replacing. This is the last point at which a
    // concurrent writer can be detected; the provider window is one rename.
    let md = fs::metadata(&target).map_err(|_| {
        FsError::new(
            "FS_STALE",
            format!("cannot modify \"{}\": file disappeared", path.display()),
        )
    })?;
    if md.len() != expected.len || md.modified().ok() != expected.modified {
        return Err(FsError::new(
            "FS_STALE",
            format!(
                "cannot modify \"{}\": file changed since it was read",
                path.display()
            ),
        ));
    }
    // Read-only fails with the same error 5 a brief lock gives; say so instead of retrying for 600 ms.
    if md.permissions().readonly() {
        return Err(FsError::new("FS_IO", format!("cannot modify \"{}\": the file is read-only", path.display())));
    }
    // An editor, indexer or virus scanner may hold the file for a moment without delete sharing.
    let mut attempt = 0;
    loop {
        match replace_file(&temp.path, &target) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 5 && matches!(e.raw_os_error(), Some(5 | 32 | 33)) => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(40 * attempt));
            }
            Err(e) => return Err(FsError::io(format!("cannot replace \"{}\"", path.display()), e)),
        }
    }
}

/// Puts the synced temp file in place under `target` only if nothing is there, in one step: the
/// new file appears whole or not at all. A move needs no hard-link support (FAT/exFAT have none).
#[cfg(windows)]
fn move_noreplace(temp: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let wide = |p: &Path| p.as_os_str().encode_wide().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (old, new) = (wide(temp), wide(target));
    // SAFETY: NUL-terminated paths. Without MOVEFILE_REPLACE_EXISTING an existing target fails
    // with ERROR_ALREADY_EXISTS / ERROR_FILE_EXISTS (both `AlreadyExists`).
    if unsafe { MoveFileExW(old.as_ptr(), new.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn move_noreplace(temp: &Path, target: &Path) -> std::io::Result<()> {
    match fs::hard_link(temp, target) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            // No hard links (FAT mounts): a rename still lands the content whole; only a file
            // created in the moment between the check and the rename could be replaced.
            if fs::symlink_metadata(target).is_ok() {
                return Err(std::io::ErrorKind::AlreadyExists.into());
            }
            fs::rename(temp, target)
        }
        r => r,
    }
}

fn create_noreplace(root: &Root, path: &Path, bytes: &[u8]) -> Result<(), FsError> {
    let (pinned, target) = pin_parent(root, path)?;
    let temp = write_temp(&pinned.dir, bytes)?;
    match move_noreplace(&temp.path, &target) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(FsError::new(
            "FS_EXISTS",
            format!(
                "cannot create \"{}\": file appeared after the observation check",
                path.display()
            ),
        )),
        Err(e) => Err(FsError::io(format!("cannot create \"{}\"", path.display()), e)),
    }
}

// -------------------------------------------------------------------- write

pub(super) fn write(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let raw = req_path(args)?;
    let content = match args.get("content").and_then(Value::as_str) {
        Some(s) => s,
        None => return Err(FsError::arg("content must be a string")),
    };
    if content.len() > MAX_WRITE_BYTES {
        return Err(FsError::new(
            "FS_TOO_LARGE",
            format!(
                "content is {} bytes (qx cap {}); use the native {} tool",
                content.len(),
                MAX_WRITE_BYTES,
                native(Native::Write)
            ),
        )
        .fb("write"));
    }
    let guard = lock_mutation();
    let (path, existed) = root.resolve_for_write(raw).map_err(|e| e.fb("write"))?;
    root.check_mutable(&path).map_err(|e| e.fb("write"))?;
    let display = root.display(&path);

    let mut written = std::borrow::Cow::Borrowed(content);
    let mut before = Vec::new();
    if existed {
        let (bytes, current) = read_file(&path, "write")?;
        check_observed(&path, current, &display).map_err(|e| e.fb("write"))?;
        // `read` showed lossy, LF-only text: writing it back must not re-encode or flatten the file.
        match carry_over_encoding(&bytes, content) {
            Ok(text) => written = text,
            Err(()) => {
                return Err(FsError::new(
                    "FS_ENCODING",
                    format!("\"{raw}\" is not plain UTF-8 text (UTF-16, a legacy code page or NUL bytes); qx write would corrupt it"),
                )
                .fb("write"));
            }
        }
        // The same bytes need no replacing: a rewrite would only wake file watchers and move the clock.
        if written.as_bytes() != bytes {
            atomic_replace(root, &path, written.as_bytes(), current).map_err(|e| e.fb("write"))?;
        }
        before = bytes;
    } else {
        create_noreplace(root, &path, content.as_bytes()).map_err(|e| e.fb("write"))?;
    }
    let content = written.as_ref();
    let version = version_after(&path, content.as_bytes()).map_err(|e| e.fb("write"))?;
    remember(&path, version);
    // Only the reply is left to do, and the line count is not part of the change: not under the lock.
    drop(guard);
    let delta = line_delta(&before, content.as_bytes());
    let verb = if existed {
        "Updated file"
    } else {
        "Created file"
    };
    let said = format!("{verb} {}", lines_note(!existed, delta));
    Ok(Reply {
        text: if lean() { said } else { format!("<path>{display}</path>\n<type>file</type>\n<content>\n{said}\n</content>") },
        structured: json!({
            "op": "write",
            "path": display,
            "created": !existed,
            "bytes": content.len(),
            "added": delta.0,
            "removed": delta.1,
        }),
        is_error: false,
    })
}

// --------------------------------------------------------------------- edit

/// What a reply says about the lines a write or edit changed.
fn lines_note(created: bool, (added, removed): (usize, usize)) -> String {
    match (added, removed) {
        (0, 0) if created => "(empty)".to_string(),
        (0, 0) => "(no changes)".to_string(),
        _ => format!("(+{added} -{removed} lines)"),
    }
}

/// Length of the common prefix of `a` and `b`; 64 bytes a step while they match (a vectorised compare).
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + 64 <= n && a[i..i + 64] == b[i..i + 64] {
        i += 64;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// Length of the common suffix of `a` and `b`.
fn common_suffix(a: &[u8], b: &[u8]) -> usize {
    let (n, la, lb) = (a.len().min(b.len()), a.len(), b.len());
    let mut i = 0;
    while i + 64 <= n && a[la - i - 64..la - i] == b[lb - i - 64..lb - i] {
        i += 64;
    }
    while i < n && a[la - 1 - i] == b[lb - 1 - i] {
        i += 1;
    }
    i
}

/// Bytes at the start of `a` and `b` that make up whole lines both have (a line keeps its `\n`).
fn shared_head(a: &[u8], b: &[u8]) -> usize {
    let n = common_prefix(a, b);
    if n == a.len() && n == b.len() {
        return n; // the same text, a last line without `\n` included
    }
    a[..n].iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1)
}

/// Bytes at the end of `a` and `b` that make up whole lines both have. Both texts start at a line start.
fn shared_tail(a: &[u8], b: &[u8]) -> usize {
    let n = common_suffix(a, b);
    let (sa, sb) = (a.len() - n, b.len() - n);
    let starts = |t: &[u8], s: usize| s == 0 || t[s - 1] == b'\n';
    if starts(a, sa) && starts(b, sb) {
        return n;
    }
    // The first shared line is cut short on one side: the shared lines begin after its end.
    a[sa..].iter().position(|&c| c == b'\n').map_or(0, |i| n - i - 1)
}

/// Lines `(added, removed)` going from `old` to `new`: the lines left in each once the whole lines they
/// share at the start and at the end are cut. A line includes its terminator, so a change of line ending
/// or of the final newline counts. Exact when the change is one stretch, as an edit or a rewrite of one
/// place is; for changes scattered through the file the lines between them count as replaced, so the
/// numbers are an upper bound.
fn line_delta(old: &[u8], new: &[u8]) -> (usize, usize) {
    let head = shared_head(old, new);
    let (old, new) = (&old[head..], &new[head..]);
    let tail = shared_tail(old, new);
    (count_lines(&new[..new.len() - tail]), count_lines(&old[..old.len() - tail]))
}

/// One `{old_string, new_string, replace_all?}` replacement, applied to `text` in memory.
fn apply_edit(text: &str, raw: &str, e: &Value, label: &str) -> Result<(String, usize), FsError> {
    let old = match e.get("old_string").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s,
        _ => return Err(FsError::arg(format!("{label}old_string must be a non-empty string"))),
    };
    let new = match e.get("new_string").and_then(Value::as_str) {
        Some(s) => s,
        None => return Err(FsError::arg(format!("{label}new_string must be a string"))),
    };
    if old == new {
        return Err(FsError::arg(format!("{label}old_string and new_string must differ")));
    }
    if old.len() > MAX_EDIT_BYTES || new.len() > MAX_EDIT_BYTES {
        return Err(FsError::new(
            "FS_TOO_LARGE",
            format!("{label}edit strings are capped at {MAX_EDIT_BYTES} bytes"),
        )
        .fb("edit"));
    }
    let replace_all = bool_arg(e, "replace_all", false);
    let (old, new) = match_line_endings(text, old, new);
    // One pass finds the matches and builds the result; a second hit without `replace_all` is an
    // error, so the scan stops there and the total is counted only for the message.
    let mut updated = String::with_capacity(text.len() + new.len().saturating_sub(old.len()));
    // `replace_all` multiplies `new` by the match count; stop before that outgrows memory.
    let cap = MAX_WRITE_BYTES.max(text.len().saturating_mul(2));
    let (mut done, mut count) = (0, 0);
    for (at, hit) in text.match_indices(&old) {
        count += 1;
        if count > 1 && !replace_all {
            break;
        }
        updated.push_str(&text[done..at]);
        updated.push_str(&new);
        done = at + hit.len();
        if updated.len() > cap {
            return Err(FsError::new(
                "FS_TOO_LARGE",
                format!("{label}the edited file would pass {cap} bytes; use the native {} tool", native(Native::Edit)),
            )
            .fb("edit"));
        }
    }
    if count == 0 {
        // Say where the closest thing is: usually an indentation or a stale line, found in one step.
        let first = old.lines().map(str::trim).find(|l| l.len() > 3).unwrap_or("");
        let hint = text
            .lines()
            .position(|l| !first.is_empty() && l.contains(first))
            .map(|i| format!("; its first line appears at line {} (whitespace or later lines differ)", i + 1))
            .unwrap_or_default();
        return Err(FsError::new(
            "FS_NO_MATCH",
            format!("{label}old_string was not found in \"{raw}\"{hint}"),
        )
        .fb("edit"));
    }
    if count > 1 && !replace_all {
        let count = text.matches(&old).count();
        return Err(FsError::new(
            "FS_AMBIGUOUS",
            format!("{label}old_string occurs {count} times in \"{raw}\"; include more context or set replace_all"),
        )
        .fb("edit"));
    }
    updated.push_str(&text[done..]);
    Ok((updated, count))
}

pub(super) fn edit(root: &Root, args: &Value) -> Result<Reply, FsError> {
    let raw = req_path(args)?;
    // `edits` is a batch on one file: applied in order to the text as the earlier ones left it, written
    // once, and all-or-nothing, so a model fixes five places in one round trip instead of five.
    let batch: Option<&Vec<Value>> = args.get("edits").and_then(Value::as_array).filter(|a| !a.is_empty());
    if batch.is_none() && args.get("edits").is_some_and(|v| !v.is_null()) {
        return Err(FsError::arg("edits must be a non-empty array of {old_string, new_string, replace_all?}"));
    }
    if batch.is_some_and(|b| b.len() > 64) {
        return Err(FsError::arg("edits is capped at 64 per call"));
    }
    let guard = lock_mutation();
    let path = root.resolve_existing(raw).map_err(|e| e.fb("edit"))?;
    root.check_mutable(&path).map_err(|e| e.fb("edit"))?;
    let (bytes, current) = read_file(&path, "edit")?;
    // No prior read is required: `old_string` must match the current text exactly once, which is a
    // stronger check than "was read", and `atomic_replace` still refuses a file that changed meanwhile.

    // Strict: a lossy decode would rewrite UTF-16/Latin-1 files as UTF-8 and drop NULs on save.
    // The UTF-16 BOMs are never valid UTF-8, so this one check covers them too.
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Err(FsError::new(
            "FS_ENCODING",
            format!("\"{raw}\" is not valid UTF-8 (UTF-16 or a legacy code page); qx edit would corrupt it"),
        )
        .fb("edit"));
    };
    // `steps` is the lines each edit of a batch changed, added up: exact for edits in different places,
    // too high when a later edit rewrites lines an earlier one touched. The whole-file figure below is
    // the other way round, so the smaller of the two is the closer bound.
    let mut steps = (0usize, 0usize);
    let (updated, replacements) = match batch {
        Some(list) => {
            let mut cur = String::new();
            let mut total = 0;
            for (i, e) in list.iter().enumerate() {
                let label = format!("edits[{i}]: ");
                // The first edit reads the file's own text; no copy of it is made.
                let before = if i == 0 { text } else { cur.as_str() };
                let (next, n) = apply_edit(before, raw, e, &label)?;
                let d = line_delta(before.as_bytes(), next.as_bytes());
                steps = (steps.0 + d.0, steps.1 + d.1);
                cur = next;
                total += n;
            }
            (cur, total)
        }
        None => apply_edit(text, raw, args, "")?,
    };
    let replace_all = batch.is_none() && bool_arg(args, "replace_all", false);
    if updated.as_bytes() != bytes {
        atomic_replace(root, &path, updated.as_bytes(), current).map_err(|e| e.fb("edit"))?;
    }
    let version = version_after(&path, updated.as_bytes()).map_err(|e| e.fb("edit"))?;
    remember(&path, version);
    // Only the reply is left to do, and the line count is not part of the change: not under the lock.
    drop(guard);
    let mut delta = line_delta(&bytes, updated.as_bytes());
    if batch.is_some() {
        delta = (delta.0.min(steps.0), delta.1.min(steps.1));
    }
    let display = root.display(&path);
    let suffix = match (batch, replace_all) {
        (Some(b), _) => format!(" {} edits applied.", b.len()),
        (None, true) => " All occurrences were successfully replaced.".to_string(),
        _ => String::new(),
    };
    Ok(Reply {
        text: format!("The file {display} has been updated successfully.{suffix} {}", lines_note(false, delta)),
        structured: json!({
            "op": "edit",
            "path": display,
            "replacements": replacements,
            "added": delta.0,
            "removed": delta.1,
            "bytes": updated.len(),
        }),
        is_error: false,
    })
}
/// The model sees LF-only text (`read` strips the CRs), so `old` and `new` arrive with LF. Try `old`
/// verbatim first (it may sit in the LF part of a mixed file); only when it is absent, spell it in the
/// file's other ending. `new` then takes the ending of the text it replaces, whether or not `old` itself
/// held a line break.
fn match_line_endings(text: &str, old: &str, new: &str) -> (String, String) {
    // Without a CR there is no CRLF to count (and `crlf > lf_only` cannot hold): skip two scans of the file.
    let (crlf, lf_only) = if text.contains('\r') {
        let crlf = text.matches("\r\n").count();
        (crlf, text.matches('\n').count().saturating_sub(crlf))
    } else {
        (0, 0)
    };
    let mut old = old.to_string();
    if !text.contains(&old) {
        if crlf > 0 && old.contains('\n') && !old.contains("\r\n") {
            old = old.replace('\n', "\r\n");
        } else if crlf == 0 && old.contains("\r\n") {
            old = old.replace("\r\n", "\n");
        }
    }
    let new = if old.contains("\r\n") || (crlf > lf_only && !old.contains('\n')) {
        new.replace("\r\n", "\n").replace('\n', "\r\n")
    } else if crlf == 0 {
        new.replace("\r\n", "\n")
    } else {
        new.to_string()
    };
    (old, new)
}

/// What `write` must keep of the file it replaces: the model read LF-only text without the BOM.
fn carry_over_encoding<'a>(before: &[u8], content: &'a str) -> Result<std::borrow::Cow<'a, str>, ()> {
    use std::borrow::Cow;
    let Ok(old) = std::str::from_utf8(before) else {
        return Err(()); // UTF-16 or a legacy code page: rewriting it as UTF-8 would corrupt it
    };
    // `read` hides NUL bytes, so content made from what it showed would silently drop them.
    if old.contains('\0') && !content.contains('\0') {
        return Err(());
    }
    // Mostly CRLF: only a file with a CR can be, so the counts are for those alone.
    let crlf_file = old.contains('\r') && {
        let crlf = old.matches("\r\n").count();
        crlf > old.matches('\n').count().saturating_sub(crlf)
    };
    // The common case, an LF file without a BOM, writes `content` as it is: no copy.
    let mut out = Cow::Borrowed(content);
    if crlf_file && !content.contains('\r') {
        out = Cow::Owned(content.replace('\n', "\r\n"));
    }
    if old.starts_with('\u{feff}') && !out.starts_with('\u{feff}') {
        out.to_mut().insert(0, '\u{feff}');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_a_read_only_file_says_so_at_once() {
        let dir = tmp("readonly");
        let f = dir.join("ro.txt");
        fs::write(&f, "a\n").unwrap();
        let mut p = fs::metadata(&f).unwrap().permissions();
        p.set_readonly(true);
        fs::set_permissions(&f, p.clone()).unwrap();
        let t = std::time::Instant::now();
        let r = invoke(&dir, "edit", json!({"path":"ro.txt","old_string":"a","new_string":"b"}));
        assert!(r.is_error && r.text.contains("read-only"), "{}", r.text);
        // ~10 ms alone, over 300 ms under full-suite load; the old retry loop took 600+ ms.
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "{:?}", t.elapsed());
        #[allow(clippy::permissions_set_readonly_false)]
        p.set_readonly(false);
        fs::set_permissions(&f, p).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn creating_a_file_moves_the_whole_temp_in_and_never_replaces() {
        let dir = tmp("noreplace");
        let f = dir.join("new.txt");
        let root = Root { cwd: fs::canonicalize(&dir).unwrap() };
        create_noreplace(&root, &f, b"first\n").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"first\n");
        let e = create_noreplace(&root, &f, b"second\n").expect_err("an existing file is never replaced");
        assert_eq!(e.code, "FS_EXISTS");
        assert_eq!(fs::read(&f).unwrap(), b"first\n");
        let temps = fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(".qx-tmp-")).count();
        assert_eq!(temps, 0, "no temp file is left behind");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_parent_swapped_for_a_junction_after_the_check_is_refused() {
        let dir = tmp("junction-race");
        let outside = tmp("junction-race-out");
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub").join("a.txt"), "old\n").unwrap();
        let root = Root { cwd: fs::canonicalize(&dir).unwrap() };
        let (new, existed) = root.resolve_for_write("sub/new.txt").unwrap();
        assert!(!existed);
        let (old, existed) = root.resolve_for_write("sub/a.txt").unwrap();
        assert!(existed);
        // The checked folder becomes a junction to a folder outside the workspace.
        fs::rename(dir.join("sub"), dir.join("moved")).unwrap();
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(dir.join("sub"))
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());
        if !made {
            eprintln!("skipped: cannot create a junction here");
            let _ = fs::remove_dir_all(&dir);
            let _ = fs::remove_dir_all(&outside);
            return;
        }
        fs::write(outside.join("a.txt"), "old\n").unwrap();
        let e = create_noreplace(&root, &new, b"x\n").expect_err("a create through the junction is refused");
        assert_eq!(e.code, "FS_OUTSIDE_WORKSPACE");
        let e = atomic_replace(&root, &old, b"y\n", Version::of(b"old\n", None)).expect_err("a replace through the junction is refused");
        assert_eq!(e.code, "FS_OUTSIDE_WORKSPACE");
        let left: Vec<_> = fs::read_dir(&outside).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, [std::ffi::OsString::from("a.txt")], "nothing lands outside");
        assert_eq!(fs::read(outside.join("a.txt")).unwrap(), b"old\n");
        let _ = fs::remove_dir(dir.join("sub"));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn the_observed_files_stay_bounded_and_the_stalest_make_room() {
        let mut map = HashMap::new();
        let v = Version::of(b"x", None);
        for i in 0..OBSERVED_MAX + 10 {
            observe_in(&mut map, Path::new(&format!("obs-{i}")), v);
        }
        assert_eq!(map.len(), OBSERVED_MAX);
        // The ones evicted were among the first read; every later one is still known.
        assert!((100..OBSERVED_MAX + 10).all(|i| map.contains_key(Path::new(&format!("obs-{i}")))));
    }

    #[test]
    fn write_existing_without_read_is_refused() {
        let dir = tmp("not-observed");
        fs::write(dir.join("a.txt"), "old\n").unwrap();
        let refused = |c| {
            crate::client::scoped(c, || {
                invoke(&dir, "write", json!({"path":"a.txt","content":"new\n"}))
            })
        };
        let r = refused(crate::client::Client::Dsh);
        assert!(r.is_error);
        assert_eq!(r.structured["code"], "FS_NOT_OBSERVED");
        assert!(r.text.contains("native `write`"), "{}", r.text);
        let codex = refused(crate::client::Client::Codex);
        assert!(codex.text.contains("native `apply_patch`"), "{}", codex.text);
        let generic = refused(crate::client::Client::Generic);
        assert!(generic.text.contains("native write tool"), "{}", generic.text);
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "old\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_all_stops_before_the_result_outgrows_the_cap() {
        let dir = tmp("edit_cap");
        let r = invoke(&dir, "write", json!({"path":"a.txt","content":"a\n".repeat(100)}));
        assert!(!r.is_error, "{}", r.text);
        let big = "b".repeat(MAX_EDIT_BYTES);
        let r = invoke(
            &dir,
            "edit",
            json!({"path":"a.txt","old_string":"a","new_string":big,"replace_all":true}),
        );
        assert!(r.is_error);
        assert_eq!(r.structured["code"], "FS_TOO_LARGE", "{}", r.text);
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n".repeat(100));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_new_file_and_edit_replace_all() {
        let dir = tmp("edit");
        let r = invoke(
            &dir,
            "write",
            json!({"path":"src/a.rs","content":"let x = 1;\nlet x = 2;\n"}),
        );
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("Created file (+2 -0 lines)"), "{}", r.text);
        let r = invoke(
            &dir,
            "edit",
            json!({"path":"src/a.rs","old_string":"let x","new_string":"let y"}),
        );
        assert!(r.is_error);
        assert_eq!(r.structured["code"], "FS_AMBIGUOUS");
        let r = invoke(
            &dir,
            "edit",
            json!({
                "path":"src/a.rs","old_string":"let x","new_string":"let y","replace_all":true
            }),
        );
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(r.structured["replacements"], 2);
        assert_eq!(
            fs::read_to_string(dir.join("src/a.rs")).unwrap(),
            "let y = 1;\nlet y = 2;\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_refuses_files_it_would_corrupt_and_keeps_nuls() {
        let dir = tmp("edit-encoding");
        let latin1: &[u8] = b"caf\xe9 line1\nline2\n";
        let utf16: &[u8] = b"\xff\xfeh\0i\0\n\0x\0\n\0";
        for (name, bytes) in [("l1.txt", latin1), ("u16.txt", utf16)] {
            fs::write(dir.join(name), bytes).unwrap();
            let r = invoke(&dir, "read", json!({"path":name}));
            assert!(!r.is_error, "{}", r.text);
            let r = invoke(&dir, "edit", json!({"path":name,"old_string":"line2","new_string":"x"}));
            assert!(r.is_error, "{}", r.text);
            assert_eq!(r.structured["code"], "FS_ENCODING", "{name}: {}", r.text);
            assert_eq!(fs::read(dir.join(name)).unwrap(), bytes, "{name} must stay byte-identical");
        }
        fs::write(dir.join("n.txt"), b"a\0b\nline2\n").unwrap();
        invoke(&dir, "read", json!({"path":"n.txt"}));
        let r = invoke(&dir, "edit", json!({"path":"n.txt","old_string":"line2","new_string":"x"}));
        if !r.is_error {
            assert_eq!(fs::read(dir.join("n.txt")).unwrap(), b"a\0b\nx\n");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_read_is_refused() {
        let dir = tmp("stale");
        fs::write(dir.join("a.txt"), "one\n").unwrap();
        let r = invoke(&dir, "read", json!({"path":"a.txt"}));
        assert!(!r.is_error, "{}", r.text);
        fs::write(dir.join("a.txt"), "two\n").unwrap();
        let r = invoke(&dir, "write", json!({"path":"a.txt","content":"three\n"}));
        assert!(r.is_error);
        assert_eq!(r.structured["code"], "FS_STALE");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_same_size_edit_in_the_middle_of_a_big_file_is_stale() {
        let dir = tmp("stale-big");
        let mut bytes = "0123456789012345678901234567890123456789\n".repeat(80_000).into_bytes(); // 3 MiB+
        fs::write(dir.join("big.txt"), &bytes).unwrap();
        assert!(!invoke(&dir, "read", json!({"path":"big.txt","limit":1})).is_error);
        let mid = bytes.len() / 2;
        bytes[mid] = b'X';
        fs::write(dir.join("big.txt"), &bytes).unwrap();
        let r = invoke(&dir, "write", json!({"path":"big.txt","content":"gone\n"}));
        assert!(r.is_error, "{}", r.text);
        assert_eq!(r.structured["code"], "FS_STALE");
        assert_eq!(fs::read(dir.join("big.txt")).unwrap(), bytes);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn edit_and_write_keep_alternate_data_streams() {
        let dir = tmp("ads");
        fs::write(dir.join("x.txt"), "main content\n").unwrap();
        // A neutral stream: security software watches writes to `Zone.Identifier`.
        fs::write(dir.join("x.txt:qx.note"), "kept\n").unwrap();
        assert!(!invoke(&dir, "read", json!({"path":"x.txt"})).is_error);
        let r = invoke(&dir, "edit", json!({"path":"x.txt","old_string":"main","new_string":"new"}));
        assert!(!r.is_error, "{}", r.text);
        let r = invoke(&dir, "write", json!({"path":"x.txt","content":"replaced\n"}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(fs::read_to_string(dir.join("x.txt")).unwrap(), "replaced\n");
        assert_eq!(fs::read_to_string(dir.join("x.txt:qx.note")).unwrap(), "kept\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn the_mutation_lock_holds_other_processes_out_too() {
        let guard = lock_mutation();
        // A thread that skips the in-process lock and asks the system for the mutex stands for another process.
        assert!(!std::thread::spawn(|| os_mutex::acquire(200)).join().unwrap());
        drop(guard);
        let free = std::thread::spawn(|| {
            let got = os_mutex::acquire(5000);
            if got {
                os_mutex::release();
            }
            got
        });
        assert!(free.join().unwrap());
    }

    #[test]
    fn a_write_removes_temp_files_a_killed_write_left_behind() {
        let dir = tmp("stale-temp");
        let (old, fresh) = (dir.join(".qx-tmp-1-0"), dir.join(".qx-tmp-1-1"));
        fs::write(&old, "x").unwrap();
        fs::write(&fresh, "x").unwrap();
        let f = OpenOptions::new().write(true).open(&old).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600)).unwrap();
        drop(f);
        let r = invoke(&dir, "write", json!({"path": "a.txt", "content": "hi"}));
        assert!(!r.is_error, "{}", r.text);
        assert!(!old.exists(), "a temp older than an hour is removed");
        assert!(fresh.exists(), "a recent one may belong to a write in flight");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_refuses_files_it_would_corrupt_and_keeps_bom_and_line_endings() {
        let dir = tmp("write-encoding");
        let latin1: &[u8] = b"caf\xe9\nx\n";
        let utf16: &[u8] = b"\xff\xfeh\0i\0\n\0x\0\n\0";
        // `read` refuses a NUL in the first 8 KiB only; past it, text made from what read showed
        // would drop the NUL.
        let nul = [b"x\n".repeat(5000), b"a\0b\n".to_vec()].concat();
        for (name, bytes) in [("l1.txt", latin1), ("u16.txt", utf16), ("zeros.txt", &nul[..])] {
            fs::write(dir.join(name), bytes).unwrap();
            assert!(!invoke(&dir, "read", json!({"path":name})).is_error);
            let r = invoke(&dir, "write", json!({"path":name,"content":"replacement\n"}));
            assert!(r.is_error, "{name}: {}", r.text);
            assert_eq!(r.structured["code"], "FS_ENCODING", "{name}: {}", r.text);
            assert_eq!(fs::read(dir.join(name)).unwrap(), bytes, "{name} was rewritten");
        }
        // The model never saw the CRs or the BOM; the file must not lose them.
        for (name, before, content, after) in [
            ("crlf.txt", &b"a\r\nb\r\n"[..], "a\nb\nc\n", &b"a\r\nb\r\nc\r\n"[..]),
            ("bom.txt", &b"\xef\xbb\xbfhello\n"[..], "hi\n", &b"\xef\xbb\xbfhi\n"[..]),
            ("bomkept.txt", &b"\xef\xbb\xbfhello\n"[..], "\u{feff}hi\n", &b"\xef\xbb\xbfhi\n"[..]),
            ("lf.txt", &b"a\nb\n"[..], "a\nc\n", &b"a\nc\n"[..]),
        ] {
            fs::write(dir.join(name), before).unwrap();
            assert!(!invoke(&dir, "read", json!({"path":name})).is_error);
            let r = invoke(&dir, "write", json!({"path":name,"content":content}));
            assert!(!r.is_error, "{name}: {}", r.text);
            assert_eq!(fs::read(dir.join(name)).unwrap(), after, "{name}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_keeps_crlf_for_multiline_new_text_and_matches_mixed_files() {
        let dir = tmp("edit-crlf");
        fs::write(dir.join("a.txt"), "a\r\nfoo();\r\nb\r\n").unwrap();
        assert!(!invoke(&dir, "read", json!({"path":"a.txt"})).is_error);
        let r = invoke(&dir, "edit", json!({"path":"a.txt","old_string":"foo();","new_string":"foo();\nbar();"}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"a\r\nfoo();\r\nbar();\r\nb\r\n");
        // a multi-line match inside the LF part of a file that also has CRLF
        fs::write(dir.join("m.txt"), "x\r\ny\nz\nw\n").unwrap();
        assert!(!invoke(&dir, "read", json!({"path":"m.txt"})).is_error);
        let r = invoke(&dir, "edit", json!({"path":"m.txt","old_string":"y\nz","new_string":"y\nq"}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(fs::read(dir.join("m.txt")).unwrap(), b"x\r\ny\nq\nw\n");
        // the old behaviour stays: an LF old_string finds CRLF text
        fs::write(dir.join("c.txt"), "p\r\nq\r\n").unwrap();
        assert!(!invoke(&dir, "read", json!({"path":"c.txt"})).is_error);
        let r = invoke(&dir, "edit", json!({"path":"c.txt","old_string":"p\nq","new_string":"p\nr"}));
        assert!(!r.is_error, "{}", r.text);
        assert_eq!(fs::read(dir.join("c.txt")).unwrap(), b"p\r\nr\r\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_far_apart_edits_report_the_lines_they_changed_not_everything_between() {
        let dir = tmp("edit-far");
        let body: String = (1..=1000).map(|i| format!("line {i}\n")).collect();
        fs::write(dir.join("f.txt"), &body).unwrap();
        let r = invoke(
            &dir,
            "edit",
            json!({"path":"f.txt","edits":[
                {"old_string":"line 1\n","new_string":"first\n"},
                {"old_string":"line 1000\n","new_string":"last\n"}]}),
        );
        assert!(!r.is_error, "{}", r.text);
        assert_eq!((r.structured["added"].as_u64(), r.structured["removed"].as_u64()), (Some(2), Some(2)), "{}", r.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_delta_counts_the_lines_between_the_shared_ends() {
        // A line keeps its terminator: a final newline or a line ending alone makes a line differ.
        assert_eq!(line_delta(b"a\nb", b"a\nb\n"), (1, 1));
        assert_eq!(line_delta(b"a\nb\n", b"a\nb"), (1, 1));
        assert_eq!(line_delta(b"a\r\nb\r\n", b"a\nb\n"), (2, 2));
        assert_eq!(line_delta(b"a\nb\nc", b"a\nx\ny\nc"), (2, 1));
        assert_eq!(line_delta(b"a", b"a\nb"), (2, 1));
        assert_eq!(line_delta(b"", b"a\nb\n"), (2, 0));
        assert_eq!(line_delta(b"a\nb\n", b""), (0, 2));
        assert_eq!(line_delta(b"", b""), (0, 0));
        assert_eq!(line_delta(b"same\n", b"same\n"), (0, 0));
        // Past the 64-byte steps of the head and tail compare.
        let (head, tail) = ("same line\n".repeat(50), "tail line\n".repeat(50));
        let (old, new) = (format!("{head}x\n{tail}"), format!("{head}y\nz\n{tail}"));
        assert_eq!(line_delta(old.as_bytes(), new.as_bytes()), (2, 1));
        // Changes in two places: the lines between them count as replaced, so these are upper bounds (exact: 2 and 2).
        assert_eq!(line_delta(b"a\nb\nc\nd\ne\n", b"a\nB\nc\nD\ne\n"), (3, 3));
    }
    #[test]
    fn an_unchanged_write_or_edit_leaves_the_file_alone() {
        let dir = tmp("unchanged");
        let path = dir.join("a.txt");
        assert!(!invoke(&dir, "write", json!({"path": "a.txt", "content": "one\ntwo\n"})).is_error);
        let mtime = || fs::metadata(&path).unwrap().modified().unwrap();
        let before = mtime();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let w = invoke(&dir, "write", json!({"path": "a.txt", "content": "one\ntwo\n"}));
        assert!(!w.is_error && w.text.contains("(no changes)"), "{}", w.text);
        let e = invoke(&dir, "edit", json!({"path": "a.txt", "edits": [
            {"old_string": "one", "new_string": "1"},
            {"old_string": "1", "new_string": "one"},
        ]}));
        assert!(!e.is_error && e.text.contains("(no changes)"), "{}", e.text);
        assert_eq!(mtime(), before);
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn write_and_edit_replies_count_the_lines_they_changed() {
        // The replies are checked in the framed shape, so pin a client that gets the frame: the
        // process-wide client is changed by other tests while this one runs.
        crate::client::scoped(crate::client::Client::Generic, write_and_edit_replies_body);
    }
    fn write_and_edit_replies_body() {
        let dir = tmp("delta");
        let write = |content: &str| invoke(&dir, "write", json!({"path": "f.txt", "content": content}));
        let reply = |what: &str| format!("<path>f.txt</path>\n<type>file</type>\n<content>\n{what}\n</content>");
        let w = write("a\nb\nc\n");
        assert_eq!(w.text, reply("Created file (+3 -0 lines)"));
        assert_eq!((&w.structured["added"], &w.structured["removed"]), (&json!(3), &json!(0)));
        // Nothing changed: said so, not `+0 -0`.
        assert_eq!(write("a\nb\nc\n").text, reply("Updated file (no changes)"));
        // Only the final newline went: that is a changed last line.
        assert_eq!(write("a\nb\nc").text, reply("Updated file (+1 -1 lines)"));
        assert_eq!(write("a\nb\nc\n").text, reply("Updated file (+1 -1 lines)"));
        let w = write("a\nx\ny\nc\n");
        assert_eq!(w.text, reply("Updated file (+2 -1 lines)"));
        assert_eq!((&w.structured["added"], &w.structured["removed"]), (&json!(2), &json!(1)));
        // A new empty file is `(empty)`; emptying a file removes its lines.
        assert_eq!(write("").text, reply("Updated file (+0 -4 lines)"));
        let e = invoke(&dir, "write", json!({"path": "empty.txt", "content": ""}));
        assert_eq!(e.text, "<path>empty.txt</path>\n<type>file</type>\n<content>\nCreated file (empty)\n</content>");
        assert_eq!((&e.structured["added"], &e.structured["removed"]), (&json!(0), &json!(0)));
        assert_eq!(fs::read(dir.join("empty.txt")).unwrap(), b"");

        // edit: replace_all counts each changed line once, two hits on a line are one changed line.
        fs::write(dir.join("k.txt"), "k = 1\nk = 2\nk = 3\nz\nx x\n").unwrap();
        let edit = |extra: Value| {
            let mut args = json!({"path": "k.txt"});
            args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            invoke(&dir, "edit", args)
        };
        let ok = "The file k.txt has been updated successfully.";
        let r = edit(json!({"old_string": "k =", "new_string": "v =", "replace_all": true}));
        assert_eq!(r.text, format!("{ok} All occurrences were successfully replaced. (+3 -3 lines)"));
        assert_eq!(r.structured["replacements"], 3);
        let r = edit(json!({"old_string": "x", "new_string": "w", "replace_all": true}));
        assert_eq!(r.text, format!("{ok} All occurrences were successfully replaced. (+1 -1 lines)"));
        assert_eq!(r.structured["replacements"], 2);
        let r = edit(json!({"old_string": "z\n", "new_string": "z\nz2\nz3\n"}));
        assert_eq!(r.text, format!("{ok} (+2 -0 lines)"));
        assert_eq!(fs::read_to_string(dir.join("k.txt")).unwrap(), "v = 1\nv = 2\nv = 3\nz\nz2\nz3\nw w\n");
        // A batch is counted on the file as a whole: a chained A -> B -> C is one changed line, and a
        // round trip A -> B -> A changes nothing.
        fs::write(dir.join("k.txt"), "A\nkeep\n").unwrap();
        let chain = |a: &str, b: &str, c: &str| edit(json!({"edits": [
            {"old_string": a, "new_string": b},
            {"old_string": b, "new_string": c},
        ]}));
        let r = chain("A", "B", "C");
        assert_eq!(r.text, format!("{ok} 2 edits applied. (+1 -1 lines)"));
        assert_eq!(r.structured["replacements"], 2);
        assert_eq!(fs::read_to_string(dir.join("k.txt")).unwrap(), "C\nkeep\n");
        let r = chain("C", "D", "C");
        assert_eq!(r.text, format!("{ok} 2 edits applied. (no changes)"));
        assert_eq!(fs::read_to_string(dir.join("k.txt")).unwrap(), "C\nkeep\n");
        // The final newline alone, through edit.
        fs::write(dir.join("k.txt"), "a\nb").unwrap();
        let r = edit(json!({"old_string": "b", "new_string": "b\n"}));
        assert_eq!(r.text, format!("{ok} (+1 -1 lines)"));
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn line_counts_see_line_endings_through_write_and_edit() {
        let dir = tmp("delta-crlf");
        fs::write(dir.join("w.txt"), "a\r\nb\r\nc\r\n").unwrap();
        assert!(!invoke(&dir, "read", json!({"path": "w.txt"})).is_error);
        // The model writes LF text; the file keeps CRLF, so only the changed line differs.
        let w = invoke(&dir, "write", json!({"path": "w.txt", "content": "a\nB\nc\n"}));
        assert!(w.text.contains("Updated file (+1 -1 lines)"), "{}", w.text);
        assert_eq!(fs::read(dir.join("w.txt")).unwrap(), b"a\r\nB\r\nc\r\n");
        // The same text again: the CRLF it is written with makes the file identical.
        let w = invoke(&dir, "write", json!({"path": "w.txt", "content": "a\nB\nc\n"}));
        assert!(w.text.contains("Updated file (no changes)"), "{}", w.text);
        // Content with a CR of its own is written as given, so its bare-LF lines now differ.
        let w = invoke(&dir, "write", json!({"path": "w.txt", "content": "a\r\nB\nc\n"}));
        assert!(w.text.contains("Updated file (+2 -2 lines)"), "{}", w.text);
        assert_eq!(fs::read(dir.join("w.txt")).unwrap(), b"a\r\nB\nc\n");

        fs::write(dir.join("e.txt"), "a\r\nb\r\nc\r\n").unwrap();
        let e = invoke(&dir, "edit", json!({"path": "e.txt", "old_string": "b\n", "new_string": "b\nb2\n"}));
        assert!(e.text.ends_with("(+1 -0 lines)"), "{}", e.text);
        assert_eq!(fs::read(dir.join("e.txt")).unwrap(), b"a\r\nb\r\nb2\r\nc\r\n");
        let e = invoke(&dir, "edit", json!({"path": "e.txt", "old_string": "b2", "new_string": "b3"}));
        assert!(e.text.ends_with("(+1 -1 lines)"), "{}", e.text);
        assert_eq!(fs::read(dir.join("e.txt")).unwrap(), b"a\r\nb\r\nb3\r\nc\r\n");
        let _ = fs::remove_dir_all(&dir);
    }
}
