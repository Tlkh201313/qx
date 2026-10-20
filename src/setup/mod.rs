//! `qx setup`: register qx in each agent CLI's own config, byte-preservingly.
//!
//! Clients disagree about the file, the format, the key path and the value shape. This
//! module owns the mapping; each format lives in its own submodule so a comments-aware
//! JSONC editor, a TOML block editor and a YAML block editor never share scope.

pub mod jsonc;
pub mod toml;
pub mod yaml;

use crate::client::Client;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Where each client keeps its configuration, resolved once from the environment.
pub struct Paths {
    pub home: PathBuf,
    /// Kept for callers that resolve a second location under it (Hermes, Codex).
    #[allow(dead_code)]
    pub local_app_data: PathBuf,
    pub dsh_home: PathBuf,
    pub hermes_home: PathBuf,
    pub codex_home: PathBuf,
}

impl Paths {
    pub fn from_env() -> Paths {
        let home = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_default();
        let local_app_data = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Local"));
        let pick = |key: &str, fallback: PathBuf| {
            std::env::var_os(key)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or(fallback)
        };
        Paths {
            dsh_home: pick("DSH_HOME", home.join(".dsh")),
            hermes_home: pick("HERMES_HOME", local_app_data.join("hermes")),
            codex_home: pick("CODEX_HOME", home.join(".codex")),
            home,
            local_app_data,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    Install,
    Remove,
}

pub enum Edit {
    Unchanged,
    Changed(String),
    /// The file does not look like what we know how to edit; carries the paste-ready snippet.
    Refuse(String),
}

pub struct Change {
    pub client: Client,
    pub file: PathBuf,
    pub edit: Edit,
}

/// One entry per client; editing a file twice for one client is a bug.
pub fn plan(c: Client, p: &Paths, exe: &Path, a: Action) -> Result<Vec<Change>, String> {
    match c {
        Client::ClaudeCode => Ok(vec![Change {
            client: c,
            file: claude_file(p),
            edit: edit_json_file(claude_file(p).as_path(), &["mcpServers", "qx"], |existing| {
                claude_entry(exe, existing)
            }, a),
        }]),
        Client::OpenCode => {
            let (file, alternative) = opencode_file(p);
            let path: &[&str] = &["mcp", "servers", "qx"];
            // A legacy `mcp.qx` object means the file shape is one we do not understand.
            if file.exists() {
                let text = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
                if legacy_opencode(&text) {
                    return Ok(vec![Change {
                        client: c,
                        file: file.clone(),
                        edit: Edit::Refuse(opencode_refusal(&file, exe)),
                    }]);
                }
            }
            let _ = alternative;
            Ok(vec![Change {
                client: c,
                file: file.clone(),
                edit: edit_json_file(&file, path, |existing| opencode_entry(exe, existing), a),
            }])
        }
        Client::Codex => {
            let file = p.codex_home.join("config.toml");
            let text = read_optional(&file)?;
            Ok(vec![Change {
                client: c,
                file,
                edit: toml::edit_codex(text.as_deref(), exe, a),
            }])
        }
        Client::Dsh => {
            let file = p.dsh_home.join("cordis.patch.yml");
            let text = read_optional(&file)?;
            Ok(vec![Change {
                client: c,
                file,
                edit: yaml::edit_dsh(text.as_deref(), exe, a),
            }])
        }
        Client::Hermes => {
            let file = p.hermes_home.join("config.yaml");
            let text = read_optional(&file)?;
            Ok(vec![Change {
                client: c,
                file,
                edit: yaml::edit_hermes(text.as_deref(), exe, a),
            }])
        }
        Client::Pi => Err(format!(
            "{} setup is not implemented in this build",
            c.key()
        )),
        Client::Generic => Err("generic is not a client that can be configured".into()),
    }
}

fn claude_file(p: &Paths) -> PathBuf {
    p.home.join(".claude.json")
}

/// Whichever OpenCode file already holds qx, else the first that exists, else `opencode.json`.
fn opencode_file(p: &Paths) -> (PathBuf, Option<PathBuf>) {
    let dir = p.home.join(".config").join("opencode");
    let json = dir.join("opencode.json");
    let jsonc = dir.join("opencode.jsonc");
    for file in [&json, &jsonc] {
        if let Ok(text) = std::fs::read_to_string(file) {
            if has_qx(&text) {
                return (file.clone(), None);
            }
        }
    }
    for file in [&json, &jsonc] {
        if file.exists() {
            return (file.clone(), None);
        }
    }
    (json, Some(jsonc))
}

fn has_qx(text: &str) -> bool {
    jsonc::member(text, &["mcp", "servers", "qx"])
        .map(|r| r.is_some())
        .unwrap_or(false)
}

fn legacy_opencode(text: &str) -> bool {
    jsonc::member(text, &["mcp", "qx"]).ok().flatten().is_some()
}

fn opencode_refusal(file: &Path, exe: &Path) -> String {
    format!(
        "{} has a legacy `mcp.qx` entry. Move it under `\"servers\"`, or paste:\n  \"mcp\": {{\n    \"servers\": {{\n      \"qx\": {}\n    }}\n  }}",
        file.display(),
        serde_json::to_string_pretty(&opencode_entry(exe, None)).unwrap_or_default()
    )
}

fn read_optional(file: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(file) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", file.display())),
    }
}

fn edit_json_file(
    file: &Path,
    path: &[&str],
    entry: impl Fn(Option<&Value>) -> Value,
    a: Action,
) -> Edit {
    let text = match read_optional(file) {
        Ok(text) => text,
        Err(e) => return Edit::Refuse(e),
    };
    match text.as_deref() {
        None => match a {
            Action::Remove => Edit::Unchanged,
            Action::Install => Edit::Changed(match jsonc::set_member("{}", path, &entry(None)) {
                Ok(text) => text,
                Err(e) => return Edit::Refuse(e),
            }),
        },
        Some(current) => {
            let existing = jsonc::member(current, path)
                .ok()
                .flatten()
                .and_then(|range| serde_json::from_str(&jsonc::to_strict_json(&current[range])).ok());
            let updated = match a {
                Action::Install => jsonc::set_member(current, path, &entry(existing.as_ref())),
                Action::Remove => jsonc::remove_member(current, path),
            };
            match updated {
                Ok(text) if text == current => Edit::Unchanged,
                Ok(text) => Edit::Changed(text),
                Err(e) => Edit::Refuse(e),
            }
        }
    }
}

/// `env` is merged: a user's own keys survive, ours overwrite.
fn merge_env(existing: Option<&Value>, client: &str, extra: &[(String, Value)]) -> Value {
    let mut env = serde_json::Map::new();
    if let Some(Value::Object(map)) = existing.and_then(|e| e.get("env")) {
        for (k, v) in map {
            env.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in extra {
        env.insert(k.clone(), v.clone());
    }
    env.insert("QX_CLIENT".to_string(), Value::String(client.to_string()));
    Value::Object(env)
}

fn claude_entry(exe: &Path, existing: Option<&Value>) -> Value {
    json!({
        "type": "stdio",
        "command": exe.to_string_lossy(),
        "args": ["mcp"],
        "env": merge_env(existing, "claude-code", &[]),
    })
}

fn opencode_entry(exe: &Path, existing: Option<&Value>) -> Value {
    let mut entry = json!({
        "type": "local",
        "command": [exe.to_string_lossy(), "mcp"],
        "environment": merge_env(existing, "opencode", &[]),
        // Without this, OpenCode hides the qx tools behind its own `execute` tool.
        "codemode": false,
        "timeout": { "execution": 600000 },
    });
    // A user's own timeout object may carry other keys; keep them.
    if let Some(t) = existing.and_then(|e| e.get("timeout")).and_then(Value::as_object) {
        let mut timeouts = t.clone();
        timeouts.insert("execution".into(), json!(600000));
        entry["timeout"] = Value::Object(timeouts);
    }
    entry
}

/// Write one planned change: back up, write a temp file, rename over the original.
pub fn apply(ch: &Change) -> std::io::Result<()> {
    let Edit::Changed(text) = &ch.edit else {
        return Ok(());
    };
    if let Some(parent) = ch.file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if ch.file.exists() {
        let backup = ch.file.with_extension(format!(
            "{}qx-bak",
            ch.file
                .extension()
                .map(|e| format!("{}.", e.to_string_lossy()))
                .unwrap_or_default()
        ));
        std::fs::copy(&ch.file, &backup)?;
    }
    let tmp = ch
        .file
        .with_extension(format!("qx-tmp-{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &ch.file).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

pub struct Status {
    pub client: Client,
    pub file: Option<PathBuf>,
    pub command: Option<String>,
    pub notes: Vec<String>,
}

pub fn status(c: Client, p: &Paths) -> Status {
    let mut notes = Vec::new();
    let (file, command) = match c {
        Client::ClaudeCode => {
            let file = claude_file(p);
            let command = read_json_command(&file, &["mcpServers", "qx", "command"]);
            (Some(file), command)
        }
        Client::OpenCode => {
            let (file, _) = opencode_file(p);
            let command = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| {
                    let range = jsonc::member(&text, &["mcp", "servers", "qx", "command"]).ok()??;
                    let value: Value =
                        serde_json::from_str(&jsonc::to_strict_json(&text[range])).ok()?;
                    Some(match value {
                        Value::Array(items) => items.first()?.as_str()?.to_string(),
                        other => other.as_str()?.to_string(),
                    })
                });
            if let Ok(text) = std::fs::read_to_string(&file) {
                let codemode = jsonc::member(&text, &["mcp", "servers", "qx", "codemode"])
                    .ok()
                    .flatten()
                    .and_then(|r| serde_json::from_str(&jsonc::to_strict_json(&text[r])).ok());
                if codemode != Some(Value::Bool(false)) {
                    notes.push("codemode is not false: the model sees qx only through `execute`".into());
                }
            }
            (Some(file), command)
        }
        Client::Codex => {
            let file = p.codex_home.join("config.toml");
            let command = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| toml::codex_command(&text));
            (Some(file), command)
        }
        Client::Dsh => {
            let file = p.dsh_home.join("cordis.patch.yml");
            let command = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| yaml::dsh_command(&text));
            (Some(file), command)
        }
        Client::Hermes => {
            let file = p.hermes_home.join("config.yaml");
            let command = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| yaml::hermes_command(&text));
            (Some(file), command)
        }
        Client::Pi => {
            let file = p.home.join(".pi").join("agent").join("mcp.json");
            let command = read_json_command(&file, &["mcpServers", "qx", "command"]);
            (Some(file), command)
        }
        Client::Generic => (None, None),
    };
    Status {
        client: c,
        file,
        command,
        notes,
    }
}

fn read_json_command(file: &Path, path: &[&str]) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let range = jsonc::member(&text, path).ok()??;
    serde_json::from_str(&jsonc::to_strict_json(&text[range])).ok()
}

pub fn run(args: &[String]) -> i32 {
    let mut action = Action::Install;
    let mut dry = false;
    let mut clients: Vec<Client> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--dry-run" => dry = true,
            "--remove" => action = Action::Remove,
            "all" => clients.extend(Client::ALL),
            other if other.starts_with('-') => {
                eprintln!("qx setup: unknown flag {other}");
                return 2;
            }
            other => match Client::from_key(other) {
                Some(Client::Generic) | None => {
                    eprintln!("qx setup: unknown client {other}");
                    return 2;
                }
                Some(c) => clients.push(c),
            },
        }
    }
    if clients.is_empty() {
        clients.extend(Client::ALL);
    }
    let paths = Paths::from_env();
    let exe = match crate::deploy::canonical() {
        Some(path) => path,
        None => {
            eprintln!("qx setup: no cargo bin directory (set CARGO_HOME or USERPROFILE)");
            return 2;
        }
    };
    let mut refused = false;
    let mut dsh_applied = false;
    for c in clients {
        let changes = match plan(c, &paths, &exe, action) {
            Ok(changes) => changes,
            Err(e) => {
                println!("{}: skipped: {e}", c.key());
                continue;
            }
        };
        if changes.is_empty() {
            println!("{}: unchanged", c.key());
            continue;
        }
        for change in changes {
            let key = change.client.key();
            match &change.edit {
                Edit::Unchanged => println!("{key}: unchanged ({})", change.file.display()),
                Edit::Refuse(reason) => {
                    refused = true;
                    println!("{key}: refused: {reason}");
                }
                Edit::Changed(_) if dry => println!("{key}: would update {}", change.file.display()),
                Edit::Changed(_) => match apply(&change) {
                    Ok(()) => {
                        if change.client == Client::Dsh {
                            dsh_applied = true;
                        }
                        println!("{key}: updated {}", change.file.display());
                    }
                    Err(e) => {
                        refused = true;
                        println!("{key}: cannot write {}: {e}", change.file.display());
                    }
                },
            }
        }
    }
    if dsh_applied && action == Action::Install {
        // dsh used to need a versioned copy per build; now one canonical binary is enough,
        // and every stale copy is a different qx under a client.
        let removed = yaml::prune_dsh_binaries(&paths.dsh_home);
        if removed > 0 {
            println!("dsh: removed {removed} unreferenced qx cop(ies) from {}", paths.dsh_home.join("bin").display());
        }
    }
    if refused {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("qx-setup-{tag}-{}", std::process::id()));
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

    fn changed(edit: &Edit) -> Option<&str> {
        match edit {
            Edit::Changed(text) => Some(text),
            _ => None,
        }
    }

    #[test]
    fn claude_install_is_idempotent_and_keeps_env() {
        let p = paths("claude");
        let file = claude_file(&p);
        std::fs::write(
            &file,
            r#"{
  "mcpServers": {
    "qx": {
      "type": "stdio",
      "command": "C:\\old\\qx.exe",
      "args": ["mcp"],
      "env": {"QX_GUARD": "safe"}
    },
    "other": {"command": "x"}
  }
}
"#,
        )
        .unwrap();
        let exe = Path::new(r"C:\new\qx.exe");
        let changes = plan(Client::ClaudeCode, &p, exe, Action::Install).unwrap();
        let text = changed(&changes[0].edit).expect("an update").to_string();
        let v: Value = serde_json::from_str(&jsonc::to_strict_json(&text)).unwrap();
        assert_eq!(v["mcpServers"]["qx"]["command"], exe.to_string_lossy().as_ref());
        assert_eq!(v["mcpServers"]["qx"]["env"]["QX_GUARD"], "safe");
        assert_eq!(v["mcpServers"]["qx"]["env"]["QX_CLIENT"], "claude-code");
        assert_eq!(v["mcpServers"]["other"]["command"], "x");

        // Applying the change and planning again must be a no-op.
        std::fs::write(&file, &text).unwrap();
        let again = plan(Client::ClaudeCode, &p, exe, Action::Install).unwrap();
        assert!(matches!(again[0].edit, Edit::Unchanged));
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn opencode_sets_codemode_false_in_the_file_that_has_qx() {
        let p = paths("opencode");
        let dir = p.home.join(".config").join("opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("opencode.json"),
            "{\n  // hand-edited\n  \"mcp\": {\"servers\": {\"qx\": {\"type\": \"local\"}}},\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("opencode.jsonc"), "{}\n").unwrap();
        let changes = plan(Client::OpenCode, &p, Path::new(r"C:\q\qx.exe"), Action::Install).unwrap();
        assert_eq!(changes[0].file.file_name().unwrap(), "opencode.json");
        let text = changed(&changes[0].edit).expect("an update");
        let v: Value = serde_json::from_str(&jsonc::to_strict_json(text)).unwrap();
        assert_eq!(v["mcp"]["servers"]["qx"]["codemode"], false);
        assert_eq!(v["mcp"]["servers"]["qx"]["timeout"]["execution"], 600000);
        assert_eq!(v["mcp"]["servers"]["qx"]["environment"]["QX_CLIENT"], "opencode");
        assert_eq!(v["mcp"]["servers"]["qx"]["command"][1], "mcp");
        assert!(text.contains("// hand-edited"), "the comment must survive");
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn opencode_refuses_unknown_shapes() {
        let p = paths("opencode-legacy");
        let dir = p.home.join(".config").join("opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("opencode.json"),
            r#"{"mcp": {"qx": {"type": "local", "command": ["x"]}}}"#,
        )
        .unwrap();
        let changes = plan(Client::OpenCode, &p, Path::new(r"C:\q\qx.exe"), Action::Install).unwrap();
        match &changes[0].edit {
            Edit::Refuse(reason) => assert!(reason.contains("\"servers\""), "{reason}"),
            _ => panic!("a legacy mcp.qx entry must be refused"),
        }
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn remove_deletes_only_the_qx_entry() {
        let p = paths("remove");
        let file = claude_file(&p);
        std::fs::write(
            &file,
            "{\n  \"mcpServers\": {\n    \"other\": {\"command\": \"x\"},\n    \"qx\": {\"command\": \"y\"}\n  },\n  \"keep\": [1, 2]\n}\n",
        )
        .unwrap();
        let changes = plan(Client::ClaudeCode, &p, Path::new(r"C:\q\qx.exe"), Action::Remove).unwrap();
        let text = changed(&changes[0].edit).expect("a removal");
        let v: Value = serde_json::from_str(&jsonc::to_strict_json(text)).unwrap();
        assert!(v["mcpServers"].get("qx").is_none());
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["keep"][1], 2);
        // Removing again changes nothing.
        std::fs::write(&file, text).unwrap();
        let again = plan(Client::ClaudeCode, &p, Path::new(r"C:\q\qx.exe"), Action::Remove).unwrap();
        assert!(matches!(again[0].edit, Edit::Unchanged));
        let _ = std::fs::remove_dir_all(&p.home);
    }
}
