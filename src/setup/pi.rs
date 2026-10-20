//! pi: its own MCP client from 0.99 on, a qx extension before that.
//!
//! pi 0.85 (what this machine has) has no MCP configuration at all, so `qx setup pi`
//! installs a small extension that speaks the protocol itself. Version detection
//! reads pi's own `package.json`: guessing from a config path would silently install
//! the wrong half.

use super::jsonc::{set_member, to_strict_json};
use super::{Action, Edit, Paths};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The version whose built-in MCP client replaces the extension.
const BUILTIN_MCP: (u32, u32) = (0, 99);

const EXTENSION: &str = include_str!("../../integrations/pi/qx.js");
const BRIDGE: &str = include_str!("../../integrations/pi/qx/bridge.js");
const BRIDGE_PACKAGE: &str = "{\n  \"type\": \"module\"\n}\n";

pub fn pi_version(_p: &Paths) -> Option<(u32, u32)> {
    let crate::resolve::Target::Direct { prefix, .. } = crate::resolve::resolve("pi", None) else {
        return None;
    };
    let mut dir = Path::new(prefix.first()?).parent()?.to_path_buf();
    loop {
        if let Ok(text) = std::fs::read_to_string(dir.join("package.json")) {
            if let Ok(pkg) = serde_json::from_str::<Value>(&text) {
                let named = pkg
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.contains("pi-coding-agent") || n == "pi");
                if named {
                    return pkg
                        .get("version")
                        .and_then(Value::as_str)
                        .and_then(minor_of);
                }
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn minor_of(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

fn extension_dir(p: &Paths) -> PathBuf {
    p.home.join(".pi").join("agent").join("extensions")
}

fn mcp_file(p: &Paths) -> PathBuf {
    p.home.join(".pi").join("agent").join("mcp.json")
}

/// One `(file, edit)` per config file this client needs.
pub fn edit_pi(
    version: Option<(u32, u32)>,
    p: &Paths,
    exe: &Path,
    a: Action,
) -> Vec<(PathBuf, Edit)> {
    let Some(version) = version else {
        return vec![(
            mcp_file(p),
            Edit::Refuse("pi not found: install it first, or run `qx setup pi` from a shell where `pi` resolves".into()),
        )];
    };
    if version >= BUILTIN_MCP {
        let file = mcp_file(p);
        let entry = json!({
            "command": exe.to_string_lossy(),
            "args": ["mcp"],
            "env": { "QX_CLIENT": "pi" },
            "exposure": "direct",
            "timeout": 600,
        });
        let edit = match std::fs::read_to_string(&file) {
            Ok(text) => match a {
                Action::Install => match set_member(&text, &["mcpServers", "qx"], &entry) {
                    Ok(out) if out == text => Edit::Unchanged,
                    Ok(out) => {
                        // Never write a config that will not parse.
                        if serde_json::from_str::<Value>(&to_strict_json(&out)).is_err() {
                            Edit::Refuse(format!("{} would not parse after the edit", file.display()))
                        } else {
                            Edit::Changed(out)
                        }
                    }
                    Err(e) => Edit::Refuse(e),
                },
                Action::Remove => match super::jsonc::remove_member(&text, &["mcpServers", "qx"]) {
                    Ok(out) if out == text => Edit::Unchanged,
                    Ok(out) => Edit::Changed(out),
                    Err(e) => Edit::Refuse(e),
                },
            },
            Err(_) => match a {
                Action::Remove => Edit::Unchanged,
                Action::Install => Edit::Changed(
                    set_member("{}", &["mcpServers", "qx"], &entry).unwrap_or_default(),
                ),
            },
        };
        return vec![(file, edit)];
    }

    // pi older than 0.99: the extension, its bridge and the ESM marker beside them.
    let dir = extension_dir(p);
    let escaped = serde_json::to_string(&exe.to_string_lossy()).unwrap_or_default();
    let escaped = escaped.trim_matches('"');
    let files = [
        (dir.join("qx.js"), EXTENSION.replace("__QX_EXE__", escaped)),
        (dir.join("qx").join("bridge.js"), BRIDGE.to_string()),
        (dir.join("qx").join("package.json"), BRIDGE_PACKAGE.to_string()),
    ];
    files
        .into_iter()
        .map(|(file, wanted)| {
            let edit = match a {
                // Empty text means "remove the file": see `apply`.
                Action::Remove => match std::fs::read_to_string(&file) {
                    Ok(_) => Edit::Changed(String::new()),
                    Err(_) => Edit::Unchanged,
                },
                Action::Install => match std::fs::read_to_string(&file) {
                    Ok(current) if current == wanted => Edit::Unchanged,
                    _ => Edit::Changed(wanted),
                },
            };
            (file, edit)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("qx-pi-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Paths {
            home: root.clone(),
            local_app_data: root.join("local"),
            dsh_home: root.join("dsh"),
            hermes_home: root.join("hermes"),
            codex_home: root.join("codex"),
        }
    }

    #[test]
    fn pi_new_versions_get_mcp_json() {
        let p = paths("new");
        let exe = Path::new(r"C:\q\qx.exe");
        let out = edit_pi(Some((1, 3)), &p, exe, Action::Install);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, mcp_file(&p));
        let Edit::Changed(text) = &out[0].1 else {
            panic!("expected a change");
        };
        let v: Value = serde_json::from_str(&to_strict_json(text)).unwrap();
        assert_eq!(v["mcpServers"]["qx"]["exposure"], "direct");
        assert_eq!(v["mcpServers"]["qx"]["timeout"], 600);
        assert_eq!(v["mcpServers"]["qx"]["env"]["QX_CLIENT"], "pi");
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn pi_old_versions_get_the_extension() {
        let p = paths("old");
        let exe = Path::new(r"C:\q\qx.exe");
        let out = edit_pi(Some((0, 85)), &p, exe, Action::Install);
        assert_eq!(out.len(), 3);
        let qx = out
            .iter()
            .find(|(f, _)| f.ends_with("qx.js"))
            .map(|(_, e)| e)
            .expect("qx.js");
        let text = match qx {
            Edit::Changed(text) => text.clone(),
            _ => panic!("qx.js must be written"),
        };
        assert!(text.contains("registerTool"), "{text}");
        assert!(text.contains("mcp__qx__run"), "{text}");
        assert!(!text.contains("__QX_EXE__"), "the exe placeholder must be replaced");
        assert!(text.contains(r"C:\\q\\qx.exe"), "the exe must be JSON-escaped: {text}");
        let bridge = out
            .iter()
            .find(|(f, _)| f.ends_with("bridge.js"))
            .map(|(_, e)| e)
            .expect("bridge.js");
        match bridge {
            Edit::Changed(text) => assert!(!text.contains("__QX_EXE__")),
            _ => panic!("bridge.js must be written"),
        }
        // A second run is a no-op.
        for (file, edit) in out {
            if let Edit::Changed(text) = edit {
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(&file, text).unwrap();
            }
        }
        let again = edit_pi(Some((0, 85)), &p, exe, Action::Install);
        assert!(again.iter().all(|(_, e)| matches!(e, Edit::Unchanged)), "second run rewrote files");
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn pi_missing_is_refused() {
        let p = paths("none");
        let out = edit_pi(None, &p, Path::new(r"C:\q\qx.exe"), Action::Install);
        assert_eq!(out.len(), 1);
        match &out[0].1 {
            Edit::Refuse(reason) => assert!(reason.contains("pi not found"), "{reason}"),
            _ => panic!("a missing pi must be refused"),
        }
        let _ = std::fs::remove_dir_all(&p.home);
    }
}
