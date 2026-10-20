//! dsh (`cordis.patch.yml`) and Hermes (`config.yaml`) setup.
//!
//! Both are hand-edited YAML, so the entry is located and rewritten line by line:
//! a nested block ends at the first later non-blank, non-comment line with an
//! indent at or above its own. Everything outside the block is copied verbatim.
//! Shapes we cannot be sure about are refused, never guessed at.

use super::{Action, Edit};
use std::path::Path;

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_noise(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

/// First later non-blank, non-comment line indented no deeper than `indent`.
fn block_end(lines: &[&str], start: usize, indent: usize) -> usize {
    lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| !is_noise(line) && indent_of(line) <= indent)
        .map(|(i, _)| i)
        .unwrap_or(lines.len())
}

fn has_tab_indent(text: &str) -> bool {
    text.lines()
        .any(|l| l.starts_with('\t') || l.trim_start_matches(' ').starts_with('\t'))
}

fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

// ------------------------------------------------------------------------ dsh

/// One item of dsh's `- insert:` list.
fn dsh_item(lines: &[&str]) -> Option<(usize, usize)> {
    let start = lines
        .iter()
        .position(|line| line.trim() == "- id: mcp-qx")?;
    let indent = indent_of(lines[start]);
    Some((start, block_end(lines, start, indent)))
}

/// Lines of the item, plus where its `env:` block starts and the indent its env uses.
fn dsh_env(lines: &[&str], start: usize, end: usize) -> Option<(usize, usize)> {
    let env = (start..end).find(|i| lines[*i].trim().starts_with("env:"))?;
    Some((env, indent_of(lines[env])))
}

pub fn edit_dsh(text: Option<&str>, exe: &Path, a: Action) -> Edit {
    let mut lines: Vec<String> = text.unwrap_or("").lines().map(str::to_string).collect();
    let had_newline = text.is_some_and(|t| t.ends_with('\n') || t.is_empty());
    let block = {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        dsh_item(&refs)
    };
    match (block, a) {
        (None, Action::Remove) => Edit::Unchanged,
        (None, Action::Install) => {
            if !lines.is_empty() && !lines.last().is_some_and(|l| l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.extend(dsh_new_item(exe).lines().map(str::to_string));
            verdict(finish(lines, had_newline), text)
        }
        (Some((start, end)), action) => {
            match action {
                Action::Remove => {
                    // Take the `- insert:` line with the item when it wraps exactly it.
                    let mut from = start;
                    let mut i = start;
                    while i > 0 {
                        i -= 1;
                        if is_noise(lines[i].as_str()) {
                            continue;
                        }
                        if lines[i].trim() == "- insert:" {
                            from = i;
                        }
                        break;
                    }
                    lines.drain(from..end);
                    verdict(finish(lines, had_newline), text)
                }
                Action::Install => {
                    let (command, env, env_indent) = {
                        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
                        let Some(command) =
                            (start..end).find(|i| lines[*i].trim().starts_with("command:"))
                        else {
                            return Edit::Refuse(refusal(exe, "the mcp-qx item has no config.command"));
                        };
                        let Some((env, env_indent)) = dsh_env(&refs, start, end) else {
                            return Edit::Refuse(refusal(exe, "the mcp-qx item has no config.env block"));
                        };
                        (command, env, env_indent)
                    };
                    let ci = indent_of(lines[command].as_str());
                    lines[command] = format!("{}command: '{}'", " ".repeat(ci), slash(exe));

                    let after = lines[env].trim()["env:".len()..].trim().to_string();
                    if !after.is_empty() && after != "{}" {
                        return Edit::Refuse(refusal(exe, "config.env is a flow-style mapping"));
                    }
                    if after == "{}" {
                        lines[env] = format!("{}env:", " ".repeat(env_indent));
                    }
                    // Children run until something is no deeper than the `env:` line.
                    let child_end = {
                        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
                        block_end(&refs, env, env_indent)
                    };
                    let existing = (env + 1..child_end).find(|i| lines[*i].trim().starts_with("QX_CLIENT:"));
                    match existing {
                        Some(i) => {
                            let ci = indent_of(lines[i].as_str());
                            lines[i] = format!("{}QX_CLIENT: dsh", " ".repeat(ci));
                        }
                        None => {
                            let ci = (env + 1..child_end)
                                .map(|i| indent_of(lines[i].as_str()))
                                .min()
                                .unwrap_or(env_indent + 2);
                            lines.insert(env + 1, format!("{}QX_CLIENT: dsh", " ".repeat(ci)));
                        }
                    }
                    verdict(finish(lines, had_newline), text)
                }
            }
        }
    }
}

fn dsh_new_item(exe: &Path) -> String {
    format!(
        "- insert:
    - id: mcp-qx
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        serverName: qx
        transport: stdio
        command: '{}'
        args:
          - mcp
        env:
          QX_CLIENT: dsh
        toolCallTimeoutMs: 300000
        failOnStartupError: true
",
        slash(exe)
    )
}

pub fn dsh_command(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let (start, end) = dsh_item(&lines)?;
    (start..end).find_map(|i| yaml_scalar(&lines[i], "command"))
}

// --------------------------------------------------------------------- hermes

fn top_child(lines: &[&str], key: &str) -> Option<(usize, usize)> {
    let want = format!("{key}:");
    let start = lines
        .iter()
        .position(|l| indent_of(l) == 0 && l.trim_start().starts_with(&want))?;
    Some((start, block_end(lines, start, 0)))
}

/// A rewrite that produces the same bytes is not a change.
fn verdict(out: String, original: Option<&str>) -> Edit {
    if original == Some(out.as_str()) {
        Edit::Unchanged
    } else {
        Edit::Changed(out)
    }
}

pub fn edit_hermes(text: Option<&str>, exe: &Path, a: Action) -> Edit {
    let source = text.unwrap_or("");
    if has_tab_indent(source) {
        return Edit::Refuse(refusal(exe, "config.yaml uses tab indentation"));
    }
    let had_newline = text.is_some_and(|t| t.ends_with('\n') || t.is_empty());
    let mut lines: Vec<String> = source.lines().map(str::to_string).collect();

    let servers = {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        top_child(&refs, "mcp_servers")
    };
    let Some((servers, end)) = servers else {
        return match a {
            Action::Remove => Edit::Unchanged,
            Action::Install => {
                if !lines.is_empty() && !lines.last().is_some_and(|l| l.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push("mcp_servers:".to_string());
                lines.push(hermes_block(exe, "  ", &[]));
                verdict(finish(expand(lines), had_newline), text)
            }
        };
    };
    // A flow-style mapping hides the whole server list on one line.
    let inline = lines[servers].trim()["mcp_servers:".len()..].trim().to_string();
    if !inline.is_empty() && inline != "{}" {
        return Edit::Refuse(refusal(exe, "mcp_servers is a flow-style mapping"));
    }
    if inline == "{}" {
        lines[servers] = "mcp_servers:".to_string();
    }

    // The qx child of `mcp_servers`, at the indent its siblings use.
    let child_indent = (servers + 1..end)
        .map(|i| indent_of(lines[i].as_str()))
        .filter(|i| *i > 0)
        .min()
        .unwrap_or(2);
    let qx = (servers + 1..end).find(|i| {
        indent_of(lines[*i].as_str()) == child_indent && lines[*i].trim().starts_with("qx:")
    });
    match (qx, a) {
        (None, Action::Remove) => Edit::Unchanged,
        (None, Action::Install) => {
            let block = hermes_block(exe, &" ".repeat(child_indent), &[]);
            let last_child = (servers + 1..end).filter(|i| !is_noise(lines[*i].as_str())).next_back();
            let insert_at = last_child
                .map(|i| {
                    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
                    block_end(&refs, i, indent_of(lines[i].as_str()))
                })
                .unwrap_or(servers + 1);
            lines.insert(insert_at, block);
            verdict(finish(expand(lines), had_newline), text)
        }
        (Some(start), action) => {
            let qend = {
                let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
                block_end(&refs, start, child_indent)
            };
            match action {
                Action::Remove => {
                    lines.drain(start..qend);
                    verdict(finish(lines, had_newline), text)
                }
                Action::Install => {
                    let kept = kept_env(&lines, start, qend);
                    let block = hermes_block(exe, &" ".repeat(child_indent), &kept);
                    lines.splice(start..qend, std::iter::once(block));
                    verdict(finish(expand(lines), had_newline), text)
                }
            }
        }
    }
}

/// Split any multi-line element back into one element per line.
fn expand(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .flat_map(|l| l.split('\n').map(str::to_string).collect::<Vec<_>>())
        .collect()
}

/// `qx:` at `indent`, with the matrix keys.
fn hermes_block(exe: &Path, indent: &str, kept: &[String]) -> String {
    let deeper = format!("{indent}  ");
    let mut out = format!(
        "{indent}qx:\n{deeper}command: '{}'\n{deeper}args: [mcp]\n{deeper}env:\n{deeper}  QX_CLIENT: hermes\n{deeper}timeout: 600\n{deeper}connect_timeout: 60",
        exe.to_string_lossy()
    );
    for line in kept {
        out.push('\n');
        out.push_str(&format!("{deeper}  {line}"));
    }
    out
}

/// `KEY: value` lines under the child's `env:`, which the user may have set.
fn kept_env(lines: &[String], start: usize, end: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut env_indent = None;
    for i in start..end {
        let line = &lines[i];
        let t = line.trim();
        if t == "env:" {
            env_indent = Some(indent_of(line.as_str()));
            continue;
        }
        if let Some(ei) = env_indent {
            if indent_of(line.as_str()) > ei && !t.is_empty() && !t.starts_with("QX_CLIENT:") {
                out.push(t.to_string());
            }
        }
    }
    out
}

pub fn hermes_command(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let (servers, end) = top_child(&lines, "mcp_servers")?;
    let child_indent = (servers + 1..end)
        .map(|i| indent_of(lines[i]))
        .filter(|i| *i > 0)
        .min()
        .unwrap_or(2);
    let qx = (servers + 1..end)
        .find(|i| indent_of(lines[*i]) == child_indent && lines[*i].trim().starts_with("qx:"))?;
    let qend = block_end(&lines, qx, child_indent);
    (qx..qend).find_map(|i| yaml_scalar(lines[i], "command"))
}

/// The value of a `key: value` line, single quotes stripped.
fn yaml_scalar(line: &str, key: &str) -> Option<String> {
    let t = line.trim();
    let rest = t.strip_prefix(&format!("{key}:"))?.trim();
    let rest = rest.strip_prefix('\'').unwrap_or(rest);
    let rest = rest.strip_suffix('\'').unwrap_or(rest);
    (!rest.is_empty()).then(|| rest.to_string())
}

fn finish(lines: Vec<String>, trailing_newline: bool) -> String {
    let mut out = lines.join("\n");
    if trailing_newline && !out.is_empty() {
        out.push('\n');
    }
    out
}

fn refusal(exe: &Path, why: &str) -> String {
    format!(
        "{why}. Edit it by hand, or paste this equivalent:\n{}",
        dsh_new_item(exe)
    )
}

/// Delete `dsh_home\bin\qx*.exe` files that no patch file under `dsh_home` references.
pub fn prune_dsh_binaries(dsh_home: &Path) -> usize {
    let bin = dsh_home.join("bin");
    let Ok(entries) = std::fs::read_dir(&bin) else {
        return 0;
    };
    let referenced = dsh_references(dsh_home);
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !path.is_file() || !name.starts_with("qx") || !name.ends_with(".exe") {
            continue;
        }
        // Match on the file name too: a patch may name the copy through another prefix
        // (forward slashes, another drive) and that copy is still in use.
        let text = path.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
        if referenced
            .iter()
            .any(|r| *r == text || r.rsplit('/').next().is_some_and(|n| n == name))
        {
            continue;
        }
        // A running server holds its image open; that copy is not ours to remove.
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn dsh_references(dsh_home: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dsh_home.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase());
                if !matches!(name.as_deref(), Some("node_modules" | ".git")) {
                    stack.push(path);
                }
                continue;
            }
            if path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase())
                != Some("cordis.patch.yml".to_string())
            {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines() {
                if let Some(value) = yaml_scalar(line, "command") {
                    out.push(value.replace('\\', "/").to_ascii_lowercase());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DSH: &str = "# Global dsh patch\n- insert:\n    - id: mcp-qx\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: qx\n        transport: stdio\n        command: 'C:/Users/USER/.dsh/bin/qx-75ce61d91cba.exe'\n        args:\n          - mcp\n        env:\n          QX_PYTHON_FAST: tls\n        toolCallTimeoutMs: 300000\n        failOnStartupError: true\n";

    fn changed(edit: Edit) -> String {
        match edit {
            Edit::Changed(text) => text,
            Edit::Refuse(reason) => panic!("refused: {reason}"),
            Edit::Unchanged => panic!("expected a change"),
        }
    }

    #[test]
    fn dsh_repoints_command_and_tags_env_only() {
        let exe = Path::new(r"C:\Users\USER\.cargo\bin\qx.exe");
        let out = changed(edit_dsh(Some(DSH), exe, Action::Install));
        assert!(
            out.contains("        command: 'C:/Users/USER/.cargo/bin/qx.exe'\n"),
            "{out}"
        );
        assert!(
            out.contains("        env:\n          QX_CLIENT: dsh\n          QX_PYTHON_FAST: tls\n"),
            "{out}"
        );
        assert_eq!(out.lines().count(), DSH.lines().count() + 1);
        // Remove our two edits and the file must be byte-identical to the original.
        let restored = out
            .replace("          QX_CLIENT: dsh\n", "")
            .replace(
                "        command: 'C:/Users/USER/.cargo/bin/qx.exe'",
                "        command: 'C:/Users/USER/.dsh/bin/qx-75ce61d91cba.exe'",
            );
        assert_eq!(restored, DSH);
        assert!(matches!(edit_dsh(Some(&out), exe, Action::Install), Edit::Unchanged));
        assert_eq!(dsh_command(&out).as_deref(), Some("C:/Users/USER/.cargo/bin/qx.exe"));
    }

    #[test]
    fn dsh_appends_when_absent_and_refuses_flow_env() {
        let exe = Path::new(r"C:\q\qx.exe");
        let out = changed(edit_dsh(None, exe, Action::Install));
        assert!(out.contains("- id: mcp-qx"), "{out}");
        assert!(out.contains("QX_CLIENT: dsh"), "{out}");
        let flow = "- insert:\n    - id: mcp-qx\n      config:\n        env: {A: b}\n";
        assert!(matches!(
            edit_dsh(Some(flow), exe, Action::Install),
            Edit::Refuse(_)
        ));
    }

    #[test]
    fn hermes_adds_qx_beside_other_servers_preserving_bytes() {
        let src = "model: x\nmcp_servers:\n  codegraph:\n    command: cg\n    timeout: 120\nplatform_toolsets:\n  cli: [mcp-codegraph]\n";
        let exe = Path::new(r"C:\q\qx.exe");
        let out = changed(edit_hermes(Some(src), exe, Action::Install));
        assert!(out.starts_with("model: x\nmcp_servers:\n  codegraph:\n    command: cg\n    timeout: 120\n"), "{out}");
        assert!(out.ends_with("platform_toolsets:\n  cli: [mcp-codegraph]\n"), "{out}");
        assert!(out.contains("  qx:\n    command: 'C:\\q\\qx.exe'\n    args: [mcp]\n"), "{out}");
        assert!(out.contains("    connect_timeout: 60\n"), "{out}");
        assert!(matches!(edit_hermes(Some(&out), exe, Action::Install), Edit::Unchanged));
        assert_eq!(hermes_command(&out).as_deref(), Some(r"C:\q\qx.exe"));
    }

    #[test]
    fn hermes_refuses_unknown_shapes() {
        let exe = Path::new(r"C:\q\qx.exe");
        assert!(matches!(
            edit_hermes(Some("mcp_servers: {codegraph: {command: cg}}\n"), exe, Action::Install),
            Edit::Refuse(_)
        ));
        assert!(matches!(
            edit_hermes(Some("mcp_servers:\n\tqx:\n\t\tcommand: c\n"), exe, Action::Install),
            Edit::Refuse(_)
        ));
    }

    #[test]
    fn yaml_remove() {
        let exe = Path::new(r"C:\q\qx.exe");
        let dsh = changed(edit_dsh(Some(DSH), exe, Action::Remove));
        assert!(!dsh.contains("mcp-qx"), "{dsh}");
        assert!(!dsh.contains("- insert:"), "{dsh}");
        let hermes = "model: x\nmcp_servers:\n  qx:\n    command: 'c'\n  other:\n    command: o\n";
        let out = changed(edit_hermes(Some(hermes), exe, Action::Remove));
        assert!(!out.contains("qx:"), "{out}");
        assert!(out.contains("  other:\n    command: o\n"), "{out}");
    }

    #[test]
    fn dsh_cleanup_deletes_only_unreferenced_copies() {
        let root = std::env::temp_dir().join(format!("qx-dshbin-{}", std::process::id()));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["qx.exe", "qx-aaa.exe", "qx-bbb.exe"] {
            std::fs::write(bin.join(name), "x").unwrap();
        }
        std::fs::write(
            root.join("cordis.patch.yml"),
            "        command: 'C:/q/.dsh/bin/qx-bbb.exe'\n",
        )
        .unwrap();
        assert_eq!(prune_dsh_binaries(&root), 2);
        assert!(bin.join("qx-bbb.exe").is_file());
        assert!(!bin.join("qx-aaa.exe").exists());
        assert!(!bin.join("qx.exe").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
