//! Codex's `~/.codex/config.toml`: a line-based block editor.
//!
//! TOML has no stable round-trip crate in this project's dependency budget
//! (`serde_json` only), so the qx tables are treated as text: every table in the
//! `[mcp_servers.qx]` namespace is found wherever it sits, its env keys are kept and
//! the block is regenerated. A shape we do not recognise is refused with a
//! paste-ready snippet rather than rewritten.

use super::{Action, Edit};
use std::path::Path;

/// `[mcp_servers.qx]`, `[mcp_servers."qx"]` and any subtable of either.
fn is_qx_header(trimmed: &str) -> bool {
    trimmed == "[mcp_servers.qx]"
        || trimmed == "[mcp_servers.\"qx\"]"
        || trimmed.starts_with("[mcp_servers.qx.")
        || trimmed.starts_with("[mcp_servers.\"qx\".")
}

fn is_env_header(trimmed: &str) -> bool {
    matches!(trimmed, "[mcp_servers.qx.env]" | "[mcp_servers.\"qx\".env]")
}

/// A header as TOML reads it: no trailing comment, no spaces (`[ mcp_servers . qx ] # note`).
fn norm_header(line: &str) -> String {
    strip_comment(line).chars().filter(|c| !c.is_whitespace()).collect()
}

/// Every run of lines belonging to a qx table, in file order.
fn qx_tables(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if is_qx_header(&norm_header(lines[i])) {
            let mut end = i + 1;
            while end < lines.len() && !lines[end].trim().starts_with('[') {
                end += 1;
            }
            // Trailing blank and comment lines introduce the next table: they are not ours,
            // unless that next table is another qx table (`[mcp_servers.qx.env]`).
            let next_is_qx = lines.get(end).is_some_and(|l| is_qx_header(&norm_header(l)));
            while !next_is_qx && end > i + 1 && {
                let t = lines[end - 1].trim();
                t.is_empty() || t.starts_with('#')
            } {
                end -= 1;
            }
            out.push((i, end));
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

pub fn edit_codex(text: Option<&str>, exe: &Path, a: Action) -> Edit {
    let Some(text) = text else {
        return match a {
            Action::Remove => Edit::Unchanged,
            Action::Install => Edit::Changed(render(exe, &[], "\n")),
        };
    };
    if refuses(text, a) {
        return Edit::Refuse(refusal(exe));
    }
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let trailing_newline = text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();
    let tables = qx_tables(&lines);
    let first = tables.first().map(|(start, _)| *start);

    if tables.is_empty() {
        if a == Action::Remove {
            return Edit::Unchanged;
        }
        let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.extend(render(exe, &[], eol).lines().map(str::to_string));
        let refs: Vec<&str> = out.iter().map(String::as_str).collect();
        return verdict(join(&refs, eol, true), text);
    }

    // The env keys the user may have set live in one of those tables, wherever it is.
    let env = kept_env(&lines, &tables);
    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    // Remove in reverse so earlier ranges keep their offsets.
    for (start, end) in tables.iter().rev() {
        out.drain(*start..*end);
    }
    let at = first.unwrap_or(0).min(out.len());
    let block_line = |i: usize| -> Option<&str> { out.get(i).map(String::as_str) };
    let _ = block_line;
    match a {
        Action::Remove => {
            // A blank line that used to separate the block from what followed stays one.
            while at + 1 < out.len() && out[at].trim().is_empty() && out[at + 1].trim().is_empty() {
                out.remove(at);
            }
        }
        Action::Install => {
            let block: Vec<String> = render(exe, &env, eol).lines().map(str::to_string).collect();
            let block_len = block.len();
            let needs_gap = out.get(at).is_some_and(|l| !l.trim().is_empty());
            for (offset, line) in block.into_iter().enumerate() {
                out.insert(at + offset, line);
            }
            if needs_gap {
                out.insert(at + block_len, String::new());
            }
        }
    }
    let refs: Vec<&str> = out.iter().map(String::as_str).collect();
    verdict(join(&refs, eol, trailing_newline), text)
}

/// The `command` of the qx entry, for `qx doctor`.
pub fn codex_command(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    for (start, end) in qx_tables(&lines) {
        if is_env_header(&norm_header(lines[start])) {
            continue;
        }
        if let Some(value) = lines[start..end]
            .iter()
            .find_map(|line| toml_value(line, "command"))
        {
            return Some(value);
        }
    }
    None
}

/// `KEY = "value"` lines under any qx env table.
fn kept_env(lines: &[&str], tables: &[(usize, usize)]) -> Vec<String> {
    let mut out = Vec::new();
    for (start, end) in tables {
        if !is_env_header(&norm_header(lines[*start])) {
            continue;
        }
        for line in &lines[start + 1..*end] {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') || !t.contains('=') {
                continue;
            }
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

/// Inline tables and dotted keys hide the entry inside one line: refuse, never guess.
fn refuses(text: &str, action: Action) -> bool {
    let mut in_mcp_servers = false;
    let mut in_qx_table = false;
    for line in text.lines() {
        let t = strip_comment(line).trim();
        if t.starts_with('[') {
            let header = norm_header(t);
            in_mcp_servers = header == "[mcp_servers]";
            in_qx_table = is_qx_header(&header) && !is_env_header(&header);
            continue;
        }
        let squashed: String = t.chars().filter(|c| !c.is_whitespace()).collect();
        // An env written inline (`env = { .. }`) or dotted (`env.K = ..`) in the qx table would be
        // regenerated away with the table.
        // Removing the whole table drops nothing the user did not ask to drop.
        if action == Action::Install && in_qx_table && (squashed.starts_with("env=") || squashed.starts_with("env.")) {
            return true;
        }
        if squashed.starts_with("mcp_servers={") || squashed.starts_with("mcp_servers.qx") {
            return true;
        }
        if in_mcp_servers && (squashed.starts_with("qx={") || squashed.starts_with("qx.")) {
            return true;
        }
    }
    false
}

fn refusal(exe: &Path) -> String {
    format!(
        "config.toml uses an inline table or dotted key qx will not rewrite. Paste this instead:\n{}",
        render(exe, &[], "\n")
    )
}

fn verdict(out: String, original: &str) -> Edit {
    if out == original {
        Edit::Unchanged
    } else {
        Edit::Changed(out)
    }
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

    fn count(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
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
        let again = edit_codex(Some(&out), Path::new(EXE), Action::Install);
        assert!(matches!(again, Edit::Unchanged), "second run rewrote the file:\n{out}");
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
        assert_eq!(count(&out, "[mcp_servers.qx.env]"), 1, "{out}");
        assert!(
            matches!(edit_codex(Some(&out), Path::new(EXE), Action::Install), Edit::Unchanged),
            "second run rewrote the file:\n{out}"
        );
        assert_eq!(codex_command(&out).as_deref(), Some(EXE));
    }

    #[test]
    fn codex_handles_quoted_env_tables_and_env_before_the_block() {
        // Valid TOML the old line scan missed: a quoted header, and the env table first.
        let quoted = "\
[mcp_servers.\"qx\"]
command = 'old'
[mcp_servers.\"qx\".env]
HTTP_PROXY = \"http://proxy\"
";
        let out = changed(edit_codex(Some(quoted), Path::new(EXE), Action::Install));
        assert_eq!(count(&out, ".env]"), 1, "{out}");
        assert!(out.contains("HTTP_PROXY = \"http://proxy\""), "{out}");
        assert!(out.contains("QX_CLIENT = \"codex\""), "{out}");

        let env_first = "\
[mcp_servers.qx.env]
HTTP_PROXY = \"http://proxy\"

[mcp_servers.qx]
command = 'old'

[profiles.fast]
model = \"y\"
";
        let out = changed(edit_codex(Some(env_first), Path::new(EXE), Action::Install));
        assert_eq!(count(&out, "[mcp_servers.qx.env]"), 1, "{out}");
        assert!(out.contains("HTTP_PROXY"), "{out}");
        assert!(out.contains("[profiles.fast]\nmodel = \"y\""), "{out}");
    }

    #[test]
    fn codex_refuses_unknown_shapes() {
        for src in [
            "mcp_servers = { qx = { command = 'x' } }\n",
            "[mcp_servers]\nqx = { command = 'x' }\n",
            "[mcp_servers]\nqx.command = 'x'\n",
            "mcp_servers.qx.command = 'x'\n",
        ] {
            match edit_codex(Some(src), Path::new(EXE), Action::Install) {
                Edit::Refuse(reason) => {
                    assert!(reason.contains("[mcp_servers.qx]"), "{src}: {reason}")
                }
                other => panic!(
                    "{src} was not refused (unchanged: {})",
                    matches!(other, Edit::Unchanged)
                ),
            }
        }
    }

    #[test]
    fn codex_never_silently_drops_an_inline_env() {
        let src = "[mcp_servers.qx]\ncommand = 'old'\nenv = { QX_GUARD = \"safe\" }\n";
        match edit_codex(Some(src), Path::new(EXE), Action::Install) {
            Edit::Refuse(reason) => assert!(reason.contains("[mcp_servers.qx]"), "{reason}"),
            Edit::Changed(out) => assert!(out.contains("QX_GUARD"), "the user's env was dropped:\n{out}"),
            Edit::Unchanged => {}
        }
    }

    #[test]
    fn codex_remove_works_when_install_would_refuse_an_inline_env() {
        let src = "[mcp_servers.qx]\ncommand = 'x'\nenv = { QX_GUARD = \"strict\" }\n\n[profiles.fast]\nmodel = \"y\"\n";
        assert!(matches!(edit_codex(Some(src), Path::new(EXE), Action::Install), Edit::Refuse(_)));
        let out = changed(edit_codex(Some(src), Path::new(EXE), Action::Remove));
        assert!(!out.contains("mcp_servers.qx"), "{out}");
        assert!(out.contains("[profiles.fast]\nmodel = \"y\""), "{out}");
    }

    #[test]
    fn codex_leaves_the_next_tables_comments_alone() {
        let src = "[mcp_servers.qx]\ncommand = 'x'\n\n# >>> caveman >>>\n[mcp_servers.caveman]\ncommand = 'c'\n";
        let out = changed(edit_codex(Some(src), Path::new(EXE), Action::Install));
        assert!(out.contains("# >>> caveman >>>\n[mcp_servers.caveman]\ncommand = 'c'"), "{out}");
        let out = changed(edit_codex(Some(src), Path::new(EXE), Action::Remove));
        assert!(out.contains("# >>> caveman >>>\n[mcp_servers.caveman]\ncommand = 'c'"), "{out}");
        assert!(!out.contains("mcp_servers.qx"), "{out}");
    }

    #[test]
    fn codex_recognises_a_header_with_a_comment_or_spaces() {
        for src in ["[mcp_servers.qx] # mine\ncommand = 'old'\n", "[ mcp_servers . qx ]\ncommand = 'old'\n"] {
            let out = changed(edit_codex(Some(src), Path::new(EXE), Action::Install));
            assert_eq!(count(&out, "[mcp_servers.qx]"), 1, "{src} -> {out}");
            assert!(!out.contains("command = 'old'"), "{src} -> {out}");
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
