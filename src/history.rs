//! Command history memory: what ran in which repo, and what qx answered.
//!
//! One append-only file per repo root in `%LOCALAPPDATA%\qx\history\` (fallback `%TEMP%`), lines of
//! `<key:16 hex> <json>`. Only a passing slow test/lint run (`cargo test`, `pytest`, ...) is answered
//! from the file (`before`), and only when the tree is unchanged since that run started; `list`/`show`
//! let an agent find what it already ran.
//! `QX_HISTORY=off` turns record and replay off. Outside a git repo there is no history at all.
//!
//! Known limit: the tree fingerprint covers what `git status` lists. Ignored files and dirs (`target/`,
//! `.venv`, `node_modules`) and paths outside the repo are not covered, so a change there is not seen.
//! A replay says so in its header: the agent runs the command again to force a real run.
//! The `key` is the hash of (cwd, command text) only: callers must pass a command line as `cmd` for
//! runs and a pattern for `fs grep`/`fs glob`, so the two do not share keys in practice.

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Run,
    Search,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Run => "run",
            Kind::Search => "search",
        }
    }
}

const REPLY_CAP: usize = 2048;
/// A replay is only offered while the stored run is younger than this.
const REPLAY_MAX_AGE: u64 = 24 * 3600;
const BRIEF_MAX: usize = 10;

struct Caps {
    /// One repo file may not grow past this: an append that would pass it compacts first.
    repo: u64,
    /// After compaction a file still over this loses its oldest half.
    kept: u64,
    /// The whole history dir; past it the least recently modified repo files are deleted.
    total: u64,
}

const CAPS: Caps = Caps { repo: 4 << 20, kept: 2 << 20, total: 64 << 20 };

struct Rec<'a> {
    cmd: &'a str,
    kind: Kind,
    code: i32,
    ms: u64,
    reply: &'a str,
    hits: Option<u64>,
    /// Tree fingerprint; `None` for commands that must never replay.
    fp: Option<&'a str>,
    ts: u64,
}

struct Query<'a> {
    n: usize,
    grep: Option<&'a str>,
    searches: bool,
    brief: bool,
}

// ---------------------------------------------------------------- public API

/// What to do about a command before it runs.
pub enum Before {
    /// Answer from memory: stored exit code and reply.
    Replay(i32, String),
    /// Run it for real; carries the tree fingerprint taken before the run, to hand back to `record`.
    Run(Option<String>),
}

/// Look `cmd` up in the history of the repo holding `cwd`. At most one fingerprint is taken: it either
/// proves the replay or is returned for the run. `fresh` never replays. The fingerprint is `None` when
/// the command is not allowlisted, `cwd` is not in a repo, or git failed.
pub fn before(cwd: &Path, cmd: &str, fresh: bool) -> Before {
    if !enabled() || !allowlisted(cmd) {
        return Before::Run(None);
    }
    let Some((root, rel)) = locate(cwd) else { return Before::Run(None) };
    let fp = fingerprint(&root);
    if !fresh && fp.is_some() {
        if let Some((code, text)) = replay_in(&dir(), &root, &rel, cmd, now(), fp.as_deref(), &CAPS) {
            return Before::Replay(code, text);
        }
    }
    Before::Run(fp)
}

/// Note a finished run. `fp` is the fingerprint `before` took before it started.
#[allow(clippy::too_many_arguments)]
pub fn record(cwd: &Path, cmd: &str, kind: Kind, code: i32, ms: u64, reply: &str, hits: Option<u64>, fp: Option<String>) {
    if !enabled() {
        return;
    }
    let Some((root, rel)) = locate(cwd) else { return };
    let rec = Rec { cmd, kind, code, ms, reply, hits, fp: fp.as_deref(), ts: now() };
    record_in(&dir(), &root, &rel, &rec, &CAPS);
}

/// `record` for a captured command: a search (`grep`, `rg`, `find`, `fd`) counts its hits, which are the
/// non-empty `stdout` lines not starting with `[qx]`; anything else is a plain run.
pub fn record_output(dir: &Path, cmd: &str, code: i32, ms: u64, stdout: &str, reply: &str, fp: Option<String>) {
    if search_cmd(cmd) {
        let hits = stdout.lines().filter(|l| !l.trim().is_empty() && !l.starts_with("[qx]")).count() as u64;
        record(dir, cmd, Kind::Search, code, ms, reply, Some(hits), fp);
    } else {
        record(dir, cmd, Kind::Run, code, ms, reply, None, fp);
    }
}

/// Whether a command line is a text or file search: its record counts hits, not just an exit code.
fn search_cmd(cmd: &str) -> bool {
    let first = cmd.split_whitespace().next().unwrap_or("");
    let base = first.rsplit(['\\', '/']).next().unwrap_or(first).to_ascii_lowercase();
    matches!(base.strip_suffix(".exe").unwrap_or(&base), "grep" | "rg" | "find" | "fd")
}

pub fn list(cwd: &Path, n: usize, grep: Option<&str>, searches: bool, brief: bool) -> String {
    let Some((root, _)) = locate(cwd) else {
        return "no history: not inside a git repo".to_string();
    };
    list_in(&log_file(&dir(), &root), now(), &Query { n, grep, searches, brief })
}

pub fn show(cwd: &Path, key: &str) -> Option<String> {
    let (root, _) = locate(cwd)?;
    show_in(&log_file(&dir(), &root), key, now())
}

// ------------------------------------------------------------------- setup

fn enabled() -> bool {
    // Same switch shape as `stats::enabled`: the test suite must not write into the user's real history.
    !cfg!(test) && !std::env::var("QX_HISTORY").is_ok_and(|v| v.eq_ignore_ascii_case("off") || v == "0")
}

/// Same base as `stats::path()` (`%LOCALAPPDATA%\qx`), with `%TEMP%` as the fallback.
fn dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, PathBuf::from);
    base.join("qx").join("history")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The repo root (nearest ancestor holding `.git`, dir or file) and `cwd` relative to it.
fn locate(cwd: &Path) -> Option<(PathBuf, String)> {
    let abs = std::path::absolute(cwd).ok()?;
    let root = abs.ancestors().find(|d| d.join(".git").exists())?.to_path_buf();
    let rel = abs
        .strip_prefix(&root)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    Some((root, if rel.is_empty() { ".".to_string() } else { rel }))
}

/// FNV-1a: unlike `DefaultHasher` its output is fixed forever, and these hashes live on disk.
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    /// Each call is one field: the trailing 0xff keeps ("ab","c") and ("a","bc") apart.
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes.iter().chain(&[0xff]) {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn hex(&self) -> String {
        format!("{:016x}", self.0)
    }
}

fn log_file(dir: &Path, root: &Path) -> PathBuf {
    let text = root.to_string_lossy().replace('\\', "/").to_lowercase();
    let mut h = Fnv::new();
    h.write(text.trim_start_matches("//?/").trim_end_matches('/').as_bytes());
    dir.join(format!("{}.log", h.hex()))
}

fn key_of(rel: &str, cmd: &str) -> String {
    let mut h = Fnv::new();
    h.write(rel.as_bytes());
    h.write(cmd.as_bytes());
    h.hex()
}

// ------------------------------------------------------------- fingerprint

fn git(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut c = Command::new("git");
    // No index lock: this status must never make the agent's own git command fail.
    c.arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000); // CREATE_NO_WINDOW
    let out = c.output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// A tree with more changed paths than this is not fingerprinted: hashing it would cost more than a replay saves.
const FP_MAX_ENTRIES: usize = 5000;

/// Hash of HEAD, the porcelain status, and (len, mtime) of every path it lists. Any error, or more than
/// `FP_MAX_ENTRIES` entries: `None`.
fn fingerprint(root: &Path) -> Option<String> {
    let head = git(root, &["rev-parse", "HEAD"])?;
    // `all`, not `normal`: with `normal` an untracked dir is one entry, so an edit inside it is invisible.
    let status = git(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    if status.split(|&b| b == 0).filter(|e| !e.is_empty()).count() > FP_MAX_ENTRIES {
        return None;
    }
    let mut h = Fnv::new();
    h.write(&head);
    h.write(&status);
    let mut entries = status.split(|&b| b == 0).filter(|e| !e.is_empty());
    while let Some(e) = entries.next() {
        if e.len() < 4 {
            continue;
        }
        // A rename/copy entry is followed by a second entry holding the source path, without a status.
        if matches!(e[0], b'R' | b'C') || matches!(e[1], b'R' | b'C') {
            entries.next();
        }
        let path = root.join(String::from_utf8_lossy(&e[3..]).as_ref());
        match std::fs::metadata(&path) {
            Ok(m) => {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                h.write(format!("{} {mtime}", m.len()).as_bytes());
            }
            Err(_) => h.write(b"gone"),
        }
    }
    Some(h.hex())
}

// --------------------------------------------------------------- allowlist

/// Only slow test/lint runs replay: anything cheaper gains nothing from it, file readers would replay
/// stale text for ignored or outside files, and a build would replay "Finished" after `cargo clean`.
/// Pipelines, redirects, command chains and env expansion never do, and neither does a mutating flag.
fn allowlisted(cmd: &str) -> bool {
    if cmd.contains(['|', '>', '<', ';', '&', '`', '\n', '\r', '$', '%']) {
        return false;
    }
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let Some(&first) = words.first() else { return false };
    if crate::exec::assignment_word(first) {
        return false;
    }
    let prog = first.rsplit(['/', '\\']).next().unwrap_or(first).to_lowercase();
    let prog = prog.strip_suffix(".exe").unwrap_or(&prog);
    let sub = words.get(1).copied().unwrap_or("");
    let args = &words[1..];
    let any = |bad: &[&str]| args.iter().any(|a| bad.iter().any(|b| a == b || a.starts_with(&format!("{b}="))));
    match prog {
        "cargo" => matches!(sub, "test" | "check" | "clippy") && !any(&["--fix"]),
        "go" => matches!(sub, "test" | "vet"),
        "dotnet" | "npm" => sub == "test",
        "pytest" => true,
        "python" | "python3" | "py" => sub == "-m" && words.get(2) == Some(&"pytest"),
        _ => false,
    }
}

// ----------------------------------------------------------------- writing

fn cut(s: String) -> String {
    if s.len() <= REPLY_CAP {
        return s;
    }
    let mut end = REPLY_CAP - '…'.len_utf8();
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn encode(rel: &str, rec: &Rec) -> String {
    let (reply, _) = crate::redact::redact(rec.reply);
    // The key hashes the command as typed; only the stored copy is masked.
    let (cmd, _) = crate::redact::redact(rec.cmd);
    // A reply that gets cut would replay as a different text than the run gave: it never replays.
    let fp = if reply.len() > REPLY_CAP { "" } else { rec.fp.unwrap_or("") };
    let mut v = json!({
        "ts": rec.ts,
        "cwd": rel,
        "cmd": cmd,
        "kind": rec.kind.as_str(),
        "code": rec.code,
        "ms": rec.ms,
        "fp": fp,
        "reply": cut(reply),
        "n": 1,
    });
    if let (Some(h), Some(o)) = (rec.hits, v.as_object_mut()) {
        o.insert("hits".into(), h.into());
    }
    format!("{} {v}\n", key_of(rel, rec.cmd))
}

fn record_in(dir: &Path, root: &Path, rel: &str, rec: &Rec, caps: &Caps) {
    append_line(dir, root, &encode(rel, rec), caps);
}

fn append_line(dir: &Path, root: &Path, line: &str, caps: &Caps) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let file = log_file(dir, root);
    // A new repo file adds to the total just as a compaction does: check the cap for both.
    let created = !file.exists();
    if append_locked(&file, line.as_bytes(), caps) || created {
        enforce_total(dir, &file, caps.total);
    }
}

/// Open for read+write, denying other writers: the same lock `stats::append_limited` takes, so one
/// process never compacts a stale copy over a line another one has just appended.
fn open_locked(path: &Path) -> Option<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true).write(true).create(true);
    // FILE_SHARE_READ: lookups can still read while a writer holds the file.
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::share_mode(&mut opts, 1);
    for _ in 0..200 {
        match opts.open(path) {
            Ok(f) => return Some(f),
            // ERROR_SHARING_VIOLATION: another qx process is appending; it holds the file for moments.
            Err(e) if cfg!(windows) && e.raw_os_error() == Some(32) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(_) => return None,
        }
    }
    None
}

/// Append `bytes`, compacting first when that would pass the per-repo cap. True when it compacted.
fn append_locked(path: &Path, bytes: &[u8], caps: &Caps) -> bool {
    let Some(mut f) = open_locked(path) else { return false };
    let mut compacted = false;
    if f.metadata().is_ok_and(|m| m.len() + bytes.len() as u64 > caps.repo) {
        let mut data = Vec::new();
        if f.read_to_end(&mut data).is_ok() {
            let kept = compact(&data, caps.kept);
            compacted = f
                .seek(SeekFrom::Start(0))
                .and_then(|_| f.write_all(&kept))
                .and_then(|_| f.set_len(kept.len() as u64))
                .is_ok();
        }
    }
    let _ = f.seek(SeekFrom::End(0)).and_then(|_| f.write_all(bytes));
    compacted
}

fn run_count(v: &Value) -> u64 {
    v.get("n").and_then(Value::as_u64).unwrap_or(1)
}

/// Newest line per key, its `n` the sum over that key; then, if still over `kept` bytes, only the
/// newer half of the lines. Lines that do not parse are dropped.
fn compact(data: &[u8], kept: u64) -> Vec<u8> {
    let mut parsed: Vec<(&[u8], Value)> = Vec::new();
    let mut newest: HashMap<&[u8], usize> = HashMap::new();
    let mut sums: HashMap<&[u8], u64> = HashMap::new();
    for l in data.split(|&b| b == b'\n') {
        if l.len() <= 17 || l[16] != b' ' {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&l[17..]) else { continue };
        if !v.is_object() {
            continue;
        }
        let key = &l[..16];
        *sums.entry(key).or_default() += run_count(&v);
        newest.insert(key, parsed.len());
        parsed.push((key, v));
    }
    let mut lines: Vec<String> = Vec::new();
    for (i, (key, mut v)) in parsed.into_iter().enumerate() {
        if newest[key] != i {
            continue;
        }
        if let Some(o) = v.as_object_mut() {
            o.insert("n".into(), sums[key].into());
            // The newest line may be a replay copy; the kept entry must stay visible to `list`/`show`.
            o.remove("replayed");
        }
        lines.push(format!("{} {v}\n", String::from_utf8_lossy(key)));
    }
    if lines.iter().map(|l| l.len() as u64).sum::<u64>() > kept {
        lines.drain(..lines.len() / 2);
    }
    lines.concat().into_bytes()
}

/// Past `cap` bytes in the history dir, delete whole repo files, least recently modified first.
/// The file just written (`keep`) stays.
fn enforce_total(dir: &Path, keep: &Path, cap: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort_by_key(|f| f.0);
    for (_, len, p) in files {
        if total <= cap {
            break;
        }
        if p != keep && std::fs::remove_file(&p).is_ok() {
            total -= len;
        }
    }
}

// ----------------------------------------------------------------- reading

/// The payload (after `<key> `) of the newest line whose key starts with `key`.
fn newest_line<'a>(data: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let k = key.as_bytes();
    data.split(|&b| b == b'\n')
        .rev()
        .find(|l| l.len() > 17 && l[16] == b' ' && l.starts_with(k))
        .map(|l| &l[17..])
}

fn age_str(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// The reply for a repeat of `cmd` in a tree whose fingerprint is `fp_now`, or `None` to run it for real.
fn replay_in(dir: &Path, root: &Path, rel: &str, cmd: &str, now: u64, fp_now: Option<&str>, caps: &Caps) -> Option<(i32, String)> {
    let key = key_of(rel, cmd);
    let data = std::fs::read(log_file(dir, root)).ok()?;
    let mut v: Value = serde_json::from_slice(newest_line(&data, &key)?).ok()?;
    // The previous answer was already a replay: the agent insisted, so run for real.
    if v.get("replayed").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let code = i32::try_from(v.get("code").and_then(Value::as_i64)?).ok()?;
    let ts = v.get("ts").and_then(Value::as_u64)?;
    let age = now.saturating_sub(ts);
    let fp = v.get("fp").and_then(Value::as_str)?;
    // Only a pass replays: a failure is what the agent is working on, and it wants the live output.
    if code != 0 || age >= REPLAY_MAX_AGE || fp.len() != 16 || fp_now != Some(fp) {
        return None;
    }
    let reply = v.get("reply").and_then(Value::as_str).unwrap_or("").to_string();
    // Remember that this answer was a replay. The copy keeps the original ts/fp (so age and the
    // fingerprint check stay about the real run) and n=0 (a replay is not a run).
    if let Some(o) = v.as_object_mut() {
        o.insert("replayed".into(), true.into());
        o.insert("n".into(), 0.into());
    }
    append_line(dir, root, &format!("{key} {v}\n"), caps);
    // The reply always comes along: another session, a subagent or a compacted context never saw it.
    let head = format!(
        "[qx] unchanged since {} ago (same tree): exit {code}; run it again to force a real run",
        age_str(age)
    );
    Some((code, if reply.is_empty() { head } else { format!("{head}\n{reply}") }))
}

fn one_line(s: &str, max: usize) -> String {
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn list_in(file: &Path, now: u64, q: &Query) -> String {
    let Ok(data) = std::fs::read(file) else {
        return "no history for this repo yet".to_string();
    };
    let limit = if q.brief { q.n.min(BRIEF_MAX) } else { q.n };
    let needle = q.grep.map(str::to_lowercase);
    let mut seen: HashSet<&[u8]> = HashSet::new();
    let mut rows: Vec<String> = Vec::new();
    for l in data.split(|&b| b == b'\n').rev() {
        if rows.len() >= limit {
            break;
        }
        if l.len() <= 17 || l[16] != b' ' {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&l[17..]) else { continue };
        let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
        let num = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        if v.get("replayed").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let search = text("kind") == "search";
        if (q.searches && !search) || needle.as_ref().is_some_and(|n| !text("cmd").to_lowercase().contains(n.as_str())) {
            continue;
        }
        if q.brief && !seen.insert(&l[..16]) {
            continue;
        }
        let age = age_str(now.saturating_sub(num("ts")));
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
        let cmd = one_line(text("cmd"), 160);
        rows.push(if q.brief {
            format!("{cmd} → exit {code} ({age})")
        } else {
            let hits = if search { format!("  {} hits", num("hits")) } else { String::new() };
            format!("{age:>4}  {code:>4}  {:>6}ms  {}  {cmd}{hits}", num("ms"), text("cwd"))
        });
    }
    if rows.is_empty() {
        "no matching history".to_string()
    } else {
        rows.join("\n")
    }
}

fn show_in(file: &Path, key: &str, now: u64) -> Option<String> {
    if key.len() < 4 || key.len() > 16 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let data = std::fs::read(file).ok()?;
    let key = key.to_lowercase();
    let v = data
        .split(|&b| b == b'\n')
        .rev()
        .filter(|l| l.len() > 17 && l[16] == b' ' && l.starts_with(key.as_bytes()))
        .filter_map(|l| serde_json::from_slice::<Value>(&l[17..]).ok())
        .find(|v| v.get("replayed").and_then(Value::as_bool) != Some(true))?;
    let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
    let num = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let hits = v.get("hits").and_then(Value::as_u64).map_or(String::new(), |h| format!(", {h} hits"));
    Some(format!(
        "{}\ncwd {}, exit {}, {} ms, {} ago, run {}x{hits}\n{}",
        text("cmd"),
        text("cwd"),
        v.get("code").and_then(Value::as_i64).unwrap_or(0),
        num("ms"),
        age_str(now.saturating_sub(num("ts"))),
        run_count(&v),
        text("reply"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qx-history-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn have_git() -> bool {
        Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    fn commit(repo: &Path) {
        git(repo, &["add", "-A"]).unwrap();
        let args = ["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false", "commit", "-qm", "x"];
        git(repo, &args).unwrap();
    }

    fn rec<'a>(cmd: &'a str, reply: &'a str, fp: Option<&'a str>, ts: u64) -> Rec<'a> {
        Rec { cmd, kind: Kind::Run, code: 3, ms: 7, reply, hits: None, fp, ts }
    }

    const FP: &str = "aaaaaaaaaaaaaaaa";

    #[test]
    fn appending_past_the_cap_compacts_to_newest_per_key_with_summed_counts() {
        let dir = tmp("cap");
        let root = dir.join("repo");
        let caps = Caps { repo: 700, kept: 100_000, total: u64::MAX };
        for i in 0..13 {
            let cmd = if i % 4 == 3 { "b" } else { "a" };
            record_in(&dir, &root, ".", &rec(cmd, "out", None, 100 + i), &caps);
        }
        let data = std::fs::read_to_string(log_file(&dir, &root)).unwrap();
        let lines: Vec<&str> = data.lines().collect();
        assert!(lines.len() < 13, "{} lines", lines.len());
        let sum = |key: &str| -> u64 {
            lines
                .iter()
                .filter(|l| l.starts_with(key))
                .map(|l| run_count(&serde_json::from_str::<Value>(&l[17..]).unwrap()))
                .sum()
        };
        assert_eq!(sum(&key_of(".", "a")), 10);
        assert_eq!(sum(&key_of(".", "b")), 3);
    }

    #[test]
    fn a_compacted_file_still_over_the_kept_size_loses_its_oldest_half() {
        let mut data = String::new();
        for i in 0..4 {
            data += &format!("{i:016x} {{\"cmd\":\"c{i}\",\"n\":1}}\n");
        }
        let out = String::from_utf8(compact(data.as_bytes(), 10)).unwrap();
        let keys: Vec<&str> = out.lines().map(|l| &l[..16]).collect();
        assert_eq!(keys, [format!("{:016x}", 2), format!("{:016x}", 3)]);
    }

    #[test]
    fn the_total_cap_deletes_the_least_recently_modified_file_first() {
        let dir = tmp("total");
        let age = |name: &str, secs: u64| {
            let p = dir.join(name);
            std::fs::write(&p, [b'x'; 100]).unwrap();
            let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
            f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)).unwrap();
            p
        };
        let old = age("old.log", 1000);
        let mid = age("mid.log", 2000);
        let cur = age("cur.log", 3000);
        enforce_total(&dir, &cur, 250);
        assert!(!old.exists() && mid.exists() && cur.exists());
    }

    #[test]
    fn the_fingerprint_moves_with_edits_new_files_and_commits_only() {
        if !have_git() {
            return;
        }
        let repo = tmp("fp");
        git(&repo, &["init", "-q"]).unwrap();
        std::fs::write(repo.join("a.txt"), "one").unwrap();
        commit(&repo);
        let base = fingerprint(&repo).unwrap();
        assert_eq!(fingerprint(&repo).unwrap(), base);
        std::fs::write(repo.join("a.txt"), "one two").unwrap();
        let edited = fingerprint(&repo).unwrap();
        assert_ne!(edited, base);
        std::fs::write(repo.join("b.txt"), "new").unwrap();
        let untracked = fingerprint(&repo).unwrap();
        assert_ne!(untracked, edited);
        assert_eq!(fingerprint(&repo).unwrap(), untracked);
        commit(&repo);
        let committed = fingerprint(&repo).unwrap();
        assert_ne!(committed, untracked);
        assert_eq!(fingerprint(&repo).unwrap(), committed);
    }

    #[test]
    fn an_edit_inside_an_untracked_dir_moves_the_fingerprint() {
        if !have_git() {
            return;
        }
        let repo = tmp("fp-untracked");
        git(&repo, &["init", "-q"]).unwrap();
        std::fs::write(repo.join("a.txt"), "one").unwrap();
        commit(&repo);
        std::fs::create_dir(repo.join("new")).unwrap();
        std::fs::write(repo.join("new").join("f.txt"), "one").unwrap();
        let before = fingerprint(&repo).unwrap();
        std::fs::write(repo.join("new").join("f.txt"), "one two").unwrap();
        assert_ne!(fingerprint(&repo).unwrap(), before);
    }

    #[test]
    fn locate_finds_the_repo_root_and_none_outside_a_repo() {
        if !have_git() {
            return;
        }
        let repo = tmp("locate");
        git(&repo, &["init", "-q"]).unwrap();
        std::fs::create_dir_all(repo.join("sub").join("deep")).unwrap();
        let (root, rel) = locate(&repo.join("sub").join("deep")).unwrap();
        assert_eq!(root, std::path::absolute(&repo).unwrap());
        assert_eq!(rel, "sub/deep");
        assert_eq!(locate(&repo).unwrap().1, ".");
    }

    #[test]
    fn secrets_in_a_reply_are_stored_masked() {
        let dir = tmp("mask");
        let root = dir.join("repo");
        let reply = "config password=hunter2hunter2 key AKIAIOSFODNN7EXAMPLE end";
        record_in(&dir, &root, ".", &rec("git log", reply, Some(FP), 100), &CAPS);
        let data = std::fs::read_to_string(log_file(&dir, &root)).unwrap();
        assert!(!data.contains("hunter2hunter2") && !data.contains("AKIAIOSFODNN7EXAMPLE"), "{data}");
        assert!(data.contains("config"));
    }

    #[test]
    fn a_long_reply_is_cut_at_a_char_boundary() {
        let dir = tmp("cut");
        let root = dir.join("repo");
        let reply = "é".repeat(3000);
        record_in(&dir, &root, ".", &rec("ls", &reply, None, 100), &CAPS);
        let data = std::fs::read_to_string(log_file(&dir, &root)).unwrap();
        let v: Value = serde_json::from_str(&data[17..]).unwrap();
        let stored = v["reply"].as_str().unwrap();
        assert!(stored.len() <= REPLY_CAP && stored.ends_with('…'));
    }

    #[test]
    fn only_slow_test_and_lint_commands_are_allowlisted() {
        for yes in ["cargo test --release", "cargo clippy", "cargo check", "python -m pytest -q", "pytest", "go vet ./...", "go test ./...", "npm test", "dotnet test"] {
            assert!(allowlisted(yes), "{yes}");
        }
        for no in [
            "git status",
            "git log -5",
            "ls",
            "cat x",
            "rg foo",
            "cargo build",
            "tsc",
            "cargo clippy --fix",
            "cargo test $X",
            "cargo test %X%",
            "git commit -m x",
            "git branch newname",
            "git status | head",
            "cargo test && echo done",
            "cargo test > out.txt",
            "date",
            "rm -rf x",
            "curl http://x",
            "find . -delete",
            "FOO=1 ls",
            "tail -f log",
            "",
        ] {
            assert!(!allowlisted(no), "{no}");
        }
    }

    fn pass<'a>(cmd: &'a str, reply: &'a str, fp: Option<&'a str>, ts: u64) -> Rec<'a> {
        Rec { code: 0, ..rec(cmd, reply, fp, ts) }
    }

    #[test]
    fn replay_follows_the_rules() {
        let dir = tmp("replay");
        let root = dir.join("repo");
        let same = || Some(FP.to_string());
        let go = |cmd: &str, now: u64, fp: Option<String>| replay_in(&dir, &root, ".", cmd, now, fp.as_deref(), &CAPS);
        record_in(&dir, &root, ".", &pass("cargo test", "all passed", Some(FP), 1000), &CAPS);

        // Same tree, passing run: the header and the stored reply, even when the run is recent.
        let (code, text) = go("cargo test", 1100, same()).unwrap();
        assert_eq!(code, 0);
        assert!(text.starts_with("[qx] unchanged since 1m ago (same tree): exit 0;"), "{text}");
        assert!(text.contains("run it again") && text.ends_with("\nall passed"), "{text}");
        // A second identical request right after a replay runs for real.
        assert!(go("cargo test", 1101, same()).is_none());

        // The real run is recorded again: replay is back.
        record_in(&dir, &root, ".", &pass("cargo test", "all passed", Some(FP), 2000), &CAPS);
        assert!(go("cargo test", 2060, same()).unwrap().1.contains("all passed"));

        // Changed tree, a different command, too old: no replay.
        record_in(&dir, &root, ".", &pass("cargo test", "all passed", Some(FP), 3000), &CAPS);
        assert!(go("cargo test", 3010, Some("bbbbbbbbbbbbbbbb".into())).is_none());
        assert!(go("cargo test", 3010, None).is_none());
        assert!(go("cargo check", 3010, same()).is_none());
        assert!(go("cargo test", 3000 + REPLAY_MAX_AGE, same()).is_none());

        // A failed run never replays.
        record_in(&dir, &root, ".", &rec("cargo test", "FAILED", Some(FP), 4000), &CAPS);
        assert!(go("cargo test", 4010, same()).is_none());

        // An entry without a fingerprint never replays.
        record_in(&dir, &root, ".", &pass("pytest", "ok", None, 5000), &CAPS);
        assert!(go("pytest", 5010, same()).is_none());
    }

    #[test]
    fn a_reply_over_the_cap_is_stored_without_a_fingerprint_and_never_replays() {
        let dir = tmp("overcap");
        let root = dir.join("repo");
        let reply = "x".repeat(REPLY_CAP + 1);
        record_in(&dir, &root, ".", &pass("cargo test", &reply, Some(FP), 1000), &CAPS);
        let data = std::fs::read_to_string(log_file(&dir, &root)).unwrap();
        let v: Value = serde_json::from_str(&data[17..]).unwrap();
        assert_eq!(v["fp"], "");
        assert!(replay_in(&dir, &root, ".", "cargo test", 1010, Some(FP), &CAPS).is_none());
    }

    #[test]
    fn compaction_keeps_an_entry_whose_newest_line_was_a_replay_visible() {
        let dir = tmp("replayed-compact");
        let root = dir.join("repo");
        let file = log_file(&dir, &root);
        record_in(&dir, &root, ".", &pass("cargo test", "all passed", Some(FP), 1000), &CAPS);
        assert!(replay_in(&dir, &root, ".", "cargo test", 1100, Some(FP), &CAPS).is_some());
        let kept = compact(&std::fs::read(&file).unwrap(), 1 << 20);
        std::fs::write(&file, kept).unwrap();
        let all = list_in(&file, 1200, &Query { n: 20, grep: None, searches: false, brief: false });
        assert!(all.contains("cargo test"), "{all}");
        assert!(show_in(&file, &key_of(".", "cargo test"), 1200).is_some());
    }

    #[test]
    fn list_and_show_read_back_what_was_recorded() {
        let dir = tmp("list");
        let root = dir.join("repo");
        let file = log_file(&dir, &root);
        record_in(&dir, &root, ".", &rec("cargo test", "ok\nall passed", Some(FP), 100), &CAPS);
        let search = Rec { cmd: "fn main", kind: Kind::Search, code: 0, ms: 2, reply: "src/main.rs", hits: Some(4), fp: None, ts: 160 };
        record_in(&dir, &root, "src", &search, &CAPS);

        let all = list_in(&file, 220, &Query { n: 20, grep: None, searches: false, brief: false });
        let lines: Vec<&str> = all.lines().collect();
        assert_eq!(lines.len(), 2, "{all}");
        assert!(lines[0].contains("fn main") && lines[0].contains("4 hits"), "{all}");
        assert!(lines[1].contains("cargo test"), "{all}");

        let searches = list_in(&file, 220, &Query { n: 20, grep: None, searches: true, brief: false });
        assert_eq!(searches.lines().count(), 1);
        let grepped = list_in(&file, 220, &Query { n: 20, grep: Some("CARGO"), searches: false, brief: false });
        assert!(grepped.contains("cargo test") && !grepped.contains("fn main"));
        let brief = list_in(&file, 220, &Query { n: 50, grep: None, searches: false, brief: true });
        assert!(brief.contains("cargo test → exit 3 (2m)"), "{brief}");

        let shown = show_in(&file, &key_of(".", "cargo test"), 220).unwrap();
        assert!(shown.starts_with("cargo test\n") && shown.ends_with("ok\nall passed"), "{shown}");
        assert!(show_in(&file, "zz", 220).is_none());
        assert!(show_in(&file, "0000000000000000", 220).is_none());
    }
}
