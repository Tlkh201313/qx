//! A live, in-memory list of the files of a workspace, so `glob` and `grep` never walk the disk.
//!
//! Built once with the ordinary parallel walk, then kept exact by a directory-change handle
//! (`ReadDirectoryChangesW`). Nothing runs in the background: the thread that asks for the list first
//! drains the changes the kernel has queued since the last call, and the kernel queues a change before
//! the operation that made it returns. A file a command, an editor or `qx` itself created a moment ago
//! is therefore always in the answer. Anything unexpected (the buffer overflowed, a junction appeared,
//! the root went away) drops the index, and the caller walks the disk until a new one is built.
//!
//! The list is only the files the plain walk would visit (generated directories like `node_modules`
//! and `target` are left out and their names remembered), so `all: true` queries do not use it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// One file: its absolute path, its path below the index root with `/` separators, its mtime.
#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub rel: String,
    pub modified: SystemTime,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    /// Sorted by `rel` (byte order), so every directory is one contiguous range.
    pub files: Vec<Entry>,
    /// Names of generated directories the walk left out.
    pub skipped: std::collections::BTreeSet<String>,
}

impl Snapshot {
    /// The entries below `prefix` (a `/`-separated path below the root; empty is everything).
    pub fn range(&self, prefix: &str) -> &[Entry] {
        if prefix.is_empty() {
            return &self.files;
        }
        let dir = format!("{}/", prefix.trim_end_matches('/'));
        let start = self.files.partition_point(|e| e.rel.as_str() < dir.as_str());
        let len = self.files[start..].iter().take_while(|e| e.rel.starts_with(&dir)).count();
        &self.files[start..start + len]
    }

    fn upsert(&mut self, rel: &str, path: PathBuf, modified: SystemTime) {
        match self.files.binary_search_by(|e| e.rel.as_str().cmp(rel)) {
            Ok(i) => {
                self.files[i].modified = modified;
                self.files[i].path = path;
            }
            Err(i) => self.files.insert(i, Entry { path, rel: rel.to_string(), modified }),
        }
    }

    /// Remove the file `rel`, or everything below it when it was a directory.
    fn remove(&mut self, rel: &str) {
        if let Ok(i) = self.files.binary_search_by(|e| e.rel.as_str().cmp(rel)) {
            self.files.remove(i);
            return;
        }
        let dir = format!("{rel}/");
        let start = self.files.partition_point(|e| e.rel.as_str() < dir.as_str());
        let len = self.files[start..].iter().take_while(|e| e.rel.starts_with(&dir)).count();
        if len > 0 {
            self.files.drain(start..start + len);
        }
    }
}

/// The files below `root` as of now, and `root` relative to the index root. `None` when no index
/// covers `root` (yet), so the caller walks the disk.
pub fn snapshot(root: &Path) -> Option<(Arc<Snapshot>, String)> {
    imp::snapshot(root)
}

/// Build (or rebuild) the index for `root` now and return once it is usable. Idempotent.
pub fn ensure(root: &Path) {
    imp::ensure(root);
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn snapshot(_root: &Path) -> Option<(Arc<Snapshot>, String)> {
        None
    }
    pub fn ensure(_root: &Path) {}
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use std::sync::Mutex;

    type Handle = *mut c_void;

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: Handle,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn CloseHandle(h: Handle) -> i32;
        fn CreateEventW(security: *mut c_void, manual: i32, initial: i32, name: *const u16) -> Handle;
        fn ReadDirectoryChangesW(
            dir: Handle,
            buf: *mut c_void,
            len: u32,
            subtree: i32,
            filter: u32,
            returned: *mut u32,
            ov: *mut Overlapped,
            completion: *mut c_void,
        ) -> i32;
        fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
        fn GetOverlappedResult(h: Handle, ov: *mut Overlapped, n: *mut u32, wait: i32) -> i32;
        fn CancelIoEx(h: Handle, ov: *mut Overlapped) -> i32;
    }

    const INVALID_HANDLE: isize = -1;
    const WAIT_OBJECT_0: u32 = 0;
    const BUFFER_WORDS: usize = 64 * 1024; // 256 KB of change records
    const MAX_ROOTS: usize = 4;
    // FILE_NOTIFY_CHANGE_FILE_NAME | DIR_NAME | SIZE | LAST_WRITE
    const FILTER: u32 = 0x1 | 0x2 | 0x8 | 0x10;

    /// What the kernel writes into while a change request is pending: it must not move.
    struct Pending {
        ov: Overlapped,
        buf: Vec<u32>,
    }

    struct Index {
        root: PathBuf,
        snap: Arc<Snapshot>,
        dir: Handle,
        event: Handle,
        pending: Box<Pending>,
        broken: bool,
    }

    // The handles are used under the registry mutex only.
    unsafe impl Send for Index {}

    impl Drop for Index {
        fn drop(&mut self) {
            unsafe {
                CancelIoEx(self.dir, &mut self.pending.ov);
                let mut n = 0u32;
                GetOverlappedResult(self.dir, &mut self.pending.ov, &mut n, 1);
                CloseHandle(self.dir);
                CloseHandle(self.event);
            }
        }
    }

    static REGISTRY: Mutex<Vec<Arc<Mutex<Index>>>> = Mutex::new(Vec::new());
    static BUILDING: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn key(p: &Path) -> String {
        let s = p.to_string_lossy().replace('/', "\\");
        s.strip_prefix("\\\\?\\").unwrap_or(&s).trim_end_matches('\\').to_ascii_lowercase()
    }

    fn wide(p: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }

    impl Index {
        fn issue(&mut self) -> bool {
            let words = self.pending.buf.len();
            let p = &mut *self.pending;
            p.ov = Overlapped { internal: 0, internal_high: 0, offset: 0, offset_high: 0, event: self.event };
            unsafe {
                ReadDirectoryChangesW(
                    self.dir,
                    p.buf.as_mut_ptr() as *mut c_void,
                    (words * 4) as u32,
                    1,
                    FILTER,
                    std::ptr::null_mut(),
                    &mut p.ov,
                    std::ptr::null_mut(),
                ) != 0
            }
        }

        /// Apply every change the kernel has queued. Cheap when there is none: one wait with no timeout.
        fn drain(&mut self) {
            while !self.broken && unsafe { WaitForSingleObject(self.event, 0) } == WAIT_OBJECT_0 {
                let mut n = 0u32;
                let ok = unsafe { GetOverlappedResult(self.dir, &mut self.pending.ov, &mut n, 0) } != 0;
                if !ok || n == 0 {
                    // 0 bytes is the kernel's buffer overflow: some changes were lost.
                    self.broken = true;
                    return;
                }
                let bytes: Vec<u8> = {
                    let words = &self.pending.buf;
                    let raw = unsafe { std::slice::from_raw_parts(words.as_ptr() as *const u8, n as usize) };
                    raw.to_vec()
                };
                // Re-arm first: changes that land while this batch is applied are queued, not lost.
                if !self.issue() {
                    self.broken = true;
                    return;
                }
                self.apply(&bytes);
            }
        }

        fn apply(&mut self, bytes: &[u8]) {
            let mut at = 0usize;
            while at + 12 <= bytes.len() {
                let next = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
                let action = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap());
                let name_len = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap()) as usize;
                let name_end = (at + 12 + name_len).min(bytes.len());
                let units: Vec<u16> = bytes[at + 12..name_end]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let rel = String::from_utf16_lossy(&units).replace('\\', "/");
                self.change(action, &rel);
                if self.broken || next == 0 {
                    break;
                }
                at += next;
            }
        }

        fn change(&mut self, action: u32, rel: &str) {
            const ADDED: u32 = 1;
            const REMOVED: u32 = 2;
            const MODIFIED: u32 = 3;
            const RENAMED_OLD: u32 = 4;
            const RENAMED_NEW: u32 = 5;
            if rel.is_empty() {
                return;
            }
            let comps: Vec<&str> = rel.split('/').collect();
            // A path inside a generated or VCS directory is not in the list: one name check, no syscall.
            for (i, c) in comps.iter().enumerate() {
                if crate::fsops::skip_dir_name(c) {
                    if i + 1 == comps.len() && (action == ADDED || action == RENAMED_NEW) && !crate::fsops::vcs_dir_name(c) {
                        Arc::make_mut(&mut self.snap).skipped.insert((*c).to_string());
                    }
                    return;
                }
            }
            let full = self.root.join(rel.replace('/', "\\"));
            match action {
                REMOVED | RENAMED_OLD => Arc::make_mut(&mut self.snap).remove(rel),
                ADDED | RENAMED_NEW | MODIFIED => {
                    let Ok(md) = std::fs::symlink_metadata(&full) else {
                        return; // gone again; its REMOVED follows
                    };
                    let ft = md.file_type();
                    if ft.is_symlink() || is_reparse(&md) {
                        self.broken = true; // let the walk decide how to treat a junction
                    } else if ft.is_dir() {
                        if action != MODIFIED {
                            self.add_tree(&full, rel);
                        }
                    } else if ft.is_file() {
                        let modified = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                        Arc::make_mut(&mut self.snap).upsert(rel, full, modified);
                    }
                }
                _ => self.broken = true,
            }
        }

        /// A directory appeared (created, renamed in, or copied whole): list what it holds.
        fn add_tree(&mut self, dir: &Path, rel: &str) {
            let mut stack = vec![(dir.to_path_buf(), rel.to_string())];
            while let Some((d, r)) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&d) else { continue };
                for e in rd.flatten() {
                    let Ok(ft) = e.file_type() else { continue };
                    let name = e.file_name().to_string_lossy().into_owned();
                    let child_rel = format!("{r}/{name}");
                    if ft.is_symlink() {
                        self.broken = true;
                        return;
                    }
                    if ft.is_dir() {
                        if crate::fsops::skip_dir_name(&name) {
                            if !crate::fsops::vcs_dir_name(&name) {
                                Arc::make_mut(&mut self.snap).skipped.insert(name);
                            }
                            continue;
                        }
                        if e.metadata().map(|m| is_reparse(&m)).unwrap_or(true) {
                            self.broken = true;
                            return;
                        }
                        stack.push((e.path(), child_rel));
                    } else if ft.is_file() {
                        let modified = e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
                        Arc::make_mut(&mut self.snap).upsert(&child_rel, e.path(), modified);
                    }
                }
            }
        }
    }

    fn is_reparse(md: &std::fs::Metadata) -> bool {
        use std::os::windows::fs::MetadataExt;
        md.file_attributes() & 0x400 != 0
    }

    /// Open the change handle first, then walk: a change made during the walk is queued, and applying
    /// it afterwards is harmless (every change is an idempotent insert or remove).
    fn build(root: &Path) -> Option<Index> {
        let name = wide(root);
        // FILE_LIST_DIRECTORY; share read/write/delete; OPEN_EXISTING; BACKUP_SEMANTICS | OVERLAPPED
        let dir = unsafe { CreateFileW(name.as_ptr(), 0x1, 0x7, std::ptr::null_mut(), 3, 0x0200_0000 | 0x4000_0000, std::ptr::null_mut()) };
        if dir as isize == INVALID_HANDLE || dir.is_null() {
            return None;
        }
        let event = unsafe { CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            unsafe { CloseHandle(dir) };
            return None;
        }
        let mut index = Index {
            root: root.to_path_buf(),
            snap: Arc::new(Snapshot::default()),
            dir,
            event,
            pending: Box::new(Pending {
                ov: Overlapped { internal: 0, internal_high: 0, offset: 0, offset_high: 0, event },
                buf: vec![0u32; BUFFER_WORDS],
            }),
            broken: false,
        };
        if !index.issue() {
            return None;
        }
        let (parts, skipped) = crate::fsops::walk_live(root, false, Vec::<Entry>::new, |found, p, meta| {
            let rel = p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/");
            found.push(Entry { path: p.to_path_buf(), rel, modified: meta.modified() });
        });
        let mut files: Vec<Entry> = parts.into_iter().flatten().collect();
        if files.len() >= crate::fsops::MAX_WALK_FILES {
            return None; // too big to hold exactly: keep walking
        }
        files.sort_by(|a, b| a.rel.cmp(&b.rel));
        index.snap = Arc::new(Snapshot { files, skipped: skipped.into_iter().collect() });
        Some(index)
    }

    fn find(root: &Path) -> Option<(Arc<Mutex<Index>>, String)> {
        let want = key(root);
        let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        for ix in reg.iter() {
            let Ok(guard) = ix.lock() else { continue };
            let base = key(&guard.root);
            if want == base {
                return Some((ix.clone(), String::new()));
            }
            if let Some(rest) = want.strip_prefix(&base).and_then(|r| r.strip_prefix('\\')) {
                // The case of the on-disk path, not of the query, names the prefix.
                let n = rest.len();
                let tail: String = root.to_string_lossy().replace('\\', "/");
                let tail = tail.trim_end_matches('/');
                if tail.len() >= n {
                    return Some((ix.clone(), tail[tail.len() - n..].to_string()));
                }
            }
        }
        None
    }

    pub fn snapshot(root: &Path) -> Option<(Arc<Snapshot>, String)> {
        let (ix, prefix) = find(root)?;
        let mut guard = ix.lock().ok()?;
        guard.drain();
        if guard.broken {
            let base = guard.root.clone();
            drop(guard);
            forget(&base);
            rebuild_in_background(base);
            return None;
        }
        Some((guard.snap.clone(), prefix))
    }

    fn forget(root: &Path) {
        let want = key(root);
        let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        reg.retain(|ix| ix.lock().map(|g| key(&g.root) != want).unwrap_or(false));
    }

    fn rebuild_in_background(root: PathBuf) {
        let _ = std::thread::Builder::new().name("qx-index".into()).spawn(move || ensure(&root));
    }

    pub fn ensure(root: &Path) {
        if std::env::var("QX_INDEX").ok().as_deref() == Some("off") {
            return;
        }
        // The same spelling the queries use (`Root` canonicalizes), so paths in the list display alike.
        let Ok(root) = std::fs::canonicalize(root) else { return };
        let root = root.as_path();
        let k = key(root);
        if find(root).is_some_and(|(_, prefix)| prefix.is_empty()) {
            return;
        }
        {
            let mut building = BUILDING.lock().unwrap_or_else(|e| e.into_inner());
            if building.contains(&k) {
                return;
            }
            building.push(k.clone());
        }
        if let Some(index) = build(root) {
            let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
            if reg.len() >= MAX_ROOTS {
                reg.remove(0);
            }
            reg.push(Arc::new(Mutex::new(index)));
        }
        BUILDING.lock().unwrap_or_else(|e| e.into_inner()).retain(|b| b != &k);
    }
}
