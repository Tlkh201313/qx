//! Command-aware digests: keep the lines the agent needs, drop the ceremony.
//!
//! This runs before generic compaction. It only fires for known output shapes
//! (git status/diff, tests, builds, grep, JSON) and returns `None` whenever the
//! shape is unclear or the saving is small, so the existing error-aware
//! compaction remains the safety net.

use crate::compact;

/// How to get the uncut text: the CLI has `--raw`, an MCP call has `mode:"full"`.
const FULL: &str = "--raw (CLI) or mode:\"full\" (MCP)";

pub struct Digest {
    pub stdout: String,
    pub stderr: String,
    pub profile: &'static str,
    pub omitted: usize,
    /// True only when useful lines were capped, not when noise was dropped.
    pub clipped: bool,
    /// The command exited 0.
    pub passed: bool,
}

impl Digest {
    /// Whether the full text is worth keeping on disk: useful lines were capped, or lines were left
    /// out of a profile that does more than drop hints (`git status`) or regroup (`paths`). A green
    /// test or build run only loses its `... ok` / `Compiling` lines, which are noise, so it is spooled
    /// only when clipped.
    pub fn lossy(&self) -> bool {
        self.clipped
            || (self.omitted > 0
                && !matches!(self.profile, "git-status" | "paths")
                && !(self.passed && matches!(self.profile, "test" | "build")))
    }
}

pub fn digest(label: &str, stdout: &str, stderr: &str, code: i32) -> Option<Digest> {
    if std::env::var("QX_DIGEST").ok().as_deref() == Some("off") {
        return None;
    }
    let l = core_label(&label.to_ascii_lowercase());
    let (profile, out, err, omitted, clipped) = if is_git_status(&l) {
        let (o, n) = git_status(stdout)?;
        ("git-status", o, keep_stderr(stderr), n, false)
    } else if is_git_log(&l) {
        let (o, n) = git_log(stdout)?;
        ("git-log", o, keep_stderr(stderr), n, false)
    } else if is_diff(&l) {
        let (o, n, c) = git_diff(stdout)?;
        ("git-diff", o, keep_stderr(stderr), n, c)
    } else if is_test(&l) {
        if verbatim(&l, code) {
            return None;
        }
        let merged = merge(stdout, stderr);
        let (o, n, c) = diagnostic(&merged, code, false)?;
        ("test", o, String::new(), n, c)
    } else if is_build(&l) {
        if verbatim(&l, code) {
            return None;
        }
        let merged = merge(stdout, stderr);
        let (o, n, c) = diagnostic(&merged, code, true)?;
        ("build", o, String::new(), n, c)
    } else if is_runtime(&l) {
        let (o, e, n, c) = runtime_digest(stdout, stderr, code)?;
        ("traceback", o, e, n, c)
    } else if is_pathlist(&l) {
        let (o, n) = paths(stdout)?;
        ("paths", o, keep_stderr(stderr), n, false)
    } else if is_search(&l) {
        let (o, n, c) = grep(stdout)?;
        ("search", o, keep_stderr(stderr), n, c)
    } else if let Some((o, n)) = json_min(&l, stdout) {
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
        passed: code == 0,
    })
}

/// The caller asked for the output itself: `--nocapture` and `-s` print what the tests print, and
/// `--json` output is for a parser. A passing run's summary-only digest would drop all of it.
fn verbatim(l: &str, code: i32) -> bool {
    let machine = |w: &str| w.starts_with('-') && ["json", "junit", "tap", "xml"].iter().any(|k| w.contains(k));
    words(l).any(machine)
        || (code == 0
            && words(l).any(|w| {
                matches!(w, "--nocapture" | "-s" | "--show-output" | "-v" | "-vv" | "-vvv" | "--verbose" | "--capture=no")
            }))
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

/// The command a label is really about: `cd app && CI=1 npx tsc 2>&1 | tail` -> `tsc`.
/// The one `&&`/`;`/`||` segment that is not setup (`cd`, `echo`, ...), the part of it before any
/// pipe, minus env words, `time` and runner wrappers. Empty when the output is not one command's:
/// two real commands share it, or a pipe stage other than head/tail/tee/cat/more/less rewrites it
/// (`cargo test | grep x` shows grep's lines, not a test run).
fn core_label(l: &str) -> String {
    let mut segs: Vec<Vec<String>> = vec![vec![String::new()]];
    let (mut q, mut prev) = (None, ' ');
    for c in l.replace("2>&1", "").chars() {
        let sep = q.is_none() && matches!(c, '&' | ';' | '\n' | '|');
        match q {
            Some(e) if c == e => q = None,
            None if c == '"' || c == '\'' => q = Some(c),
            _ => {}
        }
        if sep && c == '|' && prev != '|' {
            segs.last_mut().unwrap().push(String::new()); // next stage of the pipeline
        } else if sep {
            segs.push(vec![String::new()]);
        } else {
            segs.last_mut().unwrap().last_mut().unwrap().push(c);
        }
        prev = c;
    }
    const SETUP: &[&str] = &["cd", "pushd", "popd", "echo", "set", "export", "source", "mkdir", "rem", "true"];
    let first = |s: &str| strip_wrappers(s);
    let real: Vec<&Vec<String>> = segs
        .iter()
        .filter(|st| {
            let c = first(&st[0]);
            !c.is_empty() && !SETUP.contains(&c.split(' ').next().unwrap_or(""))
        })
        .collect();
    match real.as_slice() {
        [] => segs
            .iter()
            .rev()
            .map(|st| first(&st[0]))
            .find(|c| !c.is_empty())
            .unwrap_or_else(|| l.to_string()),
        [one] => {
            let filters_only = one[1..].iter().all(|s| {
                let c = first(s);
                matches!(c.split(' ').next().unwrap_or(""), "" | "head" | "tail" | "tee" | "cat" | "more" | "less")
            });
            if filters_only { first(&one[0]) } else { String::new() }
        }
        _ => String::new(),
    }
}

fn strip_wrappers(seg: &str) -> String {
    let mut w: Vec<&str> = seg.split_whitespace().collect();
    let mut wrapped = false;
    loop {
        let n = match w.as_slice() {
            [a, ..] if wrapped && a.starts_with('-') => 1,
            [a, ..] if a.split_once('=').is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_alphanumeric() || c == '_')) => 1,
            ["time" | "env" | "npx" | "bunx" | "nohup", ..] => 1,
            ["pnpm" | "yarn" | "npm", "exec" | "dlx", ..] | ["uv" | "poetry" | "pipenv", "run", ..] => 2,
            _ => break,
        };
        if n >= w.len() {
            break;
        }
        wrapped |= !w[0].contains('=') && !w[0].starts_with('-');
        w.drain(..n);
    }
    w.join(" ")
}

/// Git subcommand of a lowercased label, skipping `-c`/`-C <value>`.
fn git_sub(l: &str) -> Option<&str> {
    let mut w = words(l);
    if w.next() != Some("git") {
        return None;
    }
    while let Some(x) = w.next() {
        if x == "-c" {
            w.next();
        } else if !x.starts_with('-') {
            return Some(x);
        }
    }
    None
}

/// Long-form status only: `-s`, `--short` and `--porcelain` print ` M file` lines the digest has no section for.
fn is_git_status(l: &str) -> bool {
    git_sub(l) == Some("status")
        && !words(l).any(|w| {
            w.starts_with("--short") || w.starts_with("--porcelain") || (!w.starts_with("--") && w.starts_with('-') && w.contains('s'))
        })
}

/// Default `git log` only: any custom format or patch flag is left alone.
fn is_git_log(l: &str) -> bool {
    git_sub(l) == Some("log")
        && !words(l).skip_while(|w| *w != "log").any(|w| {
            matches!(
                w,
                "-p" | "--patch" | "--graph" | "--oneline" | "--stat" | "--numstat" | "--shortstat"
                    | "--name-only" | "--name-status" | "--raw" | "-u" | "-c" | "--cc" | "-m" | "--summary" | "--notes"
                    | "--show-signature"
            ) || w.starts_with("--pretty")
                || w.starts_with("--format")
                || w.starts_with("--date")
                || w.starts_with("-l") // -L<range>:<file> (the label is lowercased)
        })
}

struct Commit {
    hash: String,
    deco: String,
    who: String,
    when: String,
    subject: String,
    body: usize,
}

/// `commit <sha>/Author/Date/message` -> `sha7 subject (who, date) (refs) [+N body lines]`.
fn git_log(text: &str) -> Option<(String, usize)> {
    let mut commits: Vec<Commit> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("commit ") {
            commits.push(Commit {
                hash: rest.split_whitespace().next().unwrap_or("").chars().take(7).collect(),
                deco: rest.find('(').map(|i| rest[i..].to_string()).unwrap_or_default(),
                who: String::new(),
                when: String::new(),
                subject: String::new(),
                body: 0,
            });
            continue;
        }
        let c = commits.last_mut()?; // text before the first commit: not default log shape
        if let Some(a) = line.strip_prefix("Author:") {
            c.who = a.split('<').next().unwrap_or("").trim().to_string();
        } else if let Some(d) = line.strip_prefix("Date:") {
            // "Tue Oct 14 12:00:00 2026 +0000" -> "Oct 14 2026"
            let p: Vec<&str> = d.split_whitespace().collect();
            if p.len() >= 5 {
                c.when = format!("{} {} {}", p[1], p[2], p[4]);
            }
        } else if let Some(t) = line.strip_prefix("    ") {
            if c.subject.is_empty() {
                c.subject = t.trim().to_string();
            } else if !t.trim().is_empty() {
                c.body += 1;
            }
        }
    }
    if commits.is_empty() || commits.iter().any(|c| c.hash.is_empty() || c.subject.is_empty()) {
        return None;
    }
    let mut out = String::new();
    for c in &commits {
        out.push_str(&format!("{} {} ({}, {})", c.hash, c.subject, c.who, c.when));
        if !c.deco.is_empty() {
            out.push(' ');
            out.push_str(&c.deco);
        }
        if c.body > 0 {
            out.push_str(&format!(" [+{} body lines]", c.body));
        }
        out.push('\n');
    }
    out.push_str(&format!("[qx] git log condensed (7-char hashes, bodies dropped); full text: {FULL}\n"));
    let omitted = text.lines().count().saturating_sub(commits.len());
    Some((out, omitted))
}

fn is_diff(l: &str) -> bool {
    if l.contains(" --stat") || l.contains(" --numstat") || l.contains(" --shortstat") {
        return false;
    }
    if l.contains(" --name-only") || l.contains(" --name-status") {
        return false;
    }
    match git_sub(l) {
        Some("diff") | Some("show") => true,
        Some("stash") => l.contains(" stash show"),
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
    const BUILDS: &[&str] = &[
        "cargo build",
        "cargo check",
        "cargo clippy",
        "cargo install",
        "go build",
        "go vet",
        "dotnet publish",
        "dotnet restore",
        "msbuild",
        // installs: progress noise, the summary and errors are what matters
        "npm install",
        "npm i",
        "npm ci",
        "npm add",
        "pnpm install",
        "pnpm i",
        "pnpm add",
        "yarn install",
        "yarn add",
        "pip install",
        "pip3 install",
        "python -m pip install",
        "uv pip install",
        "uv sync",
        "poetry install",
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
    // unittest frames its own failures (`FAIL: test_x`, `Ran 3 tests`); keep that whole.
    if py && lines.iter().any(|l| l.starts_with("Ran ") && l.contains(" test")) {
        return None;
    }
    let mut out = String::new();
    let mut kept = 0usize;
    let mut clipped = false;
    let mut in_py = false;
    let mut frames = false; // a frame was seen, so the next unindented line is the exception
    let mut msg_left = 0usize; // continuation lines of the exception message still to keep
    let mut i = 0usize;
    while i < lines.len() {
        let t = lines[i].trim();
        let indented = lines[i].starts_with(char::is_whitespace);
        let keep = if t.starts_with("Traceback (most recent call last):") {
            in_py = true;
            frames = false;
            msg_left = 0;
            true
        } else if in_py {
            let frame = t.starts_with("File \"") || t.starts_with("File '");
            let link = t.starts_with("During handling") || t.starts_with("The above exception");
            frames |= frame;
            if frame || indented || link {
                msg_left = 0;
                true
            } else if is_exception(t) || (frames && !t.is_empty()) {
                // The exception line, whatever its class is called.
                frames = false;
                msg_left = 8;
                true
            } else if msg_left > 0 && !t.is_empty() {
                msg_left -= 1;
                true
            } else {
                msg_left = 0;
                false
            }
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
        // A cut of the program's own output counts as clipped, so the caller spools it.
        let (o, cut) = compact::compact(stdout, 20, 0, 20);
        return Some((o, e, n, c || cut));
    }
    let (e, n, c) = traceback(stdout)?;
    Some((String::new(), e, n, c))
}

/// Commands whose whole output is one path per line.
fn is_pathlist(l: &str) -> bool {
    let mut w = words(l);
    match (w.next(), w.next()) {
        (Some("find" | "fd" | "fdfind"), _) => !words(l).any(|x| x.starts_with("-exec") || x == "-ls"),
        (Some("git"), Some("ls-files")) => true,
        (Some("rg" | "grep"), _) => {
            words(l).any(|x| matches!(x, "--files" | "-l" | "--files-with-matches" | "-rl" | "-lr" | "-Rl"))
        }
        (Some("dir"), _) => words(l).any(|x| x == "/b"),
        _ => false,
    }
}

/// Lossless: consecutive paths in one directory become `dir/{a,b,c}`. Names that
/// hold `,{}` (or end a directory path) stay as plain lines.
fn paths(text: &str) -> Option<(String, usize)> {
    let sep = |c: char| c == '/' || c == '\\';
    let lines: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.is_empty()).collect();
    if lines.len() < 12 || lines.iter().filter(|l| l.contains(sep)).count() * 10 < lines.len() * 9 {
        return None;
    }
    let cut = |l: &str| l.rfind(sep).map_or(0, |k| k + 1);
    let mut out = String::new();
    let mut shown = 0usize;
    let mut i = 0;
    while i < lines.len() {
        let dir = &lines[i][..cut(lines[i])];
        let mut j = i + 1;
        while j < lines.len() && lines[j].starts_with(dir) && cut(lines[j]) == dir.len() {
            j += 1;
        }
        let names: Vec<&str> = lines[i..j].iter().map(|l| &l[dir.len()..]).collect();
        if names.len() < 2 || dir.contains([',', '{', '}']) || names.iter().any(|n| n.is_empty() || n.contains([',', '{', '}'])) {
            for l in &lines[i..j] {
                out.push_str(l);
                out.push('\n');
                shown += 1;
            }
        } else {
            for chunk in names.chunks(60) {
                out.push_str(&format!("{dir}{{{}}}\n", chunk.join(",")));
                shown += 1;
            }
        }
        i = j;
    }
    Some((out, lines.len().saturating_sub(shown)))
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
        if t.starts_with("nothing added to commit") {
            state.push("nothing added to commit (untracked files present)".to_string());
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
        // A line this digest does not know (`deleted by us:`, ` M file`, a rebase notice) is not ours to drop.
        return None;
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
    // A merge-conflict diff marks each side with a column of its own; this reader knows one.
    if text.starts_with("diff --cc") || text.contains("\ndiff --cc ") || text.contains("\n@@@ ") || text.starts_with("@@@ ") {
        return None;
    }
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
        // Inside a hunk `--- x` is the removed line `-- x`, not a file header.
        if in_hunk && !matches!(line.as_bytes().first(), None | Some(b' ' | b'+' | b'-' | b'\\')) {
            in_hunk = false; // `commit ...` of the next `git show` entry
        }
        if !in_hunk
            && (line.starts_with("index ") || line.starts_with("--- ") || line.starts_with("+++ "))
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
            "[qx] {omitted} diff lines omitted; full text: {FULL}\n"
        ));
    }
    Some((out, omitted, clipped))
}

fn fail_line(t: &str, build: bool) -> bool {
    if build {
        return t.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("error"))
            || t.contains(": error")
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

/// `test_x.py:14: ` / `pkg/mod.py:13: ZeroDivisionError` (unindented, so the
/// indented warnings-summary paths do not match).
fn pytest_frame(t: &str) -> bool {
    if t.starts_with(char::is_whitespace) {
        return false;
    }
    let Some(p) = t.find(".py:") else {
        return false;
    };
    let rest = &t[p + 4..];
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest.as_bytes().get(digits) == Some(&b':')
}

/// `____ test_name ____` heading that opens each failure block.
fn pytest_heading(t: &str) -> bool {
    t.starts_with("___") && t.ends_with("___") && t.trim_matches('_').trim().len() > 1
}

/// `go test -v` per-test chatter (`=== RUN   TestX`), not a pytest/jest banner.
fn go_run_marker(t: &str) -> bool {
    ["=== RUN", "=== PAUSE", "=== CONT", "=== NAME"].iter().any(|p| t.starts_with(p))
}

/// `go test -v` logs a failing test's messages between its `=== RUN` and `--- FAIL:`
/// lines, so they sit before the failure line the forward scan starts from.
fn go_detail(lines: &[&str], at: usize, header: &str, keep: &mut [bool]) {
    let name = header.split_whitespace().next().unwrap_or("");
    let Some(run) = (0..at).rev().take(400).find(|&k| {
        lines[k].trim().strip_prefix("=== RUN").is_some_and(|n| n.trim() == name)
    }) else {
        return;
    };
    let msgs = (run + 1..at).filter(|&k| {
        let t = lines[k].trim();
        !t.is_empty() && !t.starts_with("=== ") && !t.starts_with("--- ")
    });
    for k in msgs.take(18) {
        keep[k] = true;
    }
}

fn summary_line(t: &str) -> bool {
    t.starts_with("test result:")
        || t.starts_with("Tests:")
        || t.starts_with("Test Suites:")
        || t.starts_with("Snapshots:")
        || (t.starts_with("=== ") && !go_run_marker(t))
        // pytest's closing `===== 1 failed, 2 passed in 0.4s =====`
        || (t.starts_with("===") && ["passed", "failed", "error", "skipped", "no tests ran"].iter().any(|k| t.contains(k)))
        // vitest: `Test Files  1 failed | 2 passed (3)` / `     Tests  1 failed | 4 passed (5)`
        || t.starts_with("Test Files ")
        || t.strip_prefix("Tests ").is_some_and(|r| r.trim_start().starts_with(|c: char| c.is_ascii_digit()))
        || t.starts_with("ok  ")
        || t.starts_with("failures:")
        || t.starts_with("error: test failed")
        || t.starts_with("Finished ")
        || t.starts_with("Compiled ")
        || t.starts_with("built in")
        || t.starts_with("Build complete")
        || t.starts_with("Build succeeded")
        || t.starts_with("added ")
        || t.starts_with("removed ")
        || t.starts_with("changed ")
        || t.starts_with("up to date")
        || t.starts_with("Successfully installed")
        || t.starts_with("audited ")
        || t.starts_with("Installed ")
        || t.starts_with("Done in")
        || t.starts_with("Packages:")
        || t.contains("vulnerabilit")
}

fn diagnostic(text: &str, code: i32, build: bool) -> Option<(String, usize, bool)> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let mut keep = vec![false; lines.len()];
    let mut hits = 0usize;
    let mut i = 0usize;
    let mut deprecated: Vec<&str> = Vec::new();

    while i < lines.len() {
        let t = lines[i].trim();
        if fail_line(t, build) {
            hits += 1;
            keep[i] = true;
            if let Some(h) = t.strip_prefix("--- FAIL: ") {
                go_detail(&lines, i, h, &mut keep);
            }
            let mut j = i + 1;
            let mut n = 0usize;
            // Jest and vitest put a blank line between the message, Expected/Received, the code frame
            // and the location; test blocks step over a single blank, build blocks end at it.
            let cap = if build { 18 } else { 30 };
            while j < lines.len() && n < cap {
                let u = lines[j].trim();
                if u.is_empty() {
                    if build || lines.get(j + 1).is_none_or(|v| v.trim().is_empty()) {
                        break;
                    }
                    j += 1;
                    continue;
                }
                if (fail_line(u, build) && j > i + 1) || (!build && (summary_line(u) || u.starts_with("⎯⎯"))) {
                    break;
                }
                keep[j] = true;
                j += 1;
                n += 1;
            }
            i = j;
            continue;
        }
        if let Some(pkg) = t.strip_prefix("npm warn deprecated ") {
            // The package is the signal; npm's reason text is the noise.
            deprecated.push(pkg.split(": ").next().unwrap_or(pkg));
        } else if t.starts_with("npm warn ") {
            keep[i] = true; // EBADENGINE, peer conflicts: not noise
        }
        if t.starts_with("WARNING in ") {
            // webpack: the block under the heading says what and where.
            let end = (i + 1..lines.len().min(i + 12)).find(|&k| lines[k].trim().is_empty()).unwrap_or(lines.len().min(i + 12));
            keep[i..end].iter_mut().for_each(|k| *k = true);
        }
        // pip's WARNING carries things like "script not on PATH".
        if summary_line(t)
            || t.starts_with("warning:")
            || t.starts_with("WARNING: ")
            || t.starts_with("--> ")
        {
            keep[i] = true;
        }
        i += 1;
    }

    if hits == 0 && code != 0 {
        // Unknown failure shape: let generic compaction rescue it instead.
        return None;
    }

    // A bare pytest `E   ` block loses what an agent needs to open the right
    // place: the failing source line, each `file:line:` frame, the test name.
    if !build && lines.iter().any(|l| l.starts_with("E   ")) {
        for (i, l) in lines.iter().enumerate() {
            let t = l.trim_end();
            if pytest_frame(t) || pytest_heading(t) {
                keep[i] = true;
            } else if l.starts_with("E   ") && (i == 0 || !lines[i - 1].starts_with("E   ")) {
                if let Some(src) = (i.saturating_sub(2)..i).rev().find(|&k| lines[k].starts_with("> ")) {
                    keep[src] = true;
                }
            }
        }
    }

    let indices: Vec<usize> = (0..lines.len()).filter(|&i| keep[i]).collect();
    if indices.is_empty() {
        return None;
    }

    let mut out = String::new();
    let mut kept = 0usize;
    let mut clipped = false;
    // msbuild/tsc print each located error again in their summary; the location
    // makes an exact repeat a true duplicate (rustc headers repeat without one).
    let mut seen = std::collections::HashSet::new();
    for (pos, &idx) in indices.iter().enumerate() {
        if pos >= 240 && pos + 20 < indices.len() {
            clipped = true;
            continue;
        }
        let t = lines[idx].trim();
        if (t.contains("): error ") || t.contains("): warning ")) && !seen.insert(t) {
            continue;
        }
        out.push_str(lines[idx]);
        out.push('\n');
        kept += 1;
    }
    if !deprecated.is_empty() {
        let shown = deprecated.iter().take(12).copied().collect::<Vec<_>>().join(", ");
        let more = deprecated.len().saturating_sub(12);
        out.push_str(&format!("npm deprecated: {shown}"));
        if more > 0 {
            out.push_str(&format!(" +{more} more"));
        }
        out.push('\n');
    }
    if clipped {
        out.push_str(&format!(
            "[qx] {} diagnostic lines omitted; full text: {FULL}\n",
            lines.len().saturating_sub(kept)
        ));
    }
    Some((out, lines.len().saturating_sub(kept), clipped))
}

/// `path:line:content`, else bare `path:content` (rg piped without line numbers).
fn split_match(line: &str) -> Option<(&str, &str, &str)> {
    split_numbered(line).or_else(|| split_bare(line))
}

/// The path must look like one, and a numeric tail is `grep -c` output, not a match.
fn split_bare(line: &str) -> Option<(&str, &str, &str)> {
    let drive = matches!(line.as_bytes(), [d, b':', b'\\' | b'/', ..] if d.is_ascii_alphabetic());
    let from = if drive { 2 } else { 0 };
    let i = from + line.get(from..)?.find(':')?;
    let (path, content) = (&line[..i], line[i + 1..].trim_end());
    let path_like = !path.is_empty()
        && !path.contains(char::is_whitespace)
        && path.contains(['.', '/', '\\']);
    // An empty tail is a matched blank line (rg without -n), not a count.
    let counted = !content.trim().is_empty() && content.trim().bytes().all(|b| b.is_ascii_digit());
    (path_like && !counted).then_some((path, "", content))
}

/// Parse `path:line:content` from the left; Windows drive colons are skipped
/// because the segment after them is not numeric.
fn split_numbered(line: &str) -> Option<(&str, &str, &str)> {
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
    // (path, matches seen, matches kept): the caps are per file (25) and overall
    // (300), and a capped file still shows its true count.
    type Group = (String, usize, Vec<(String, String)>);
    let mut groups: Vec<Group> = Vec::new();
    let mut index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut parsed = 0usize;
    let mut kept = 0usize;
    let mut clipped = false;

    for line in text.lines() {
        let Some((path, num, content)) = split_match(line) else {
            // Context lines (`-A`), `Binary file x matches` and the like cannot be placed in a group,
            // so a digest would drop them without a trace: leave the output to generic compaction.
            if line.trim().is_empty() || line == "--" {
                continue;
            }
            return None;
        };
        parsed += 1;
        let g = *index.entry(path).or_insert_with(|| {
            groups.push((path.to_string(), 0, Vec::new()));
            groups.len() - 1
        });
        groups[g].1 += 1;
        if groups[g].2.len() >= 25 || kept >= 300 {
            clipped = true;
            continue;
        }
        let content = content.trim_end();
        let content = if content.len() > 500 {
            format!("{}…", &content[..content.floor_char_boundary(500)])
        } else {
            content.to_string()
        };
        groups[g].2.push((num.to_string(), content));
        kept += 1;
    }

    if parsed < 3 {
        return None;
    }

    let mut out = String::new();
    for (shown, (path, seen, items)) in groups.iter().enumerate() {
        if shown >= 200 && items.is_empty() {
            let rest = groups[shown..].iter().filter(|g| g.2.is_empty()).count();
            out.push_str(&format!("[qx] +{rest} more files with matches (cap reached)\n"));
            break;
        }
        out.push_str(path);
        if *seen > items.len() {
            out.push_str(&format!(" ({} of {seen})\n", items.len()));
        } else {
            out.push_str(&format!(" ({seen})\n"));
        }
        for (num, content) in items {
            out.push_str("  ");
            if !num.is_empty() {
                out.push_str(num);
                out.push_str(": ");
            }
            out.push_str(content);
            out.push('\n');
        }
    }
    let hidden = parsed - kept;
    if clipped {
        out.push_str(&format!("[qx] match cap reached: {hidden} matches not shown\n"));
    }
    Some((out, hidden, clipped))
}

/// Minify a JSON reply. Never a file the agent asked to read: `cat package.json` is read to be edited.
fn json_min(label: &str, text: &str) -> Option<(String, usize)> {
    let t = text.trim();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    let reader = label.split_whitespace().next().unwrap_or("");
    if matches!(reader, "cat" | "type" | "jq" | "get-content" | "gc" | "bat" | "head" | "tail" | "less" | "more") {
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
    fn grep_clips_long_multibyte_lines_on_a_char_boundary() {
        // 3-byte chars: byte 500 falls inside one.
        let raw = (1..=3)
            .map(|n| format!("src/a.rs:{n}:{}", "€".repeat(400)))
            .collect::<Vec<_>>()
            .join("\n");
        let (out, _, _) = grep(&raw).unwrap();
        assert!(out.contains('…'));
    }

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
    fn pytest_failure_keeps_source_line_and_frames() {
        let raw = "collected 2 items\n\n=== FAILURES ===\n____ test_boom ____\n\n    def test_boom():\n        def inner(x):\n            return 1 / x\n>       inner(0)\n\ntest_fail.py:14: \n_ _ _ _ _\n\nx = 0\n\n    def inner(x):\n>       return 1 / x\n               ^^^^^\nE       ZeroDivisionError: division by zero\n\ntest_fail.py:13: ZeroDivisionError\n=== warnings summary ===\ntest_fail.py::test_warn\n  C:\\p\\test_fail.py:5: DeprecationWarning: old api\n=== short test summary info ===\nFAILED test_fail.py::test_boom - ZeroDivisionError\n=== 1 failed, 1 passed in 0.5s ===\n";
        let (out, _, _) = diagnostic(raw, 1, false).unwrap();
        assert!(out.contains("____ test_boom ____"), "{out}");
        assert!(out.contains(">       return 1 / x"), "{out}");
        assert!(out.contains("test_fail.py:14:") && out.contains("test_fail.py:13: ZeroDivisionError"));
        assert!(!out.contains("DeprecationWarning: old api")); // indented warning path is noise
        assert!(!out.contains("def inner"));
    }

    #[test]
    fn jest_and_vitest_failures_keep_expected_received_and_location() {
        let jest = "FAIL src/add.test.js\n  add\n    ✕ adds one (3 ms)\n\n  ● add › adds one\n\n    expect(received).toBe(expected)\n\n    Expected: 2\n    Received: 3\n\n      4 | test('adds one', () => {\n    > 6 |   expect(add(1, 1)).toBe(2);\n\n      at Object.<anonymous> (src/add.test.js:6:22)\n\nTests:       1 failed, 1 total\n";
        let (out, _, _) = diagnostic(jest, 1, false).unwrap();
        for want in ["● add › adds one", "Expected: 2", "Received: 3", "expect(add(1, 1))", "src/add.test.js:6:22", "Tests:       1 failed"] {
            assert!(out.contains(want), "{want}: {out}");
        }
        let vitest = " FAIL  src/a.test.ts > adds\nAssertionError: expected 3 to be 2\n\n- Expected\n+ Received\n\n- 2\n+ 3\n\n ❯ src/a.test.ts:5:15\n\n⎯⎯⎯⎯⎯⎯⎯ Failed Tests 1 ⎯⎯⎯⎯⎯⎯⎯\n\n Test Files  1 failed (1)\n";
        let (out, _, _) = diagnostic(vitest, 1, false).unwrap();
        assert!(out.contains("+ Received") && out.contains("src/a.test.ts:5:15") && out.contains("Test Files"), "{out}");
    }

    #[test]
    fn runtime_failure_flags_cut_stdout_as_clipped() {
        // letters, not digits: lines that differ only in digits collapse into one
        let out: String = (0..100u8).map(|i| format!("step {}{}\n", (b'a' + i % 26) as char, (b'a' + i / 26) as char)).collect();
        let tb = "Traceback (most recent call last):\n  File \"a.py\", line 3, in <module>\nValueError: bad\n";
        let (o, _, _, clipped) = runtime_digest(&out, tb, 1).unwrap();
        assert!(clipped && o.contains("lines omitted"), "{o}");
        let (_, _, _, clipped) = runtime_digest("one\n", tb, 1).unwrap();
        assert!(!clipped);
    }

    #[test]
    fn npm_script_banner_is_not_a_pytest_hit() {
        let raw = "> pkg@1.0.0 test\n> node t.js\n\nok\n";
        assert!(diagnostic(raw, 0, false).is_none_or(|(o, _, _)| !o.contains("node t.js")));
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
        let (out, _) = json_min("curl http://x", raw).unwrap();
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
    fn path_lists_group_by_directory_without_losing_names() {
        assert!(is_pathlist("find . -name *.rs") && is_pathlist("git ls-files") && is_pathlist("rg --files src"));
        assert!(!is_pathlist("find . -exec rm {} ;") && !is_pathlist("rg fn src") && !is_pathlist("git status"));
        let mut raw: String = (0..20).map(|i| format!("src/mod/f{i}.rs\n")).collect();
        raw.push_str("src/a b.rs\nsrc/c,d.rs\ndocs\\x.md\ndocs\\y.md\nsrc/mod/\nsingle.rs/x\n");
        let (out, hidden) = paths(&raw).unwrap();
        assert!(out.starts_with("src/mod/{f0.rs,f1.rs,"), "{out}");
        assert!(out.contains("\nsrc/a b.rs\nsrc/c,d.rs\n") && out.contains("docs\\{x.md,y.md}\n"));
        assert!(out.contains("single.rs/x") && out.contains("\nsrc/mod/\n") && hidden == 20);
        assert!(paths("a/b\nc/d\n").is_none() && paths(&"plain\n".repeat(20)).is_none());
    }

    #[test]
    fn grep_caps_per_file_without_dropping_later_files() {
        let mut raw = String::new();
        for i in 1..=40 {
            raw.push_str(&format!("big.rs:{i}:hit {i}\n"));
        }
        raw.push_str("late.rs:3:needle\nlate2.rs:9:needle\n");
        let (out, hidden, clipped) = grep(&raw).unwrap();
        assert!(clipped && hidden == 15, "{out}");
        assert!(out.contains("big.rs (25 of 40)") && out.contains("late.rs (1)") && out.contains("9: needle"));
        // past the overall cap, files are still named with their counts
        let many: String = (0..400).map(|i| format!("f{i}.rs:1:x\n")).collect();
        let (out, hidden, _) = grep(&many).unwrap();
        assert_eq!(hidden, 100);
        assert!(out.contains("f399.rs (1)") || out.contains("more files with matches"), "{out}");
    }

    #[test]
    fn grep_leaves_context_binary_and_separator_output_to_generic_compaction() {
        let m = "a.txt:1:foo\na.txt:2:foo\nb.txt:3:foo\n";
        assert!(grep(m).is_some());
        for extra in ["a.txt-4-context line\n", "Binary file c.bin matches\n", "--\n"] {
            let raw = format!("{m}{extra}");
            // `--` alone is a harmless group separator; context and binary lines would be dropped silently
            assert_eq!(grep(&raw).is_some(), extra == "--\n", "{extra:?}");
        }
    }

    #[test]
    fn grep_groups_rg_output_without_line_numbers() {
        let raw = "src\\a.rs:let x = 1; // a: b\nsrc\\a.rs:let y = 2;\nC:\\w\\b.rs:fn z() {}\nsrc\\a.rs:  \n";
        let (out, _, _) = grep(raw).unwrap();
        assert_eq!(out.matches("src\\a.rs (").count(), 1, "{out}");
        assert!(out.contains("  let x = 1; // a: b") && out.contains("C:\\w\\b.rs (1)"));
        // `grep -c` / `-l` / prose are not matches
        assert!(split_match("src/a.rs:12").is_none() && split_match("src/a.rs").is_none());
        assert!(split_match("note: build failed").is_none() && split_match("é:é").is_none());
    }

    #[test]
    fn windows_paths_parse() {
        let (p, n, c) = split_match(r"C:\src\a.rs:12:hello: world").unwrap();
        assert_eq!(p, r"C:\src\a.rs");
        assert_eq!(n, "12");
        assert_eq!(c, "hello: world");
    }

    #[test]
    fn git_log_condenses_and_respects_formats() {
        let raw = "commit e9082e3aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa (HEAD -> main)\nAuthor: qx <qx@local>\nDate:   Tue Oct 14 12:00:00 2026 +0000\n\n    fix guard\n\n    body line\n\ncommit 1111111bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\nAuthor: Ann <a@b.c>\nDate:   Mon Oct 13 09:00:00 2026 +0000\n\n    older\n";
        let (out, _) = git_log(raw).unwrap();
        assert!(out.contains("e9082e3 fix guard (qx, Oct 14 2026) (HEAD -> main) [+1 body lines]"));
        assert!(out.contains("1111111 older (Ann, Oct 13 2026)"));
        assert!(is_git_log("git log -5") && is_git_log("git -c repo log"));
        assert!(!is_git_log("git log --oneline") && !is_git_log("git log -p"));
        assert!(git_log("abc\ndef\n").is_none());
    }

    #[test]
    fn git_status_sees_global_options() {
        assert!(is_git_status("git -c repo status"));
        assert!(!is_git_status("git commit -m status"));
        assert!(is_diff("git -c repo diff"));
    }

    #[test]
    fn install_output_keeps_summary_and_errors() {
        let ok = "npm warn deprecated a@1\nnpm warn deprecated b@1\nnpm warn deprecated c@1\n\nadded 120 packages, and audited 121 packages in 4s\n\n3 vulnerabilities (1 moderate, 2 high)\n";
        let (out, _, _) = diagnostic(ok, 0, true).unwrap();
        assert!(out.contains("added 120 packages") && out.contains("3 vulnerabilities"));
        assert!(out.contains("npm deprecated: a@1, b@1, c@1"));
        let pip = "Collecting x\n  Downloading x-1.whl (3 kB)\nWARNING: The script x.exe is installed in 'C:\\S' which is not on PATH.\nSuccessfully installed x-1\n";
        let (out, _, _) = diagnostic(pip, 0, true).unwrap();
        assert!(out.contains("not on PATH") && out.contains("Successfully installed x-1"));
        assert!(!out.contains("Downloading"));
        assert!(is_build("npm install left-pad") && is_build("pip install -r req.txt"));
        let bad = "ERROR: Could not find a version that satisfies the requirement nope\n";
        assert!(diagnostic(bad, 1, true).unwrap().0.contains("ERROR: Could not find"));
    }

    #[test]
    fn go_test_v_keeps_failure_messages_and_drops_run_chatter() {
        let mut s = String::new();
        for i in 0..30 {
            s.push_str(&format!("=== RUN   TestOk{i}\n--- PASS: TestOk{i} (0.00s)\n"));
        }
        s.push_str("=== RUN   TestBad\n=== RUN   TestBad/sub\n    calc_test.go:54: got 2 want 3\n--- FAIL: TestBad/sub (0.00s)\n--- FAIL: TestBad (0.00s)\nFAIL\nFAIL\tgotest\t0.031s\nFAIL\n");
        let (out, _, _) = diagnostic(&s, 1, false).unwrap();
        assert!(out.contains("calc_test.go:54: got 2 want 3"), "{out}");
        assert!(out.contains("--- FAIL: TestBad/sub") && out.contains("gotest"));
        assert!(!out.contains("=== RUN") && !out.contains("TestOk"));
    }

    #[test]
    fn located_errors_printed_twice_show_once_but_headers_do_not_merge() {
        let e = "P.cs(1,9): error CS0029: Cannot convert";
        let text = format!("{e} [x.csproj]\nwarning: noise\n{e} [x.csproj]\nBuild FAILED.\n{e} [x.csproj]\n");
        let (out, _, _) = diagnostic(&text, 1, true).unwrap();
        assert_eq!(out.matches("CS0029").count(), 1, "{out}");
        let rustc = "error[E0308]: mismatched types\n --> a.rs:1:1\n\nerror[E0308]: mismatched types\n --> b.rs:2:2\n";
        let (out, _, _) = diagnostic(rustc, 101, true).unwrap();
        assert_eq!(out.matches("E0308").count(), 2);
    }

    #[test]
    fn core_label_finds_the_command_behind_the_ceremony() {
        for (label, core) in [
            ("cd app && cargo build", "cargo build"),
            ("cd app; ci=1 npx -y tsc --noemit 2>&1 | tail -30", "tsc --noemit"),
            ("time rust_backtrace=1 cargo test -- --nocapture", "cargo test -- --nocapture"),
            ("uv run pytest -q", "pytest -q"),
            ("pnpm exec eslint . || true", "eslint ."),
            ("echo hi && git status", "git status"),
            ("git commit -m \"a && b\"", "git commit -m \"a && b\""),
            ("git log | grep x", ""),
            ("npx", "npx"),
            ("   ", "   "),
        ] {
            assert_eq!(core_label(label), core, "{label}");
        }
        assert!(is_build(&core_label("cd x && npm run build")));
        assert!(is_test(&core_label("ci=1 npm test")));
    }

    #[test]
    fn status_short_forms_and_unknown_lines_are_left_alone() {
        for l in ["git status -s", "git status --short", "git status --porcelain", "git status -sb", "git status --porcelain=v2"] {
            assert!(!is_git_status(l), "{l}");
        }
        assert!(is_git_status("git status") && is_git_status("git status -uno"));
        assert!(git_status(" M src/a.rs\n?? b.txt\n").is_none());
        assert!(git_status("On branch main\nUnmerged paths:\n\tdeleted by us:   x.rs\n").is_none());
    }

    #[test]
    fn status_nothing_added_is_not_an_untracked_file() {
        let raw = "On branch main\nUntracked files:\n  (use \"git add <file>...\" to include in what will be committed)\n\tnew.txt\n\nnothing added to commit but untracked files present (use \"git add\" to track)\n";
        let (out, _) = git_status(raw).unwrap();
        assert!(out.contains("untracked (1):"), "{out}");
        assert!(!out.contains("  nothing added"), "{out}");
    }

    #[test]
    fn diff_keeps_removed_dash_lines_and_skips_conflicts() {
        let raw = "diff --git a/q.sql b/q.sql\nindex 1..2 100644\n--- a/q.sql\n+++ b/q.sql\n@@ -1,2 +1,2 @@\n--- drop comment\n+++ added plus\n ctx\n";
        let (out, _, _) = git_diff(raw).unwrap();
        assert!(out.contains("--- drop comment") && out.contains("+++ added plus"), "{out}");
        assert!(!out.contains("--- a/q.sql"), "{out}");
        assert!(git_diff("diff --cc f\nindex 1,2..3\n--- a/f\n+++ b/f\n@@@ -1,2 -1,2 +1,3 @@@\n  x\n++<<<<<<< ours\n").is_none());
        let shows = "commit aaa\n\n    one\n\ndiff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\ncommit bbb\n\n    two\n";
        let (out, _, _) = git_diff(shows).unwrap();
        assert!(out.contains("commit bbb") && out.contains("    two"), "{out}");
    }

    #[test]
    fn python_exception_line_survives_whatever_its_name() {
        let raw = "Traceback (most recent call last):\n  File \"a.py\", line 3, in <module>\n    next(it)\nStopIteration\n";
        let (out, _, _) = traceback(raw).unwrap();
        assert!(out.contains("StopIteration"), "{out}");
        let chained = "Traceback (most recent call last):\n  File \"a.py\", line 1, in f\nKeyError: 'k'\n\nDuring handling of the above exception, another exception occurred:\n\nTraceback (most recent call last):\n  File \"a.py\", line 2, in g\nMyAppFailure: gave up\n";
        let (out, _, _) = traceback(chained).unwrap();
        assert!(out.contains("KeyError: 'k'") && out.contains("MyAppFailure: gave up"), "{out}");
        let unit = "FAIL: test_x (t.T)\n----\nTraceback (most recent call last):\n  File \"t.py\", line 3, in test_x\nAssertionError: 1 != 2\n\n----\nRan 1 test in 0.001s\n\nFAILED (failures=1)\n";
        assert!(traceback(unit).is_none());
    }

    #[test]
    fn requested_and_machine_output_is_not_digested() {
        let mut pass = String::from("running 30 tests\n");
        for i in 0..30 {
            pass.push_str(&format!("test t{i} ... ok\n"));
        }
        pass.push_str("test result: ok. 30 passed; 0 failed\n");
        let green = digest("cargo test", &pass, "", 0).unwrap();
        assert!(green.omitted > 0 && !green.lossy(), "a green run drops only `... ok` noise: no spool file");
        let mut red = pass.replace("test t3 ... ok", "test t3 ... FAILED");
        red.push_str("---- t3 stdout ----\nthread 't3' panicked at src/lib.rs:3:5:\nassertion failed: x\n\nfailures:\n    t3\n");
        let d = digest("cargo test", &red, "", 101).unwrap();
        assert!(d.omitted > 0 && d.lossy(), "a failing run keeps the full text on disk");
        for l in ["cargo test -- --nocapture", "pytest -s", "cargo test --message-format=json", "vitest --reporter=json", "go test -v ./..."] {
            assert!(digest(l, &pass, "", 0).is_none(), "{l}");
        }

        let mut install = String::from("npm warn EBADENGINE Unsupported engine\nnpm warn deprecated old@1: gone\n");
        for i in 0..30 {
            install.push_str(&format!("npm http fetch GET 200 https://registry/pkg{i} 12ms\n"));
        }
        install.push_str("added 3 packages in 2s\n");
        let d = digest("npm install", &install, "", 0).unwrap();
        assert!(d.stdout.contains("EBADENGINE") && d.stdout.contains("added 3 packages") && d.stdout.contains("old@1"), "{}", d.stdout);

        let mut pack = String::from("WARNING in ./src/a.js\nModule not found: x\n  at ./src/a.js 3:1\n\n");
        for i in 0..30 {
            pack.push_str(&format!("asset chunk{i}.js 12 KiB\n"));
        }
        pack.push_str("webpack compiled with 1 warning\n");
        let d = digest("npm run build", &pack, "", 0).unwrap();
        assert!(d.stdout.contains("Module not found: x") && d.stdout.contains("at ./src/a.js"), "{}", d.stdout);
        assert!(!is_build("make"));
    }

    #[test]
    fn pytest_and_vitest_summaries_are_kept() {
        assert!(summary_line("===== 1 failed, 2 passed in 0.4s ====="));
        assert!(summary_line("Test Files  1 failed | 2 passed (3)"));
        assert!(summary_line("Tests  1 failed | 4 passed (5)"));
        assert!(!summary_line("Tests are great"));
        assert!(!summary_line("=== RUN   TestX"));
    }

    #[test]
    fn a_pipe_to_a_real_filter_or_two_commands_are_not_one_profile() {
        assert_eq!(core_label("cargo test | grep foo"), "");
        assert_eq!(core_label("cargo test 2>&1 | tail -5"), "cargo test");
        assert_eq!(core_label("cargo build; cargo test"), "");
        assert_eq!(core_label("cd x && cargo test | head"), "cargo test");
    }

    #[test]
    fn git_log_with_other_dates_or_ranges_is_left_alone() {
        for l in ["git log --date=relative", "git log -l1,5:f.rs", "git log --summary", "git log -m", "git log --cc"] {
            assert!(!is_git_log(l), "{l}");
        }
        assert!(is_git_log("git log -5"));
    }

    #[test]
    fn braces_in_a_directory_name_keep_paths_plain() {
        let raw: String = (0..14).map(|i| format!("a,b/f{i}.txt\n")).collect();
        let (out, _) = paths(&raw).unwrap();
        assert!(out.contains("a,b/f0.txt") && !out.contains('{'), "{out}");
    }

    #[test]
    fn a_file_the_agent_reads_is_not_minified() {
        let raw = "{\n  \"a\": 1,\n  \"b\": [1, 2]\n}\n";
        assert!(json_min("cat package.json", raw).is_none());
        assert!(json_min("jq . x.json", raw).is_none());
        assert!(json_min("curl http://x", raw).is_some());
    }

    #[test]
    fn a_condensed_git_log_names_the_way_back_for_both_transports() {
        let mut raw = String::new();
        for i in 0..6 {
            raw.push_str(&format!(
                "commit {:040x}\nAuthor: A <a@b.c>\nDate:   Mon Oct 19 10:0{i}:00 2026 +0000\n\n    subject {i}\n\n    a long body line one\n    a long body line two\n\n",
                i + 1
            ));
        }
        let d = digest("git log", &raw, "", 0).unwrap();
        assert!(d.stdout.contains("--raw (CLI)") && d.stdout.contains("mode:\"full\" (MCP)"), "{}", d.stdout);
    }
}
