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

/// A diff up to this many lines keeps every line; only its file headers fold (see `digest`).
const DIFF_WHOLE: usize = 400;

/// Line budget `(max, head, tail)` for the plain compaction of a command that `digest` declined: a diff that
/// fits `DIFF_WHOLE` must not be cut at the usual 200 lines, or its middle files vanish.
pub fn plain_budget(label: &str, usual: (usize, usize, usize)) -> (usize, usize, usize) {
    if is_diff(&core_label(&label.to_ascii_lowercase())) {
        (DIFF_WHOLE.max(usual.0), DIFF_WHOLE * 2 / 5, DIFF_WHOLE * 3 / 10)
    } else {
        usual
    }
}

/// Git's Windows line-ending warnings are noise, not content: the callers drop them from a git command's
/// stderr before it is digested or compacted, so each path keeps its own budget (the CLI's `--max-lines`,
/// the MCP reply cap).
pub fn drop_git_eol_warnings(label: &str, stderr: &mut String) {
    if git_sub(&core_label(&label.to_ascii_lowercase())).is_some() && stderr.lines().any(eol_warning) {
        *stderr = stderr.lines().filter(|l| !eol_warning(l)).fold(String::new(), |mut s, l| {
            s.push_str(l);
            s.push('\n');
            s
        });
    }
}

/// git >= 2.37: `warning: in the working copy of 'X', LF will be replaced by CRLF the next time Git
/// touches it`; older: `warning: LF will be replaced by CRLF in X.` plus an explanatory line.
fn eol_warning(l: &str) -> bool {
    (l.starts_with("warning: in the working copy of '")
        && (l.contains(", LF will be replaced by CRLF the next time Git touches it")
            || l.contains(", CRLF will be replaced by LF the next time Git touches it")))
        || l.starts_with("warning: LF will be replaced by CRLF in ")
        || l.starts_with("warning: CRLF will be replaced by LF in ")
        || l.starts_with("The file will have its original line endings in your working directory")
}

pub fn digest(label: &str, stdout: &str, stderr: &str, code: i32) -> Option<Digest> {
    if std::env::var("QX_DIGEST").ok().as_deref() == Some("off") {
        return None;
    }
    let l = core_label(&label.to_ascii_lowercase());
    // Progress bars redrawn with `\r` (tqdm, curl, downloaders) reach the digests as their last frame.
    let ((so, sd), (se, ed)) = (compact::settle_frames(stdout), compact::settle_frames(stderr));
    let (stdout, stderr) = (&*so, &*se);
    // `None` stderr: the digest leaves it alone, so it is shown compacted. Tests and builds merge it.
    let (profile, out, err, omitted, clipped) = if is_git_status(&l) {
        let (o, n) = git_status(stdout)?;
        ("git-status", o, None, n, false)
    } else if is_git_log(&l) {
        let (o, n) = git_log(stdout)?;
        ("git-log", o, None, n, false)
    } else if is_diff(&l) {
        // A diff that fits is shown with every hunk header, context, `+` and `-` line: without them the
        // hunk offsets no longer match what is shown. Only its file headers fold, which loses nothing,
        // so it skips the saving gate below; only a minified line past the per-line cap is cut (and
        // spooled). Only a long diff is condensed.
        if stdout.lines().count() <= DIFF_WHOLE {
            let (o, cut_o) = bound(trim_diff_headers(stdout)?, DIFF_WHOLE);
            let (err, cut) = keep_stderr(stderr);
            return Some(Digest { stdout: o, stderr: err, profile: "git-diff", omitted: 0, clipped: sd || ed || cut || cut_o, passed: code == 0 });
        }
        let (o, n, c) = git_diff(stdout)?;
        ("git-diff", o, None, n, c)
    } else if is_pathlist(&l) {
        // Searches and listings first: `rg -n jest` is a search, whatever word it searches for.
        // Sorted output (`rg --files --sort modified`, `dir /b /o:d`, a pipe into `sort`): the order is the answer.
        let sorted = label.contains('|') || words(&l).any(|w| w.starts_with("--sort") || w.to_ascii_lowercase().starts_with("/o"));
        let (o, n) = paths(stdout, !sorted)?;
        ("paths", o, None, n, false)
    } else if is_search(&l) {
        let (o, n, c) = grep(stdout)?;
        ("search", o, None, n, c)
    } else if is_test(&l) {
        if verbatim(&l) {
            return None;
        }
        let merged = merge(stdout, stderr);
        let (o, n, c) = diagnostic(&merged, code, false)?;
        ("test", o, Some(String::new()), n, c)
    } else if is_build(&l) {
        if verbatim(&l) {
            return None;
        }
        let merged = merge(stdout, stderr);
        let (o, n, c) = diagnostic(&merged, code, true)?;
        ("build", o, Some(String::new()), n, c)
    } else if is_runtime(&l) {
        let (o, e, n, c) = runtime_digest(stdout, stderr, code)?;
        ("traceback", o, Some(e), n, c)
    } else if let Some((o, n)) = json_min(&l, stdout) {
        ("json", o, None, n, false)
    } else {
        return None;
    };
    let (err, clipped) = match err {
        Some(e) => (e, clipped),
        None => {
            let (e, cut) = keep_stderr(stderr);
            (e, clipped || cut)
        }
    };
    // A digest is still a reply: a 5000-commit log or a 1 MB JSON line is held to a budget, and
    // anything cut makes it lossy, so the full text is spooled.
    let (out, cut) = bound(out, if profile == "git-diff" { 1500 } else { 400 });
    let clipped = clipped || cut;

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
        clipped: clipped || sd || ed,
        passed: code == 0,
    })
}

/// Lines past `cap`, or one longer than a line may be, go through compaction; the flag says
/// something was cut.
fn bound(text: String, cap: usize) -> (String, bool) {
    if text.lines().count() <= cap && !text.lines().any(|l| l.len() > compact::MAX_LINE_CHARS) {
        return (text, false);
    }
    compact::compact(&text, cap, cap * 2 / 5, cap * 3 / 10)
}

/// The caller asked for the output itself: `--nocapture` and `-s` print what the tests print, and
/// `--json` output is for a parser. A passing run's summary-only digest would drop all of it.
fn verbatim(l: &str) -> bool {
    let machine = |w: &str| w.starts_with('-') && ["json", "junit", "tap", "xml"].iter().any(|k| w.contains(k));
    let shows = |w: &str| {
        matches!(
            w,
            "--nocapture" | "--no-capture" | "-s" | "--show-output" | "-v" | "-vv" | "-vvv" | "--verbose" | "--capture=no" | "--capture=tee-sys"
        ) || w.starts_with("--verbose=")
            || w == "--reporter=verbose"
            // `-sv`, `-xvs`: a cluster of short flags that holds -s or -v (not -k/-m/-p/-r, which take a value).
            || (w.len() > 2
                && w.starts_with('-')
                && !w.starts_with("--")
                && w[1..].bytes().all(|c| c.is_ascii_alphabetic())
                && !matches!(w.as_bytes()[1], b'k' | b'm' | b'p' | b'r')
                && w[1..].contains(['s', 'v']))
    };
    let w: Vec<&str> = words(l).collect();
    // A failing run that asked for its output (`--nocapture`, `-v`) wants the prints most of all.
    w.iter().any(|x| machine(x))
        || w.iter().any(|x| shows(x))
        || w.windows(2).any(|p| p[0] == "--reporter" && p[1] == "verbose")
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

/// Compacted stderr and whether lines were cut: a cut must reach `clipped`, or the full text is
/// never spooled and the cut lines are gone for good.
fn keep_stderr(stderr: &str) -> (String, bool) {
    if stderr.trim().is_empty() {
        return (String::new(), false);
    }
    compact::compact(stderr, 120, 50, 40)
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
            [a, ..] if crate::exec::assignment_word(a) => 1,
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
            // `--parents` puts more hashes here; only a decoration may follow the hash.
            let mut w = rest.split_whitespace();
            w.next();
            if w.next().is_some_and(|x| !x.starts_with('(')) {
                return None;
            }
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
        } else if !(line.is_empty() || line.starts_with("Merge:") || line.starts_with("    ")) {
            // A patch, stat, reflog or other line this shape does not know: leave the text whole.
            return None;
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
    out.push_str("[qx] git log condensed (7-char hashes, bodies dropped)\n");
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
    // The runner in command position (`pytest`, `python -m pytest`, `yarn jest`, `cargo nextest`),
    // not anywhere: `rg -n jest` searches for the word.
    let runner = |w: &str| matches!(w, "pytest" | "py.test" | "jest" | "vitest" | "nextest");
    let w: Vec<&str> = words(l).take(3).collect();
    let at_front = match w.as_slice() {
        [first, ..] if runner(first) => true,
        ["python" | "python3" | "py", "-m", r, ..] => runner(r),
        ["yarn" | "pnpm" | "bun" | "bunx" | "npx" | "cargo", r, ..] => runner(r),
        _ => false,
    };
    if at_front {
        return true;
    }
    if !words(l).any(|w| w == "test" || w == "tests" || w.starts_with("test:")) {
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
        .any(|p| l.strip_prefix(p).is_some_and(|r| r.is_empty() || r.starts_with(' ')))
}

/// Paths under the cwd in the text a reply shows (the spool keeps the original). A build or test shortens
/// both streams; a script's traceback, digested or too short to be, sits on stderr, so the program's own
/// output stays as printed.
pub fn shorten_paths(profile: &str, label: &str, so: &mut String, se: &mut String, cwd: Option<&std::path::Path>) {
    match profile {
        "build" | "test" => compact::relative_to_cwd(so, se, cwd),
        "traceback" | "" if is_runtime(&core_label(&label.to_ascii_lowercase())) => compact::relative_to_cwd(&mut String::new(), se, cwd),
        _ => {}
    }
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
    let mut keep = vec![false; lines.len()];
    let mut in_py = false;
    let mut frames = false; // a frame was seen, so the next unindented line is the exception
    let mut msg_left = 0usize; // continuation lines of the exception message still to keep
    let mut i = 0usize;
    while i < lines.len() {
        let t = lines[i].trim();
        let indented = lines[i].starts_with(char::is_whitespace);
        keep[i] = if t.starts_with("Traceback (most recent call last):") {
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
                // `/proj/app.js:3` above the source line and its caret: where the error is.
                || (i + 2 < lines.len() && lines[i + 2].trim().starts_with('^') && ends_with_line_number(t))
                || t.contains("caused by")
        } else {
            false
        };
        i += 1;
    }
    if !keep.contains(&true) {
        return None;
    }
    // The program's own lines around the traceback (a log line naming the cause, a warning) are kept, the
    // first few and the last ones before the failure; a longer run is cut and says so.
    let outside: Vec<usize> = (0..lines.len()).filter(|&i| !keep[i] && !lines[i].trim().is_empty()).collect();
    let (n, mut clipped) = (outside.len(), false);
    for (k, &i) in outside.iter().enumerate() {
        if n <= 40 || k < 10 || k >= n - 30 {
            keep[i] = true;
        } else {
            clipped = true;
        }
    }
    let mut out = String::new();
    let mut kept = 0usize;
    for i in 0..lines.len() {
        if keep[i] {
            if kept < 160 {
                out.push_str(lines[i]);
                out.push('\n');
                kept += 1;
            } else {
                clipped = true;
            }
        }
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

/// Lossless as a set: every path of one directory goes on ONE line `dir/{a,b,c}` (directories in order
/// of first appearance, names in their original order), so the order across directories is regrouped.
/// When the order is the answer (`rg --files --sort modified`, a pipe into `sort`), `regroup` is false
/// and only consecutive paths of one directory share a line. When 3+ names of a line share one
/// extension it is factored out: `dir/{a,b,c}.rs`. Names or dirs that hold `,{}`, a name without an
/// extension, or a directory path itself stay as plain lines.
fn paths(text: &str, regroup: bool) -> Option<(String, usize)> {
    let sep = |c: char| c == '/' || c == '\\';
    let lines: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.is_empty()).collect();
    if lines.len() < 12 || lines.iter().filter(|l| l.contains(sep)).count() * 10 < lines.len() * 9 {
        return None;
    }
    let cut = |l: &str| l.rfind(sep).map_or(0, |k| k + 1);
    // A line that ends in a separator is a directory entry: it never joins the files of that directory.
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut index: std::collections::HashMap<(&str, bool), usize> = std::collections::HashMap::new();
    for l in &lines {
        let dir = &l[..cut(l)];
        let key = (dir, dir.len() == l.len());
        let g = match index.get(&key) {
            Some(&g) if regroup || g + 1 == groups.len() => g,
            _ => {
                groups.push((dir, Vec::new()));
                index.insert(key, groups.len() - 1);
                groups.len() - 1
            }
        };
        groups[g].1.push(&l[dir.len()..]);
    }
    // `.rs` of `a.rs`; none for `Makefile` or a dotfile (`.gitignore`).
    fn ext(n: &str) -> Option<&str> {
        n.rfind('.').filter(|&k| k > 0 && k + 1 < n.len()).map(|k| &n[k..])
    }
    let mut out = String::new();
    let mut shown = 0usize;
    for (dir, names) in &groups {
        if names.len() < 2 || dir.contains([',', '{', '}']) || names.iter().any(|n| n.is_empty() || n.contains([',', '{', '}'])) {
            for n in names {
                out.push_str(dir);
                out.push_str(n);
                out.push('\n');
                shown += 1;
            }
            continue;
        }
        for chunk in names.chunks(60) {
            let common = ext(chunk[0]).filter(|e| chunk.len() >= 3 && chunk.iter().all(|n| ext(n) == Some(*e)));
            match (chunk.len(), common) {
                (1, _) => out.push_str(&format!("{dir}{}", chunk[0])),
                (_, Some(e)) => {
                    let stems: Vec<&str> = chunk.iter().map(|n| &n[..n.len() - e.len()]).collect();
                    out.push_str(&format!("{dir}{{{}}}{e}", stems.join(",")));
                }
                _ => out.push_str(&format!("{dir}{{{}}}", chunk.join(","))),
            }
            out.push('\n');
            shown += 1;
        }
    }
    Some((out, lines.len().saturating_sub(shown)))
}

/// `path:3`
fn ends_with_line_number(t: &str) -> bool {
    t.rsplit_once(':').is_some_and(|(p, n)| !p.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn is_search(l: &str) -> bool {
    if l.starts_with("git grep") {
        return true;
    }
    ["grep", "rg", "findstr"].iter().any(|p| {
        l == *p || l.starts_with(&format!("{p} ")) || l.ends_with(&format!(" {p}"))
    })
}

/// The entry of a long-format status as `(code, path)`. The code is one letter (`M A D R C T`) whose
/// column (index or worktree) the section decides, or a full two-letter code that does not depend on
/// it: a conflict, or `??` for `?? path`. For a rename or copy the path reads `old -> new`.
fn status_path(t: &str) -> Option<(&'static str, String)> {
    if let Some(p) = t.strip_prefix("?? ") {
        return Some(("??", p.trim().to_string()));
    }
    for (k, code) in [
        ("both modified:", "UU"),
        ("both deleted:", "DD"),
        ("both added:", "AA"),
        ("added by us:", "AU"),
        ("added by them:", "UA"),
        ("deleted by us:", "DU"),
        ("deleted by them:", "UD"),
        ("modified:", "M"),
        ("new file:", "A"),
        ("deleted:", "D"),
        ("renamed:", "R"),
        ("copied:", "C"),
        ("typechange:", "T"),
        ("unmerged:", "UU"),
    ] {
        if let Some(p) = t.strip_prefix(k) {
            return Some((code, p.trim().to_string()));
        }
    }
    None
}

/// `'origin/main'` out of a branch sentence.
fn quoted(s: &str) -> Option<&str> {
    let a = s.find('\'')? + 1;
    let b = a + s[a..].find('\'')?;
    Some(&s[a..b])
}

/// `Your branch is ahead of 'origin/main' by 2 commits.` -> `("origin/main", Some("ahead 2"))`; `None` for
/// a sentence this reader does not know. A diverged branch gets its counts from the following line.
fn branch_line(t: &str) -> Option<(String, Option<String>)> {
    let rest = t.strip_prefix("Your branch ")?;
    let up = quoted(rest)?.to_string();
    let by = || rest.split(" by ").nth(1)?.split_whitespace().next()?.parse::<usize>().ok();
    let track = if rest.starts_with("is up to date") {
        None
    } else if rest.starts_with("is ahead of ") {
        Some(format!("ahead {}", by()?))
    } else if rest.starts_with("is behind ") {
        Some(format!("behind {}", by()?))
    } else if rest.starts_with("is based on ") && rest.contains("upstream is gone") {
        Some("gone".to_string())
    } else if rest.contains("have diverged") {
        Some("diverged".to_string())
    } else {
        return None;
    };
    Some((up, track))
}

/// Lossless rewrite of the long `git status` into the short `-sb` encoding every agent reads: a `## branch`
/// header, then one line per `XY` code (index column, worktree column) listing its paths.
fn git_status(text: &str) -> Option<(String, usize)> {
    #[derive(Clone, Copy, PartialEq)]
    enum Sec {
        Staged,
        Unstaged,
        Untracked,
        Ignored,
        Unmerged,
    }
    let mut branch: Option<String> = None;
    let mut detached: Option<String> = None;
    let mut no_commits = false;
    let mut upstream: Option<(String, Option<String>)> = None;
    let mut state: Vec<String> = Vec::new();
    // `(code, path)` in order of first appearance; a path staged and also modified is one entry (`MM`).
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut staged: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut cur: Option<Sec> = None;
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
        if let Some(d) = t.strip_prefix("HEAD detached ") {
            detached = Some(format!("detached {d}")); // `at abc123` or `from abc123`
            continue;
        }
        if t == "No commits yet" || t == "Initial commit" {
            no_commits = true;
            continue;
        }
        if t.starts_with("Your branch ") {
            match branch_line(t) {
                Some(u) => upstream = Some(u),
                None => state.push(t.trim_end_matches('.').to_string()),
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("and have ") {
            if let Some((_, track)) = upstream.as_mut() {
                let mut w = rest.split_whitespace();
                if let (true, Some(a), Some("and"), Some(b)) = (track.as_deref() == Some("diverged"), w.next(), w.next(), w.next()) {
                    if a.parse::<usize>().is_ok() && b.parse::<usize>().is_ok() {
                        *track = Some(format!("ahead {a}, behind {b}"));
                        continue;
                    }
                }
            }
        }
        let section = match t {
            "Changes to be committed:" => Some(Sec::Staged),
            "Changes not staged for commit:" => Some(Sec::Unstaged),
            "Untracked files:" => Some(Sec::Untracked),
            "Ignored files:" => Some(Sec::Ignored), // `git status --ignored`
            "Unmerged paths:" => Some(Sec::Unmerged),
            _ => None,
        };
        if section.is_some() {
            cur = section;
            continue;
        }
        if t.starts_with("nothing to commit") {
            state.push("clean".to_string());
            continue;
        }
        // Both say what the `??` lines or the absent `A`/`M` lines already show.
        if t.starts_with("nothing added to commit")
            || t.starts_with("no changes added to commit")
            || t.starts_with("(use \"")
            || t.starts_with("(commit or discard")
            || t.starts_with("(fix conflicts")
        {
            omitted += 1;
            continue;
        }
        // `--show-stash`, and the merge notices: state, not a file of the section above. A merge in
        // progress must stay visible even once its `UU` lines are gone.
        if t.starts_with("Your stash currently has ") || t == "You have unmerged paths." || t.starts_with("All conflicts fixed but you are still merging") {
            state.push(t.to_string());
            continue;
        }
        // git indents a listed path with one TAB; any other line here is a note this digest does not know.
        if matches!(cur, Some(Sec::Untracked | Sec::Ignored)) && raw.starts_with('\t') {
            let code = if cur == Some(Sec::Ignored) { "!!" } else { "??" };
            entries.push((code.to_string(), t.to_string()));
            continue;
        }
        if let Some((code, path)) = status_path(t) {
            let letter = code.as_bytes()[0] as char;
            let code = match (code.len(), cur) {
                (2, _) => code.to_string(),
                (_, Some(Sec::Staged)) => format!("{letter} "),
                (_, Some(Sec::Unstaged)) => format!(" {letter}"),
                // A bare entry outside any section: the column is unknown, so do not guess.
                _ => return None,
            };
            // A change made after staging belongs on the line of its staged entry (`MM`).
            if cur == Some(Sec::Unstaged) {
                if let Some(&i) = staged.get(&path) {
                    if entries[i].0.ends_with(' ') {
                        entries[i].0 = format!("{}{letter}", &entries[i].0[..1]);
                        continue;
                    }
                }
            }
            if cur == Some(Sec::Staged) {
                // The worktree side names the renamed file by its new path.
                let key = path.rsplit_once(" -> ").map_or(path.as_str(), |(_, n)| n).to_string();
                staged.insert(key, entries.len());
            }
            entries.push((code, path));
            continue;
        }
        // A line this digest does not know (a rebase notice, ` M file`) is not ours to drop.
        return None;
    }

    let mut out = String::new();
    let head = match (&detached, &branch) {
        (Some(d), _) => Some(format!("## HEAD ({d})")),
        (None, Some(b)) if no_commits => Some(format!("## No commits yet on {b}")),
        (None, Some(b)) => Some(format!("## {b}")),
        _ => None,
    };
    if no_commits && (detached.is_some() || branch.is_none()) {
        state.push("No commits yet".to_string());
    }
    if let Some(mut h) = head {
        if detached.is_none() {
            if let Some((up, track)) = &upstream {
                h.push_str("...");
                h.push_str(up);
                if let Some(t) = track {
                    h.push_str(&format!(" [{t}]"));
                }
            }
        }
        out.push_str(&h);
        out.push('\n');
    }
    for s in state {
        out.push_str(&s);
        out.push('\n');
    }
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for (code, path) in entries {
        match groups.iter().position(|(c, _)| *c == code) {
            Some(i) => groups[i].1.push(path),
            None => groups.push((code, vec![path])),
        }
    }
    for (code, paths) in groups {
        // A rename (`old -> new`) or any path with whitespace would not survive a space-joined list.
        let joinable = paths.iter().all(|p| !p.is_empty() && !p.contains(char::is_whitespace));
        if !joinable {
            for p in &paths {
                out.push_str(&format!("{code} {p}\n"));
            }
            continue;
        }
        // Wrapped at ~200 chars so no line reaches the per-line cap, which would cut the list; every
        // line repeats its code, so each stands on its own.
        let mut cur = String::new();
        for p in &paths {
            if !cur.is_empty() && cur.len() + p.len() + 1 > 200 {
                out.push_str(&cur);
                out.push('\n');
                cur.clear();
            }
            if cur.is_empty() {
                cur = format!("{code} {p}");
            } else {
                cur.push(' ');
                cur.push_str(p);
            }
        }
        out.push_str(&cur);
        out.push('\n');
    }
    if out.is_empty() {
        return None;
    }
    Some((out, omitted))
}

/// The name `diff --git a/X b/X` shows when both sides are the same path. `None` for quoted, prefixed
/// differently (`--no-prefix`, `i/`, `w/`) or differing names: those are kept as git printed them.
fn same_diff_path(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("diff --git a/")?;
    let n = rest.len().checked_sub(3)? / 2;
    (rest.len() == 2 * n + 3 && rest.get(n..n + 3) == Some(" b/") && rest.get(..n) == rest.get(n + 3..)).then(|| &rest[..n])
}

/// Fold the header block that starts at `lines[i]` (a `diff --git` line) into compact lines and return
/// them with the index of the first line after the block. Nothing is lost: the `index` line is a blob
/// hash, `---`/`+++` repeat the path, and mode, rename and similarity info move into the title line:
/// `diff X`, `diff X (deleted 100644)`, `diff A -> B (renamed 95%)`. Lines it cannot place (mode
/// change, copy, odd prefixes) stay as git printed them.
fn fold_diff_header(lines: &[&str], i: usize) -> (Vec<String>, usize) {
    let mut j = i + 1;
    let (mut new_mode, mut del_mode, mut sim) = (None, None, None);
    let (mut from, mut to) = (None, None);
    let (mut minus, mut plus) = (None, None);
    let mut others: Vec<&str> = Vec::new();
    while let Some(&l) = lines.get(j) {
        if let Some(m) = l.strip_prefix("new file mode ") {
            new_mode = Some(m);
        } else if let Some(m) = l.strip_prefix("deleted file mode ") {
            del_mode = Some(m);
        } else if let Some(s) = l.strip_prefix("similarity index ") {
            sim = Some(s);
        } else if let Some(f) = l.strip_prefix("rename from ") {
            from = Some(f);
        } else if let Some(t) = l.strip_prefix("rename to ") {
            to = Some(t);
        } else if l.starts_with("--- ") {
            minus = Some(l);
        } else if l.starts_with("+++ ") {
            plus = Some(l);
        } else if l.starts_with("old mode ")
            || l.starts_with("new mode ")
            || l.starts_with("dissimilarity index ")
            || l.starts_with("copy from ")
            || l.starts_with("copy to ")
        {
            others.push(l);
        } else if !l.starts_with("index ") {
            break;
        }
        j += 1;
    }
    let line = lines[i];
    let renamed = from.zip(to);
    let (a, b) = match (same_diff_path(line), renamed) {
        (Some(p), None) => (p, p),
        (_, Some((f, t))) if new_mode.is_none() && del_mode.is_none() => (f, t),
        _ => {
            // Unplaceable names: only the blob-hash line goes.
            let mut out = vec![line.to_string()];
            out.extend(lines[i + 1..j].iter().filter(|l| !l.starts_with("index ")).map(|l| l.to_string()));
            return (out, j);
        }
    };
    let mut title = if a == b { format!("diff {a}") } else { format!("diff {a} -> {b}") };
    let mut tags: Vec<String> = Vec::new();
    if let Some(m) = new_mode {
        tags.push(format!("new {m}"));
    }
    if let Some(m) = del_mode {
        tags.push(format!("deleted {m}"));
    }
    if renamed.is_some() {
        tags.push(format!("renamed {}", sim.unwrap_or("")).trim_end().to_string());
    }
    if !tags.is_empty() {
        title.push_str(&format!(" ({})", tags.join(", ")));
    }
    let mut out = vec![title];
    out.extend(others.iter().map(|l| l.to_string()));
    let want_minus = if new_mode.is_some() { "--- /dev/null".to_string() } else { format!("--- a/{a}") };
    let want_plus = if del_mode.is_some() { "+++ /dev/null".to_string() } else { format!("+++ b/{b}") };
    for (got, want) in [(minus, want_minus), (plus, want_plus)] {
        if let Some(g) = got.filter(|g| *g != want) {
            out.push(g.to_string());
        }
    }
    (out, j)
}

/// A diff short enough to show whole: only its headers are folded, every hunk header, context, `+` and
/// `-` line stays. `None` when there is nothing to fold or the diff has merge-conflict columns.
fn trim_diff_headers(text: &str) -> Option<String> {
    if diff_is_combined(text) {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with("diff --git ") {
            let (folded, next) = fold_diff_header(&lines, i);
            for l in folded {
                out.push_str(&l);
                out.push('\n');
            }
            i = next;
        } else {
            out.push_str(lines[i]);
            out.push('\n');
            i += 1;
        }
    }
    (out.len() < text.len()).then_some(out)
}

/// A merge-conflict diff marks each side with a column of its own; this reader knows one.
fn diff_is_combined(text: &str) -> bool {
    text.starts_with("diff --cc") || text.contains("\ndiff --cc ") || text.contains("\n@@@ ") || text.starts_with("@@@ ")
}

fn git_diff(text: &str) -> Option<(String, usize, bool)> {
    if diff_is_combined(text) {
        return None;
    }
    let mut out = String::new();
    let mut omitted = 0usize;
    let mut clipped = false;
    let mut in_hunk = false;
    let mut changed = 0usize;
    let mut kept = 0usize;
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.starts_with("diff --git ") {
            let (folded, next) = fold_diff_header(&lines, i - 1);
            omitted += (next - (i - 1)).saturating_sub(folded.len());
            for l in folded {
                out.push_str(&l);
                out.push('\n');
            }
            i = next;
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
        // Inside a hunk `--- x` is the removed line `-- x`, not a file header. A note qx's redaction
        // put there (`[qx] no END line ...`) is not the end of the hunk either.
        if in_hunk && line.starts_with("[qx]") {
            out.push_str(line);
            out.push('\n');
            continue;
        }
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
        out.push_str(&format!("[qx] {omitted} diff lines omitted\n"));
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
        || t.starts_with("FAIL\t")
        || t == "FAIL"
        || t.starts_with("FAIL:")
        // node:test `✖ name`, dotnet `Failed Name [3 ms]`, maven `[ERROR] ...`, mocha `1) name`
        || t.starts_with('\u{2716}')
        || t.starts_with("Failed ")
        || t.starts_with("[ERROR]")
        || mocha_item(t)
        || t.starts_with("ERROR:")
        || t.starts_with("ERROR ")
        || t.starts_with("Jest: ")
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
        || t.starts_with("not ok")
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

/// mocha's numbered failure list: `1) Suite name`.
fn mocha_item(t: &str) -> bool {
    let n = t.bytes().take_while(u8::is_ascii_digit).count();
    n > 0 && t[n..].starts_with(") ")
}

/// mocha's `1 passing (5ms)` / `2 failing` counts.
fn mocha_count(t: &str) -> bool {
    let mut w = t.split_whitespace();
    w.next().is_some_and(|n| n.parse::<u64>().is_ok()) && w.next().is_some_and(|k| matches!(k, "passing" | "failing" | "pending"))
}

fn summary_line(t: &str) -> bool {
    t.starts_with("test result:")
        || mocha_count(t)
        // dotnet `Failed!  - Failed: 1, Passed: 1 ...`; node:test `ℹ tests 3` / `ℹ pass 1` / `ℹ fail 2`
        || t.starts_with("Failed!")
        || t.starts_with("Passed!")
        || t.strip_prefix('\u{2139}').is_some_and(|r| {
            let r = r.trim_start();
            r.starts_with("tests ") || r.starts_with("pass ") || r.starts_with("fail ")
        })
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
        // `Finished` is routine: `diagnostic` keeps it only when nothing else survives.
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

/// MSBuild's `    1 Warning(s)` / `    2 Error(s)` with a count above zero.
fn nonzero_tally(t: &str) -> bool {
    (t.ends_with(" Warning(s)") || t.ends_with(" Error(s)"))
        && t.split_whitespace().next().is_some_and(|n| n.parse::<u64>().is_ok_and(|n| n > 0))
}

/// Progress and bookkeeping a green test or build run prints: dropping it loses nothing.
fn routine_line(t: &str) -> bool {
    const STARTS: &[&str] = &[
        "Compiling ", "Checking ", "Finished ", "Running ", "Fresh ", "Downloaded ", "Downloading ", "Updating ",
        "Locking ", "Adding ", "Blocking ", "Installing ", "Installed ", "Replacing ", "Replaced ", "Doc-tests ",
        "running ", "collecting ", "collected ", "platform ", "rootdir:", "cachedir:", "plugins:", "configfile:", "asyncio: mode=",
        "Collecting ", "Using cached ", "Requirement already satisfied", "Building wheel", "Created wheel",
        "Stored in directory", "Preparing metadata", "Getting requirements", "Determining projects to restore",
        "All projects are up-to-date", "Restored ", "Time Elapsed", "Ran all test suites", "Test Suites:", "Tests",
        "Snapshots:", "Time:", "Duration", "Test Files", "Start at", "RUN ", "PASS ", "ok ", "--- PASS", "=== RUN",
        "=== PAUSE", "=== CONT", "npm warn deprecated", "up to date", "added ", "removed ", "changed ", "audited ",
        "found 0 vulnerabilities", "To address", "npm audit", "Run `npm audit", "npm fund", "run `npm fund`",
        "Build succeeded", "0 Warning(s)", "0 Error(s)", "\u{2713}", "\u{221a}", "\u{2714}", "\u{2139}", "Passed ",
    ];
    t.is_empty()
        || (t.starts_with('_') && t.chars().all(|c| c == '_'))
        // progress dots (`cargo test -q`, pytest) and pytest's `file.py .... [ 50%]`
        || t.chars().all(|c| matches!(c, '.' | 's' | 'x' | 'S' | 'X'))
        || t.ends_with("%]")
        || t.ends_with(" ok")
        || t.ends_with(" PASSED")
        || t == "PASSED"
        || t.ends_with("packages are looking for funding")
        || t.starts_with('=')
        || (t.starts_with('-') && t.chars().all(|c| c == '-'))
        // `dotnet build`'s `App -> bin\App.dll`; any other arrow may be a result (`answer -> 42`).
        || t.split_once(" -> ").is_some_and(|(_, out)| out.ends_with(".dll") || out.ends_with(".exe"))
        || STARTS.iter().any(|p| t.starts_with(p))
}

fn failure_word(t: &str) -> bool {
    let t = t.to_ascii_lowercase();
    // `fail` only opens a word, so a file named test_fail.py is not a failure.
    ["error", "panic", "exception", "fatal"].iter().any(|w| t.contains(w)) || t.split_whitespace().any(|w| w.starts_with("fail"))
}

/// Lint name in rustc's `= note: `#[warn(clippy::x)]` on by default` or `= note: `-D clippy::x` implied by ...`.
fn lint_name(t: &str) -> Option<&str> {
    let t = t.strip_prefix("= note: `")?;
    if let Some(r) = t.strip_prefix("#[") {
        return r.split_once('(')?.1.split_once(")]").map(|(n, _)| n);
    }
    let n = t.strip_prefix("-D ")?.split('`').next()?;
    (n != "warnings").then_some(n)
}

/// `^^^ help: use `is_empty`: `v.is_empty()`` label: the text after `help: `.
fn inline_help(t: &str) -> Option<&str> {
    let r = t[t.find('^')?..].trim_start_matches('^').trim_start();
    r.strip_prefix("help: ")
}

/// `5 +     v[0]` line of a rustc suggestion diff.
fn plus_line(t: &str) -> bool {
    let n = t.bytes().take_while(u8::is_ascii_digit).count();
    n > 0 && t[n..].starts_with(" + ")
}

/// rustc/clippy blocks: the lint name moves onto the header line; the `= note`/`= help: ...visit` lines it
/// came from, the source snippet and the suggestion diff are noise. A warning keeps its header, `-->`, and one
/// fix (the inline `help:` label, else the suggestion's `help:` + `+` lines). Errors keep their whole block.
fn fold_rustc(lines: &[&str], keep: &mut [bool], noise: &mut [bool], over: &mut [Option<String>]) {
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim_end();
        let header = t.starts_with("warning:") || t.starts_with("error[") || t.starts_with("error:");
        if !header || !lines.get(i + 1).is_some_and(|n| n.trim_start().starts_with("--> ")) {
            i += 1;
            continue;
        }
        let warn = t.starts_with("warning");
        let end = (i + 1..lines.len()).take(200).find(|&k| lines[k].trim().is_empty()).unwrap_or(lines.len().min(i + 201));
        let mut lint = None;
        let mut inline = false;
        let mut fix: Vec<usize> = Vec::new();
        let pin = |j: usize, keep: &mut [bool], noise: &mut [bool]| {
            noise[j] = false;
            keep[j] = true;
        };
        for j in i + 1..end {
            let u = lines[j].trim();
            if warn {
                noise[j] = true;
            }
            if let Some(n) = lint_name(u) {
                lint = lint.or(Some(n));
                noise[j] = true;
            } else if u.starts_with("= help: for further information visit") || u == "|" {
                noise[j] = true;
            } else if warn {
                if u.starts_with("--> ") || u.starts_with("= note:") || u.starts_with("= help:") {
                    pin(j, keep, noise);
                } else if let Some(h) = inline_help(u) {
                    over[j] = Some(format!("help: {h}"));
                    inline = true;
                    pin(j, keep, noise);
                } else if lines[j].starts_with("help: ") || (!fix.is_empty() && plus_line(u)) {
                    fix.push(j);
                }
            }
        }
        if !inline {
            for j in fix {
                pin(j, keep, noise);
            }
        }
        if let Some(n) = lint {
            if !t.ends_with(']') {
                over[i] = Some(format!("{t} [{n}]"));
            }
        }
        i = end;
    }
}

/// cargo test prints a failing test three times: `test x ... FAILED`, its `---- x stdout ----` block, and the
/// closing `failures:` list. The block carries the facts, so the other two are noise when it exists.
fn fold_cargo_test(lines: &[&str], noise: &mut [bool]) {
    let blocks: std::collections::HashSet<&str> =
        lines.iter().filter_map(|l| l.trim().strip_prefix("---- ")?.strip_suffix(" stdout ----")).collect();
    let results = lines.iter().filter(|l| l.trim().starts_with("test result:")).count();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if t.starts_with("note: run with `RUST_BACKTRACE") {
            noise[i] = true;
        } else if t.starts_with("error: test failed, to rerun pass") && results <= 1 {
            // With several test binaries it is the only clue which one failed.
            noise[i] = true;
        } else if let Some(n) = t.strip_prefix("test ").and_then(|r| r.strip_suffix(" ... FAILED")) {
            // `#[should_panic]` tests read `test x - should panic ... FAILED`; their block is `---- x stdout ----`.
            noise[i] = blocks.contains(n.strip_suffix(" - should panic").unwrap_or(n));
        } else if t == "failures:" {
            let mut k = i + 1;
            while lines.get(k).is_some_and(|v| v.trim().is_empty()) {
                k += 1;
            }
            if lines.get(k).is_some_and(|v| v.trim().starts_with("---- ")) {
                noise[i] = true;
                continue;
            }
            let names: Vec<usize> = (i + 1..lines.len()).take_while(|&k| lines[k].starts_with("    ") && !lines[k].trim().is_empty()).collect();
            if !names.is_empty() && names.iter().all(|&k| blocks.contains(lines[k].trim())) {
                noise[i] = true;
                names.iter().for_each(|&k| noise[k] = true);
            }
        }
    }
}

/// pytest: `=====` banners carry no facts, the failure heading becomes `name:`, the closing counts lose their
/// padding, long-style source context (indented, before a block's `>` line) is noise, and the short summary's
/// `+  where` repeats the `E` lines. Native/short frames and captured output are content and stay.
fn fold_pytest(lines: &[&str], keep: &mut [bool], noise: &mut [bool], over: &mut [Option<String>]) {
    let where_text: std::collections::HashSet<&str> = lines
        .iter()
        .filter(|l| l.starts_with("E   "))
        .filter_map(|l| l.strip_prefix('E'))
        .map(str::trim)
        .filter(|r| r.starts_with('+'))
        .collect();
    let mut in_fail = false;
    // Inside a frame block, before its `>` line: the indented lines there are `def` and source context.
    let mut context = false;
    // Failure headings as (line, test key, has an `E` line), and `FAILED nodeid` lines of the short summary.
    let mut heads: Vec<(usize, String, bool)> = Vec::new();
    let mut failed: Vec<(usize, &str)> = Vec::new();
    // Consecutive `>`/`E` lines, re-indented together.
    let mut run: Vec<usize> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_end();
        if t.starts_with("==") && t.ends_with("==") {
            dedent_run(lines, &mut run, over);
            let title = t.trim_matches('=').trim();
            if ["test session starts", "FAILURES", "ERRORS", "short test summary info"].contains(&title) {
                noise[i] = true;
                in_fail = title == "FAILURES" || title == "ERRORS";
                continue;
            }
            in_fail = false;
            context = false;
            let counts = ["passed", "failed", "error", "skipped", "no tests ran"].iter().any(|k| title.contains(k));
            if counts && (title.contains(|c: char| c.is_ascii_digit()) || title == "no tests ran") {
                over[i] = Some(title.to_string());
            }
            continue;
        }
        if let Some(id) = t.strip_prefix("FAILED ") {
            failed.push((i, id.split(" - ").next().unwrap_or(id)));
        }
        if in_fail && !t.trim().is_empty() {
            let u = t.trim();
            if !l.starts_with(char::is_whitespace) {
                let rest = t.find(".py:").map(|p| t[p + 4..].trim_start_matches(|c: char| c.is_ascii_digit()));
                if pytest_heading(t) {
                    heads.push((i, pytest_key(t.trim_matches('_').trim()), false));
                }
                if pytest_heading(t) || t.chars().all(|c| matches!(c, '_' | ' ')) {
                    context = true;
                } else if pytest_frame(t) {
                    // `file.py:5: in f` is a --tb=short frame whose source line follows: content.
                    context = !rest.is_some_and(|r| r.trim_start_matches(':').trim_start().starts_with("in "));
                } else if t.starts_with("> ") || t.starts_with("---") || t.starts_with("Traceback") {
                    context = false;
                }
                if t.starts_with("E   ") {
                    if let Some(h) = heads.last_mut() {
                        h.2 = true;
                    }
                }
            } else if u.starts_with("File \"") {
                context = false;
                keep[i] = true;
            } else if context || u.chars().all(|c| matches!(c, '^' | '~' | ' ')) {
                noise[i] = true;
            } else {
                // --tb=native/short frames, captured stdout/log text: content.
                keep[i] = true;
            }
            if t.starts_with("> ") || t.starts_with("E   ") || t == "E" {
                run.push(i);
            } else if !noise[i] {
                dedent_run(lines, &mut run, over);
            }
        } else {
            dedent_run(lines, &mut run, over);
        }
        if !l.starts_with('E') && t.trim_start().starts_with('+') && where_text.contains(t.trim()) {
            noise[i] = true;
        }
    }
    dedent_run(lines, &mut run, over);

    // A failure block that was shown carries the node id; its `FAILED` line only repeats the first `E` line.
    // A `FAILED` line whose test has no block here (summary-only, `-rA`) is the only record and stays.
    let mut used = vec![false; heads.len()];
    for (f, id) in failed {
        let key = pytest_key_of_node(id);
        if let Some(h) = (0..heads.len()).find(|&h| !used[h] && heads[h].2 && heads[h].1 == key) {
            used[h] = true;
            over[heads[h].0] = Some(format!("{id}:"));
            noise[f] = true;
            // The message's further lines (indented under it) repeat the block's `E` lines too.
            for k in f + 1..lines.len() {
                let l = lines[k];
                if !l.trim().is_empty() && !l.starts_with(char::is_whitespace) {
                    break;
                }
                noise[k] = true;
            }
        }
    }
}

/// `TestX.test_y[1-2]` heading name to `test_y[1-2]`: the part a node id ends with.
fn pytest_key(name: &str) -> String {
    let (base, param) = name.split_once('[').map_or((name, String::new()), |(b, p)| (b, format!("[{p}")));
    format!("{}{param}", base.rsplit('.').next().unwrap_or(base))
}

/// `a.py::TestX::test_y[1-2]` to `test_y[1-2]`.
fn pytest_key_of_node(id: &str) -> String {
    // A parameter id may contain `::`, so cut at the last `::` before any `[`.
    let head = id.split('[').next().unwrap_or(id);
    let start = head.rfind("::").map_or(0, |p| p + 2);
    id[start..].to_string()
}

/// Remove the indent shared by a run of `>`/`E` lines so `>       assert` reads `> assert`, keeping each
/// line's indent relative to the others (pytest's diffs depend on it). A bare `E` (a blank line of the
/// message) has no indent to share and prints as `E`.
fn dedent_run(lines: &[&str], run: &mut Vec<usize>, over: &mut [Option<String>]) {
    let gap = |l: &str| l[1..].len() - l[1..].trim_start_matches(' ').len();
    let min = run.iter().map(|&i| lines[i].trim_end()).filter(|l| l.len() > 1).map(gap).min();
    if let Some(m) = min.filter(|&m| m > 1) {
        for &i in run.iter() {
            let l = lines[i].trim_end();
            over[i] = Some(if l.len() > 1 {
                format!("{}{}{}", &l[..1], " ".repeat(gap(l) - m + 1), l[1..].trim_start_matches(' '))
            } else {
                l.to_string()
            });
        }
    }
    run.clear();
}

fn diagnostic(text: &str, code: i32, build: bool) -> Option<(String, usize, bool)> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let mut keep = vec![false; lines.len()];
    // Lines that only repeat or decorate something kept (see `fold_*`): dropping them is not a loss.
    let mut noise = vec![false; lines.len()];
    let mut over: Vec<Option<String>> = vec![None; lines.len()];
    fold_rustc(&lines, &mut keep, &mut noise, &mut over);
    fold_cargo_test(&lines, &mut noise);
    fold_pytest(&lines, &mut keep, &mut noise, &mut over);
    // rustc's closing hints repeat the E-codes already on the `error[E....]` headers.
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if t.starts_with("Some errors have detailed explanations:")
            || t.starts_with("For more information about an error, try `rustc --explain")
            || t.starts_with("For more information about this error, try `rustc --explain")
        {
            noise[i] = true;
        }
    }
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
        if t == "failures:" {
            // cargo lists the failed test names indented under this heading.
            let mut k = i + 1;
            while k < lines.len() && k - i <= 80 && lines[k].starts_with("    ") && !lines[k].trim().is_empty() {
                keep[k] = true;
                k += 1;
            }
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
            // located warnings: MSBuild/MSVC `file(3,13): warning CS0168`, gcc/clang `a.c:3:5: warning:`
            || (build && (t.contains("): warning ") || t.contains(": warning:") || t.contains(": warning ")))
            || nonzero_tally(t)
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
            if pytest_frame(t) {
                keep[i] = true;
            } else if pytest_heading(t) {
                keep[i] = true;
                if over[i].is_none() {
                    over[i] = Some(format!("{}:", t.trim_matches('_').trim()));
                }
            } else if l.starts_with("E   ") && (i == 0 || !lines[i - 1].starts_with("E   ")) {
                if let Some(src) = (i.saturating_sub(2)..i).rev().find(|&k| lines[k].starts_with("> ")) {
                    keep[src] = true;
                }
            }
        }
    }
    for (k, n) in keep.iter_mut().zip(&noise) {
        *k &= !n;
    }
    // A green cargo run with nothing else to say still shows its `Finished` line.
    if !keep.iter().any(|&k| k) {
        if let Some(k) = lines.iter().rposition(|l| l.trim().starts_with("Finished ")) {
            keep[k] = true;
        }
    }

    // A red run keeps what it cannot call routine and an agent may need: a line at column 0 (a package, a
    // log line, a compiler note) or one that names a failure. Indented detail stays cut, and is spooled.
    // They only fill the room the recognised lines leave, failure words first, so they never push a
    // recognised failure out of the 240-line window.
    if code != 0 {
        let mut room = 240usize.saturating_sub(keep.iter().filter(|&&k| k).count());
        for words in [true, false] {
            for (i, l) in lines.iter().enumerate() {
                let t = l.trim();
                let want = if words { failure_word(t) } else { !l.starts_with(char::is_whitespace) };
                if room > 0 && !keep[i] && !noise[i] && !routine_line(t) && want {
                    keep[i] = true;
                    room -= 1;
                }
            }
        }
    }

    let indices: Vec<usize> = (0..lines.len()).filter(|&i| keep[i]).collect();
    if indices.is_empty() {
        return None;
    }

    let mut out = String::new();
    // Lines the output shows, or repeats of ones it shows.
    let mut shown = vec![false; lines.len()];
    // A green run that drops more than progress (prints, console.log, unknown notes) keeps the
    // full text on disk: the caller sees it was cut and where it went.
    let mut clipped = lines.iter().zip(keep.iter().zip(&noise)).any(|(l, (k, n))| !k && !n && !routine_line(l.trim()));
    // msbuild/tsc print each located error again in their summary; the location
    // makes an exact repeat a true duplicate (rustc headers repeat without one).
    let mut seen = std::collections::HashSet::new();
    for (pos, &idx) in indices.iter().enumerate() {
        if pos >= 240 && pos + 20 < indices.len() {
            clipped = true;
            continue;
        }
        shown[idx] = true;
        let t = lines[idx].trim();
        if (t.contains("): error ") || t.contains("): warning ")) && !seen.insert(t) {
            continue;
        }
        out.push_str(over[idx].as_deref().unwrap_or(lines[idx]));
        out.push('\n');
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
    // Only lines that carry content count as omitted: blank, decoration and folded lines do not.
    let omitted = (0..lines.len()).filter(|&i| !shown[i] && !noise[i] && !routine_line(lines[i].trim())).count();
    if clipped {
        out.push_str(&format!("[qx] {omitted} diagnostic lines omitted\n"));
    }
    Some((out, omitted, clipped))
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
        // Another colon in the path part means an earlier one was not the separator: the
        // `12:30:` of a timestamp is not a line number. Nor is `2024-01-01 12` a path: a space
        // needs a directory separator beside it, and a path has a letter.
        let drive = matches!(b, [d, b':', b'\\' | b'/', ..] if d.is_ascii_alphabetic());
        let path = &line[if drive { 2 } else { 0 }..i];
        if path.contains(':')
            || (path.contains(char::is_whitespace) && !path.contains(['/', '\\']))
            || !path.chars().any(char::is_alphabetic)
        {
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
    let mut cut_lines = 0usize;

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
        // A minified line is cut in the middle, not at the end: its tail (where a stack, a path or the
        // match often sits) stays, and the flag spools the whole line.
        let content = if content.len() > 500 {
            let head = (0..=250).rev().find(|&i| content.is_char_boundary(i)).unwrap_or(0);
            let tail = (content.len() - 250..=content.len()).find(|&i| content.is_char_boundary(i)).unwrap_or(content.len());
            cut_lines += 1;
            format!("{} … {}", &content[..head], &content[tail..])
        } else {
            content.to_string()
        };
        groups[g].2.push((num.to_string(), content));
        kept += 1;
    }

    if parsed < 3 {
        return None;
    }

    // The per-file counts alone make the agent add them up; one total survives the grouping.
    let mut out = format!("{parsed} matches in {} files\n", groups.len());
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
    if cut_lines > 0 {
        out.push_str(&format!("[qx] {cut_lines} long lines cut to their first and last 250 bytes\n"));
    }
    Some((out, hidden, clipped || cut_lines > 0))
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
    // Validate, then strip whitespace only: numbers and key order stay byte-exact. A raw value is
    // checked without building the tree, which for a 16 MiB reply would be ten times its size.
    serde_json::from_str::<&serde_json::value::RawValue>(t).ok()?;
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
    fn an_invalid_assignment_word_is_not_stripped() {
        assert_eq!(core_label("1X=foo cargo test"), "1X=foo cargo test");
        assert_eq!(core_label("CI=1 cargo test"), "cargo test");
    }

    #[test]
    fn only_build_artifact_arrows_are_routine() {
        assert!(routine_line(r"App -> C:\src\App\bin\Debug\net8.0\App.dll"));
        assert!(!routine_line("answer -> 42"));
        assert!(is_build("cargo build") && is_build("cargo build --release") && !is_build("cargo buildx"));
    }

    #[test]
    fn a_cut_stderr_marks_the_digest_lossy_so_it_is_spooled() {
        let files: String = (0..300).map(|i| format!("src/deep/dir/file{i}.rs\n")).collect();
        let noisy: String = (0..400).map(|i| format!("find: cannot read dir{i}: Permission denied\n")).collect();
        let d = digest("find src -name *.rs", &files, &noisy, 1).expect("the path digest fires");
        assert_eq!(d.profile, "paths");
        assert!(d.lossy(), "stderr was cut but the digest is not spooled");
        // The same digest with a short stderr loses nothing, so nothing is spooled.
        let d = digest("find src -name *.rs", &files, "find: one warning\n", 1).unwrap();
        assert!(!d.lossy());
    }

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
        assert!(out.starts_with("## main...origin/main\n"));
        assert!(out.contains("src/main.rs"));
        assert!(out.contains("new.txt"));
        assert!(!out.contains("use \"git add"));
        assert!(omitted > 0);
    }

    #[test]
    fn a_diff_that_fits_is_not_cut_at_the_plain_budget() {
        let (max, head, tail) = plain_budget("git diff", (200, 80, 60));
        assert!(max >= DIFF_WHOLE && head + tail < max);
        let body: String = (0..DIFF_WHOLE).map(|i| format!("+line {i} {}\n", "x".repeat(i % 7))).collect();
        assert!(digest("git diff", &body, "", 0).is_none());
        assert!(!compact::compact(&body, max, head, tail).1);
        assert_eq!(plain_budget("ls", (200, 80, 60)), (200, 80, 60));
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

    const CRLF_WARN: &str = "warning: in the working copy of 'README.md', LF will be replaced by CRLF the next time Git touches it\nwarning: in the working copy of 'src/util.py', CRLF will be replaced by LF the next time Git touches it\n";

    fn short_diff() -> String {
        let mut s = String::from("diff --git a/README.md b/README.md\nindex e5ad4d9..7a3f716 100644\n--- a/README.md\n+++ b/README.md\n@@ -3,7 +3,7 @@\n # README line 3\n-retries = 3\n+retries = 5\n # README line 7\n");
        s.push_str("diff --git a/old.txt b/old.txt\ndeleted file mode 100644\nindex fe33913..0000000\n--- a/old.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-# old line 1\n-# old line 2\n");
        s.push_str("diff --git a/new.txt b/new.txt\nnew file mode 100755\nindex 0000000..fe33913\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n");
        s
    }

    #[test]
    fn a_short_diff_folds_headers_and_keeps_every_hunk_line() {
        let raw = short_diff();
        let d = digest("git diff", &raw, "", 0).expect("short diff trims");
        let out = &d.stdout;
        assert!(!d.lossy(), "{out}");
        assert!(out.starts_with("diff README.md\n@@ -3,7 +3,7 @@\n # README line 3\n-retries = 3\n+retries = 5\n # README line 7\n"), "{out}");
        assert!(out.contains("diff old.txt (deleted 100644)\n@@ -1,2 +0,0 @@\n-# old line 1\n-# old line 2\n"), "{out}");
        assert!(out.contains("diff new.txt (new 100755)\n@@ -0,0 +1 @@\n+hello\n"), "{out}");
        assert!(!out.contains("index ") && !out.contains("--- ") && !out.contains("+++ ") && !out.contains("diff --git"), "{out}");
    }

    #[test]
    fn a_short_diff_of_a_minified_line_is_still_held_to_the_line_cap() {
        let raw = format!("diff --git a/m.js b/m.js\nindex 1..2 100644\n--- a/m.js\n+++ b/m.js\n@@ -1 +1 @@\n-{}\n+{}\n", "x".repeat(50_000), "y".repeat(50_000));
        let d = digest("git diff", &raw, "", 0).unwrap();
        assert!(d.stdout.len() < 10_000 && d.lossy() && d.stdout.starts_with("diff m.js\n@@ -1 +1 @@\n"), "{}", d.stdout.len());
    }

    #[test]
    fn diff_folds_rename_and_keeps_mode_change_and_odd_headers() {
        let raw = "diff --git a/a.rs b/b.rs\nsimilarity index 95%\nrename from a.rs\nrename to b.rs\nindex 111..222 100644\n--- a/a.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-x\n+y\ndiff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\ndiff --git a/img.png b/img.png\nindex 1..2 100644\nBinary files a/img.png and b/img.png differ\ndiff --git i/odd.txt w/odd.txt\nindex 1..2 100644\n--- i/odd.txt\n+++ w/odd.txt\n@@ -1 +1 @@\n-p\n+q\n";
        let out = trim_diff_headers(raw).unwrap();
        assert!(out.starts_with("diff a.rs -> b.rs (renamed 95%)\n@@ -1 +1 @@\n-x\n+y\n"), "{out}");
        assert!(out.contains("diff run.sh\nold mode 100644\nnew mode 100755\n"), "{out}");
        assert!(out.contains("diff img.png\nBinary files a/img.png and b/img.png differ\n"), "{out}");
        assert!(out.contains("diff --git i/odd.txt w/odd.txt\n--- i/odd.txt\n+++ w/odd.txt\n@@ -1 +1 @@\n-p\n+q\n"), "{out}");
    }

    #[test]
    fn show_keeps_the_commit_header_and_body() {
        let raw = "commit d1d3fe3f1d7521048c3411ef8fcc3a8aacbbefa1\nAuthor: T <t@example.test>\nDate:   Wed Nov 18 19:35:24 2026 +0000\n\n    fix(core): switch to safe mode\n    \n    Body: fast mode corrupted the cache under load.\n\ndiff --git a/src/core.py b/src/core.py\nindex e350f92..c549c17 100644\n--- a/src/core.py\n+++ b/src/core.py\n@@ -7,7 +7,7 @@\n # core line 9\n-mode = fast\n+mode = safe\n # core line 11\n";
        let d = digest("git show", raw, "", 0).unwrap();
        assert!(d.stdout.starts_with(&raw[..raw.find("diff --git").unwrap()]), "{}", d.stdout);
        assert!(d.stdout.contains("diff src/core.py\n@@ -7,7 +7,7 @@\n # core line 9\n-mode = fast\n+mode = safe\n # core line 11\n"), "{}", d.stdout);
        assert!(!d.lossy());
    }

    #[test]
    fn git_line_ending_warnings_are_dropped_from_git_commands_only() {
        let mut err = format!("{CRLF_WARN}fatal: pathspec 'x' did not match\n");
        drop_git_eol_warnings("git add x", &mut err);
        assert_eq!(err, "fatal: pathspec 'x' did not match\n");
        // Other commands keep their stderr as is.
        let mut other = CRLF_WARN.to_string();
        drop_git_eol_warnings("echo hi", &mut other);
        assert_eq!(other, CRLF_WARN);
    }

    #[test]
    fn status_uses_the_short_encoding_and_merges_staged_with_modified() {
        let raw = "On branch main\nChanges to be committed:\n  (use \"git restore --staged <file>...\" to unstage)\n\tmodified:   src/core.py\n\tnew file:   src/new_feature.py\n\trenamed:    src/legacy.py -> src/renamed.py\n\tmodified:   src/util.py\n\nChanges not staged for commit:\n  (use \"git add/rm <file>...\" to update what will be committed)\n  (use \"git restore <file>...\" to discard changes in working directory)\n\tmodified:   README.md\n\tmodified:   docs/guide.md\n\tdeleted:    old.txt\n\tmodified:   src/util.py\n\nUntracked files:\n  (use \"git add <file>...\" to include in what will be committed)\n\tbuild.log\n\tnotes.tmp\n\tscratch/\n";
        let (out, _) = git_status(raw).unwrap();
        assert_eq!(
            out,
            "## main\nM  src/core.py\nA  src/new_feature.py\nR  src/legacy.py -> src/renamed.py\nMM src/util.py\n M README.md docs/guide.md\n D old.txt\n?? build.log notes.tmp scratch/\n"
        );
    }

    #[test]
    fn status_header_carries_upstream_ahead_behind_detached_and_no_commits() {
        let head = |raw: &str| git_status(raw).unwrap().0.lines().next().unwrap().to_string();
        assert_eq!(head("On branch main\nYour branch is up to date with 'origin/main'.\n\nnothing to commit, working tree clean\n"), "## main...origin/main");
        assert_eq!(head("On branch main\nYour branch is ahead of 'origin/main' by 2 commits.\n  (use \"git push\" to publish your local commits)\n\nnothing to commit, working tree clean\n"), "## main...origin/main [ahead 2]");
        assert_eq!(head("On branch main\nYour branch is behind 'origin/main' by 1 commit, and can be fast-forwarded.\n  (use \"git pull\" to update your local branch)\n\nnothing to commit, working tree clean\n"), "## main...origin/main [behind 1]");
        assert_eq!(
            head("On branch main\nYour branch and 'origin/main' have diverged,\nand have 2 and 1 different commits each, respectively.\n  (use \"git pull\" to merge the remote branch into yours)\n\nnothing to commit, working tree clean\n"),
            "## main...origin/main [ahead 2, behind 1]"
        );
        assert_eq!(head("On branch main\nYour branch is based on 'origin/gone', but the upstream is gone.\n\nnothing to commit, working tree clean\n"), "## main...origin/gone [gone]");
        assert_eq!(head("HEAD detached at abc1234\nnothing to commit, working tree clean\n"), "## HEAD (detached at abc1234)");
        let (out, _) = git_status("On branch main\n\nNo commits yet\n\nChanges to be committed:\n  (use \"git rm --cached <file>...\" to unstage)\n\tnew file:   a.rs\n").unwrap();
        assert_eq!(out, "## No commits yet on main\nA  a.rs\n");
        let (out, _) = git_status("On branch main\nnothing to commit, working tree clean\n").unwrap();
        assert_eq!(out, "## main\nclean\n");
    }

    #[test]
    fn status_maps_conflicts_to_their_two_letter_codes() {
        let raw = "On branch main\ninteractive rebase in progress; onto abc1234\n";
        assert!(git_status(raw).is_none(), "an unknown notice declines");
        // As git prints a merge with conflicts: the notice stays, its hints go.
        let raw = "On branch main\nYou have unmerged paths.\n  (fix conflicts and run \"git commit\")\n  (use \"git merge --abort\" to abort the merge)\n\nUnmerged paths:\n  (use \"git add <file>...\" to mark resolution)\n\tboth modified:   a.rs\n\tboth added:      b.rs\n\tboth deleted:    c.rs\n\tadded by us:     d.rs\n\tadded by them:   e.rs\n\tdeleted by us:   f.rs\n\tdeleted by them: g.rs\n\tboth modified:   h.rs\n\nno changes added to commit (use \"git add\" and/or \"git commit -a\")\n";
        let (out, _) = git_status(raw).unwrap();
        assert_eq!(out, "## main\nYou have unmerged paths.\nUU a.rs h.rs\nAA b.rs\nDD c.rs\nAU d.rs\nUA e.rs\nDU f.rs\nUD g.rs\n");
    }

    #[test]
    fn a_long_status_group_wraps_with_its_code_repeated_and_loses_no_path() {
        let paths: Vec<String> = (0..200).map(|i| format!("src/deep/module{i}/file_{i}.rs")).collect();
        let raw = format!("On branch main\nChanges not staged for commit:\n{}", paths.iter().map(|p| format!("\tmodified:   {p}\n")).collect::<String>());
        let (out, _) = git_status(&raw).unwrap();
        assert!(out.lines().all(|l| l.len() <= 260 && l.len() < compact::MAX_LINE_CHARS), "{out}");
        assert!(out.lines().skip(1).all(|l| l.starts_with(" M src/")), "{out}");
        assert!(out.lines().count() > 3, "{out}");
        let tokens: Vec<&str> = out.split_whitespace().filter(|t| t.starts_with("src/")).collect();
        assert_eq!(tokens, paths.iter().map(String::as_str).collect::<Vec<_>>());
        let (b, cut) = bound(out, 400);
        assert!(!cut && b.contains("src/deep/module199/file_199.rs"));
    }

    #[test]
    fn status_group_with_a_space_in_a_path_goes_one_path_per_line() {
        let raw = "On branch main\nChanges not staged for commit:\n\tmodified:   ok.rs\n\tmodified:   a b.txt\n\tdeleted:    gone.rs\n\nUntracked files:\n\tx.txt\n\ty.txt\n";
        let (out, _) = git_status(raw).unwrap();
        assert_eq!(out, "## main\n M ok.rs\n M a b.txt\n D gone.rs\n?? x.txt y.txt\n");
    }

    #[test]
    fn old_git_line_ending_warnings_are_filtered_too() {
        let old = "warning: CRLF will be replaced by LF in a.txt.\nThe file will have its original line endings in your working directory\nwarning: LF will be replaced by CRLF in b.txt.\nThe file will have its original line endings in your working directory\nfatal: boom\n";
        let mut old = old.to_string();
        drop_git_eol_warnings("git add .", &mut old);
        assert_eq!(old, "fatal: boom\n");
    }

    #[test]
    fn grep_states_the_total_match_count() {
        let raw = "src/a.rs:12:let x = 1;\nsrc/a.rs:40:let y = 2;\nsrc/b.rs:7:fn z() {}\n";
        let (out, _, _) = grep(raw).unwrap();
        assert!(out.starts_with("3 matches in 2 files\n"), "{out}");
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
        assert!(out.contains("test_boom:") && !out.contains("____"), "{out}");
        assert!(out.contains("> return 1 / x") && out.contains("E ZeroDivisionError"), "{out}");
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

    fn assert_has(out: &str, wants: &[&str]) {
        for w in wants {
            assert!(out.contains(w), "{w}: {out}");
        }
    }

    #[test]
    fn clippy_warnings_show_the_lint_on_the_header_and_drop_the_rest() {
        let raw = "    Checking lintcrate v0.1.0 (C:\\x\\lintcrate)\nwarning: unneeded `return` statement\n --> src\\lib.rs:5:5\n  |\n5 |     return v[0];\n  |     ^^^^^^^^^^^\n  |\n  = help: for further information visit https://rust-lang.github.io/rust-clippy/rust-1.95.0/index.html#needless_return\n  = note: `#[warn(clippy::needless_return)]` on by default\nhelp: remove `return`\n  |\n5 -     return v[0];\n5 +     v[0]\n  |\n\nwarning: writing `&Vec` instead of `&[_]` involves a new object where a slice will do\n --> src\\lib.rs:1:17\n  |\n1 | pub fn first(v: &Vec<i32>) -> i32 {\n  |                 ^^^^^^^^^\n  |\n  = help: for further information visit https://rust-lang.github.io/rust-clippy/rust-1.95.0/index.html#ptr_arg\n  = note: `#[warn(clippy::ptr_arg)]` on by default\nhelp: change this to\n  |\n1 - pub fn first(v: &Vec<i32>) -> i32 {\n1 + pub fn first(v: &[i32]) -> i32 {\n  |\n\nwarning: length comparison to zero\n --> src\\lib.rs:2:8\n  |\n2 |     if v.len() == 0 {\n  |        ^^^^^^^^^^^^ help: using `is_empty` is clearer and more explicit: `v.is_empty()`\n  |\n  = help: for further information visit https://rust-lang.github.io/rust-clippy/rust-1.95.0/index.html#len_zero\n  = note: `#[warn(clippy::len_zero)]` on by default\n\nwarning: `lintcrate` (lib) generated 3 warnings (run `cargo clippy --fix --lib -p lintcrate -- ` to apply 2 suggestions)\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.11s\n";
        let (out, omitted, clipped) = diagnostic(raw, 0, true).unwrap();
        assert_has(&out, &[
            "warning: unneeded `return` statement [clippy::needless_return]",
            "warning: length comparison to zero [clippy::len_zero]",
            "[clippy::ptr_arg]",
            "--> src\\lib.rs:5:5",
            "--> src\\lib.rs:1:17",
            "--> src\\lib.rs:2:8",
            "help: using `is_empty` is clearer and more explicit: `v.is_empty()`",
            "5 +     v[0]",
            "1 + pub fn first(v: &[i32]) -> i32 {",
            "generated 3 warnings",
        ]);
        for gone in ["for further information", "on by default", "Finished", "Checking", "return v[0]", "5 -", "^^^"] {
            assert!(!out.contains(gone), "{gone}: {out}");
        }
        assert!(!clipped && omitted == 0 && !out.contains("omitted"), "{out}");
    }

    #[test]
    fn clippy_implied_lint_note_names_the_lint() {
        let raw = "error: unneeded `return` statement\n --> a.rs:1:1\n  |\n1 | x\n  |\n  = note: `-D clippy::needless_return` implied by `-D warnings`\n  = help: to override `-D warnings` add `#[allow(clippy::needless_return)]`\n";
        let (out, _, _) = diagnostic(raw, 1, true).unwrap();
        assert!(out.contains("[clippy::needless_return]") && out.contains("--> a.rs:1:1"), "{out}");
        assert!(!out.contains("implied by"), "{out}");
    }

    #[test]
    fn rustc_errors_keep_codes_snippets_and_locations() {
        let raw = "   Compiling errcrate v0.1.0 (C:\\x)\nerror[E0308]: mismatched types\n --> src\\lib.rs:2:18\n  |\n2 |     let n: i32 = s;\n  |            ---   ^ expected `i32`, found `&str`\n  |            |\n  |            expected due to this\n\nerror[E0425]: cannot find function `undefined_fn` in this scope\n --> src\\lib.rs:7:5\n  |\n7 |     undefined_fn(3)\n  |     ^^^^^^^^^^^^ not found in this scope\n\nSome errors have detailed explanations: E0308, E0425.\nFor more information about an error, try `rustc --explain E0308`.\nerror: could not compile `errcrate` (lib) due to 2 previous errors\n";
        let (out, _, _) = diagnostic(raw, 101, true).unwrap();
        assert_has(&out, &[
            "error[E0308]: mismatched types",
            "error[E0425]: cannot find function `undefined_fn` in this scope",
            "--> src\\lib.rs:2:18",
            "--> src\\lib.rs:7:5",
            "expected `i32`, found `&str`",
            "expected due to this",
            "not found in this scope",
            "could not compile `errcrate` (lib) due to 2 previous errors",
        ]);
        assert!(!out.contains("Compiling") && !out.contains("detailed explanations") && !out.contains("--explain"), "{out}");
    }

    #[test]
    fn cargo_test_failure_shows_the_stdout_block_and_the_result_once() {
        let raw = "\nrunning 7 tests\ntest tests::adds_ok_0 ... ok\ntest tests::adds_broken ... FAILED\ntest tests::adds_ok_3 ... ok\n\nfailures:\n\n---- tests::adds_broken stdout ----\n\nthread 'tests::adds_broken' (19364) panicked at src\\lib.rs:41:9:\nassertion `left == right` failed: math is hard\n  left: 4\n right: 5\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\nfailures:\n    tests::adds_broken\n\ntest result: FAILED. 6 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n   Compiling okcrate v0.1.0 (C:\\x)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.31s\n     Running unittests src\\lib.rs (C:\\x\\okcrate-3.exe)\nerror: test failed, to rerun pass `--lib`\n";
        let (out, omitted, clipped) = diagnostic(raw, 101, false).unwrap();
        assert_has(&out, &[
            "---- tests::adds_broken stdout ----",
            "panicked at src\\lib.rs:41:9",
            "math is hard",
            "left: 4",
            "right: 5",
            "test result: FAILED. 6 passed; 1 failed",
        ]);
        assert_eq!(out.matches("adds_broken").count(), 2, "{out}"); // block heading + panic line
        for gone in ["... FAILED", "RUST_BACKTRACE", "failures:", "error: test failed", "Finished", "Running"] {
            assert!(!out.contains(gone), "{gone}: {out}");
        }
        assert!(!clipped && omitted == 0 && !out.contains("omitted"), "{out}");
    }

    #[test]
    fn cargo_test_without_stdout_block_keeps_the_failed_line_and_binary_clue() {
        let raw = "running 1 test\ntest slow ... FAILED\n\nfailures:\n    slow\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored\n\ntest result: ok. 3 passed; 0 failed; 0 ignored\nerror: test failed, to rerun pass `--test it`\n";
        let (out, _, _) = diagnostic(raw, 101, false).unwrap();
        assert_has(&out, &["test slow ... FAILED", "failures:", "    slow", "error: test failed, to rerun pass `--test it`"]);
    }

    #[test]
    fn pytest_failure_drops_banners_and_repeated_where_but_keeps_facts() {
        let raw = "============================= test session starts =============================\nplatform win32 -- Python 3.12.6, pytest-9.0.2, pluggy-1.6.0\nrootdir: C:\\x\\py\nplugins: anyio-4.12.1, asyncio-1.3.0\nasyncio: mode=Mode.STRICT, debug=False, asyncio_default_fixture_loop_scope=None\ncollected 24 items\n\ntest_calc.py .......................F                                    [100%]\n\n================================== FAILURES ===================================\n_______________________________ test_total_bad ________________________________\n\n    def test_total_bad():\n>       assert total([1, 2, 3]) == 7\nE       assert 6 == 7\nE        +  where 6 = total([1, 2, 3])\n\ntest_calc.py:75: AssertionError\n=========================== short test summary info ===========================\nFAILED test_calc.py::test_total_bad - assert 6 == 7\n +  where 6 = total([1, 2, 3])\n======================== 1 failed, 23 passed in 0.30s =========================\n";
        let (out, omitted, clipped) = diagnostic(raw, 1, false).unwrap();
        assert_has(&out, &[
            "test_calc.py::test_total_bad:",
            "> assert total([1, 2, 3]) == 7",
            "E assert 6 == 7",
            "E  +  where 6 = total([1, 2, 3])",
            "test_calc.py:75: AssertionError",
            "1 failed, 23 passed in 0.30s",
        ]);
        assert_eq!(out.matches("where 6 = total").count(), 1, "{out}");
        for gone in ["platform", "rootdir", "plugins", "asyncio", "collected", "====", "____", "[100%]", "short test summary"] {
            assert!(!out.contains(gone), "{gone}: {out}");
        }
        assert!(!clipped && omitted == 0 && !out.contains("omitted"), "{out}");
    }

    #[test]
    fn pytest_native_and_short_tracebacks_keep_every_frame_line() {
        let native = "=== FAILURES ===\n____ test_x ____\nTraceback (most recent call last):\n  File \"C:\\\\p\\\\test_x.py\", line 5, in test_x\n    assert f() == 2\nAssertionError: assert 1 == 2\n=== short test summary info ===\nFAILED test_x.py::test_x - AssertionError: assert 1 == 2\n=== 1 failed in 0.1s ===\n";
        let (out, omitted, clipped) = diagnostic(native, 1, false).unwrap();
        assert_has(&out, &["Traceback (most recent call last):", "File \"C:\\\\p\\\\test_x.py\", line 5, in test_x", "    assert f() == 2", "AssertionError: assert 1 == 2"]);
        assert!(!clipped && omitted == 0, "{out}");
        let short = "=== FAILURES ===\n____ test_y ____\ntest_y.py:7: in test_y\n    helper(2)\ntest_y.py:3: in helper\n    assert n == 3\nE   assert 2 == 3\n=== 1 failed in 0.1s ===\n";
        let (out, _, _) = diagnostic(short, 1, false).unwrap();
        assert_has(&out, &["test_y.py:7: in test_y", "    helper(2)", "test_y.py:3: in helper", "    assert n == 3", "E assert 2 == 3"]);
    }

    #[test]
    fn pytest_failed_line_stays_when_its_test_has_no_block_and_e_indent_is_relative() {
        let raw = "=== FAILURES ===\n____ TestC.test_a[1-2] ____\n\n    def test_a(self):\n>       assert x == y\nE       assert 1 == 2\nE         + extra\nE       - more\n\ntest_c.py:9: AssertionError\n=== short test summary info ===\nFAILED test_c.py::TestC::test_a[1-2] - assert 1 == 2\nFAILED test_c.py::test_other - boom\n=== 2 failed in 0.1s ===\n";
        let (out, _, _) = diagnostic(raw, 1, false).unwrap();
        assert_has(&out, &["test_c.py::TestC::test_a[1-2]:", "> assert x == y", "E assert 1 == 2", "E   + extra", "E - more", "FAILED test_c.py::test_other - boom"]);
        assert!(!out.contains("FAILED test_c.py::TestC"), "{out}");
    }

    #[test]
    fn should_panic_and_multiline_summary_failures_show_once() {
        let cargo = "test boom - should panic ... FAILED\n\nfailures:\n\n---- boom stdout ----\n\nthread 'boom' panicked at src/lib.rs:3:5:\nwrong\nnote: panic did not contain expected string\n\nfailures:\n    boom\n\ntest result: FAILED. 0 passed; 1 failed\n";
        let (out, _, _) = diagnostic(cargo, 101, false).unwrap();
        assert!(!out.contains("should panic ... FAILED") && out.contains("---- boom stdout ----"), "{out}");
        let py = "=== FAILURES ===\n____ test_d ____\n>       assert a == b\nE       AssertionError: assert {'b': 2} == {'b': 3}\nE         \nE         Differing items:\nE           {'b': 2} != {'b': 3}\n\ntest_d.py:6: AssertionError\n=== short test summary info ===\nFAILED test_d.py::test_d - AssertionError: assert {'b': 2} == {'b': 3}\n  \n  Differing items:\n    {'b': 2} != {'b': 3}\nSKIPPED [1] test_d.py:9: later\n=== 1 failed, 1 skipped in 0.1s ===\n";
        let (out, omitted, _) = diagnostic(py, 1, false).unwrap();
        assert_eq!(out.matches("Differing items").count(), 1, "{out}");
        // A blank `E` line stays in the run, so the explanation keeps its indent under the message.
        assert_has(&out, &["E AssertionError: assert {'b': 2} == {'b': 3}\nE\nE   Differing items:\nE     {'b': 2} != {'b': 3}\n", "SKIPPED [1] test_d.py:9: later"]);
        assert_eq!(omitted, 0, "{out}");
    }

    #[test]
    fn pytest_captured_stdout_stays() {
        let raw = "=== FAILURES ===\n____ test_p ____\n\n    def test_p():\n        print('hello from test')\n>       assert 0\nE       assert 0\n\ntest_p.py:4: AssertionError\n----- Captured stdout call -----\n  hello from test\n=== 1 failed in 0.1s ===\n";
        let (out, _, _) = diagnostic(raw, 1, false).unwrap();
        assert_has(&out, &["----- Captured stdout call -----", "  hello from test", "test_p.py:4: AssertionError"]);
        assert!(!out.contains("def test_p"), "{out}");
    }

    #[test]
    fn python_traceback_keeps_the_log_line_before_it() {
        let raw = "warning: something\nTraceback (most recent call last):\n  File \"a.py\", line 3, in <module>\n    boom()\nValueError: bad\n";
        let (out, _, _) = traceback(raw).unwrap();
        assert!(out.contains("Traceback"));
        assert!(out.contains("ValueError: bad"));
        assert!(out.contains("warning: something"));
    }

    #[test]
    fn node_stack_keeps_the_log_line_before_it() {
        let raw = "log line\nTypeError: x is not a function\n    at foo (a.js:1:2)\n    at bar (b.js:3:4)\n";
        let (out, _, _) = traceback(raw).unwrap();
        assert!(out.contains("TypeError"));
        assert!(out.contains("at foo"));
        assert!(out.contains("log line"));
    }

    #[test]
    fn node_keeps_the_file_and_line_above_the_caret() {
        let raw = "/proj/app.js:3\nconst x = ;\n          ^\n\nSyntaxError: Unexpected token ';'\n    at internalCompileFunction (node:internal/vm:76:18)\n";
        let (out, _, _) = traceback(raw).unwrap();
        assert!(out.starts_with("/proj/app.js:3\nconst x = ;\n"), "{out}");
    }

    #[test]
    fn json_is_minified() {
        let raw = "{\n  \"a\": 1,\n  \"b\": [1, 2]\n}\n";
        let (out, _) = json_min("curl http://x", raw).unwrap();
        assert_eq!(out.trim(), "{\"a\":1,\"b\":[1,2]}");
    }

    #[test]
    fn json_min_refuses_text_that_is_not_json() {
        // Padded so the whitespace saving is large: only the validity check can say no.
        let hole = format!("{{\"a\":{}}}", " ".repeat(200));
        let trailing = format!("{{\"a\":1{}}} x", " ".repeat(200));
        let truncated = format!("[1,{}2,", " ".repeat(200));
        for bad in [hole, trailing, truncated] {
            assert!(json_min("curl http://x", &bad).is_none(), "{bad:?}");
        }
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
        let (out, hidden) = paths(&raw, true).unwrap();
        assert!(out.starts_with("src/mod/{f0,f1,f2,"), "{out}");
        assert!(out.contains("f19}.rs\n"), "{out}");
        assert!(out.contains("\nsrc/a b.rs\nsrc/c,d.rs\n") && out.contains("docs\\{x.md,y.md}\n"));
        assert!(out.contains("single.rs/x") && out.contains("\nsrc/mod/\n") && hidden == 20);
        assert!(paths("a/b\nc/d\n", true).is_none() && paths(&"plain\n".repeat(20), true).is_none());
    }

    #[test]
    fn paths_of_one_directory_share_one_line_even_when_subdirs_interleave() {
        let mut raw = String::new();
        for (d, names) in [("src", &["a.rs", "b.rs", "c.rs"][..]), ("src/sub", &["m.rs", "n.rs", "o.rs"][..]), ("src", &["d.rs", "e.rs"][..])] {
            for n in names {
                raw.push_str(&format!("{d}/{n}\n"));
            }
        }
        raw.push_str("x/1\nx/2\nx/3\nx/4\nx/5\nx/6\n");
        let (out, _) = paths(&raw, true).unwrap();
        assert!(out.starts_with("src/{a,b,c,d,e}.rs\nsrc/sub/{m,n,o}.rs\n"), "{out}");
        // Sorted output keeps its order: only consecutive paths of one directory share a line.
        let (out, _) = paths(&raw, false).unwrap();
        assert!(out.starts_with("src/{a,b,c}.rs\nsrc/sub/{m,n,o}.rs\nsrc/{d.rs,e.rs}\n"), "{out}");
        let d = digest("rg --files --sort modified", &raw, "", 0).unwrap();
        assert!(d.stdout.starts_with("src/{a,b,c}.rs\nsrc/sub/"), "{}", d.stdout);
    }

    #[test]
    fn paths_do_not_factor_an_extension_that_is_missing_mixed_or_unsafe() {
        let mut raw = String::from("a/Makefile\na/x.rs\na/y.rs\nb/x.rs\nb/y.rs\nb/z.md\nc/.hidden\nc/.other\nc/{q}.rs\nc/r.rs\nc/s.rs\nd/p.rs\nd/q.rs\nd/r.rs\n");
        raw.push_str("e/a.b.rs\ne/c.rs\ne/d.rs\n");
        let (out, _) = paths(&raw, true).unwrap();
        assert!(out.contains("a/{Makefile,x.rs,y.rs}\n"), "{out}");
        assert!(out.contains("b/{x.rs,y.rs,z.md}\n"), "{out}");
        assert!(out.contains("c/.hidden\nc/.other\nc/{q}.rs\nc/r.rs\nc/s.rs\n"), "{out}");
        assert!(out.contains("d/{p,q,r}.rs\n") && out.contains("e/{a.b,c,d}.rs\n"), "{out}");
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
    fn a_timestamp_in_bare_grep_output_is_not_a_line_number() {
        let (p, n, c) = split_match("a.log:2024-01-01 12:30:45 ERROR x").unwrap();
        assert_eq!((p, n, c), ("a.log", "", "2024-01-01 12:30:45 ERROR x"));
        let raw: String = (0..100).map(|i| format!("logs/app.log:2024-01-01 12:{:02}:45 ERROR x\n", i % 60)).collect();
        let (out, _, _) = grep(&raw).unwrap();
        assert!(out.contains("logs/app.log (25 of 100)") && !out.contains(".log:2024-01-01 12"), "{out}");
    }

    #[test]
    fn git_status_keeps_ignored_files_out_of_untracked() {
        let raw = "On branch main\nUntracked files:\n  (use \"git add <file>...\" to include in what will be committed)\n\tnew.txt\n\nIgnored files:\n  (use \"git add -f <file>...\" to include in what will be committed)\n\ttarget/\n";
        let (out, _) = git_status(raw).unwrap();
        assert!(out.contains("?? new.txt"), "{out}");
        assert!(out.contains("!! target/"), "{out}");
        assert!(!out.contains("Ignored files:"), "{out}");
    }

    #[test]
    fn git_status_keeps_the_kind_of_change() {
        let raw = "On branch main\nChanges to be committed:\n\tnew file:   a.rs\n\trenamed:    b.rs -> c.rs\n\nChanges not staged for commit:\n\tmodified:   d.rs\n\tdeleted:    old.rs\n";
        let (out, _) = git_status(raw).unwrap();
        assert!(out.contains("A  a.rs\nR  b.rs -> c.rs\n"), "{out}");
        assert!(out.contains(" M d.rs\n D old.rs\n"), "{out}");
    }

    #[test]
    fn digest_replies_stay_within_the_output_budget() {
        // 1 MB of pretty JSON minifies to one huge line; 5000 commits condense to 5000 lines.
        let items: Vec<serde_json::Value> = (0..12_000).map(|i| serde_json::json!({"name": format!("pod-{i}"), "ready": true})).collect();
        let json = serde_json::to_string_pretty(&items).unwrap();
        let d = digest("curl http://x", &json, "", 0).unwrap();
        assert!(d.stdout.lines().all(|l| l.len() <= 4200) && d.lossy(), "json line of {} chars", d.stdout.len());
        let log: String = (0..5000)
            .map(|i| format!("commit {i:040x}\nAuthor: a <a@b.c>\nDate:   Tue Oct 14 12:00:00 2026 +0000\n\n    change {i}\n\n"))
            .collect();
        let d = digest("git log", &log, "", 0).unwrap();
        assert!(d.stdout.lines().count() <= 450 && d.lossy(), "{} lines", d.stdout.lines().count());
    }

    #[test]
    fn requested_test_output_survives_a_green_run() {
        let pass = "test_a.py::test_x hello-from-print\nPASSED\n===== 1 passed in 0.01s =====\n";
        for label in ["pytest -sv", "pytest -xvs", "pytest --capture=tee-sys", "vitest --reporter=verbose", "jest --verbose=true", "cargo nextest run --no-capture"] {
            assert!(digest(label, pass, "", 0).is_none(), "{label}");
        }
        // jest prints console.log of a passing test with no flag at all: dropping it must spool.
        let jest = "PASS src/a.test.js\n  console.log\n    hello-from-log\n\n      at Object.<anonymous> (src/a.test.js:3:11)\n\nTest Suites: 1 passed, 1 total\nTests:       1 passed, 1 total\nSnapshots:   0 total\nTime:        0.5 s\nRan all test suites.\n";
        if let Some(d) = digest("npx jest", jest, "", 0) {
            assert!(d.stdout.contains("hello-from-log") || d.lossy(), "{}", d.stdout);
        }
        // Progress noise alone is not worth a spool file.
        let noise: String = (0..50).map(|i| format!("test_a.py::test_{i} PASSED\n")).collect::<String>() + "===== 50 passed in 0.10s =====\n";
        let d = digest("pytest -k smoke", &noise, "", 0).unwrap();
        assert!(!d.lossy(), "{}", d.stdout);
    }

    #[test]
    fn a_passing_build_keeps_its_located_warnings() {
        let out = "  P -> C:\\p\\bin\\P.dll\nC:\\p\\Program.cs(3,13): warning CS0168: The variable 'x' is declared but never used [C:\\p\\P.csproj]\n\nBuild succeeded.\n    1 Warning(s)\n    0 Error(s)\n\nTime Elapsed 00:00:02.08\n";
        let d = digest("dotnet build", out, "", 0).unwrap();
        assert!(d.stdout.contains("warning CS0168") && d.stdout.contains("1 Warning(s)"), "{}", d.stdout);
    }

    #[test]
    fn a_bare_timestamp_is_not_a_grep_location() {
        assert!(split_match("2024-01-01 12:30:45 ERROR x").is_none());
        let raw: String = (0..60).map(|i| format!("2024-01-01 12:{i:02}:45 ERROR x\n")).collect();
        assert!(grep(&raw).is_none_or(|(o, _, _)| !o.contains("2024-01-01 12 (")));
        // a path with a space and a separator is still a path
        assert_eq!(split_match("My Docs/a.txt:3:x").map(|m| m.0), Some("My Docs/a.txt"));
    }

    #[test]
    fn a_redaction_note_inside_a_hunk_does_not_end_it() {
        let raw = "diff --git a/c.yml b/c.yml\n--- a/c.yml\n+++ b/c.yml\n@@ -1,1 +1,6 @@\n config:\n+    [REDACTED PRIVATE KEY]\n[qx] no END line within 200 lines; showing output again\n+  password_file: /etc/x\n";
        let (out, _, _) = git_diff(raw).unwrap();
        assert!(out.contains("+  password_file"), "{out}");
    }

    #[test]
    fn searching_for_a_runner_name_is_a_search_not_a_test_run() {
        let mut out: String = (0..40).map(|i| format!("src/a{i}.test.js:1:import {{ jest }} from '@jest/globals';\n")).collect();
        out.push_str("src/z.test.js:9:  expect(f).toThrow('Error: boom');\n");
        for label in ["rg -n jest", "grep -rn pytest .", "git grep vitest"] {
            if let Some(d) = digest(label, &out, "", 0) {
                assert_ne!(d.profile, "test", "{label}");
            }
        }
        assert!(is_test("python -m pytest -q") && is_test("npx jest") && is_test("cargo test"));
        assert!(is_test("npm run test:unit") && is_test("make tests"));
    }

    #[test]
    fn unittest_errors_keep_their_heading_and_frames() {
        let raw = "E\n======================================================================\nERROR: test_x (app.tests.T)\n----------------------------------------------------------------------\nTraceback (most recent call last):\n  File \"/app/tests.py\", line 5, in test_x\n    1/0\nZeroDivisionError: division by zero\n\n----------------------------------------------------------------------\nRan 1 test in 0.001s\n\nFAILED (errors=1)\n";
        let (out, _, _) = diagnostic(raw, 1, false).unwrap();
        assert!(out.contains("ERROR: test_x (app.tests.T)"), "{out}");
        assert!(out.contains("File \"/app/tests.py\", line 5"), "{out}");
    }

    #[test]
    fn a_cut_stderr_marks_the_digest_lossy() {
        let err: String = (0..300).map(|i| format!("warning: could not open directory 'd{i}/': Permission denied\n")).collect();
        let d = digest("git status", "On branch main\nnothing to commit, working tree clean\n", &err, 0).unwrap();
        assert!(d.lossy(), "{}", d.stderr);
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
        assert!(git_status("On branch main\nYou are currently rebasing.\n\tmodified:   x.rs\n").is_none());
        // A listed path before any section header has no known column.
        assert!(git_status("On branch main\n\tmodified:   x.rs\n").is_none());
    }

    #[test]
    fn status_nothing_added_is_not_an_untracked_file() {
        let raw = "On branch main\nUntracked files:\n  (use \"git add <file>...\" to include in what will be committed)\n\tnew.txt\n\nnothing added to commit but untracked files present (use \"git add\" to track)\n";
        let (out, _) = git_status(raw).unwrap();
        assert!(out.contains("?? new.txt"), "{out}");
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
        assert!(!green.lossy(), "a green run drops only `... ok` noise: no spool file");
        let mut red = pass.replace("test t3 ... ok", "test t3 ... FAILED");
        red.push_str("---- t3 stdout ----\nthread 't3' panicked at src/lib.rs:3:5:\nassertion failed: x\n\nfailures:\n    t3\n");
        let d = digest("cargo test", &red, "", 101).unwrap();
        assert!(!d.lossy() && d.omitted == 0, "the FAILED line and failures list repeat the stdout block: nothing lost");
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
        let (out, _) = paths(&raw, true).unwrap();
        assert!(out.contains("a,b/f0.txt") && !out.contains('{'), "{out}");
    }

    #[test]
    fn a_file_the_agent_reads_is_not_minified() {
        let raw = "{\n  \"a\": 1,\n  \"b\": [1, 2]\n}\n";
        assert!(json_min("cat package.json", raw).is_none());
        assert!(json_min("jq . x.json", raw).is_none());
        assert!(json_min("curl http://x", raw).is_some());
    }

    /// What the caller sees: the digest's two streams, or `None` (generic compaction keeps the text).
    fn reply(d: Option<Digest>) -> Option<String> {
        d.map(|d| format!("{}{}", d.stdout, d.stderr))
    }

    #[test]
    fn go_test_keeps_the_package_that_did_not_build_and_its_compiler_error() {
        let so = "--- FAIL: TestAdd (0.00s)\n    a_test.go:8: got 3, want 4\nFAIL\nFAIL\texample.com/m/a\t0.003s\nok  \texample.com/m/b\t(cached)\nFAIL\texample.com/m/c [build failed]\n";
        let se = "# example.com/m/c [example.com/m/c.test]\nc\\c_test.go:6:14: undefined: nope\n";
        let (out, _, _) = diagnostic(&merge(so, se), 1, false).unwrap();
        for want in ["--- FAIL: TestAdd", "FAIL\texample.com/m/a", "FAIL\texample.com/m/c [build failed]", "c_test.go:6:14: undefined: nope"] {
            assert!(out.contains(want), "{want}: {out}");
        }
        // Whatever the digest does with it, the reply must still say both packages failed.
        if let Some(r) = reply(digest("go test ./...", so, se, 1)) {
            assert!(r.contains("[build failed]") && r.contains("undefined: nope"), "{r}");
        }
    }

    #[test]
    fn node_test_runner_keeps_failing_test_names_and_the_counts() {
        let raw = "\n> demo@1.0.0 test\n> node --test\n\n\u{2714} passes (0.5ms)\n\u{2716} adds numbers (1.2ms)\n  AssertionError [ERR_ASSERTION]: Expected values to be strictly equal:\n  \n  3 !== 4\n  \n      at TestContext.<anonymous> (a.test.js:5:10)\n\u{2716} also fails (0.3ms)\n  Error: nope\n      at TestContext.<anonymous> (a.test.js:9:10)\n\u{2139} tests 3\n\u{2139} suites 0\n\u{2139} pass 1\n\u{2139} fail 2\n\u{2139} cancelled 0\n\u{2139} skipped 0\n\u{2139} todo 0\n\u{2139} duration_ms 45.1\n\n\u{2716} failing tests:\n\ntest at a.test.js:3:1\n\u{2716} adds numbers (1.2ms)\n  AssertionError [ERR_ASSERTION]: Expected values to be strictly equal:\n";
        let (out, _, _) = diagnostic(raw, 1, false).unwrap();
        for want in ["\u{2716} adds numbers", "test at a.test.js:3:1", "\u{2139} tests 3", "\u{2139} fail 2", "a.test.js:5:10"] {
            assert!(out.contains(want), "{want}: {out}");
        }
        assert!(!out.contains("passes (0.5ms)") && !out.contains("duration_ms"), "{out}");
    }

    #[test]
    fn mocha_dotnet_and_maven_failures_keep_names_counts_and_places() {
        let mocha = "\n> demo@1.0.0 test\n> mocha\n\n\n  Calculator\n    \u{2714} adds\n    1) subtracts\n\n\n  1 passing (5ms)\n  1 failing\n\n  1) Calculator\n       subtracts:\n\n      AssertionError: expected 1 to equal 2\n      + expected - actual\n\n      -1\n      +2\n      \n      at Context.<anonymous> (test/calc.js:9:30)\n";
        let (out, _, _) = diagnostic(mocha, 1, false).unwrap();
        for want in ["1) subtracts", "1 failing", "1 passing", "1) Calculator", "test/calc.js:9:30"] {
            assert!(out.contains(want), "{want}: {out}");
        }
        let dotnet = "  Passed Adds [2 ms]\n  Failed Subtracts [3 ms]\n  Error Message:\n   Assert.Equal() Failure\n  Stack Trace:\n     at CalcTests.Subtracts() in /src/CalcTests.cs:line 14\n\nFailed!  - Failed:     1, Passed:     1, Skipped:     0, Total:     2\n";
        let (out, _, _) = diagnostic(dotnet, 1, false).unwrap();
        for want in ["Failed Subtracts [3 ms]", "CalcTests.cs:line 14", "Failed!  - Failed:     1"] {
            assert!(out.contains(want), "{want}: {out}");
        }
        let mvn = "[INFO] Running com.x.CalcTest\n[ERROR] Tests run: 3, Failures: 1, Errors: 0, Skipped: 0, Time elapsed: 0.05 s <<< FAILURE!\n[ERROR] subtracts  Time elapsed: 0.01 s  <<< FAILURE!\norg.opentest4j.AssertionFailedError: expected: <2> but was: <1>\n\tat com.x.CalcTest.subtracts(CalcTest.java:12)\n[INFO] Results:\n[ERROR] Failures: \n[ERROR]   CalcTest.subtracts:12 expected: <2> but was: <1>\n";
        let (out, _, _) = diagnostic(mvn, 1, false).unwrap();
        for want in ["Tests run: 3, Failures: 1", "CalcTest.java:12", "CalcTest.subtracts:12"] {
            assert!(out.contains(want), "{want}: {out}");
        }
    }

    #[test]
    fn a_red_run_keeps_a_line_nobody_taught_the_digest_to_recognise() {
        // A made-up runner: the only line it knows is `FAILED`, yet the line naming the cause must stay.
        let raw = "Compiling x v0.1.0\nrunning 2 tests\nFAILED t1\nlib.rs:9:5: left != right (3, 4)\nconnection to db:5432 refused\n\tdetail: error code 7\n";
        let (out, _, _) = diagnostic(raw, 3, false).unwrap();
        for want in ["lib.rs:9:5: left != right", "connection to db:5432 refused", "error code 7"] {
            assert!(out.contains(want), "{want}: {out}");
        }
        assert!(!out.contains("Compiling x"), "{out}");
    }

    #[test]
    fn unknown_lines_of_a_red_run_never_push_out_the_failure() {
        let log = |a: usize, b: usize| (a..b).map(|i| format!("log {i}\n")).collect::<String>();
        let raw = format!("{}thread 't1' panicked at src/lib.rs:9:5\n{}", log(0, 300), log(300, 330));
        let (out, _, clipped) = diagnostic(&raw, 101, false).unwrap();
        assert!(out.contains("panicked at src/lib.rs:9:5"), "{out}");
        assert!(clipped);
    }
    #[test]
    fn a_log_line_before_a_python_traceback_is_kept() {
        let se = "INFO:root:connecting\nERROR:root:database refused connection at 10.0.0.7:5432 (attempt 3 of 3)\nTraceback (most recent call last):\n  File \"job.py\", line 9, in <module>\n    run()\nRuntimeError: job aborted\n";
        let (_, e, _, _) = runtime_digest("", se, 1).unwrap();
        assert!(e.contains("ERROR:root:database refused connection"), "{e}");
        assert!(e.contains("RuntimeError: job aborted"), "{e}");
        // Many log lines: the traceback and the nearest lines stay, and the cut says so (so it is spooled).
        let many: String = (0..100u8)
            .map(|i| format!("log {}{} of the batch\n", (b'a' + i % 26) as char, (b'a' + i / 26) as char))
            .collect();
        let (_, e, _, clipped) = runtime_digest("", &format!("{many}{se}"), 1).unwrap();
        assert!(e.contains("database refused") && e.contains("RuntimeError: job aborted"), "{e}");
        assert!(clipped, "a cut that is not flagged is never spooled");
        assert!(e.lines().count() < 100, "{}", e.lines().count());
    }

    #[test]
    fn a_search_hit_cut_at_500_chars_is_flagged_and_keeps_its_tail() {
        let hit = |n: usize| format!("src/a.rs:{n}:{}needle_here{}", "x".repeat(1215), "y".repeat(300));
        let raw = format!("{}\n{}\nsrc/b.rs:3:short\n", hit(1), hit(2));
        let (out, _, clipped) = grep(&raw).unwrap();
        assert!(clipped, "a cut hit must make the digest lossy so it is spooled");
        assert!(out.contains(&"x".repeat(200)) && out.contains(&"y".repeat(200)) && out.contains(" … "), "head and tail stay: {out}");
        assert!(out.contains("long lines cut") && !out.contains("full text"), "{out}");
        let d = digest("rg -n needle_here .", &raw, "", 0).unwrap();
        assert!(d.lossy());
    }

    #[test]
    fn git_log_with_a_patch_stat_reflog_or_parents_is_left_alone() {
        let head = "commit 1111111111111111111111111111111111111111\nAuthor: A <a@b.c>\nDate:   Mon Oct 19 10:00:00 2026 +0000\n\n    subject one\n";
        let patch = format!("{head}\ndiff --git a/x.txt b/x.txt\nindex 111..222 100644\n--- a/x.txt\n+++ b/x.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+c\n d\n");
        assert!(git_log(&patch).is_none(), "-U1 patch");
        let stat = format!("{head}\n x.txt | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)\n\ndiff --git a/x.txt b/x.txt\n--- a/x.txt\n+++ b/x.txt\n@@ -1 +1 @@\n-b\n+c\n");
        assert!(git_log(&stat).is_none(), "--patch-with-stat");
        let reflog = "commit 1111111111111111111111111111111111111111 (HEAD -> main)\nReflog: HEAD@{0} (A <a@b.c>)\nReflog message: commit: subject one\nAuthor: A <a@b.c>\nDate:   Mon Oct 19 10:00:00 2026 +0000\n\n    subject one\n".to_string();
        assert!(git_log(&reflog).is_none(), "-g");
        let parents = "commit 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222\nAuthor: A <a@b.c>\nDate:   Mon Oct 19 10:00:00 2026 +0000\n\n    subject one\n".to_string();
        assert!(git_log(&parents).is_none(), "--parents");
        // The default shape (decorations, Merge:, message bodies) still condenses.
        let plain = "commit 1111111111111111111111111111111111111111 (HEAD -> main, origin/main)\nMerge: 222 333\nAuthor: A <a@b.c>\nDate:   Mon Oct 19 10:00:00 2026 +0000\n\n    subject one\n\n    body line\n";
        assert!(git_log(plain).is_some());
        // Each of the four commands reaches the reply whole: the digest declines, generic compaction keeps the patch.
        for (label, text) in [("git log -2 -U1", &patch), ("git log -2 --patch-with-stat", &stat), ("git log -g -3", &reflog), ("git log -2 --parents", &parents)] {
            assert!(digest(label, text, "", 0).is_none(), "{label}");
        }
    }

    #[test]
    fn a_status_note_is_not_an_untracked_file() {
        let raw = "On branch main\nChanges not staged for commit:\n  (use \"git add <file>...\" to update what will be committed)\n\tmodified:   a.txt\n\nUntracked files:\n  (use \"git add <file>...\" to include in what will be committed)\n\tnew1.txt\n\tnew2.txt\n\nYour stash currently has 1 entry\nno changes added to commit (use \"git add\" and/or \"git commit -a\")\n";
        let (out, _) = git_status(raw).expect("the stash note is state, not a reason to decline");
        assert!(out.contains("?? new1.txt new2.txt") && !out.contains("new3"), "{out}");
        assert!(out.contains("Your stash currently has 1 entry"), "{out}");
        assert!(!out.contains("  Your stash"), "{out}");
        // A note this digest does not know declines rather than becoming a file.
        let slow = raw.replace("Your stash currently has 1 entry", "It took 5.12 seconds to enumerate untracked files.");
        assert!(git_status(&slow).is_none());
    }
}
