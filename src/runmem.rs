//! Run memory: a command run again in the same folder whose reply text is the same as the one already
//! sent gets a one-line note instead of the same text again (a test that still fails the same way, a
//! build re-run after an edit that changed nothing). Only a hash is kept, never the text, so the cost
//! is a few bytes per command. `mode:"full"` and `QX_RUN_MEMORY=off` always send the text.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Entries kept; the oldest goes first.
const MAX_ENTRIES: usize = 32;
/// After this long the text is sent again: the agent may have lost it to compaction.
const IDLE: Duration = Duration::from_secs(30 * 60);
/// A shorter reply is not worth a note: the note would be about as long.
const MIN_BYTES: usize = 600;

struct Seen {
    hash: u64,
    code: i32,
    /// When the text was last sent in full (a note does not renew it).
    sent: Instant,
}

fn table() -> &'static Mutex<HashMap<String, Seen>> {
    static T: OnceLock<Mutex<HashMap<String, Seen>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn off() -> bool {
    std::env::var("QX_RUN_MEMORY").ok().as_deref() == Some("off")
}

/// The note to send in place of `stdout`/`stderr`, when this command already produced exactly this
/// text (and exit code) less than 30 minutes ago; otherwise remembers it and returns `None`.
/// `timing`: durations ("finished in 18.96s") are ignored when comparing, as test and build summaries
/// carry one that differs on every run.
pub fn seen(key: &str, stdout: &str, stderr: &str, code: i32, timing: bool) -> Option<String> {
    let len = stdout.len() + stderr.len();
    if len < MIN_BYTES {
        // Never noted, so never stored: short replies must not evict entries that can save text.
        // The command's older text is dropped, so a later repeat of it is not called "the same".
        table().lock().unwrap_or_else(|e| e.into_inner()).remove(key);
        return None;
    }
    let hash = fingerprint(stdout, stderr, timing);
    let now = Instant::now();
    let mut t = table().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = t.get(key) {
        if s.hash == hash && s.code == code && now.duration_since(s.sent) < IDLE {
            return Some(format!(
                "[qx] same output as the run {} ago (exit {code}, {len} bytes); not repeated. mode:\"full\" shows it again.",
                age(now.duration_since(s.sent))
            ));
        }
    }
    if t.len() >= MAX_ENTRIES && !t.contains_key(key) {
        let oldest = t.iter().min_by_key(|(_, s)| s.sent).map(|(k, _)| k.clone());
        if let Some(k) = oldest {
            t.remove(&k);
        }
    }
    t.insert(key.to_string(), Seen { hash, code, sent: now });
    None
}

fn age(d: Duration) -> String {
    match d.as_secs() {
        s if s < 90 => format!("{s}s"),
        s => format!("{}m", s / 60),
    }
}

fn fingerprint(stdout: &str, stderr: &str, timing: bool) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (i, text) in [stdout, stderr].into_iter().enumerate() {
        i.hash(&mut h);
        if timing {
            hash_without_durations(text, &mut h);
        } else {
            text.hash(&mut h);
        }
    }
    h.finish()
}

/// Hashes `text` with every number that is directly followed by a time unit (`18.96s`, `120ms`, `3m`)
/// replaced by one marker.
fn hash_without_durations(text: &str, h: &mut impl Hasher) {
    let b = text.as_bytes();
    let (mut i, mut from) = (0, 0);
    while i < b.len() {
        if !b[i].is_ascii_digit() || (i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && (b[i].is_ascii_digit() || (b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))) {
            i += 1;
        }
        let unit = ["ms", "µs", "ns", "s", "m"].iter().find(|u| b[i..].starts_with(u.as_bytes()));
        match unit {
            Some(u) if !b.get(i + u.len()).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') => {
                text[from..start].hash(h);
                '#'.hash(h);
                i += u.len();
                from = i;
            }
            _ => {}
        }
    }
    text[from..].hash(h);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(n: usize) -> String {
        format!("{}\n", "failure line that is long enough\n".repeat(n))
    }

    #[test]
    fn the_same_text_a_second_time_is_a_note_and_a_different_one_is_not() {
        let text = big(30);
        assert!(seen("rm-a", &text, "", 1, false).is_none());
        let note = seen("rm-a", &text, "", 1, false).expect("same text twice");
        assert!(note.contains("same output") && note.contains("exit 1") && note.contains("mode:\"full\""));
        // Different text, a different exit code and a different command all send the text.
        assert!(seen("rm-a", &big(31), "", 1, false).is_none());
        assert!(seen("rm-a", &big(31), "", 2, false).is_none());
        assert!(seen("rm-b", &big(31), "", 2, false).is_none());
    }

    #[test]
    fn a_short_reply_is_always_sent() {
        assert!(seen("rm-short", "ok\n", "", 0, false).is_none());
        assert!(seen("rm-short", "ok\n", "", 0, false).is_none());
    }

    #[test]
    fn short_replies_do_not_evict_remembered_ones() {
        let text = big(30);
        assert!(seen("rm-keep", &text, "", 1, false).is_none());
        for i in 0..MAX_ENTRIES + 8 {
            assert!(seen(&format!("rm-short-{i}"), "ok\n", "", 0, false).is_none());
        }
        assert!(seen("rm-keep", &text, "", 1, false).is_some(), "short replies evicted the entry");
    }

    #[test]
    fn stdout_and_stderr_are_not_interchangeable() {
        let text = big(30);
        assert!(seen("rm-streams", &text, "", 0, false).is_none());
        assert!(seen("rm-streams", "", &text, 0, false).is_none());
    }

    #[test]
    fn a_changed_duration_does_not_make_a_test_run_new_but_a_changed_count_does() {
        let run = |secs: &str, passed: u32| format!("{}test result: FAILED. {passed} passed; 1 failed; finished in {secs}\n", big(30));
        assert!(seen("rm-time", &run("18.96s", 556), "", 101, true).is_none());
        assert!(seen("rm-time", &run("19.2s", 556), "", 101, true).is_some());
        assert!(seen("rm-time", &run("19.2s", 557), "", 101, true).is_none());
        // Without `timing` a different duration is a different text.
        assert!(seen("rm-time2", &run("1s", 5), "", 1, false).is_none());
        assert!(seen("rm-time2", &run("2s", 5), "", 1, false).is_none());
    }

    #[test]
    fn digits_inside_names_are_not_taken_for_durations() {
        let a = format!("{}sha256 of v1s2\n", big(30));
        let b = format!("{}sha256 of v1s3\n", big(30));
        assert!(seen("rm-names", &a, "", 0, true).is_none());
        assert!(seen("rm-names", &b, "", 0, true).is_none());
    }

    #[test]
    fn ages_read_as_seconds_then_minutes() {
        assert_eq!(age(Duration::from_secs(45)), "45s");
        assert_eq!(age(Duration::from_secs(300)), "5m");
    }
}
