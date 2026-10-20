//! Codex's `~/.codex/config.toml`: a line-based block editor.
//!
//! TOML has no stable round-trip crate in this project's dependency budget
//! (`serde_json` only), so the qx entry is treated as text: the block is found by
//! its header, replaced line by line, and every other byte is copied through. A
//! shape we do not recognise is refused with a paste-ready snippet rather than
//! rewritten.

use super::{Action, Edit};
use std::path::Path;

const HEADER: &[&str] = &["[mcp_servers.qx]", "[mcp_servers.\"qx\"]"];

pub fn edit_codex(text: Option<&str>, exe: &Path, a: Action) -> Edit {
    let Some(text) = text else {
        return match a {
            Action::Remove => Edit::Unchanged,
            Action::Install => Edit::Changed(render(exe, &[], "\n")),
        };
    };
    if refuses(text) {
        return Edit::Refuse(refusal(exe));
    }
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let trailing_newline = text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();
    let block = find_block(&lines);
    match a {
        Action::Remove => match block {
            None => Edit::Unchanged,
            Some((start, end)) => {
                let kept: Vec<&str> = lines[..start].iter().chain(&lines[end..]).copied().collect();
                let out = join(&kept, eol, trailing_newline);
                if out == text {
                    Edit::Unchanged
                } else {
                    Edit::Changed(out)
                }
            }
        },
        Action::Install => {
            let env = block.map(|(s, e)| kept_env(&lines[s..e])).unwrap_or_default();
            let body = render(exe, &env, eol);
            let mut out: Vec<String> = Vec::new();
            match block {
                Some((start, end)) => {
                    out.extend(lines[..start].iter().map(|s| s.to_string()));
                    // A blank line that separated the block from what follows belongs to
                    // the file, not to the entry: keep it.
                    let trailing_blank = lines[start..end].last().is_some_and(|l| l.trim().is_empty());
                    out.extend(body.lines().map(str::to_string));
                    if trailing_blank {
                        out.push(String::new());
                    }
                    out.extend(lines[end..].iter().map(|s| s.to_string()));
                }
                None => {
                    out.extend(lines.iter().map(|s| s.to_string()));
                    if out.last().is_some_and(|l| !l.trim().is_empty()) {
                        out.push(String::new());
                    }
                    out.extend(body.lines().map(str::to_string));
                }
            }
            let refs: Vec<&str> = out.iter().map(String::as_str).collect();
            let text_out = join(&refs, eol, true);
            if text_out == text {
                Edit::Unchanged
            } else {
                Edit::Changed(text_out)
            }
        }
    }
}

/// The `command` of the qx entry, for `qx doctor`.
pub fn codex_command(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let (start, end) = find_block(&lines)?;
    lines[start..end]
        .iter()
        .find_map(|line| toml_value(line, "command"))
}

fn find_block(lines: &[&str]) -> Option<(usize, usize)> {
    let start = lines
        .iter()
        .position(|line| HEADER.contains(&line.trim()))?;
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| {
            let t = line.trim();
            t.starts_with('[') && !t.starts_with("[mcp_servers.qx.")
        })
        .map(|(i, _)| i)
        .unwrap_or(lines.len());
    Some((start, end))
}

/// `KEY = "value"` lines under the qx env subtable, which the user may have set.
fn kept_env(block: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_env = false;
    for line in block {
        let t = line.trim();
        if t.starts_with('[') {
            in_env = t.starts_with("[mcp_servers.qx.env") || t.starts_with("[mcp_servers.\"qx\".env");
            continue;
        }
        if in_env && !t.is_empty() && !t.starts_with('#') && t.contains('=') {
            let key = t.split('=').next().unwrap_or("").trim();
            if key != "QX_CLIENT" {
                out.push(t.to_string());
            }
        }
    }
    out
}

fn render(exe: &Path, env: &[String], eol: &str) -> String {
    let mut lines = vec![
        "[mcp_servers.qx]".to_string(),
        format!("command = {}", toml_path(exe)),
        "args = [\"mcp\"]".to_string(),
        "startup_timeout_sec = 20".to_string(),
        "tool_timeout_sec = 600".to_string(),
        String::new(),
        "[mcp_servers.qx.env]".to_string(),
        "QX_CLIENT = \"codex\"".to_string(),
    ];
    lines.extend(env.iter().cloned());
    lines.join(eol)
}

/// A TOML literal string keeps Windows paths readable; a path with `'` needs escapes.
fn toml_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.contains('\'') {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("'{s}'")
    }
}

fn toml_value(line: &str, key: &str) -> Option<String> {
    let (k, v) = line.split_once('=')?;
    if k.trim() != key {
        return None;
    }
    let v = strip_comment(v).trim();
    if let Some(rest) = v.strip_prefix('\'') {
        return Some(rest.split('\'').next()?.to_string());
    }
    let rest = v.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                other => out.push(other),
            },
            other => out.push(other),
        }
    }
    None
}

fn strip_comment(line: &str) -> &str {
    let mut in_basic = false;
    let mut in_literal = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' if !in_literal => in_basic = !in_basic,
            '\'' if !in_basic => in_literal = !in_literal,
            '#' if !in_basic && !in_literal => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Flow-style tables hide the entry inside one line: refuse instead of guessing.
fn refuses(text: &str) -> bool {
    let mut in_mcp_servers = false;
    for line in text.lines() {
        let t = strip_comment(line).trim();
        if t.starts_with('[') {
            in_mcp_servers = t == "[mcp_servers]";
            continue;
        }
        let squashed: String = t.chars().filter(|c| !c.is_whitespace()).collect();
        if squashed.starts_with("mcp_servers={") {
            return true;
        }
        if in_mcp_servers && squashed.starts_with("qx={") {
            return true;
        }
    }
    false
}

fn refusal(exe: &Path) -> String {
    format!(
        "config.toml uses a flow-style (inline) table qx will not rewrite. Paste this instead:\n{}",
        render(exe, &[], "\n")
    )
}

fn join(lines: &[&str], eol: &str, trailing_newline: bool) -> String {
    let mut out = lines.join(eol);
    if trailing_newline && !out.is_empty() {
        out.push_str(eol);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\q\qx.exe";

    fn changed(edit: Edit) -> String {
        match edit {
            Edit::Changed(text) => text,
            Edit::Refuse(reason) => panic!("refused: {reason}"),
            Edit::Unchanged => panic!("expected a change"),
        }
    }

    #[test]
    fn codex_appends_and_preserves_unrelated_bytes() {
        let src = "# >>> caveman\nmodel = \"x\"\n\n[mcp_servers.caveman]\ncommand = 'c'\n# --- end\n";
        let out = changed(edit_codex(Some(src), Path::new(EXE), Action::Install));
        assert!(out.starts_with(src), "{out}");
        assert!(
            out.contains("[mcp_servers.qx]\ncommand = 'C:\\q\\qx.exe'\nargs = [\"mcp\"]\nstartup_timeout_sec = 20\ntool_timeout_sec = 600\n"),
            "{out}"
        );
        assert!(out.contains("[mcp_servers.qx.env]\nQX_CLIENT = \"codex\"\n"), "{out}");
        // A comment-only file stays readable, and running again changes nothing.
        let again = edit_codex(Some(&out), Path::new(EXE), Action::Install);
        assert!(matches!(again, Edit::Unchanged), "second run rewrote the file");
    }

    #[test]
    fn codex_replaces_its_block_and_keeps_env() {
        let src = "\
model = \"x\"

[mcp_servers.qx]
command = 'old.exe'
args = [\"mcp\"]

[mcp_servers.qx.env]
QX_GUARD = \"safe\"

[profiles.fast]
model = \"y\"
";
        let out = changed(edit_codex(Some(src), Path::new(EXE), Action::Install));
        assert!(out.contains("command = 'C:\\q\\qx.exe'"), "{out}");
        assert!(out.contains("QX_GUARD = \"safe\""), "{out}");
        assert!(out.contains("QX_CLIENT = \"codex\""), "{out}");
        assert!(out.contains("[profiles.fast]\nmodel = \"y\""), "{out}");
        assert!(out.starts_with("model = \"x\"\n"), "{out}");
        assert!(
            matches!(edit_codex(Some(&out), Path::new(EXE), Action::Install), Edit::Unchanged),
            "second run rewrote the file:\n{out}"
        );
        assert_eq!(codex_command(&out).as_deref(), Some(EXE));
    }

    #[test]
    fn codex_refuses_unknown_shapes() {
        let flow = "mcp_servers = { qx = { command = 'x' } }\n";
        assert!(matches!(
            edit_codex(Some(flow), Path::new(EXE), Action::Install),
            Edit::Refuse(_)
        ));
        let nested = "[mcp_servers]\nqx = { command = 'x' }\n";
        match edit_codex(Some(nested), Path::new(EXE), Action::Install) {
            Edit::Refuse(reason) => assert!(reason.contains("[mcp_servers.qx]"), "{reason}"),
            _ => panic!("an inline qx table must be refused"),
        }
    }

    #[test]
    fn codex_remove() {
        let src = "\
[mcp_servers.qx]
command = 'x'

[mcp_servers.qx.env]
QX_CLIENT = \"codex\"

[profiles.fast]
model = \"y\"
";
        let out = changed(edit_codex(Some(src), Path::new(EXE), Action::Remove));
        assert!(!out.contains("mcp_servers.qx"), "{out}");
        assert!(out.contains("[profiles.fast]\nmodel = \"y\""), "{out}");
        assert!(matches!(edit_codex(Some(&out), Path::new(EXE), Action::Remove), Edit::Unchanged));
    }
}
