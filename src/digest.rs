//! Command-aware digests: keep the lines the agent needs, drop the ceremony.
//!
//! This runs before generic compaction. It only fires for known output shapes
//! (git status/diff, tests, builds, grep, JSON) and returns `None` whenever the
//! shape is unclear or the saving is small, so the existing error-aware
//! compaction remains the safety net.

use crate::compact;

pub struct Digest {
    pub stdout: String,
    pub stderr: String,
    pub profile: &'static str,
    pub omitted: usize,
    /// True only when useful lines were capped, not when noise was dropped.
    pub clipped: bool,
}

pub fn digest(label: &str, stdout: &str, stderr: &str, code: i32) -> Option<Digest> {
    if std::env::var("QX_DIGEST").ok().as_deref() == Some("off") {
        return None;
    }
    let l = label.to_ascii_lowercase();
    let (profile, out, err, omitted, clipped) = if is_git_status(&l) {
        let (o, n) = git_status(stdout)?;
        ("git-status", o, keep_stderr(stderr), n, false)
    } else if is_diff(&l) {
        let (o, n, c) = git_diff(stdout)?;
        ("git-diff", o, keep_stderr(stderr), n, c)
    } else if is_test(&l) {
        let merged = merge(stdout, stderr);
        let (o, n, c) = diagnostic(&merged, code, false)?;
        ("test", o, String::new(), n, c)
    } else if is_build(&l) {
        let merged = merge(stdout, stderr);
        let (o, n, c) = diagnostic(&merged, code, true)?;
        ("build", o, String::new(), n, c)
    } else if is_runtime(&l) {
        let (o, e, n, c) = runtime_digest(stdout, stderr, code)?;
        ("traceback", o, e, n, c)
    } else if is_search(&l) {
        let (o, n, c) = grep(stdout)?;
        ("search", o, keep_stderr(stderr), n, c)
    } else if let Some((o, n)) = json_min(stdout) {
        ("json", o, keep_stderr(stderr), n, false)
    } else {
        return None;
    };

    let before = stdout.len() + stderr.len();
    let after = out.len() + err.len();
    if before == 0 || after == 0 || after.saturating_mul(10) >= before.saturating_mul(9) {
        return None;
    }
    Some(Digest {
        stdout: out,
        stderr: err,
        profile,
        omitted,
        clipped,
    })
}

fn merge(stdout: &str, stderr: &str) -> String {
    if stderr.trim().is_empty() {
        return stdout.to_string();
    }
    if stdout.trim().is_empty() {
        return stderr.to_string();
    }
    let mut s = String::with_capacity(stdout.len() + stderr.len() + 1);
    s.push_str(stdout);
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(stderr);
    s
}

fn keep_stderr(stderr: &str) -> String {
    if stderr.trim().is_empty() {
        return String::new();
    }
    let (s, _) = compact::compact(stderr, 120, 50, 40);
    s
}

fn words(l: &str) -> impl Iterator<Item = &str> {
    l.split_whitespace()
}

fn is_git_status(l: &str) -> bool {
    if !l.starts_with("git ") {
        return false;
    }
    let mut w = words(l);
    let _ = w.next();
    match w.next() {
        Some("status") => true,
        Some("-c") => w.any(|x| x == "status"),
        _ => false,
    }
}

fn is_diff(l: &str) -> bool {
    if l.contains(" --stat") || l.contains(" --numstat") || l.contains(" --shortstat") {
        return false;
    }
    if l.contains(" --name-only") || l.contains(" --name-status") {
        return false;
    }
    let mut w = words(l);
    if w.next() != Some("git") {
        return false;
    }
    match w.next() {
        Some("diff") | Some("show") => true,
        Some("stash") => w.next() == Some("show"),
        _ => false,
    }
}

fn is_test(l: &str) -> bool {
    if words(l).any(|w| matches!(w, "pytest" | "jest" | "vitest" | "nextest")) {
        return true;
    }
    if !words(l).any(|w| w == "test") {
        return false;
    }
    ["cargo ", "npm ", "pnpm ", "yarn ", "npx ", "go ", "dotnet ", "mvn ", "gradle ", "make ", "python ", "py "]
        .iter()
        .any(|p| l.starts_with(p))
}

fn is_build(l: &str) -> bool {
    const BUILDS: [&str; 18] = [
        "cargo build",
        "cargo check",
        "cargo clippy",
        "npm run build",
        "npm build",
        "pnpm build",
        "pnpm run build",
        "yarn build",
        "yarn run build",
        "tsc",
        "vite build",
        "next build",
        "dotnet build",
        "mvn package",
        "mvn install",
        "gradle build",
        "make",
        "cmake --build",
    ];
    BUILDS
        .iter()
        .any(|p| l == *p || l.starts_with(&format!("{p} ")))
}

fn is_runtime(l: &str) -> bool {
    let first = l.split_whitespace().next().unwrap_or("");
    matches!(
        first.trim_end_matches(".exe"),
        "python" | "python3" | "py" | "node" | "tsx"
    )
}

fn traceback(text: &str) -> Option<(String, usize, bool)> {
    let py = text.contains("Traceback (most recent call last):");
    let node = text.contains("\n    at ") || text.contains("\n  at ");
    if !py && !node {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    let mut kept = 0usize;
    let mut clipped = false;
    let mut in_py = false;
    let mut i = 0usize;
    while i < lines.len() {
        let t = lines[i].trim();
        let keep = if t.starts_with("Traceback (most recent call last):") {
            in_py = true;
            true
        } else if in_py {
            t.starts_with("File \"")
                || t.starts_with("File '")
                || lines[i].starts_with("    ")
                || t.starts_with("During handling")
                || t.starts_with("The above exception")
                || is_exception(t)
        } else if node {
            t.starts_with("Error")
                || t.contains("Error:")
                || t.starts_with("at ")
                || t.starts_with("node:internal")
                || t.starts_with("throw ")
                || t.starts_with("code:")
                || t.starts_with('^')
                || (i + 1 < lines.len() && lines[i + 1].trim().starts_with('^'))
                || t.contains("caused by")
        } else {
            false
        };
        if keep {
            if kept < 120 {
                out.push_str(lines[i]);
                out.push('\n');
                kept += 1;
            } else {
                clipped = true;
            }
        }
        i += 1;
    }
    if out.is_empty() {
        return None;
    }
    Some((out, lines.len().saturating_sub(kept), clipped))
}

fn is_exception(t: &str) -> bool {
    let head = t.split(':').next().unwrap_or("");
    !head.is_empty()
        && head
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        && (head.ends_with("Error")
            || head.ends_with("Exception")
            || head.ends_with("Interrupt")
            || head.ends_with("Exit"))
}

fn runtime_digest(
    stdout: &str,
    stderr: &str,
    code: i32,
) -> Option<(String, String, usize, bool)> {
    if code == 0 {
        return None;
    }
    if let Some((e, n, c)) = traceback(stderr) {
        let (o, _) = compact::compact(stdout, 20, 0, 20);
        return Some((o, e, n, c));
    }
    let (e, n, c) = traceback(stdout)?;
    Some((String::new(), e, n, c))
}

fn is_search(l: &str) -> bool {
    if l.starts_with("git grep") {
        return true;
    }
    ["grep", "rg", "findstr"].iter().any(|p| {
        l == *p || l.starts_with(&format!("{p} ")) || l.ends_with(&format!(" {p}"))
    })
}

fn status_path(t: &str) -> Option<String> {
    if let Some(p) = t.strip_prefix("?? ") {
        return Some(p.trim().to_string());
    }
    for k in [
        "both modified:",
        "both deleted:",
        "both added:",
        "modified:",
        "new file:",
        "deleted:",
        "renamed:",
        "copied:",
        "typechange:",
        "unmerged:",
    ] {
        if let Some(p) = t.strip_prefix(k) {
            return Some(p.trim().to_string());
        }
    }
    None
}

fn git_status(text: &str) -> Option<(String, usize)> {
    let mut branch: Option<String> = None;
    let mut state: Vec<String> = Vec::new();
    let mut sections: Vec<(&'static str, Vec<String>)> = Vec::new();
    let mut cur: Option<usize> = None;
    let mut omitted = 0usize;

    for raw in text.lines() {
        let t = raw.trim();
        if t.is_empty() {
            omitted += 1;
            continue;
        }
        if let Some(b) = t.strip_prefix("On branch ") {
            branch = Some(b.to_string());
            continue;
        }
        if t.starts_with("HEAD detached at ") {
            branch = Some(t.to_string());
            continue;
        }
        if let Some(b) = t.strip_prefix("Your branch is ") {
            state.push(b.trim_end_matches('.').to_string());
            continue;
        }
        let section = match t {
            "Changes to be committed:" => Some("staged"),
            "Changes not staged for commit:" => Some("modified"),
            "Untracked files:" => Some("untracked"),
            "Unmerged paths:" => Some("conflicted"),
            _ => None,
        };
        if let Some(name) = section {
            let idx = match sections.iter().position(|(n, _)| *n == name) {
                Some(i) => i,
                None => {
                    sections.push((name, Vec::new()));
                    sections.len() - 1
                }
            };
            cur = Some(idx);
            continue;
        }
        if t.starts_with("nothing to commit") {
            state.push("clean".to_string());
            continue;
        }
        if t.starts_with("no changes added to commit") {
            state.push("no changes added to commit".to_string());
            continue;
        }
        if t.starts_with("(use \"")
            || t.starts_with("(commit or discard")
            || t.starts_with("(fix conflicts")
        {
            omitted += 1;
            continue;
        }
        if let Some(i) = cur {
            if sections[i].0 == "untracked" {
                sections[i].1.push(t.to_string());
                continue;
            }
        }
        if let Some(path) = status_path(t) {
            let idx = match cur {
                Some(i) => i,
                None => {
                    sections.push(("changed", Vec::new()));
                    sections.len() - 1
                }
            };
            sections[idx].1.push(path);
            continue;
        }
        omitted += 1;
    }

    let mut out = String::new();
    if let Some(b) = branch {
        out.push_str("branch ");
        out.push_str(&b);
        out.push('\n');
    }
    for s in state {
        out.push_str(&s);
        out.push('\n');
    }
    for (name, files) in sections {
        if files.is_empty() {
            continue;
        }
        out.push_str(name);
        out.push_str(" (");
        out.push_str(&files.len().to_string());
        out.push_str("):\n");
        for f in files {
            out.push_str("  ");
            out.push_str(&f);
            out.push('\n');
        }
    }
    if out.is_empty() {
        return None;
    }
    Some((out, omitted))
}

fn git_diff(text: &str) -> Option<(String, usize, bool)> {
    let mut out = String::new();
    let mut omitted = 0usize;
    let mut clipped = false;
    let mut in_hunk = false;
    let mut changed = 0usize;
    let mut kept = 0usize;

    for line in text.lines() {
        if line.starts_with("diff --git ") {
            out.push_str(line);
            out.push('\n');
            in_hunk = false;
            continue;
        }
        if line.starts_with("@@") {
            if clipped {
                omitted += 1;
                continue;
            }
            out.push_str(line);
            out.push('\n');
            in_hunk = true;
            continue;
        }
        if line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
        {
            omitted += 1;
            continue;
        }
        if in_hunk {
            if line.starts_with('+') || line.starts_with('-') {
                changed += 1;
                if kept >= 1200 {
                    clipped = true;
                    omitted += 1;
                    continue;
                }
                out.push_str(line);
                out.push('\n');
                kept += 1;
                continue;
            }
            if line.starts_with('\\') {
                out.push_str(line);
                out.push('\n');
                continue;
            }
            omitted += 1; // unchanged context
            continue;
        }
        let header = line.starts_with("commit ")
            || line.starts_with("Author:")
            || line.starts_with("Date:")
            || line.starts_with("Merge:")
            || line.starts_with("    ")
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("similarity index")
            || line.starts_with("rename from")
            || line.starts_with("rename to")
            || line.starts_with("Binary files ")
            || line.starts_with("GIT binary patch");
        if header && !clipped {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        omitted += 1;
    }

    if changed == 0 {
        return None;
    }
    if clipped {
        out.push_str(&format!(
            "[qx] {omitted} diff lines omitted; use --raw for full\n"
        ));
    }
    Some((out, omitted, clipped))
}

fn fail_line(t: &str, build: bool) -> bool {
    if build {
        return t.starts_with("error")
            || t.starts_with("Error:")
            || t.starts_with("npm ERR!")
            || t.contains("error TS")
            || t.starts_with("FAILED")
            || t.starts_with("FAIL ")
            || t.starts_with("Build failed")
            || t.starts_with("Failed to compile")
            || t.starts_with("The following tests FAILED");
    }
    t.starts_with("FAILED")
        || t.starts_with("FAIL ")
        || t.starts_with("FAIL:")
        || t.starts_with("---- ")
        || t.starts_with("● ")
        || t.starts_with('✕')
        || t.starts_with('×')
        || t.starts_with("--- FAIL:")
        || t.starts_with("panic:")
        || t.starts_with("thread '")
        || t.contains("panicked at")
        || t.starts_with("AssertionError")
        || t.starts_with("assertion")
        || t.contains("assertion failed")
        || t.starts_with("E   ")
        || t.contains("Error:")
        || t.starts_with("error:")
        || t.starts_with("error[")
        || t.starts_with("The following tests FAILED")
        || t.starts_with("test failed")
}

fn summary_line(t: &str) -> bool {
    t.starts_with("test result:")
        || t.starts_with("Tests:")
        || t.starts_with("Test Suites:")
        || t.starts_with("Snapshots:")
        || t.starts_with("=== ")
        || t.starts_with("failures:")
        || t.starts_with("error: test failed")
        || t.starts_with("Finished ")
        || t.starts_with("Compiled ")
        || t.starts_with("built in")
        || t.starts_with("Build complete")
        || t.starts_with("Build succeeded")
}

fn diagnostic(text: &str, code: i32, build: bool) -> Option<(String, usize, bool)> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let mut keep = vec![false; lines.len()];
    let mut hits = 0usize;
    let mut i = 0usize;

    while i < lines.len() {
        let t = lines[i].trim();
        if fail_line(t, build) {
            hits += 1;
            keep[i] = true;
            let mut j = i + 1;
            let mut n = 0usize;
            while j < lines.len() && n < 18 {
                let u = lines[j].trim();
                if u.is_empty() {
                    break;
                }
                if fail_line(u, build) && j > i + 1 {
                    break;
                }
                keep[j] = true;
                j += 1;
                n += 1;
            }
            i = j;
            continue;
        }
        if summary_line(t) || t.starts_with("warning:") || t.starts_with("--> ") {
            keep[i] = true;
        }
        i += 1;
    }

    if hits == 0 && code != 0 {
        // Unknown failure shape: let generic compaction rescue it instead.
        return None;
    }

    let indices: Vec<usize> = (0..lines.len()).filter(|&i| keep[i]).collect();
    if indices.is_empty() {
        return None;
    }

    let mut out = String::new();
    let mut kept = 0usize;
    let mut clipped = false;
    for (pos, &idx) in indices.iter().enumerate() {
        if pos >= 240 && pos + 20 < indices.len() {
            clipped = true;
            continue;
        }
        out.push_str(lines[idx]);
        out.push('\n');
        kept += 1;
    }
    if clipped {
        out.push_str(&format!(
            "[qx] {} diagnostic lines omitted; use --raw for full\n",
            lines.len().saturating_sub(kept)
        ));
    }
    Some((out, lines.len().saturating_sub(kept), clipped))
}

/// Parse `path:line:content` from the left; Windows drive colons are skipped
/// because the segment after them is not numeric.
fn split_match(line: &str) -> Option<(&str, &str, &str)> {
    let b = line.as_bytes();
    for i in 0..b.len() {
        if b[i] != b':' || i == 0 {
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == start || j >= b.len() || b[j] != b':' {
            continue;
        }
        let content = &line[j + 1..];
        if content.is_empty() {
            continue;
        }
        return Some((&line[..i], &line[start..j], content));
    }
    None
}

fn grep(text: &str) -> Option<(String, usize, bool)> {
    let mut groups: Vec<(String, Vec<(String, String)>)> = Vec::new();
    let mut total = 0usize;
    let mut parsed = 0usize;
    let mut clipped = false;

    for line in text.lines() {
        total += 1;
        let Some((path, num, content)) = split_match(line) else {
            continue;
        };
        parsed += 1;
        if groups.iter().any(|(_, items)| items.len() >= 25) {
            clipped = true;
            continue;
        }
        if groups.iter().map(|(_, v)| v.len()).sum::<usize>() >= 300 {
            clipped = true;
            continue;
        }
        let content = content.trim_end();
        let content = if content.len() > 500 {
            format!("{}…", &content[..500])
        } else {
            content.to_string()
        };
        match groups.iter_mut().find(|(p, _)| p == path) {
            Some((_, items)) => items.push((num.to_string(), content)),
            None => groups.push((path.to_string(), vec![(num.to_string(), content)])),
        }
    }

    // Require the output to be mostly grep-shaped, or leave it alone.
    if parsed < 3 || parsed * 4 < total * 3 {
        return None;
    }

    let mut out = String::new();
    let mut kept = 0usize;
    for (path, items) in &groups {
        out.push_str(path);
        out.push_str(" (");
        out.push_str(&items.len().to_string());
        out.push_str(")\n");
        for (num, content) in items {
            out.push_str("  ");
            out.push_str(num);
            out.push_str(": ");
            out.push_str(content);
            out.push('\n');
            kept += 1;
        }
    }
    if clipped {
        out.push_str("[qx] match cap reached; use --raw for full\n");
    }
    Some((out, parsed.saturating_sub(kept), clipped))
}

fn json_min(text: &str) -> Option<(String, usize)> {
    let t = text.trim();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    // Validate, then strip whitespace only: numbers and key order stay byte-exact.
    serde_json::from_str::<serde_json::Value>(t).ok()?;
    let mut out = String::with_capacity(t.len());
    let mut in_str = false;
    let mut esc = false;
    for ch in t.chars() {
        if in_str {
            out.push(ch);
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_str = false;
            }
        } else if ch == '"' {
            in_str = true;
            out.push(ch);
        } else if !ch.is_whitespace() {
            out.push(ch);
        }
    }
    if out.len() + 1 >= t.len() {
        return None;
    }
    Some((format!("{out}\n"), 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_drops_hints_keeps_files() {
        let raw = "On branch main\nYour branch is up to date with 'origin/main'.\n\nChanges not staged for commit:\n  (use \"git add <file>...\" to update what will be committed)\n\tmodified:   src/main.rs\n\nUntracked files:\n  (use \"git add <file>...\" to include in what will be committed)\n\tnew.txt\n\nno changes added to commit (use \"git add\" and/or \"git commit -a\")\n";
        let (out, omitted) = git_status(raw).unwrap();
        assert!(out.contains("branch main"));
        assert!(out.contains("src/main.rs"));
        assert!(out.contains("new.txt"));
        assert!(!out.contains("use \"git add"));
        assert!(omitted > 0);
    }

    #[test]
    fn diff_drops_context_keeps_changes() {
        let raw = "diff --git a/src/a.rs b/src/a.rs\nindex 111..222 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,3 @@\n fn keep() {}\n-old\n+new\n tail\n";
        let (out, _, clipped) = git_diff(raw).unwrap();
        assert!(out.contains("@@ -1,3 +1,3 @@"));
        assert!(out.contains("-old"));
        assert!(out.contains("+new"));
        assert!(!out.contains(" fn keep"));
        assert!(!clipped);
    }

    #[test]
    fn test_pass_keeps_only_summary() {
        let raw = "running 43 tests\n....................................\ntest result: ok. 43 passed; 0 failed; 0 ignored\n";
        let (out, _, _) = diagnostic(raw, 0, false).unwrap();
        assert_eq!(out.trim(), "test result: ok. 43 passed; 0 failed; 0 ignored");
    }

    #[test]
    fn test_fail_keeps_failure_block() {
        let raw = "test a ... ok\ntest b ... FAILED\n\nfailures:\n\n---- b stdout ----\nthread 'b' panicked at src/lib.rs:9:\nassertion failed\n\ntest result: FAILED. 1 passed; 1 failed\n";
        let (out, _, _) = diagnostic(raw, 1, false).unwrap();
        assert!(out.contains("---- b stdout ----"));
        assert!(out.contains("assertion failed"));
        assert!(out.contains("test result: FAILED"));
        assert!(!out.contains("test a ... ok"));
    }

    #[test]
    fn python_traceback_drops_noise() {
        let raw = "warning: something\nTraceback (most recent call last):\n  File \"a.py\", line 3, in <module>\n    boom()\nValueError: bad\n";
        let (out, _, _) = traceback(raw).unwrap();
        assert!(out.contains("Traceback"));
        assert!(out.contains("ValueError: bad"));
        assert!(!out.contains("warning:"));
    }

    #[test]
    fn node_stack_drops_noise() {
        let raw = "log line\nTypeError: x is not a function\n    at foo (a.js:1:2)\n    at bar (b.js:3:4)\n";
        let (out, _, _) = traceback(raw).unwrap();
        assert!(out.contains("TypeError"));
        assert!(out.contains("at foo"));
        assert!(!out.contains("log line"));
    }

    #[test]
    fn json_is_minified() {
        let raw = "{\n  \"a\": 1,\n  \"b\": [1, 2]\n}\n";
        let (out, _) = json_min(raw).unwrap();
        assert_eq!(out.trim(), "{\"a\":1,\"b\":[1,2]}");
    }

    #[test]
    fn grep_groups_by_file() {
        let raw = "src/a.rs:12:let x = 1;\nsrc/a.rs:40:let y = 2;\nsrc/b.rs:7:fn z() {}\n";
        let (out, _, _) = grep(raw).unwrap();
        assert_eq!(out.matches("src/a.rs").count(), 1);
        assert!(out.contains("12: let x = 1;"));
        assert!(out.contains("7: fn z() {}"));
    }

    #[test]
    fn windows_paths_parse() {
        let (p, n, c) = split_match(r"C:\src\a.rs:12:hello: world").unwrap();
        assert_eq!(p, r"C:\src\a.rs");
        assert_eq!(n, "12");
        assert_eq!(c, "hello: world");
    }
}
