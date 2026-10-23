//! `qx hook claude`: Claude Code's PreToolUse hook. It routes the commands Claude gives its own
//! Bash and PowerShell tools through qx, so the model gets qx's guard, digests and fast paths
//! without having to remember to type `qx`.
//!
//! A command whose output goes straight back to the model gets `qx ` (captured and compacted).
//! A command that feeds a pipe, a file or a loop gets `qx --stream ` (its output passes through
//! untouched) when qx starts it faster than the shell does. `rtk proxy X` and the programs rtk
//! only passes through (Python, PowerShell) run on qx instead. Shell builtins, servers, and
//! commands a deny or ask rule names stay exactly as written. When rtk's hook also rewrites the
//! line, this one builds on rtk's version: Claude Code applies one hook's rewrite per call, and
//! either result is then correct.

use serde_json::{json, Value};
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn run(args: &[String]) -> i32 {
    if args.first().map(String::as_str) != Some("claude") {
        eprintln!("usage: qx hook claude   (Claude Code PreToolUse hook; reads the event on stdin)");
        return 2;
    }
    // A broken hook must never block the tool call: on any doubt, print nothing and exit 0.
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 0;
    }
    if let Some(out) = serde_json::from_str::<Value>(&input).ok().and_then(|e| respond(&e)) {
        println!("{out}");
    }
    0
}

fn respond(event: &Value) -> Option<Value> {
    let tool = event["tool_name"].as_str()?;
    let input = event["tool_input"].as_object()?;
    // A background command's output is read while it runs; qx would hold it until the end.
    if input.get("run_in_background").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let command = input.get("command")?.as_str()?;
    let mut rewritten = match tool {
        "Bash" => rewrite_bash(command)?,
        "PowerShell" => rewrite_powershell(command)?,
        _ => return None,
    };
    let rules = Rules::load(event["cwd"].as_str().map(Path::new))?;
    if rules.restricts(tool, command) || !qx_on_path() {
        return None;
    }
    let mut base = command.to_string();
    if tool == "Bash" && rules.rtk {
        // rtk only swaps a program for its own wrapper, so a line this hook leaves alone stays
        // alone after rtk: asking rtk only now keeps its cost off every other command.
        match rtk_rewrite(command) {
            Rtk::Keep => {}
            Rtk::Rewritten(line) => {
                rewritten = rewrite_bash(&line)?;
                base = line;
            }
            Rtk::Unknown => return None,
        }
    }
    // Claude Code checks permissions on the rewritten line: `Bash(qx:*)` would auto-approve a
    // `qx git push` that used to prompt, and `Bash(git status:*)` would stop covering `qx git status`.
    let bypass = event["permission_mode"].as_str() == Some("bypassPermissions");
    if rules.restricts(tool, &rewritten) || !(bypass || rules.same_answer(tool, &base, &rewritten)) {
        return None;
    }
    let mut updated = input.clone();
    updated.insert("command".into(), Value::String(rewritten));
    Some(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "updatedInput": updated}}))
}

/// Builtins and keywords run inside the shell for free (and `cd`/`export` must stay there);
/// wrappers run another command qx cannot see; editors and pagers wait for a terminal.
const BASH_WORDS: &[&str] = &[
    ".", ":", "alias", "bg", "bind", "break", "builtin", "caller", "case", "cd", "command",
    "compgen", "complete", "continue", "coproc", "declare", "dirs", "disown", "do", "done", "echo",
    "elif", "else", "enable", "esac", "eval", "exec", "exit", "export", "false", "fc", "fg", "fi",
    "for", "function", "getopts", "hash", "help", "history", "if", "in", "jobs", "kill", "let",
    "local", "logout", "mapfile", "popd", "printf", "pushd", "pwd", "read", "readarray",
    "readonly", "return", "select", "set", "shift", "shopt", "source", "suspend", "test", "then",
    "time", "times", "trap", "true", "type", "typeset", "ulimit", "umask", "unalias", "unset",
    "until", "wait", "while", "qx", "rtk",
];

/// Wrappers run another command qx cannot see; editors, pagers and `ssh` wait for a terminal.
const WRAPPERS: &[&str] = &[
    "env", "nice", "nohup", "timeout", "xargs", "sudo", "stdbuf", "watch", "less", "more", "vi", "vim",
    "nano", "ssh", "top", "htop", "man",
];

/// Words that can define a function or alias the shell runs instead of the program of that name.
const SHADOWING_WORDS: &[&str] = &["function", "eval", "source", "."];

/// Reserved words that run the command after them (`if python x.py; then`).
const LEADING_WORDS: &[&str] = &["!", "do", "then", "else", "elif", "if", "while", "until", "time", "(", "{"];

/// Compound commands, subshells and groups: the output of a command inside one may be piped or
/// redirected as a whole after its closing word, which a stage-by-stage scan cannot see.
const COMPOUND_WORDS: &[&str] = &["for", "while", "until", "if", "select", "(", "{"];

/// One pipeline stage of a Bash line and where its output goes.
struct Stage {
    words: Vec<Range<usize>>,
    /// Nothing stands between this stage's output and the model: no pipe, no redirect but `2>&1`.
    to_model: bool,
    /// Reads the previous stage's output.
    piped_in: bool,
    background: bool,
    /// Where the stage's text ends, its redirects included.
    end: usize,
}

impl Default for Stage {
    fn default() -> Self {
        Stage { words: Vec::new(), to_model: true, piped_in: false, background: false, end: 0 }
    }
}

#[derive(Default)]
struct Scan {
    stages: Vec<Stage>,
    cur: Stage,
    word: Option<usize>,
    /// The word being read names a redirect's file, not an argument.
    target: bool,
}

impl Scan {
    fn end_word(&mut self, at: usize) {
        if let Some(start) = self.word.take() {
            if !std::mem::take(&mut self.target) {
                self.cur.words.push(start..at);
            }
        }
    }

    /// `pipe`: the next stage reads this one's output.
    fn end_stage(&mut self, at: usize, pipe: bool) {
        self.end_word(at);
        self.target = false;
        if pipe {
            self.cur.to_model = false;
        }
        self.cur.end = at;
        let next = Stage { piped_in: pipe, ..Stage::default() };
        self.stages.push(std::mem::replace(&mut self.cur, next));
    }
}

/// `cmd` with `qx ` before each stage qx should run, or `None` when nothing qualifies or the
/// line uses syntax this scanner does not follow (substitutions, groups, functions).
fn rewrite_bash(cmd: &str) -> Option<String> {
    Some(apply(cmd, bash_edits(cmd)?))
}

fn bash_edits(cmd: &str) -> Option<Vec<(Range<usize>, String)>> {
    let stages = bash_stages(cmd)?;
    let first = |s: &Stage| {
        // `time ( ... ) | wc` and `! { ...; }` still pipe the group as a whole.
        let mut words = s.words.iter().map(|r| &cmd[r.clone()]);
        words.find(|w| !is_assignment(w) && !matches!(*w, "!" | "time" | "-p"))
    };
    if stages.iter().any(|s| first(s).is_some_and(|w| SHADOWING_WORDS.contains(&w))) {
        return None;
    }
    let compound = stages.iter().any(|s| first(s).is_some_and(|w| COMPOUND_WORDS.contains(&w)));
    let edits: Vec<(Range<usize>, String)> = stages.iter().filter_map(|s| plan(cmd, s, compound)).collect();
    (!edits.is_empty()).then_some(edits)
}

fn apply(cmd: &str, edits: Vec<(Range<usize>, String)>) -> String {
    let mut out = String::with_capacity(cmd.len() + 16 * edits.len());
    let mut last = 0;
    for (range, text) in edits {
        out.push_str(&cmd[last..range.start]);
        out.push_str(&text);
        last = range.end;
    }
    out.push_str(&cmd[last..]);
    out
}

fn bash_stages(cmd: &str) -> Option<Vec<Stage>> {
    // Each `(` can nest one recursion of `scan`; deep nesting would overflow the stack.
    if cmd.bytes().filter(|&c| c == b'(').count() > 256 {
        return None;
    }
    let (stages, end) = scan(cmd, 0, false)?;
    (end == cmd.len()).then_some(stages)
}

/// The stages from `i` on and where the scan stopped: the end of the line, or with `nested`
/// just past the `)` that closes a substitution.
fn scan(cmd: &str, mut i: usize, nested: bool) -> Option<(Vec<Stage>, usize)> {
    let b = cmd.as_bytes();
    let mut s = Scan::default();
    // Here-documents whose bodies start after the current line: (delimiter, strip leading tabs).
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    // Open subshells of the line itself.
    let mut depth = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' => {
                s.word.get_or_insert(i);
                i += 2 + cmd[i + 1..].find('\'')?;
            }
            b'"' => {
                s.word.get_or_insert(i);
                i = skip_double_quoted(cmd, i)?;
            }
            b'`' => {
                s.word.get_or_insert(i);
                i = skip_backticks(b, i)?;
            }
            b'$' if b.get(i + 1) == Some(&b'(') => {
                s.word.get_or_insert(i);
                i = skip_substitution(cmd, i + 2)?;
            }
            b'$' if b.get(i + 1) == Some(&b'{') => {
                s.word.get_or_insert(i);
                i = skip_parameter(cmd, i + 2)?;
            }
            b'\\' if b.get(i + 1) == Some(&b'\n') && s.word.is_none() => i += 2,
            b'\\' => {
                s.word.get_or_insert(i);
                i += 2;
            }
            // Inside a substitution a subshell is only skipped; in the line itself its output
            // could be piped or redirected as a whole, which this scan does not follow.
            b'(' if nested && s.word.is_none() => i = skip_substitution(cmd, i + 1)?,
            b')' if nested => {
                s.end_stage(i, false);
                return heredocs.is_empty().then_some((s.stages, i + 1));
            }
            // `((x))` is arithmetic, one word to this scan.
            b'(' if s.word.is_none() && b.get(i + 1) == Some(&b'(') => {
                s.word = Some(i);
                i = skip_substitution(cmd, i + 1)?;
            }
            // `( a; b )` and `{ a; b; }`: their own words (compound, see `COMPOUND_WORDS`), so
            // the commands inside are found. `{}` and `a{1,2}` are plain words.
            b'(' if s.word.is_none() && !s.target && s.cur.words.iter().all(|r| LEADING_WORDS.contains(&&cmd[r.clone()])) => {
                s.cur.words.push(i..i + 1);
                depth += 1;
                i += 1;
            }
            b'{' if s.word.is_none() && !s.target && matches!(b.get(i + 1), None | Some(b' ' | b'\t' | b'\r' | b'\n')) => {
                s.cur.words.push(i..i + 1);
                i += 1;
            }
            b')' if depth > 0 => {
                depth -= 1;
                s.end_stage(i, false);
                i += 1;
            }
            b'(' | b')' => return None,
            b'#' if s.word.is_none() => i += cmd[i..].find('\n').unwrap_or(b.len() - i),
            b' ' | b'\t' | b'\r' => {
                s.end_word(i);
                i += 1;
            }
            b'\n' => {
                // After `|`, `&&` or `||` the command goes on on the next line.
                if s.word.is_some() || !s.cur.words.is_empty() {
                    s.end_stage(i, false);
                }
                i = skip_heredocs(cmd, i + 1, &mut heredocs)?;
            }
            b';' => {
                s.end_stage(i, false);
                i += 1;
            }
            b'&' | b'|' if b.get(i + 1) == Some(&c) => {
                s.end_stage(i, false);
                i += 2;
            }
            b'|' => {
                s.end_stage(i, true);
                i += if b.get(i + 1) == Some(&b'&') { 2 } else { 1 };
            }
            b'&' if b.get(i + 1) == Some(&b'>') => {
                // `&>file`, `&>>file`: both streams to a file.
                s.end_word(i);
                s.cur.to_model = false;
                s.target = true;
                i += if b.get(i + 2) == Some(&b'>') { 3 } else { 2 };
            }
            b'&' => {
                s.cur.to_model = false;
                s.cur.background = true;
                s.end_stage(i, false);
                i += 1;
            }
            b'<' | b'>' => {
                // An fd number written against the operator (`2>`) belongs to the redirect.
                let fd = s.word.filter(|&w| cmd[w..i].bytes().all(|d| d.is_ascii_digit()));
                if fd.is_some() {
                    s.word = None;
                } else {
                    s.end_word(i);
                }
                let fd = fd.map(|w| &cmd[w..i]);
                i = if c == b'<' { input_redirect(cmd, i, &mut s, &mut heredocs)? } else { output_redirect(b, i, fd, &mut s) };
            }
            _ => {
                s.word.get_or_insert(i);
                i += 1;
            }
        }
    }
    if nested || depth > 0 || !heredocs.is_empty() {
        return None;
    }
    s.end_stage(b.len(), false);
    Some((s.stages, b.len()))
}

/// The index just past a double-quoted string starting at `i`.
fn skip_double_quoted(cmd: &str, mut i: usize) -> Option<usize> {
    let b = cmd.as_bytes();
    i += 1;
    loop {
        match *b.get(i)? {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            b'`' => i = skip_backticks(b, i)?,
            b'$' if b.get(i + 1) == Some(&b'(') => i = skip_substitution(cmd, i + 2)?,
            _ => i += 1,
        }
    }
}

/// The index just past the backquoted command starting at `i`.
fn skip_backticks(b: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    loop {
        match *b.get(i)? {
            b'\\' => i += 2,
            b'`' => return Some(i + 1),
            _ => i += 1,
        }
    }
}

/// The index just past the `)` that closes the substitution (or subshell) whose body starts at
/// `i`. Its commands have run by the time qx starts, so they are only skipped.
fn skip_substitution(cmd: &str, i: usize) -> Option<usize> {
    let (stages, end) = scan(cmd, i, true)?;
    // A `case` pattern's `)` would have ended the scan early.
    let case = stages.iter().flat_map(|s| &s.words).any(|r| &cmd[r.clone()] == "case");
    (!case).then_some(end)
}

/// The index just past the `}` of a `${...}` whose body starts at `i`; `None` for a body that
/// nests quotes or further expansions.
fn skip_parameter(cmd: &str, i: usize) -> Option<usize> {
    let close = i + cmd[i..].find('}')?;
    (!cmd[i..close].contains(['{', '$', '`', '\'', '"', '\n'])).then_some(close + 1)
}

/// `<file`, `<<<word` and `<<DELIM`: input only, so where the output goes is unchanged.
fn input_redirect(cmd: &str, mut i: usize, s: &mut Scan, heredocs: &mut Vec<(String, bool)>) -> Option<usize> {
    let b = cmd.as_bytes();
    if b[i..].starts_with(b"<<<") {
        s.target = true;
        return Some(i + 3);
    }
    if !b[i..].starts_with(b"<<") {
        // `<&`, `<>`, `<(`: rare, and not worth following.
        if matches!(b.get(i + 1), Some(b'&' | b'>' | b'(')) {
            return None;
        }
        s.target = true;
        return Some(i + 1);
    }
    i += 2;
    let strip = b.get(i) == Some(&b'-');
    i += usize::from(strip);
    while matches!(b.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    // The delimiter is the word with its quoting removed.
    let mut delim = String::new();
    while let Some(&c) = b.get(i) {
        match c {
            b'\'' | b'"' => {
                let close = i + 1 + cmd[i + 1..].find(c as char)?;
                delim.push_str(&cmd[i + 1..close]);
                i = close + 1;
            }
            b'\\' => i += 1,
            b' ' | b'\t' | b'\r' | b'\n' | b';' | b'&' | b'|' | b'<' | b'>' | b'(' | b')' => break,
            _ => {
                let len = cmd[i..].chars().next()?.len_utf8();
                delim.push_str(&cmd[i..i + len]);
                i += len;
            }
        }
    }
    if delim.is_empty() {
        return None;
    }
    heredocs.push((delim, strip));
    Some(i)
}

/// `>file`, `>>file`, `>|file`, `N>&M`: any of them sends output elsewhere than the model,
/// except `2>&1`, which only joins the two streams the model reads anyway.
fn output_redirect(b: &[u8], mut i: usize, fd: Option<&str>, s: &mut Scan) -> usize {
    i += 1;
    if matches!(b.get(i), Some(b'>' | b'|')) {
        i += 1;
    }
    if b.get(i) == Some(&b'&') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(|c| c.is_ascii_digit() || *c == b'-') {
            i += 1;
        }
        let dup = &b[start..i];
        if dup.is_empty() {
            s.target = true;
        }
        if !(fd == Some("2") && dup == b"1") {
            s.cur.to_model = false;
        }
        return i;
    }
    s.cur.to_model = false;
    s.target = true;
    i
}

/// Past the bodies of the here-documents opened on the line that just ended.
fn skip_heredocs(cmd: &str, mut i: usize, heredocs: &mut Vec<(String, bool)>) -> Option<usize> {
    for (delim, strip) in heredocs.drain(..) {
        loop {
            if i >= cmd.len() {
                return None;
            }
            let end = cmd[i..].find('\n').map_or(cmd.len(), |n| i + n);
            let line = cmd[i..end].trim_end_matches('\r');
            let line = if strip { line.trim_start_matches('\t') } else { line };
            i = (end + 1).min(cmd.len());
            if line == delim {
                break;
            }
        }
    }
    Some(i)
}

/// The edit that routes one stage through qx: `qx ` (or `qx --stream `) before its program,
/// replacing a leading `timeout N` with qx's own `--timeout`.
fn plan(cmd: &str, stage: &Stage, compound: bool) -> Option<(Range<usize>, String)> {
    if stage.background {
        return None;
    }
    let mut words = stage.words.iter().map(|r| (r.start, &cmd[r.clone()]));
    let (start, mut name) = words.find(|(_, w)| !is_assignment(w) && !LEADING_WORDS.contains(w))?;
    let mut at = start;
    let mut timeout = None;
    if name == "timeout" {
        // qx kills the whole tree and still prints what it captured, with timeout's exit 124.
        timeout = Some(timeout_secs(words.next()?.1)?);
        (at, name) = words.next()?;
    }
    // rtk runs `rtk proxy X`, and Python and PowerShell, which it has no filter for, untouched,
    // only slower.
    let (mut via_rtk, mut raw) = (false, false);
    if name == "rtk" {
        via_rtk = true;
        let (next_at, next) = words.next()?;
        if next == "proxy" {
            raw = true;
            (at, name) = words.next()?;
        } else if python_name(next) || program_name(next).is_some_and(|n| n.eq_ignore_ascii_case("powershell")) {
            (at, name) = (next_at, next);
        } else {
            return None;
        }
    }
    let args: Vec<&str> = words.map(|(_, w)| w).collect();
    let program = program_name(name)?;
    if BASH_WORDS.contains(&program)
        || WRAPPERS.contains(&program)
        || crate::SUBCOMMANDS.contains(&program)
        || long_running(program, &args)
    {
        return None;
    }
    // Whole: the model reads this command's own output, which qx captures and compacts. `rtk
    // proxy` asked for the exact bytes.
    let whole = stage.to_model && !stage.piped_in && !compound && !raw;
    // Git Bash converts `/x/y` arguments when it starts a native program such as qx or rtk, but
    // not when it starts one of its own MSYS tools, which qx's built-ins stand in for.
    // A `$var` or `` `cmd` `` may expand to such a path, which only the shell sees.
    let same_args = via_rtk || !msys_tool(program) || !args.iter().any(|a| msys_converts(a) || a.contains(['$', '`']));
    // Streamed output gains only where qx starts the program faster than the shell (or rtk) does.
    let ok = accelerated(name) || ((whole || via_rtk) && plain_name(name) && known_program(name) && same_args);
    if !ok {
        return None;
    }
    let mut text = String::from(if whole { "qx " } else { "qx --stream " });
    if let Some(secs) = timeout {
        text.push_str(&format!("--timeout {secs} "));
    }
    Some((start..at, text))
}

/// `timeout`'s duration in whole seconds: `30`, `30s`, `5m`, `1h`. Options and fractions are
/// left to the real `timeout`.
fn timeout_secs(dur: &str) -> Option<u64> {
    let (num, unit) = match dur.char_indices().last()? {
        (i, c) if c.is_ascii_alphabetic() => (&dur[..i], c),
        _ => (dur, 's'),
    };
    if num.is_empty() || !num.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let scale = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => return None,
    };
    let secs = num.parse::<u64>().ok()?.checked_mul(scale)?;
    (secs > 0).then_some(secs)
}

/// The program a word names, quotes removed; `None` for anything the shell would expand.
fn program_name(word: &str) -> Option<&str> {
    let w = match word.as_bytes() {
        [q @ (b'\'' | b'"'), .., last] if last == q => &word[1..word.len() - 1],
        _ => word,
    };
    (!w.is_empty() && !w.contains(['$', '`', '*', '?', '\'', '"', '~'])).then_some(w)
}

/// A program qx runs itself: a built-in, or one it resolves the way the shell would.
/// A name it cannot find may be a shell alias or function, which only the shell knows.
fn known_program(name: &str) -> bool {
    use crate::resolve::{resolve, Target};
    crate::builtin::is_builtin(name) || matches!(resolve(name, None), Target::Direct { .. } | Target::Shim(_))
}

/// Whether qx starts this program measurably faster than the shell does, which pays for
/// qx's own ~10 ms even where qx only passes the output through: Python (startup hooks
/// stubbed, venv redirector skipped), npm/npx/pnpm scripts (no npm CLI), rustup proxies
/// (straight to the toolchain), Windows PowerShell (no profile), and npm or pip shims
/// that qx runs on their interpreter directly.
fn accelerated(word: &str) -> bool {
    use crate::resolve::{resolve, Target};
    let Some(name) = program_name(word) else { return false };
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).to_ascii_lowercase();
    let stem = base.strip_suffix(".exe").unwrap_or(&base);
    let path = base.len() != name.len();
    if python_name(name) {
        // A path is resolved against the cwd the line has by then, which only the shell knows.
        return path || !matches!(resolve(name, None), Target::Missing);
    }
    if path || !plain_name(name) {
        return false;
    }
    let named = stem == "powershell"
        || crate::npm::is_npm(name)
        || crate::npm::is_npx(name)
        || crate::npm::is_pnpm(name)
        || crate::resolve::rustup_tool(name).is_some();
    match resolve(name, None) {
        Target::Missing => false,
        Target::Direct { prefix, .. } => named || !prefix.is_empty(),
        _ => named,
    }
}

fn python_name(word: &str) -> bool {
    program_name(word).is_some_and(|n| n.rsplit(['/', '\\']).next().unwrap_or(n).to_ascii_lowercase().starts_with("python"))
}

/// Git Bash starts these without converting `/x/y` arguments: its own MSYS tools, and the
/// coreutils qx's built-ins stand in for.
fn msys_tool(program: &str) -> bool {
    crate::builtin::is_builtin(program) || crate::resolve::msys_tool(program)
}

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(k, _)| {
        k.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// A bare program name: no path, quote, variable or glob for qx to resolve differently.
fn plain_name(name: &str) -> bool {
    !name.starts_with('-') && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.+".contains(c))
}

/// Servers, watchers and followers stream until stopped; qx shows output only at exit.
/// ponytail: word heuristic; a false hit only leaves the command unwrapped.
fn long_running(name: &str, args: &[&str]) -> bool {
    const SERVERS: &[&str] = &[
        "vite", "nodemon", "uvicorn", "gunicorn", "live-server", "http-server", "http.server", "streamlit",
        "jupyter", "tensorboard", "flask", "storybook", "webpack-dev-server",
    ];
    const VERBS: &[&str] = &["dev", "start", "serve", "server", "runserver", "watch", "preview", "up"];
    // `-f` anywhere in a short-option cluster: `tail -fn 20`, `tail -qf`.
    let follow = matches!(name, "tail" | "docker" | "docker-compose" | "podman" | "kubectl" | "journalctl")
        && args.iter().any(|a| {
            a.starts_with("--follow") || (a.starts_with('-') && !a.starts_with("--") && a.contains(['f', 'F']))
        });
    // Subcommands and modules come first (`npm run web:dev`, `python -m uvicorn`, `jest --watchAll`);
    // later words are files, scripts and values (`--device`, `server/Cargo.toml`).
    follow
        || SERVERS.contains(&name)
        || args.iter().take(4).any(|a| {
            let a = a.trim_start_matches('-');
            SERVERS.contains(&a) || a.starts_with("watch") || a.split(':').any(|v| VERBS.contains(&v))
        })
        || (args.contains(&"-w") && (name == "tsc" || args.first() == Some(&"tsc")))
        || (name == "ping" && args.iter().any(|a| a.eq_ignore_ascii_case("-t")))
}

/// Git Bash rewrites `/x/y` arguments into Windows paths when it starts a native program such as
/// qx, but not when it starts its own tools (sed, awk, grep). `/c/Users/...` becomes
/// `C:/Users/...`, the same file, so a drive path whose first folder exists is safe; any other
/// leading `/` (a sed script such as `/x/p`, a regex, `/dev/null`) could change meaning.
fn msys_converts(arg: &str) -> bool {
    let a = arg.trim_start_matches(['\'', '"']);
    let Some(rest) = a.strip_prefix('/') else {
        return a.contains("=/");
    };
    let drive_path = rest
        .split_once('/')
        .filter(|(d, _)| d.len() == 1 && d.bytes().all(|c| c.is_ascii_alphabetic()))
        .is_some_and(|(d, tail)| Path::new(&format!("{d}:/{}", tail.split('/').next().unwrap_or(""))).exists());
    !drive_path
}

/// PowerShell words that are not external programs, including the default aliases that shadow
/// a same-named exe (`sort`, `where`, `curl`) and Git's coreutils (`ls`, `cat`, `rm`).
const POWERSHELL_WORDS: &[&str] = &[
    "begin", "break", "catch", "cat", "cd", "chdir", "class", "clear", "cls", "compare", "continue",
    "copy", "cp", "curl", "data", "del", "diff", "dir", "do", "echo", "else", "elseif", "end", "enum",
    "erase", "exit", "fc", "filter", "finally", "for", "foreach", "function", "group", "h", "help",
    "history", "if", "kill", "ls", "man", "md", "measure", "mkdir", "more", "mount", "move", "mv",
    "param", "popd", "process", "ps", "pushd", "pwd", "r", "rd", "ren", "return", "rm", "rmdir", "sc",
    "select", "set", "sleep", "sort", "start", "switch", "tee", "throw", "trap", "try", "type",
    "until", "using", "wget", "where", "while", "write", "qx", "rtk",
];

/// `qx ` before each `;`/newline separated PowerShell statement that is one plain native command,
/// or `None` when no statement qualifies.
fn rewrite_powershell(cmd: &str) -> Option<String> {
    let mut out = String::with_capacity(cmd.len() + 8);
    let mut last = 0;
    let mut changed = false;
    let stmts = powershell_statements(cmd)?;
    // An alias or a dot-sourced script can shadow a native program for the statements after it.
    let shadows = |s: &Range<usize>| {
        let first = cmd[s.clone()].split_whitespace().next().unwrap_or("").to_ascii_lowercase();
        matches!(first.as_str(), "set-alias" | "sal" | "new-alias" | "nal" | "." | "import-module" | "ipmo")
    };
    if stmts.iter().any(shadows) {
        return None;
    }
    for stmt in stmts {
        let text = &cmd[stmt.clone()];
        if powershell_target(text).is_some_and(known_program) {
            let at = stmt.start + (text.len() - text.trim_start().len());
            out.push_str(&cmd[last..at]);
            out.push_str("qx ");
            last = at;
            changed = true;
        }
    }
    changed.then(|| {
        out.push_str(&cmd[last..]);
        out
    })
}

/// The `;`/newline separated statements of a PowerShell line, or `None` for syntax that can join
/// or hide statements (blocks, subexpressions, escapes, splats and here-strings).
fn powershell_statements(cmd: &str) -> Option<Vec<Range<usize>>> {
    let b = cmd.as_bytes();
    let mut stmts = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\'' => i += 2 + cmd[i + 1..].find('\'')?,
            b'"' => {
                i += 1;
                loop {
                    match *b.get(i)? {
                        b'"' => break,
                        b'`' => return None,
                        b'$' if b.get(i + 1) == Some(&b'(') => return None,
                        _ => i += 1,
                    }
                }
                i += 1;
            }
            b'{' | b'}' | b'(' | b')' | b'`' | b'@' => return None,
            b';' | b'\n' => {
                stmts.push(start..i);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    stmts.push(start..b.len());
    Some(stmts)
}

/// The program of a PowerShell statement that is one plain native command, else `None`: no
/// pipeline, redirect but a final `2>&1`, call operator, array or cmdlet.
fn powershell_target(stmt: &str) -> Option<&str> {
    let line = stmt.trim();
    let line = line.strip_suffix("2>&1").map_or(line, str::trim_end);
    let b = line.as_bytes();
    let mut words = Vec::new();
    let (mut i, mut start) = (0, None);
    while i <= b.len() {
        match b.get(i) {
            None | Some(b' ' | b'\t' | b'\r') => {
                if let Some(s) = start.take() {
                    words.push(&line[s..i]);
                }
            }
            Some(b'|' | b'>' | b'<' | b'&' | b',') => return None,
            Some(&q @ (b'\'' | b'"')) => {
                start.get_or_insert(i);
                i += line[i + 1..].find(q as char)? + 1;
            }
            Some(_) => {
                start.get_or_insert(i);
            }
        }
        i += 1;
    }
    let (&name, args) = words.split_first()?;
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    let ok = plain_name(name)
        && !name.contains('-')
        && !POWERSHELL_WORDS.contains(&stem)
        && !WRAPPERS.contains(&stem)
        && !crate::SUBCOMMANDS.contains(&stem)
        && !long_running(stem, args);
    ok.then_some(name)
}

/// The rewrite names `qx`, so the shell must find this binary by that name.
fn qx_on_path() -> bool {
    let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) else {
        return false;
    };
    let trim = |p: &Path| p.to_string_lossy().trim_end_matches(['\\', '/']).to_ascii_lowercase();
    let want = trim(&dir);
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| trim(&d) == want))
}

enum Rtk {
    Keep,
    Rewritten(String),
    /// rtk denies the line, or its answer could not be read: rtk's own hook decides.
    Unknown,
}

/// What rtk's hook does with this line: `rtk rewrite` exits 1 to leave it alone and otherwise
/// prints its version (exit 0, or 3 when Claude Code should still ask).
fn rtk_rewrite(command: &str) -> Rtk {
    let run = Command::new("rtk")
        .args(["rewrite", command])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = run else { return Rtk::Keep };
    // rtk answers in ~30 ms; a hung rtk must not hold every Bash call until the hook's own timeout.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(2)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Rtk::Unknown;
            }
        }
    };
    let mut stdout = Vec::new();
    if child.stdout.take().map(|mut o| o.read_to_end(&mut stdout)).is_none() {
        return Rtk::Unknown;
    }
    match (status.code(), String::from_utf8(stdout)) {
        (Some(1), _) => Rtk::Keep,
        (Some(0 | 3), Ok(line)) if !line.trim().is_empty() => {
            let line = line.strip_suffix('\n').unwrap_or(&line);
            Rtk::Rewritten(line.strip_suffix('\r').unwrap_or(line).to_string())
        }
        _ => Rtk::Unknown,
    }
}

/// What the user's Claude Code settings say that matters here: deny and ask rules, and whether
/// rtk's hook also rewrites Bash commands.
struct Rules {
    /// Deny and ask rules.
    patterns: Vec<String>,
    allow: Vec<String>,
    rtk: bool,
}

impl Rules {
    /// `None` when a settings file cannot be read as JSON: it may hold a deny rule.
    fn load(cwd: Option<&Path>) -> Option<Rules> {
        let mut files: Vec<PathBuf> = Vec::new();
        if let Some(home) = crate::resolve::home_dir() {
            files.push(home.join(".claude"));
        }
        // Project settings sit at the project root, which may be any ancestor of the cwd.
        files.extend(cwd.into_iter().flat_map(Path::ancestors).map(|d| d.join(".claude")));
        if let Some(pf) = std::env::var_os("ProgramFiles") {
            files.push(Path::new(&pf).join("ClaudeCode"));
        }
        files.sort();
        files.dedup();
        let mut rules = Rules { patterns: Vec::new(), allow: Vec::new(), rtk: false };
        for dir in files {
            for name in ["settings.json", "settings.local.json", "managed-settings.json"] {
                let Ok(text) = std::fs::read_to_string(dir.join(name)) else { continue };
                rules.rtk |= text.contains("rtk hook claude") || text.contains("rtk-rewrite");
                let strict = crate::setup::jsonc::to_strict_json(&text);
                let v = serde_json::from_str::<Value>(&strict).ok()?;
                let list = |key: &str| {
                    let list = v["permissions"][key].as_array().into_iter().flatten();
                    list.filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>()
                };
                rules.patterns.extend(list("deny"));
                rules.patterns.extend(list("ask"));
                rules.allow.extend(list("allow"));
            }
        }
        Some(rules)
    }

    /// Whether the allow rules answer each command of `rewritten` as they answer the same
    /// command of `base`, so the rewrite neither skips a prompt nor adds one. Each command is
    /// tried with and without its leading `VAR=x` words, and the whole line once more, since
    /// how Claude Code splits a line is its own business.
    fn same_answer(&self, tool: &str, base: &str, rewritten: &str) -> bool {
        let (Some(a), Some(b)) = (commands(tool, base), commands(tool, rewritten)) else { return false };
        a.len() == b.len() && a.iter().zip(&b).all(|(x, y)| self.allows(tool, x) == self.allows(tool, y))
    }

    /// `Tool`, `Tool(exact)`, `Tool(prefix:*)` and `Tool(glob *)` allow rules.
    fn allows(&self, tool: &str, command: &str) -> bool {
        self.allow.iter().any(|rule| {
            let Some(rest) = rule.strip_prefix(tool) else { return false };
            let Some(pat) = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')) else {
                return rest.is_empty();
            };
            match pat.strip_suffix(":*") {
                Some(prefix) => glob(&format!("{prefix}*"), command),
                None => glob(pat, command),
            }
        })
    }

    /// Whether a deny or ask rule could name this command. Such a rule must keep seeing the
    /// command as Claude wrote it: after a rewrite Claude Code would match `qx ...` instead.
    /// Over-matching is harmless, it only leaves the command unwrapped, so a rule matches
    /// anywhere in the line (`cd x && git push` still meets `Bash(git push:*)`).
    fn restricts(&self, tool: &str, command: &str) -> bool {
        self.patterns.iter().any(|rule| {
            let Some(rest) = rule.strip_prefix(tool) else { return false };
            if rest.is_empty() {
                return true;
            }
            let Some(pat) = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')) else {
                return false;
            };
            let pat = pat.strip_suffix(":*").unwrap_or(pat).trim_end_matches('*').trim_end();
            glob(&format!("*{pat}*"), command)
        })
    }
}

/// The texts `Rules::same_answer` compares: the whole line, then per command its text from the
/// first word and from the program on.
fn commands<'a>(tool: &str, line: &'a str) -> Option<Vec<&'a str>> {
    let mut out = vec![line.trim()];
    if tool == "PowerShell" {
        out.extend(powershell_statements(line)?.into_iter().map(|s| line[s].trim()));
        return Some(out);
    }
    for stage in bash_stages(line)? {
        let mut words = stage.words.iter();
        let Some(first) = words.clone().next() else { continue };
        out.push(line[first.start..stage.end].trim_end());
        let program = words.find(|r| !is_assignment(&line[(*r).clone()])).map_or(stage.end, |r| r.start);
        out.push(line[program..stage.end].trim_end());
    }
    Some(out)
}

/// `*` matches any run of characters, everything else matches itself.
fn glob(pat: &str, s: &str) -> bool {
    let (p, s) = (pat.as_bytes(), s.as_bytes());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < s.len() {
        if p.get(i) == Some(&b'*') {
            star = Some(i);
            mark = j;
            i += 1;
        } else if p.get(i) == Some(&s[j]) {
            i += 1;
            j += 1;
        } else if let Some(st) = star {
            i = st + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_wraps_plain_commands() {
        for (cmd, want) in [
            // Built-ins, so the cases do not depend on what this machine has on PATH.
            ("cat x.py", Some("qx cat x.py")),
            ("cd src && wc -l a.rs", Some("cd src && qx wc -l a.rs")),
            ("FOO=1 head a.js", Some("FOO=1 qx head a.js")),
            ("wc -l a 2>&1", Some("qx wc -l a 2>&1")),
            ("head a ; tail b", Some("qx head a ; qx tail b")),
            ("wc -c \"a b.txt\" 'c d'", Some("qx wc -c \"a b.txt\" 'c d'")),
            ("cat /c/Users/x/a.txt", Some("qx cat /c/Users/x/a.txt")),
            ("cat héllo.txt", Some("qx cat héllo.txt")),
            ("head a.txt # first lines", Some("qx head a.txt # first lines")),
            ("# note\nhead a.txt", Some("# note\nqx head a.txt")),
            ("head \\\n  a.txt", Some("qx head \\\n  a.txt")),
            ("wc -l < a.txt", Some("qx wc -l < a.txt")),
            ("timeout 30 head a", Some("qx --timeout 30 head a")),
            ("timeout 2m head a", Some("qx --timeout 120 head a")),
            ("cat <<'EOF'\ngit status\nEOF", Some("qx cat <<'EOF'\ngit status\nEOF")),
            ("cat <<'EOF' > a.txt\nhead b\nEOF\nwc -l a.txt", Some("cat <<'EOF' > a.txt\nhead b\nEOF\nqx wc -l a.txt")),
            ("cat <<-\"EOF\"\n\thead\n\tEOF\nhead x", Some("qx cat <<-\"EOF\"\n\thead\n\tEOF\nqx head x")),
            // Substitutions run before qx starts; only the scan has to get past them.
            ("./v/python \"$(ls)\"", Some("qx ./v/python \"$(ls)\"")),
            ("cat $(ls) | tail", None),
            ("x=$(head a); ./v/python `ls` ${HOME}/b", Some("x=$(head a); qx ./v/python `ls` ${HOME}/b")),
            ("./v/python \"$(cat <<'EOF'\na) b\nEOF\n)\" x", Some("qx ./v/python \"$(cat <<'EOF'\na) b\nEOF\n)\" x")),
            ("./v/python \"$(echo \"(\")\" $((1 + (2)))", Some("qx ./v/python \"$(echo \"(\")\" $((1 + (2)))")),
            ("head a &&\n  wc -l b", Some("qx head a &&\n  qx wc -l b")),
            ("cat a{1,2}.txt", Some("qx cat a{1,2}.txt")),
            // rtk passes `proxy` and Python through untouched; qx does that faster.
            ("rtk ./v/python x.py", Some("qx ./v/python x.py")),
            ("rtk proxy ./v/python a.py | tail", Some("qx --stream ./v/python a.py | tail")),
            ("rtk proxy cat a.txt", Some("qx --stream cat a.txt")),
            ("timeout 60 rtk proxy cat a", Some("qx --stream --timeout 60 cat a")),
            // rtk is native: Git Bash already converted `/x/p` for it the way it does for qx.
            ("rtk proxy sed -n '/x/p' f | head", Some("qx --stream sed -n '/x/p' f | head")),
            // Left alone: shell words, unknown programs, output that does not reach the model.
            ("timeout -k 5 30 head a", None),
            ("if head a; then wc b; fi", None),
            ("no-such-program-qx arg", None),
            ("echo hi", None),
            ("cd src", None),
            (". ./env.sh", None),
            ("cat a | tail -5", None),
            ("cat a > out.txt", None),
            ("cat a 2>/dev/null", None),
            ("cat a &", None),
            ("qx git status", None),
            ("rtk git status", None),
            ("npm run dev", None),
            ("tail -f log.txt", None),
            ("sed -n '/x/p' f", None),
            ("sed -n '/c/p' f", None),
            ("cat <<EOF\nno end", None),
            ("cat $(ls", None),
            ("cat \"$(case x in a) echo;; esac)\"", None),
            ("$(which python) a.py", None),
            ("{ head a; }", None),
            ("(cd x && head a)", None),
            ("cat (x)", None),
            ("f() { head a; }", None),
            ("cat <(ls)", None),
            ("rtk proxy", None),
            ("./build.sh", None),
            ("'unterminated", None),
            // A pipe continued on the next line still feeds the next stage.
            ("cat a |\n  tail -5", None),
            // An expansion may yield a `/x/p` that Git Bash converts for qx but not for wc.
            ("wc -l \"$f\" `ls`", None),
            // qx's own subcommand, not the program.
            ("which git", None),
            // Functions, `eval` and `source` can redefine what a later name runs.
            ("function head { :; }; head a", None),
            ("eval \"$X\"; head a", None),
            ("source env.sh && head a", None),
        ] {
            assert_eq!(rewrite_bash(cmd).as_deref(), want, "{cmd}");
        }
    }

    #[test]
    fn stages_know_where_output_goes() {
        let cmd = "a 2>&1 | b > f; c 2>/dev/null; d &>>log; e >&2; f <<< x; g & h";
        let shape: Vec<(String, bool, bool, bool)> = bash_stages(cmd)
            .unwrap()
            .iter()
            .map(|s| {
                let words: Vec<&str> = s.words.iter().map(|r| &cmd[r.clone()]).collect();
                (words.join(" "), s.to_model, s.piped_in, s.background)
            })
            .collect();
        let want = [
            ("a", false, false, false),
            ("b", false, true, false),
            ("c", false, false, false),
            ("d", false, false, false),
            ("e", false, false, false),
            ("f", true, false, false),
            ("g", false, false, true),
            ("h", true, false, false),
        ];
        let got: Vec<(&str, bool, bool, bool)> = shape.iter().map(|s| (s.0.as_str(), s.1, s.2, s.3)).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn accelerated_programs_stream_through_pipes_and_loops() {
        for (cmd, want) in [
            (".venv/Scripts/python x.py 2>&1 | tail -20", Some("qx --stream .venv/Scripts/python x.py 2>&1 | tail -20")),
            ("\"C:\\Py\\python.exe\" a.py > out.txt", Some("qx --stream \"C:\\Py\\python.exe\" a.py > out.txt")),
            ("for f in a b; do ./v/python3 $f; done", Some("for f in a b; do qx --stream ./v/python3 $f; done")),
            ("timeout 60 ./v/python a.py | head", Some("qx --stream --timeout 60 ./v/python a.py | head")),
            ("cat a | ./v/python -c 'import sys'", Some("cat a | qx --stream ./v/python -c 'import sys'")),
            ("./v/python - <<'EOF'\nprint(1)\nEOF", Some("qx ./v/python - <<'EOF'\nprint(1)\nEOF")),
            ("./v/python app.py &", None),
            ("$PY a.py | tail", None),
            ("./v/python -m http.server 8000 | tee log", None),
            ("./v/python manage.py runserver", None),
            // Subshells and groups: their output may be redirected as a whole, so only streaming.
            ("(cd x && ./v/python a.py) > log", Some("(cd x && qx --stream ./v/python a.py) > log")),
            ("{ ./v/python a; head b; } | tail", Some("{ qx --stream ./v/python a; head b; } | tail")),
            ("( ( ./v/python a ) )", Some("( ( qx --stream ./v/python a ) )")),
            ("((n++)); ./v/python a", Some("((n++)); qx ./v/python a")),
            ("rtk ./v/python a | tail", Some("qx --stream ./v/python a | tail")),
            ("./v/python -u -m uvicorn app:app", None),
            ("./v/python run.py --device cuda | tail", Some("qx --stream ./v/python run.py --device cuda | tail")),
            ("./v/python -c 'print(\"observe start\")'", Some("qx ./v/python -c 'print(\"observe start\")'")),
        ] {
            assert_eq!(rewrite_bash(cmd).as_deref(), want, "{cmd}");
        }
    }

    #[test]
    fn timeout_durations() {
        assert_eq!(timeout_secs("30"), Some(30));
        assert_eq!(timeout_secs("30s"), Some(30));
        assert_eq!(timeout_secs("5m"), Some(300));
        assert_eq!(timeout_secs("1h"), Some(3600));
        for bad in ["0", "1.5", "-k", "--signal=KILL", "5x", "s", ""] {
            assert_eq!(timeout_secs(bad), None, "{bad}");
        }
    }

    #[test]
    fn powershell_takes_plain_native_statements() {
        assert_eq!(powershell_target("git status"), Some("git"));
        assert_eq!(powershell_target("  cargo test --quiet"), Some("cargo"));
        assert_eq!(powershell_target("git commit -m 'fix (x) | y'"), Some("git"));
        assert_eq!(powershell_target("cargo build 2>&1"), Some("cargo"));
        for cmd in ["Get-ChildItem", "ls -la", "sort x", "git status | Select -First 3", "& 'C:\\x.exe'", "npm run dev", "git a > f"] {
            assert_eq!(powershell_target(cmd), None, "{cmd}");
        }
        assert_eq!(powershell_statements("cd x; git status").map(|v| v.len()), Some(2));
        assert!(powershell_statements("Get-Process | % { $_.Id }").is_none());
        assert_eq!(rewrite_powershell("Get-Date; Set-Location x"), None);
        assert_eq!(rewrite_powershell("Set-Alias git x; git status"), None);
        assert_eq!(powershell_target("which git"), None);
        assert_eq!(powershell_target("nohup.exe git status"), None);
    }

    #[test]
    fn long_running_commands_stay_unwrapped() {
        for (name, args) in [
            ("tail", &["-fn", "20", "log"][..]),
            ("docker", &["compose", "logs", "--follow"]),
            ("npm", &["run", "web:dev"]),
            ("npx", &["jest", "--watchAll"]),
            ("npx", &["tsc", "-w"]),
            ("ping", &["-t", "host"]),
        ] {
            assert!(long_running(name, args), "{name} {args:?}");
        }
        for (name, args) in [("tail", &["-n", "5", "f"][..]), ("git", &["log", "-5"]), ("npm", &["run", "build"])] {
            assert!(!long_running(name, args), "{name} {args:?}");
        }
    }

    #[test]
    fn deep_nesting_is_refused_not_overflowed() {
        assert!(bash_stages(&"(".repeat(300)).is_none());
    }

    #[test]
    fn allow_rules_answer_the_same_after_rewrite() {
        let qx = Rules { patterns: vec![], allow: vec!["Bash(qx:*)".into(), "Bash(git status:*)".into()], rtk: false };
        assert!(qx.same_answer("Bash", "git status", "qx git status"));
        assert!(!qx.same_answer("Bash", "git push", "qx git push"));
        assert!(!qx.same_answer("Bash", "FOO=1 git push", "FOO=1 qx git push"));
        assert!(!qx.same_answer("Bash", "git status && git push", "qx git status && qx git push"));
        // A redirect is part of the command an exact rule must match.
        let exact = Rules { patterns: vec![], allow: vec!["Bash(qx:*)".into(), "Bash(git status)".into()], rtk: false };
        assert!(exact.same_answer("Bash", "git status", "qx git status"));
        assert!(!exact.same_answer("Bash", "git status > out", "qx git status > out"));
        let plain = Rules { patterns: vec![], allow: vec!["Bash(git status *)".into()], rtk: false };
        assert!(!plain.same_answer("Bash", "git status -s", "qx git status -s"));
        let none = Rules { patterns: vec![], allow: vec![], rtk: false };
        assert!(none.same_answer("Bash", "git push", "qx git push"));
        let all = Rules { patterns: vec![], allow: vec!["Bash".into()], rtk: false };
        assert!(all.same_answer("Bash", "git push", "qx git push"));
        let ps = Rules { patterns: vec![], allow: vec!["PowerShell(qx:*)".into()], rtk: false };
        assert!(!ps.same_answer("PowerShell", "cd x; git push", "cd x; qx git push"));
    }

    #[test]
    fn deny_and_ask_rules_keep_their_command() {
        let rules = Rules {
            patterns: vec!["Bash(git push:*)".into(), "Bash(npm install *)".into(), "PowerShell(cargo publish)".into()],
            allow: vec![],
            rtk: false,
        };
        assert!(rules.restricts("Bash", "git push origin main"));
        assert!(rules.restricts("Bash", "cd x && FOO=1 git push"));
        assert!(rules.restricts("Bash", "npm install"));
        assert!(rules.restricts("PowerShell", "cargo publish"));
        assert!(!rules.restricts("Bash", "cargo publish"));
        assert!(!rules.restricts("Bash", "git status"));
        let all = Rules { patterns: vec!["Bash".into()], allow: vec![], rtk: false };
        assert!(all.restricts("Bash", "anything"));
        let other = Rules { patterns: vec!["BashOutput".into()], allow: vec![], rtk: false };
        assert!(!other.restricts("Bash", "anything"));
    }

    /// Replays real commands (`QX_HOOK_CORPUS`: a JSON list of `{"tool", "command"}`) and writes
    /// what the hook does with each to `<corpus>.out.json`.
    #[test]
    #[ignore]
    fn corpus_coverage() {
        let path = std::env::var("QX_HOOK_CORPUS").expect("QX_HOOK_CORPUS");
        let items: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let report: Vec<Value> = items
            .iter()
            .filter_map(|it| {
                let (tool, cmd) = (it["tool"].as_str()?, it["command"].as_str()?);
                let (out, scanned) = match tool {
                    "Bash" => (rewrite_bash(cmd), bash_stages(cmd).is_some()),
                    "PowerShell" => (rewrite_powershell(cmd), powershell_statements(cmd).is_some()),
                    _ => return None,
                };
                // Each edit as one marker word, for a shell parser to check where it landed.
                let marked = (tool == "Bash")
                    .then(|| bash_edits(cmd))
                    .flatten()
                    .map(|e| apply(cmd, e.into_iter().map(|(r, _)| (r, "QXMARK ".to_string())).collect()));
                Some(json!({"tool": tool, "command": cmd, "rewritten": out, "scanned": scanned, "marked": marked}))
            })
            .collect();
        std::fs::write(format!("{path}.out.json"), serde_json::to_string(&report).unwrap()).unwrap();
    }
}
