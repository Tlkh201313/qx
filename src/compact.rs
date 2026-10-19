//! Output cleanup: ANSI stripping, duplicate collapsing, and error-aware compaction.
//!
//! Naive head+tail truncation can drop the one line that matters (a test failure
//! or a compiler error in the middle of a long log). `compact` rescues lines that
//! look diagnostic before it truncates.

/// Remove ANSI escape sequences (colors, cursor moves, OSC titles).
pub fn strip_ansi(s: &str) -> String {
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
            } else if i < b.len() {
                i += 1;
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

const MAX_LINE_CHARS: usize = 4000;

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

fn push_capped(out: &mut String, line: &str) {
    let n = line.chars().count();
    if n > MAX_LINE_CHARS {
        out.extend(line.chars().take(MAX_LINE_CHARS));
        out.push_str(&format!(" ...[+{} chars]", n - MAX_LINE_CHARS));
    } else {
        out.push_str(line);
    }
    out.push('\n');
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

/// Keep the first `head` and last `tail` lines when output exceeds `max_lines`.
/// Diagnostic-looking lines from the omitted middle are rescued first.
/// Returns the compacted text and whether anything was omitted.
pub fn compact(text: &str, max_lines: usize, head: usize, tail: usize) -> (String, bool) {
    if text.is_empty() {
        return (String::new(), false);
    }
    let normalized = text.replace("\r\n", "\n");
    let trailing = normalized.ends_with('\n');
    let mut lines: Vec<&str> = normalized.split('\n').collect();
    if trailing {
        lines.pop();
    }

    let (lines, collapsed) = collapse(&lines);

    if lines.len() <= max_lines {
        let mut out = String::with_capacity(normalized.len());
        for l in &lines {
            push_capped(&mut out, l);
        }
        if collapsed > 0 {
            out.push_str(&format!("[qx] collapsed {collapsed} repeated line(s)\n"));
        }
        return (out, false);
    }

    let h = head.min(lines.len());
    let remaining = lines.len() - h;
    let t = tail.min(remaining);
    let omitted = remaining - t;
    let middle = &lines[h..lines.len() - t];

    // Rescue diagnostics from the middle, with a bounded budget so a log that
    // is nothing but "error" lines cannot defeat compaction.
    const BUDGET: usize = 40;
    let mut rescued: Vec<&String> = middle.iter().filter(|l| interesting(l)).collect();
    let mut rescued_elided = 0usize;
    if rescued.len() > BUDGET {
        let keep_first = BUDGET / 2;
        let keep_last = BUDGET - keep_first;
        rescued_elided = rescued.len() - BUDGET;
        let mut picked: Vec<&String> = Vec::with_capacity(BUDGET);
        picked.extend(rescued.iter().take(keep_first).copied());
        picked.extend(rescued.iter().skip(rescued.len() - keep_last).copied());
        rescued = picked;
    }

    let mut out = String::new();
    for l in &lines[..h] {
        push_capped(&mut out, l);
    }
    if !rescued.is_empty() {
        out.push_str(&format!(
            "[qx] --- {} important line(s) rescued from the omitted region ---\n",
            rescued.len()
        ));
        for l in &rescued {
            push_capped(&mut out, l);
        }
        if rescued_elided > 0 {
            out.push_str(&format!(
                "[qx] ... {rescued_elided} more important line(s) omitted ...\n"
            ));
        }
    }
    out.push_str(&format!(
        "[qx] ... {} lines omitted ({} total) ...\n",
        omitted,
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let text: String = (0..100).map(|i| format!("line{i}\n")).collect();
        let (s, cut) = compact(&text, 10, 3, 2);
        assert!(cut);
        assert!(s.contains("line0"));
        assert!(s.contains("line99"));
        assert!(s.contains("95 lines omitted"));
        assert!(!s.contains("line50"));
    }

    #[test]
    fn rescues_errors_from_the_middle() {
        let mut text = String::new();
        for i in 0..100 {
            if i == 50 {
                text.push_str("ERROR: database connection failed\n");
            } else {
                text.push_str(&format!("line{i}\n"));
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
}
