//! The call ledger: one JSON line per `run`/`fs` call in `%LOCALAPPDATA%\qx\stats.jsonl`, read back by
//! `qx gain`. It exists so decisions about speed and tokens come from real calls, not guesses.
//!
//! Private by construction: a line holds the tool, the op or the command's first word, timings and sizes.
//! Never arguments, paths or output. Writes happen on a background thread, so a call never waits for disk.
//! `QX_STATS=off` turns it off; the file is cut to its newest half past 8 MiB.

use std::cell::Cell;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Mutex, OnceLock};

const ROTATE_AT: u64 = 8 * 1024 * 1024;

pub struct Entry {
    /// `run` or `fs` for the server, `cli` for `qx <cmd>`.
    pub tool: &'static str,
    /// The `fs` op, or the first word of the command. Nothing after it.
    pub key: String,
    pub ms: u64,
    pub code: i32,
    /// Bytes the call produced before compaction, and bytes of the reply the model reads.
    pub raw: usize,
    pub reply: usize,
    pub flags: &'static str,
}

thread_local! {
    static RAW: Cell<usize> = const { Cell::new(0) };
}

/// A `run` that produced `n` bytes of output before compaction. Read back by the call's own thread.
pub fn note_raw(n: usize) {
    RAW.with(|r| r.set(n));
}

/// Take the byte count noted since the last call on this thread.
pub fn take_raw() -> usize {
    RAW.with(|r| r.replace(0))
}

pub fn enabled() -> bool {
    // The test suite calls the server hundreds of times: it must not write into the user's real ledger.
    !cfg!(test)
        && !std::env::var("QX_STATS").is_ok_and(|v| v.eq_ignore_ascii_case("off") || v == "0")
}

pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("qx").join("stats.jsonl"))
}

/// The first word of a command line, cut to a bare program name: no path, no `.exe`, no arguments.
pub fn first_word(line: &str) -> String {
    let mut rest = line.trim_start();
    // `NAME=value` words in front hold values (a token, a password) and are never the key.
    loop {
        let word = rest.split_whitespace().next().unwrap_or("");
        if crate::exec::assignment_word(word) {
            rest = rest[word.len()..].trim_start();
        } else {
            break;
        }
    }
    let word = match rest.chars().next() {
        // A quoted program path may hold spaces: take it whole, up to the closing quote.
        Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or(""),
        _ => rest.split_whitespace().next().unwrap_or("?"),
    };
    let name = word.rsplit(['\\', '/']).next().unwrap_or(word);
    let name = name.strip_suffix(".exe").unwrap_or(name);
    let key: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).take(32).collect();
    if key.is_empty() { "?".to_string() } else { key }
}

static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The writer thread, kept so `flush` can wait for it.
fn writer() -> &'static Mutex<Option<std::thread::JoinHandle<()>>> {
    static H: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
    &H
}

/// Write what is queued and stop the writer. Call once when the server ends; a later `record` is dropped.
pub fn flush() {
    // Nothing was ever recorded: do not start a writer just to stop it.
    if !STARTED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    if let Some(tx) = sender().lock().unwrap_or_else(|p| p.into_inner()).take() {
        drop(tx);
    }
    if let Some(h) = writer().lock().unwrap_or_else(|p| p.into_inner()).take() {
        let _ = h.join();
    }
}

/// Lines waiting for the writer. A full queue (storage stalled) drops new lines instead of growing.
const QUEUE: usize = 1024;

fn sender() -> &'static Mutex<Option<SyncSender<String>>> {
    static TX: OnceLock<Mutex<Option<SyncSender<String>>>> = OnceLock::new();
    TX.get_or_init(|| {
        STARTED.store(true, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = sync_channel::<String>(QUEUE);
        let spawned = std::thread::Builder::new().name("qx-stats".into()).spawn(move || {
            let Some(path) = path() else { return };
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            while let Ok(first) = rx.recv() {
                let mut batch = first;
                while let Ok(more) = rx.try_recv() {
                    batch.push_str(&more);
                }
                append(&path, batch.as_bytes());
            }
        });
        match spawned {
            Ok(h) => {
                *writer().lock().unwrap_or_else(|p| p.into_inner()) = Some(h);
                Mutex::new(Some(tx))
            }
            Err(_) => Mutex::new(None),
        }
    })
}

/// Append `text`, first keeping only the newest half (from a line boundary) once the file passes
/// `ROTATE_AT`. The file is held without write sharing for the whole step, so one qx process never
/// rotates a stale copy over lines another one has just appended.
fn append(path: &std::path::Path, text: &[u8]) {
    use std::io::{Read, Seek, SeekFrom};
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true).write(true).create(true);
    // FILE_SHARE_READ: `qx gain` can still read while a writer holds the file.
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::share_mode(&mut opts, 1);
    let mut held = None;
    for _ in 0..200 {
        match opts.open(path) {
            Ok(f) => {
                held = Some(f);
                break;
            }
            // ERROR_SHARING_VIOLATION: another qx process is appending; it holds the file for moments.
            Err(e) if cfg!(windows) && e.raw_os_error() == Some(32) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(_) => return,
        }
    }
    let Some(mut f) = held else { return };
    if f.metadata().is_ok_and(|m| m.len() >= ROTATE_AT) {
        let mut data = Vec::new();
        if f.read_to_end(&mut data).is_ok() {
            let from = data.len() / 2;
            let start = data[from..].iter().position(|&b| b == b'\n').map_or(data.len(), |i| from + i + 1);
            let kept = &data[start..];
            let _ = f
                .seek(SeekFrom::Start(0))
                .and_then(|_| f.write_all(kept))
                .and_then(|_| f.set_len(kept.len() as u64));
        }
    }
    let _ = f.seek(SeekFrom::End(0)).and_then(|_| f.write_all(text));
}

fn line(e: &Entry) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut v = serde_json::json!({
        "ts": ts, "tool": e.tool, "cmd": e.key, "ms": e.ms, "code": e.code, "in": e.raw, "out": e.reply,
    });
    if !e.flags.is_empty() {
        v["flags"] = serde_json::json!(e.flags);
    }
    format!("{v}\n")
}

/// Queue one entry. Cheap: a non-blocking send. A closed, missing or full writer drops the entry.
pub fn record(e: Entry) {
    if !enabled() {
        return;
    }
    if let Some(tx) = sender().lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        let _ = tx.try_send(line(&e));
    }
}

/// Write one entry now, for a process that exits right after (the CLI): no thread to flush.
pub fn record_now(e: Entry) {
    if !enabled() {
        return;
    }
    let Some(path) = path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    append(&path, line(&e).as_bytes());
}

/// One tool's row of the `qx gain` report.
#[derive(Default)]
struct Group {
    pub ms: Vec<u64>,
    pub raw: u64,
    pub reply: u64,
}

/// Read the ledger from `since_secs` ago (0: everything) and print the report. Returns the exit code.
pub fn report(since_secs: u64) -> i32 {
    let Some(path) = path() else {
        println!("stats path unavailable");
        return 0;
    };
    let Ok(data) = std::fs::read_to_string(&path) else {
        println!("no stats yet: the qx server records every call from now on (QX_STATS=off stops it)");
        return 0;
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let floor = if since_secs == 0 { 0 } else { now.saturating_sub(since_secs) };
    let mut groups: std::collections::HashMap<(String, String), Group> = std::collections::HashMap::new();
    let (mut unchanged, mut failed) = (0u64, 0u64);
    for l in data.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(l) else { continue };
        if v.get("ts").and_then(|x| x.as_u64()).unwrap_or(0) < floor {
            continue;
        }
        let tool = v.get("tool").and_then(|x| x.as_str()).unwrap_or("cli").to_string();
        let key = v.get("cmd").and_then(|x| x.as_str()).unwrap_or("?").to_string();
        let g = groups.entry((tool, key)).or_default();
        g.ms.push(v.get("ms").and_then(|x| x.as_u64()).unwrap_or(0));
        g.raw += v.get("in").and_then(|x| x.as_u64()).unwrap_or(0);
        g.reply += v.get("out").and_then(|x| x.as_u64()).unwrap_or(0);
        unchanged += u64::from(v.get("flags").and_then(|x| x.as_str()) == Some("unchanged"));
        failed += u64::from(v.get("code").and_then(|x| x.as_i64()).unwrap_or(0) != 0);
    }
    let calls: usize = groups.values().map(|g| g.ms.len()).sum();
    let (raw, reply): (u64, u64) = groups.values().fold((0, 0), |a, g| (a.0 + g.raw, a.1 + g.reply));
    let total_ms: u64 = groups.values().map(|g| g.ms.iter().sum::<u64>()).sum();
    println!("qx gain{}", if since_secs == 0 { String::new() } else { format!(" (last {})", human(since_secs)) });
    println!("  calls      : {calls} ({failed} non-zero)");
    println!("  exec time  : {:.1}s", total_ms as f64 / 1000.0);
    println!("  output     : {} KiB -> {} KiB ({}% saved)", raw / 1024, reply / 1024, ((raw.saturating_sub(reply)) * 100).checked_div(raw).unwrap_or(0));
    if unchanged > 0 {
        println!("  re-reads   : {unchanged} answered with a one-line stub");
    }
    let mut rows: Vec<_> = groups.into_iter().collect();
    // Most reply bytes first: that is what the model pays for.
    rows.sort_by_key(|(_, g)| std::cmp::Reverse(g.reply));
    println!("  {:<4} {:<14} {:>6} {:>9} {:>9} {:>11}", "tool", "key", "calls", "median ms", "p90 ms", "reply KiB");
    for ((tool, key), mut g) in rows.into_iter().take(15) {
        let mut sorted = g.ms.clone();
        sorted.sort_unstable();
        let p90 = sorted[(sorted.len() * 9 / 10).min(sorted.len() - 1)];
        println!("  {:<4} {:<14} {:>6} {:>9} {:>9} {:>11.1}", tool, key, g.ms.len(), median(&mut g.ms), p90, g.reply as f64 / 1024.0);
    }
    0
}

fn human(secs: u64) -> String {
    match secs {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 => format!("{}h", s / 3_600),
        s => format!("{}m", s / 60),
    }
}

/// Median of a non-empty slice (sorts it).
fn median(v: &mut [u64]) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

/// Parse `7d`, `12h`, `30m` or a bare number of days into seconds.
pub fn parse_since(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, unit) = match s.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => (&s[..i], c.to_ascii_lowercase()),
        _ => (s, 'd'),
    };
    let n: u64 = num.parse().ok()?;
    n.checked_mul(match unit {
        'd' => 86_400,
        'h' => 3_600,
        'm' => 60,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_overflow_is_none_and_path_with_equals_is_program() {
        assert_eq!(parse_since("18446744073709551615d"), None);
        // The program is kept as the key (not `--version`); the key filter drops `=`.
        assert_eq!(first_word(r"C:\tools\a=b.exe --version"), "ab");
    }

    #[test]
    fn the_key_is_only_a_bare_program_name() {
        assert_eq!(first_word(r#""C:\Program Files\Git\bin\git.exe" push origin main"#), "git");
        assert_eq!(first_word("git status --short"), "git");
        assert_eq!(first_word(r"C:\tools\rg.exe -n secret"), "rg");
        assert_eq!(first_word("   "), "?");
        // A value assigned in front is never recorded, not even in part.
        assert_eq!(first_word("TOKEN=abc123 API_KEY=zzz ls -la"), "ls");
        assert_eq!(first_word("TOKEN=abc123"), "?");
    }

    #[test]
    fn a_line_carries_sizes_and_timings_but_nothing_else() {
        let l = line(&Entry { tool: "run", key: "git".into(), ms: 12, code: 0, raw: 900, reply: 80, flags: "" });
        let v: serde_json::Value = serde_json::from_str(l.trim()).unwrap();
        assert_eq!(v["tool"], "run");
        assert_eq!(v["cmd"], "git");
        assert_eq!(v["in"], 900);
        assert_eq!(v["out"], 80);
        assert!(v.get("flags").is_none());
        assert!(l.ends_with('\n'));
    }

    #[test]
    fn since_accepts_days_hours_and_minutes() {
        assert_eq!(parse_since("7d"), Some(7 * 86_400));
        assert_eq!(parse_since("12h"), Some(12 * 3_600));
        assert_eq!(parse_since("30m"), Some(1_800));
        assert_eq!(parse_since("2"), Some(2 * 86_400));
        assert_eq!(parse_since("x"), None);
        assert_eq!(parse_since("5w"), None);
    }

    #[test]
    fn raw_bytes_noted_on_a_thread_are_taken_once() {
        note_raw(123);
        assert_eq!(take_raw(), 123);
        assert_eq!(take_raw(), 0);
    }

    #[test]
    fn the_median_of_an_odd_and_even_list() {
        assert_eq!(median(&mut [5, 1, 9]), 5);
        assert_eq!(median(&mut [4, 1, 9, 2]), 4);
    }

    #[cfg(windows)]
    #[test]
    fn a_rotation_waits_for_another_writer_and_keeps_its_line() {
        use std::io::{Seek, SeekFrom};
        use std::os::windows::fs::OpenOptionsExt;
        let path = std::env::temp_dir().join(format!("qx-stats-rotate-{}.jsonl", std::process::id()));
        let row = "x".repeat(99) + "\n";
        std::fs::write(&path, row.repeat((ROTATE_AT / 100 + 1) as usize)).unwrap();
        // Another qx process holds the ledger mid-append, as `append` does.
        let mut other = std::fs::OpenOptions::new().read(true).write(true).share_mode(1).open(&path).unwrap();
        let p = path.clone();
        let ours = std::thread::spawn(move || append(&p, b"ours\n"));
        std::thread::sleep(std::time::Duration::from_millis(100));
        other.seek(SeekFrom::End(0)).unwrap();
        other.write_all(b"theirs\n").unwrap();
        drop(other);
        ours.join().unwrap();
        let data = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(data.ends_with("theirs\nours\n"), "another process's line was lost");
        assert!((data.len() as u64) < ROTATE_AT, "the file was not rotated");
    }
}
