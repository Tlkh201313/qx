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
    append_limited(path, text, ROTATE_AT);
}

/// `append` with the rotation size as a parameter. Also keeps the totals sidecar current while the
/// ledger handle is held, which is what makes the read-modify-write of the sidecar safe across processes.
fn append_limited(path: &std::path::Path, text: &[u8], rotate_at: u64) {
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
    let mut totals = read_totals(path);
    let rotate = f.metadata().is_ok_and(|m| m.len() >= rotate_at);
    // The ledger is read whole only when it must be: to cut it, or to seed missing totals from it.
    if rotate || totals.is_none() {
        let mut data = Vec::new();
        if f.read_to_end(&mut data).is_ok() {
            if totals.is_none() {
                totals = Some(scan(&String::from_utf8_lossy(&data), 0).t);
            }
            if rotate {
                let from = data.len() / 2;
                let start = data[from..].iter().position(|&b| b == b'\n').map_or(data.len(), |i| from + i + 1);
                let kept = &data[start..];
                let _ = f
                    .seek(SeekFrom::Start(0))
                    .and_then(|_| f.write_all(kept))
                    .and_then(|_| f.set_len(kept.len() as u64));
            }
        }
    }
    if f.seek(SeekFrom::End(0)).and_then(|_| f.write_all(text)).is_ok() {
        if let Some(mut t) = totals {
            for l in String::from_utf8_lossy(text).lines() {
                if let Some(r) = Row::parse(l) {
                    t.add(&r);
                }
            }
            write_totals(path, &t);
        }
    }
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

/// The only fields read from a ledger line.
struct Row {
    ts: u64,
    tool: String,
    cmd: String,
    ms: u64,
    code: i64,
    raw: u64,
    reply: u64,
    unchanged: bool,
}

impl Row {
    fn parse(l: &str) -> Option<Row> {
        let v: serde_json::Value = serde_json::from_str(l).ok()?;
        let n = |k: &str| v[k].as_u64().unwrap_or(0);
        let s = |k: &str, d: &str| v[k].as_str().unwrap_or(d).to_string();
        Some(Row {
            ts: n("ts"),
            tool: s("tool", "cli"),
            cmd: s("cmd", "?"),
            ms: n("ms"),
            code: v["code"].as_i64().unwrap_or(0),
            raw: n("in"),
            reply: n("out"),
            unchanged: v["flags"] == "unchanged",
        })
    }
}

/// Running totals of the whole ledger. Also the sidecar file `stats.totals.json`, which keeps all-time
/// figures across rotation (the ledger itself only keeps its newest half).
#[derive(Default, Clone, PartialEq, Debug)]
struct Totals {
    calls: u64,
    failed: u64,
    raw_bytes: u64,
    reply_bytes: u64,
    cmd_raw_bytes: u64,
    cmd_reply_bytes: u64,
    stub_reads: u64,
}

impl Totals {
    fn add(&mut self, r: &Row) {
        self.calls += 1;
        self.failed += u64::from(r.code != 0);
        self.raw_bytes += r.raw;
        self.reply_bytes += r.reply;
        if r.tool == "run" || r.tool == "cli" {
            self.cmd_raw_bytes += r.raw;
            self.cmd_reply_bytes += r.reply;
        }
        self.stub_reads += u64::from(r.unchanged);
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "calls": self.calls, "failed": self.failed, "raw_bytes": self.raw_bytes, "reply_bytes": self.reply_bytes,
            "cmd_raw_bytes": self.cmd_raw_bytes, "cmd_reply_bytes": self.cmd_reply_bytes, "stub_reads": self.stub_reads,
        })
    }

    fn from_json(v: &serde_json::Value) -> Option<Totals> {
        let n = |k: &str| v[k].as_u64();
        Some(Totals {
            calls: n("calls")?,
            failed: n("failed")?,
            raw_bytes: n("raw_bytes")?,
            reply_bytes: n("reply_bytes")?,
            cmd_raw_bytes: n("cmd_raw_bytes")?,
            cmd_reply_bytes: n("cmd_reply_bytes")?,
            stub_reads: n("stub_reads")?,
        })
    }
}

/// Everything one pass over the ledger yields; the text report, `--line` and `--json` all read it.
#[derive(Default)]
struct Scan {
    groups: std::collections::HashMap<(String, String), Group>,
    t: Totals,
}

/// Aggregate every line at or after `floor` (unix seconds). Bad lines are skipped.
fn scan(data: &str, floor: u64) -> Scan {
    let mut s = Scan::default();
    for l in data.lines() {
        let Some(r) = Row::parse(l) else { continue };
        if r.ts < floor {
            continue;
        }
        s.t.add(&r);
        let g = s.groups.entry((r.tool, r.cmd)).or_default();
        g.ms.push(r.ms);
        g.raw += r.raw;
        g.reply += r.reply;
    }
    s
}

fn sidecar(ledger: &std::path::Path) -> PathBuf {
    ledger.with_extension("totals.json")
}

fn read_totals(ledger: &std::path::Path) -> Option<Totals> {
    Totals::from_json(&serde_json::from_slice(&std::fs::read(sidecar(ledger)).ok()?).ok()?)
}

/// Write the sidecar through a temp file and a rename, so a reader never sees half of it. Errors are
/// ignored: totals are a convenience, never a reason to fail a call.
fn write_totals(ledger: &std::path::Path, t: &Totals) {
    let side = sidecar(ledger);
    let tmp = side.with_extension("tmp");
    if let Ok(body) = serde_json::to_vec(&t.to_json()) {
        let written = std::fs::File::create(&tmp).and_then(|mut f| {
            use std::io::Write;
            f.write_all(&body)?;
            // On disk before the rename, so a crash leaves the old sidecar or the whole new one.
            f.sync_all()
        });
        if written.and_then(|()| std::fs::rename(&tmp, &side)).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// What `qx gain` prints.
#[derive(Clone, Copy, PartialEq)]
pub enum Format {
    Text,
    Line,
    Json,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Short count: `999`, `1.5k`, `12.3k`, `690k`, `1.8M`. One decimal below 100 of a unit, none above.
fn humanize(n: u64) -> String {
    fn unit(v: f64, suffix: &str) -> String {
        if v < 99.95 {
            let s = format!("{v:.1}");
            format!("{}{suffix}", s.strip_suffix(".0").unwrap_or(&s))
        } else {
            format!("{v:.0}{suffix}")
        }
    }
    if n < 1000 {
        n.to_string()
    } else if n < 999_500 {
        unit(n as f64 / 1e3, "k")
    } else {
        unit(n as f64 / 1e6, "M")
    }
}

/// Signed change from `raw` to `reply` in whole percent: `−62%` saved, `+5%` grown, `0%` even.
fn delta(raw: u64, reply: u64) -> String {
    let pct = ((reply as f64 - raw as f64) * 100.0 / raw.max(1) as f64).round() as i64;
    match pct {
        0 => "0%".into(),
        p if p < 0 => format!("\u{2212}{}%", -p),
        p => format!("+{p}%"),
    }
}

/// The one-line summary every agent-CLI widget shows. Tokens are bytes / 4.
fn format_line(t: &Totals) -> String {
    if t.calls == 0 {
        return "qx \u{25b8} no calls yet".into();
    }
    let mut out = format!("qx \u{25b8} {} \u{b7} {}\u{2192}{} tok", delta(t.raw_bytes, t.reply_bytes), humanize(t.raw_bytes / 4), humanize(t.reply_bytes / 4));
    if t.cmd_raw_bytes > 0 {
        out += &format!(" \u{b7} cmds {}", delta(t.cmd_raw_bytes, t.cmd_reply_bytes));
    }
    if t.stub_reads > 0 {
        out += &format!(" \u{b7} {} stub reads", t.stub_reads);
    }
    out + &format!(" \u{b7} {} calls", humanize(t.calls))
}

fn format_json(t: &Totals, since_secs: u64) -> String {
    let mut v = t.to_json();
    v["since_secs"] = since_secs.into();
    v.to_string()
}

/// Read the ledger from `since_secs` ago (0: everything) and print it as `fmt`. Returns the exit code.
/// `Line` and `Json` never fail: a missing ledger reads as no calls. With no window they read only the
/// sidecar of running totals (cheap, and not cut by rotation); a missing sidecar falls back to a scan.
pub fn report(since_secs: u64, fmt: Format) -> i32 {
    let ledger = path();
    let floor = if since_secs == 0 { 0 } else { now_secs().saturating_sub(since_secs) };
    if fmt != Format::Text {
        let side = if since_secs == 0 { ledger.as_deref().and_then(read_totals) } else { None };
        let t = side.unwrap_or_else(|| {
            let data = ledger.as_deref().and_then(|p| std::fs::read_to_string(p).ok());
            scan(data.as_deref().unwrap_or(""), floor).t
        });
        println!("{}", if fmt == Format::Line { format_line(&t) } else { format_json(&t, since_secs) });
        return 0;
    }
    let Some(ledger) = ledger else {
        println!("stats path unavailable");
        return 0;
    };
    let Ok(data) = std::fs::read_to_string(&ledger) else {
        println!("no stats yet: the qx server records every call from now on (QX_STATS=off stops it)");
        return 0;
    };
    let Scan { groups, t: Totals { calls, failed, raw_bytes: raw, reply_bytes: reply, stub_reads: unchanged, .. } } = scan(&data, floor);
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

    fn ledger_row(tool: &str, raw: u64, reply: u64, flags: &str, ts: u64) -> String {
        format!("{{\"ts\":{ts},\"tool\":\"{tool}\",\"cmd\":\"x\",\"ms\":1,\"code\":0,\"in\":{raw},\"out\":{reply},\"flags\":\"{flags}\"}}\n")
    }

    fn temp_ledger(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qx-stats-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("stats.jsonl")
    }

    #[test]
    fn counts_are_humanized() {
        assert_eq!(humanize(999), "999");
        assert_eq!(humanize(1500), "1.5k");
        assert_eq!(humanize(12_345), "12.3k");
        assert_eq!(humanize(690_000), "690k");
        assert_eq!(humanize(1_800_000), "1.8M");
    }

    #[test]
    fn the_line_shows_savings_and_hides_zero_stub_reads() {
        let data = ledger_row("run", 4000, 1000, "", 1) + &ledger_row("fs", 4000, 4000, "unchanged", 1);
        let line = format_line(&scan(&data, 0).t);
        assert_eq!(line, "qx \u{25b8} \u{2212}38% \u{b7} 2k\u{2192}1.2k tok \u{b7} cmds \u{2212}75% \u{b7} 1 stub reads \u{b7} 2 calls");
        let none = format_line(&scan(&ledger_row("run", 4000, 1000, "", 1), 0).t);
        assert_eq!(none, "qx \u{25b8} \u{2212}75% \u{b7} 1k\u{2192}250 tok \u{b7} cmds \u{2212}75% \u{b7} 1 calls");
    }

    #[test]
    fn an_empty_ledger_has_no_calls_yet() {
        assert_eq!(format_line(&scan("", 0).t), "qx \u{25b8} no calls yet");
        assert_eq!(format_line(&scan("not json\n", 0).t), "qx \u{25b8} no calls yet");
    }

    #[test]
    fn since_drops_older_lines() {
        let data = ledger_row("run", 100, 10, "", 50) + &ledger_row("run", 200, 20, "", 150);
        let t = scan(&data, 100).t;
        assert_eq!((t.calls, t.raw_bytes, t.reply_bytes), (1, 200, 20));
    }

    #[test]
    fn two_appends_add_up_and_the_first_seeds_existing_history_once() {
        let p = temp_ledger("seed");
        std::fs::write(&p, ledger_row("run", 100, 10, "", 1)).unwrap();
        append(&p, ledger_row("run", 200, 20, "", 2).as_bytes());
        append(&p, ledger_row("fs", 300, 30, "unchanged", 3).as_bytes());
        let t = read_totals(&p).unwrap();
        assert_eq!((t.calls, t.raw_bytes, t.reply_bytes, t.cmd_raw_bytes, t.stub_reads), (3, 600, 60, 300, 1));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn rotation_does_not_lower_the_totals() {
        let p = temp_ledger("rotate");
        let row = ledger_row("run", 100, 10, "", 1);
        for _ in 0..10 {
            append_limited(&p, row.as_bytes(), 500);
        }
        let t = read_totals(&p).unwrap();
        assert_eq!((t.calls, t.raw_bytes), (10, 1000));
        assert!(std::fs::metadata(&p).unwrap().len() < 10 * row.len() as u64, "the ledger was not cut");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_json_object_carries_the_window() {
        let v: serde_json::Value = serde_json::from_str(&format_json(&scan(&ledger_row("run", 8, 4, "", 1), 0).t, 604_800)).unwrap();
        assert_eq!((v["calls"].as_u64(), v["raw_bytes"].as_u64(), v["since_secs"].as_u64()), (Some(1), Some(8), Some(604_800)));
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
        let _ = std::fs::remove_file(sidecar(&path));
        assert!(data.ends_with("theirs\nours\n"), "another process's line was lost");
        assert!((data.len() as u64) < ROTATE_AT, "the file was not rotated");
    }
}
