//! Output cleanup: ANSI stripping, duplicate collapsing, and error-aware compaction.
//!
//! Naive head+tail truncation can drop the one line that matters (a test failure
//! or a compiler error in the middle of a long log). `compact` rescues lines that
//! look diagnostic before it truncates.

/// Remove ANSI escape sequences (colors, cursor moves, OSC titles).
pub fn strip_ansi(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == 0x1b {
            i += 1;
            if i < b.len() && b[i] == b'[' {
                i += 1;
                while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
                    i += 1;
                }
                if i < b.len() {
                    i += 1;
                }
            } else if i < b.len() && b[i] == b']' {
                i += 1;
                while i < b.len() {
                    if b[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if b[i] == 0x1b && i + 1 < b.len() && b[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            } else if i < b.len() && b[i].is_ascii() {
                // ESC 7 / ESC c, or a charset select like ESC ( B. A non-ASCII
                // byte after ESC is text: skipping it would split a UTF-8 char.
                let select = matches!(b[i], b'(' | b')' | b'*' | b'+' | b'#' | b'%');
                i += 1;
                if select && i < b.len() && b[i].is_ascii() {
                    i += 1;
                }
            }
            continue;
        }
        match s[i..].chars().next() {
            Some(ch) => {
                out.push(ch);
                i += ch.len_utf8();
            }
            None => break,
        }
    }
    out
}

pub(crate) const MAX_LINE_CHARS: usize = 4000;

/// Markers that usually mean "the agent needs this line even if it is buried".
const IMPORTANT_MARKERS: &[&str] = &[
    "error",
    "panic",
    "exception",
    "traceback",
    "fatal",
    "failed",
    "failure",
    "assertion",
    "not found",
    "cannot",
    "could not",
    "unable to",
    "denied",
    "segmentation fault",
    "warn",
    "fail",
    "✗",
    "✘",
    "timed out",
    "timeout",
    "unresolved",
    "undefined",
];

pub fn interesting(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    IMPORTANT_MARKERS.iter().any(|m| l.contains(m))
}

/// Appends the line, cut at `MAX_LINE_CHARS`; true when it was cut (the caller spools the original).
fn push_capped(out: &mut String, line: &str) -> bool {
    let n = line.chars().count();
    out.push_str(&line[..line.char_indices().nth(MAX_LINE_CHARS).map_or(line.len(), |(i, _)| i)]);
    let cut = n > MAX_LINE_CHARS;
    if cut {
        out.push_str(&format!(" ...[+{} chars]", n - MAX_LINE_CHARS));
    }
    out.push('\n');
    cut
}

/// Collapse runs of three or more identical lines into `line  (xN)`.
/// Returns the new lines and how many source lines were removed.
fn collapse(lines: &[&str]) -> (Vec<String>, usize) {
    let mut out = Vec::with_capacity(lines.len());
    let mut collapsed = 0usize;
    let mut i = 0usize;
    while i < lines.len() {
        let mut j = i + 1;
        while j < lines.len() && lines[j] == lines[i] {
            j += 1;
        }
        let run = j - i;
        if run >= 3 {
            out.push(format!("{}  (x{})", lines[i], run));
            collapsed += run - 1;
        } else {
            for line in &lines[i..j] {
                out.push((*line).to_string());
            }
        }
        i = j;
    }
    (out, collapsed)
}

/// A line that ends in a file name or holds a path: a listing (`img_001.png` ... `img_260.png`) is data the
/// reader needs entry by entry, unlike a progress counter.
fn names_a_file(line: &str) -> bool {
    let last = line.split_whitespace().last().unwrap_or("");
    if last.contains(['/', '\\']) {
        return true;
    }
    match last.rsplit_once('.') {
        Some((stem, ext)) => !stem.is_empty() && (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()) && ext.chars().any(|c| c.is_ascii_alphabetic()),
        None => false,
    }
}

/// Runs of 8+ lines that differ only in their digits (progress counters,
/// timestamps) keep the first and last line plus a count. Only used over budget,
/// where head/tail would drop them anyway; diagnostics are never collapsed.
fn collapse_similar(lines: Vec<String>) -> (Vec<String>, bool) {
    let keys: Vec<String> = lines
        .iter()
        .map(|l| {
            let mut k = String::with_capacity(l.len());
            let mut digits = false;
            for c in l.chars() {
                if c.is_ascii_digit() {
                    if !digits {
                        k.push('#');
                    }
                } else {
                    k.push(c);
                }
                digits = c.is_ascii_digit();
            }
            k
        })
        .collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut changed = false;
    let mut i = 0usize;
    while i < lines.len() {
        let mut j = i + 1;
        while j < keys.len() && keys[j] == keys[i] {
            j += 1;
        }
        if j - i >= 8 && keys[i].contains('#') && !interesting(&lines[i]) && !names_a_file(&lines[i]) {
            out.push(lines[i].clone());
            out.push(format!("[qx] ... {} similar lines (only digits differ) ...", j - i - 2));
            out.push(lines[j - 1].clone());
            changed = true;
        } else {
            out.extend_from_slice(&lines[i..j]);
        }
        i = j;
    }
    (out, changed)
}

/// Drop the working directory from located paths (`C:\proj\src\a.rs:3` ->
/// `src\a.rs:3`); the caller already knows it. Returns how many were shortened.
pub fn relative_paths(text: &mut String, cwd: &str) -> usize {
    let dir = cwd.trim_end_matches(['\\', '/']);
    if dir.len() < 3 {
        return 0;
    }
    let mut total = 0;
    for sep in ['\\', '/'] {
        let pat = format!("{}{sep}", dir.replace(['\\', '/'], &sep.to_string())).to_ascii_lowercase();
        let hay = text.to_ascii_lowercase(); // same byte offsets as `text`
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        while let Some(p) = hay[at..].find(&pat) {
            let p = at + p;
            // Mid-word or mid-path means a different (longer) path, not ours.
            let glued = text[..p]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || "_-./\\".contains(c));
            out.push_str(&text[at..p]);
            if glued {
                out.push_str(&text[p..p + pat.len()]);
            } else {
                total += 1;
            }
            at = p + pat.len();
        }
        out.push_str(&text[at..]);
        *text = out;
    }
    total
}

/// Save the uncut output (already redacted) so a truncation never loses facts:
/// the agent can grep the file. Keeps the newest 40; None on any I/O error.
pub fn spool(stdout: &str, stderr: &str) -> Option<std::path::PathBuf> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join("qx-spool");
    std::fs::create_dir_all(&dir).ok()?;
    let mut old: Vec<_> = std::fs::read_dir(&dir).ok()?.flatten().map(|e| e.path()).collect();
    old.sort();
    for p in &old[..old.len().saturating_sub(39)] {
        let _ = std::fs::remove_file(p);
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let path = dir.join(format!("{ms:015}-{}-{}.log", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let mut body = stdout.to_string();
    if !stderr.is_empty() {
        body.push_str("\n[stderr]\n");
        body.push_str(stderr);
    }
    std::fs::write(&path, body).ok()?;
    Some(path)
}

/// Cargo build-script chatter: the `cargo:rerun-if-*` directives and the `NAME = None` lines that follow
/// them print when a build script panics, dozens of lines saying nothing about why. Dropped only when
/// there are four or more, with a count; returns how many lines went.
pub fn drop_build_noise(text: &mut String) -> usize {
    let is_directive = |l: &str| {
        let l = l.trim_start();
        l.starts_with("cargo:rerun-if-") || l.starts_with("cargo::rerun-if-")
    };
    let is_env_value = |l: &str| {
        let Some((name, value)) = l.trim().split_once(" = ") else { return false };
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && (value == "None" || (value.starts_with("Some(") && value.ends_with(')')))
    };
    let lines: Vec<&str> = text.lines().collect();
    let directives = lines.iter().filter(|l| is_directive(l)).count();
    if directives < 4 {
        return 0;
    }
    let mut kept = String::with_capacity(text.len());
    let mut dropped = 0;
    let mut prev_dropped = false;
    for l in &lines {
        if is_directive(l) {
            dropped += 1;
            prev_dropped = true;
            continue;
        }
        if prev_dropped && is_env_value(l) {
            dropped += 1;
            prev_dropped = false;
            continue;
        }
        prev_dropped = false;
        kept.push_str(l);
        kept.push('\n');
    }
    kept.push_str(&format!("[qx] {dropped} cargo build-script lines omitted\n"));
    *text = kept;
    dropped
}

/// `relative_paths` on both streams of a digested build/test/traceback, with a
/// note saying what the paths are relative to. Under 3 hits the note costs more
/// than it saves, so nothing changes.
pub fn relative_to_cwd(so: &mut String, se: &mut String, cwd: Option<&std::path::Path>) {
    let Some(dir) = cwd
        .and_then(|d| std::path::absolute(d).ok())
        .or_else(|| std::env::current_dir().ok())
    else {
        return;
    };
    let dir = dir.to_string_lossy();
    let dir = dir.trim_start_matches(r"\\?\");
    let (mut a, mut b) = (so.clone(), se.clone());
    let (na, nb) = (relative_paths(&mut a, dir), relative_paths(&mut b, dir));
    if na + nb < 3 {
        return;
    }
    let t = if na > 0 { &mut a } else { &mut b };
    if !t.is_empty() && !t.ends_with('\n') {
        t.push('\n');
    }
    t.push_str(&format!("[qx] paths relative to {dir}\n"));
    (*so, *se) = (a, b);
}

/// A line redrawn with lone `\r` (a progress bar) shows only its last frame on a terminal. Keep that
/// frame and any earlier one that reports a problem, instead of one huge line whose length cap
/// keeps the first frames and cuts the final status. CRLF becomes LF on the way. The flag says a
/// frame was dropped, so callers spool the original (a file with lone-`\r` line ends is data).
pub fn settle_frames(text: &str) -> (std::borrow::Cow<'_, str>, bool) {
    if !text.contains('\r') {
        return (std::borrow::Cow::Borrowed(text), false);
    }
    let mut dropped = false;
    let mut out = String::with_capacity(text.len().min(1 << 20));
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !line.contains('\r') {
            out.push_str(line);
            continue;
        }
        let frames: Vec<&str> = line.split('\r').filter(|f| !f.trim().is_empty()).collect();
        let Some((last, earlier)) = frames.split_last() else { continue };
        let mut kept: Vec<&str> = earlier.iter().copied().filter(|f| interesting(f)).collect();
        kept.dedup();
        kept.push(last);
        dropped |= kept.len() < frames.len();
        out.push_str(&kept.join("\n"));
    }
    (std::borrow::Cow::Owned(out), dropped)
}

/// Keep the first `head` and last `tail` lines when output exceeds `max_lines`.
/// Diagnostic-looking lines from the omitted middle are rescued first.
/// Returns the compacted text and whether anything was omitted.
pub fn compact(text: &str, max_lines: usize, head: usize, tail: usize) -> (String, bool) {
    if text.is_empty() {
        return (String::new(), false);
    }
    let (normalized, dropped) = settle_frames(text);
    let trailing = normalized.ends_with('\n');
    let mut lines: Vec<&str> = normalized.split('\n').collect();
    if trailing {
        lines.pop();
    }

    let (lines, collapsed) = collapse(&lines);
    let (lines, similar) = if lines.len() > max_lines {
        collapse_similar(lines)
    } else {
        (lines, false)
    };

    if lines.len() <= max_lines {
        let mut out = String::with_capacity(normalized.len());
        let mut capped = false;
        for l in &lines {
            capped |= push_capped(&mut out, l);
        }
        if collapsed > 0 {
            out.push_str(&format!("[qx] collapsed {collapsed} repeated line(s)\n"));
        }
        return (out, similar || capped || dropped);
    }

    // `--max-lines 20` with the default 80/60 window: the window shrinks to the cap, in proportion,
    // instead of printing everything under a "0 lines omitted" marker.
    let window = head.saturating_add(tail);
    let (head, tail) = if window > max_lines {
        let h = (max_lines as u128 * head as u128 / window as u128) as usize;
        (h, max_lines - h)
    } else {
        (head, tail)
    };
    let h = head.min(lines.len());
    let remaining = lines.len() - h;
    let t = tail.min(remaining);
    let omitted = remaining - t;
    let middle = &lines[h..lines.len() - t];

    // Rescue diagnostics from the middle with their message/location lines, with a bounded budget so a
    // log that is nothing but "error" lines cannot defeat compaction.
    const BUDGET: usize = 40; // marker lines kept (first half, last half)
    const MAX_RESCUED: usize = 120; // hard cap on lines, context included
    let mut groups = rescue_groups(middle, 3);
    let mut rescued_elided = 0usize;
    if groups.len() > BUDGET {
        rescued_elided = groups.len() - BUDGET;
        groups.drain(BUDGET / 2..groups.len() - (BUDGET - BUDGET / 2));
    }
    if groups.iter().map(|g| g.len()).sum::<usize>() > MAX_RESCUED {
        for g in &mut groups {
            g.end = g.end.min(g.start + MAX_RESCUED / BUDGET);
        }
    }
    let rescued: usize = groups.iter().map(|g| g.len()).sum();

    let mut out = String::new();
    for l in &lines[..h] {
        push_capped(&mut out, l);
    }
    if rescued > 0 {
        out.push_str(&format!(
            "[qx] --- {rescued} important line(s) rescued from the omitted region ---\n"
        ));
        for g in &groups {
            for l in &middle[g.clone()] {
                push_capped(&mut out, l);
            }
        }
        if rescued_elided > 0 {
            out.push_str(&format!(
                "[qx] ... {rescued_elided} more important line(s) omitted ...\n"
            ));
        }
    }
    // Rescued lines are printed above, so they are not among the omitted.
    out.push_str(&format!(
        "[qx] ... {} lines omitted ({} total) ...\n",
        omitted - rescued,
        lines.len()
    ));
    for l in &lines[lines.len() - t..] {
        push_capped(&mut out, l);
    }
    if collapsed > 0 {
        out.push_str(&format!("[qx] collapsed {collapsed} repeated line(s)\n"));
    }
    (out, true)
}

/// Each diagnostic line in `middle` with up to `follow` plain lines under it: those carry the
/// `--> file:line:col` or the panic message that makes the marker line usable. A following marker
/// line starts its own group, so every group holds exactly one marker.
fn rescue_groups(middle: &[String], follow: usize) -> Vec<std::ops::Range<usize>> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < middle.len() {
        if !interesting(&middle[i]) {
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < middle.len() && end - i <= follow && !middle[end].trim().is_empty() && !interesting(&middle[end]) {
            end += 1;
        }
        groups.push(i..end);
        i = end;
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_frames_keep_the_last_frame_and_problems() {
        let mut bar: String = (0..3000).map(|i| format!("Downloading {i}/3000\r")).collect();
        bar.push_str("warning: retrying mirror\rDownloading 3000/3000\rDone: installed pkg-1.0\n");
        let (out, _) = compact(&format!("start\n{bar}end\n"), 200, 80, 60);
        assert!(out.contains("Done: installed pkg-1.0"), "{out}");
        assert!(out.contains("warning: retrying mirror"), "{out}");
        assert!(!out.contains("Downloading 1/3000"), "{out}");
        assert_eq!(out.lines().next(), Some("start"));
        assert_eq!(out.lines().last(), Some("end"));
    }

    #[test]
    fn dropped_frames_count_as_a_cut_and_crlf_does_not() {
        // Classic Mac line ends: every record but the last is a dropped frame, so it must spool.
        let (out, cut) = compact("rec 1\rrec 2\rrec 3\n", 200, 80, 60);
        assert_eq!(out, "rec 3\n");
        assert!(cut);
        let (out, cut) = compact("a\r\nb\r\n", 200, 80, 60);
        assert_eq!(out, "a\nb\n");
        assert!(!cut);
    }

    /// Filler that differs in letters, so head/tail is what gets tested.
    fn word(i: usize) -> String {
        format!("{}{}", (b'a' + (i / 26) as u8) as char, (b'a' + (i % 26) as u8) as char)
    }

    #[test]
    fn stray_escape_before_multibyte_text_does_not_panic() {
        let out = strip_ansi("a\x1bé b\x1b(é c");
        assert!(out.contains('é') && out.contains('b') && out.contains('c'));
    }

    #[test]
    fn progress_runs_collapse_but_numbered_errors_stay() {
        let mut s = String::new();
        for i in 0..300 {
            s.push_str(&format!("step {i}/300 ok\n"));
        }
        s.push_str("error: boom\n");
        let (out, cut) = compact(&s, 50, 10, 10);
        assert!(cut && out.contains("298 similar lines") && out.contains("error: boom"));
        assert!(out.contains("step 0/300") && out.contains("step 299/300"));
        assert!(out.lines().count() < 10, "{out}");
        let e: String = (0..60).map(|i| format!("error: item {i} failed\n")).collect();
        assert!(!compact(&e, 20, 5, 5).0.contains("similar lines"));
        // under budget nothing is touched
        let small: String = (0..12).map(|i| format!("n {i}\n")).collect();
        assert_eq!(compact(&small, 50, 10, 10).0, small);
    }

    #[test]
    fn rescued_errors_keep_their_location_and_message() {
        let mut s = String::new();
        for i in 0..200 {
            s.push_str(&format!("noise {}\n", word(i)));
        }
        s.push_str("error[E0308]: mismatched types\n  --> src/main.rs:42:18\n   |\n   | ^^^\n42 | let x: u32 = \"a\";\n\n");
        s.push_str("thread 'main' panicked at src/lib.rs:77:9:\nindex out of bounds: the len is 3\n");
        for i in 200..400 {
            s.push_str(&format!("noise {}\n", word(i)));
        }
        let (out, cut) = compact(&s, 100, 20, 20);
        assert!(cut && out.contains("src/main.rs:42:18") && out.contains("index out of bounds"), "{out}");
        assert!(!out.contains("42 | let x"), "context stops at 3 lines: {out}");
        // a log of nothing but multi-line errors still compacts under the hard cap
        let many: String = (0..300).map(|i| format!("error: e{i}\n  --> a.rs:{i}\n  | x\n  | y\n")).collect();
        let (out, _) = compact(&many, 100, 10, 10);
        assert!(out.lines().count() < 150 && out.contains("more important line(s) omitted"), "{}", out.lines().count());
    }

    #[test]
    fn spool_keeps_both_streams_whole() {
        let p = spool("out\n", "err\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "out\n\n[stderr]\nerr\n");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn relative_paths_shorten_only_paths_that_start_at_the_cwd() {
        let mut t = String::from(
            r"C:\Proj\src\a.rs:3, c:\proj\b.rs, é D:\x\C:\proj\no, C:/proj/c.rs, C:\ProjX\z",
        );
        assert_eq!(relative_paths(&mut t, r"C:\proj\"), 3);
        assert_eq!(t, r"src\a.rs:3, b.rs, é D:\x\C:\proj\no, c.rs, C:\ProjX\z");
        let mut root = String::from(r"C:\a");
        assert_eq!(relative_paths(&mut root, "C:\\"), 0);
    }

    #[test]
    fn ansi_is_removed() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
    }

    #[test]
    fn small_output_untouched() {
        let (s, cut) = compact("a\nb\n", 10, 3, 3);
        assert!(!cut);
        assert_eq!(s, "a\nb\n");
    }

    #[test]
    fn large_output_keeps_edges() {
        let text: String = (0..100).map(|i| format!("line-{}\n", word(i))).collect();
        let (s, cut) = compact(&text, 10, 3, 2);
        assert!(cut);
        assert!(s.contains("line-aa"));
        assert!(s.contains("line-dv"));
        assert!(s.contains("95 lines omitted"));
        assert!(!s.contains("line-by"));
    }

    #[test]
    fn a_cap_below_the_window_shrinks_the_window() {
        // `--max-lines 20` with the default 80/60 split: 20 lines kept, not all 50.
        let text: String = (0..50).map(|i| format!("line-{}\n", word(i))).collect();
        let (s, cut) = compact(&text, 20, 80, 60);
        assert!(cut);
        assert!(s.contains("line-aa\n") && s.contains("line-bx\n"), "{s}");
        assert!(s.contains("30 lines omitted (50 total)"), "{s}");
    }

    #[test]
    fn rescues_errors_from_the_middle() {
        let mut text = String::new();
        for i in 0..100 {
            if i == 50 {
                text.push_str("ERROR: database connection failed\n");
            } else {
                text.push_str(&format!("line-{}\n", word(i)));
            }
        }
        let (s, cut) = compact(&text, 10, 3, 2);
        assert!(cut);
        assert!(s.contains("ERROR: database connection failed"));
        assert!(s.contains("important line(s) rescued"));
    }

    #[test]
    fn collapses_repeated_lines() {
        let text: String = (0..50).map(|_| "same warning\n").collect();
        let (s, cut) = compact(&text, 200, 80, 60);
        assert!(!cut);
        assert!(s.contains("same warning  (x50)"));
        assert!(s.contains("collapsed 49 repeated line(s)"));
    }

    #[test]
    fn keeps_short_repeats() {
        let text = "a\na\nb\n";
        let (s, _) = compact(text, 10, 3, 3);
        assert_eq!(s, "a\na\nb\n");
    }

    #[test]
    fn rescue_budget_is_bounded() {
        let mut text = String::new();
        for i in 0..500 {
            text.push_str(&format!("error {i}\n"));
        }
        let (s, _) = compact(&text, 20, 5, 5);
        assert!(s.contains("more important line(s) omitted"));
        assert!(s.matches("error ").count() < 100);
    }

    #[test]
    fn a_capped_line_reports_the_cut() {
        let (out, cut) = compact(&"x".repeat(5000), 200, 80, 60);
        assert!(cut && out.contains("...[+1000 chars]"), "{out}");
        assert!(!compact("short\n", 200, 80, 60).1);
    }

    #[test]
    fn build_script_chatter_goes_but_the_panic_stays() {
        let mut s = String::new();
        for i in 0..6 {
            s.push_str(&format!("  cargo:rerun-if-env-changed=V{i}\n  V{i} = None\n"));
        }
        s.push_str("  Y = Some(\"x\")\nthread 'main' panicked at b.rs:1:1:\nUnable to find libclang\n");
        let n = drop_build_noise(&mut s);
        assert_eq!(n, 12);
        assert!(s.contains("Unable to find libclang") && s.contains("Y = Some"), "{s}");
        assert!(!s.contains("rerun-if") && s.contains("12 cargo build-script lines omitted"), "{s}");
        let mut few = "cargo:rerun-if-changed=a\nV = None\n".to_string();
        assert_eq!(drop_build_noise(&mut few), 0);
    }}
