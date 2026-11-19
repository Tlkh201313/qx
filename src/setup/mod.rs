//! `qx setup`: register qx in each agent CLI's own config, byte-preservingly.
//!
//! Clients disagree about the file, the format, the key path and the value shape. This
//! module owns the mapping; each format lives in its own submodule so a comments-aware
//! JSONC editor, a TOML block editor and a YAML block editor never share scope.

pub mod jsonc;
pub mod pi;
pub mod toml;
pub mod yaml;

use crate::client::Client;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Where each client keeps its configuration, resolved once from the environment.
pub struct Paths {
    pub home: PathBuf,
    pub dsh_home: PathBuf,
    pub hermes_home: PathBuf,
    pub codex_home: PathBuf,
    /// `CLAUDE_CONFIG_DIR`: Claude Code then keeps both its files there instead of under the home.
    pub claude_config_dir: Option<PathBuf>,
    /// omp's agent dir: `PI_CODING_AGENT_DIR`, else `~/<PI_CONFIG_DIR or .omp>/agent`.
    /// shortcut: an `OMP_PROFILE` profile is not followed; add it if someone runs omp profiles.
    pub omp_agent_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Paths {
        Self::from_lookup(|k| std::env::var_os(k))
    }

    /// The home directory is `USERPROFILE`, else `HOME`; empty when neither is set (callers refuse
    /// then, rather than write client configs relative to the current directory).
    pub fn from_lookup(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Paths {
        let var = |k: &str| env(k).filter(|v| !v.is_empty());
        let home = var("USERPROFILE").or_else(|| var("HOME")).map(PathBuf::from).unwrap_or_default();
        let local_app_data = var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Local"));
        let pick = |key: &str, fallback: PathBuf| var(key).map(PathBuf::from).unwrap_or(fallback);
        Paths {
            dsh_home: pick("DSH_HOME", home.join(".dsh")),
            hermes_home: pick("HERMES_HOME", local_app_data.join("hermes")),
            codex_home: pick("CODEX_HOME", home.join(".codex")),
            claude_config_dir: var("CLAUDE_CONFIG_DIR").map(PathBuf::from),
            omp_agent_dir: pick(
                "PI_CODING_AGENT_DIR",
                home.join(var("PI_CONFIG_DIR").unwrap_or_else(|| ".omp".into())).join("agent"),
            ),
            home,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    Install,
    Remove,
}

#[derive(Debug)]
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
    plan_with(c, p, exe, a, crate::resolve::find_on_path("node").is_some())
}

/// `plan`, with whether `node` resolves on PATH given: the Claude statusLine is a node script.
fn plan_with(c: Client, p: &Paths, exe: &Path, a: Action, node: bool) -> Result<Vec<Change>, String> {
    match c {
        Client::ClaudeCode => {
            let settings = claude_settings(p);
            let script = claude_dir(p).join("qx-statusline.js");
            let sidecar = claude_dir(p).join("qx-statusline.json");
            let sidecar_text = read_optional(&sidecar)?;
            // An unreadable settings file is refused by its own edit, not by the whole plan; the
            // sidecar sees it as "unknown" and is left alone.
            let settings_text = read_optional(&settings);
            // Without node the statusLine could not run: install leaves settings.json's slot, the
            // script and the sidecar alone. Remove always runs, so it can give back what was there.
            let line = a == Action::Remove || node;
            let wanted = if line {
                claude_sidecar(settings_text.as_ref().map(Option::as_deref).map_err(|_| ()), sidecar_text.as_deref(), a)
            } else {
                None
            };
            let settings_edit = edit_claude_settings(&settings, exe, &script, sidecar_text.as_deref(), line, a);
            // A removal that refuses leaves qx's statusLine in place, so the script and the record of
            // what to restore must stay with it.
            let keep = !line || (a == Action::Remove && matches!(settings_edit, Edit::Refuse(_)));
            let (script_edit, sidecar_edit) = if keep {
                (Edit::Unchanged, Edit::Unchanged)
            } else {
                (
                    edit_generated(&script, fill_exe(STATUSLINE_SCRIPT, exe), a),
                    wanted.map_or(Edit::Unchanged, |wanted| edit_generated(&sidecar, wanted, a)),
                )
            };
            let mcp = Change {
                client: c,
                file: claude_file(p),
                edit: edit_json_file(claude_file(p).as_path(), &["mcpServers", "qx"], |existing| {
                    claude_entry(exe, existing)
                }, a),
            };
            let script = Change { client: c, edit: script_edit, file: script };
            let sidecar = Change { client: c, edit: sidecar_edit, file: sidecar };
            let settings = Change { client: c, edit: settings_edit, file: settings };
            // The settings file points at the script and names the sidecar's record: on install they
            // are written first, so a failed write never leaves the statusLine without them. On
            // remove the statusLine goes first, so a failure never leaves it without its script.
            Ok(match a {
                Action::Install => vec![mcp, script, sidecar, settings],
                Action::Remove => vec![mcp, settings, script, sidecar],
            })
        }
        Client::OpenCode => {
            let file = opencode_file(p);
            // V1 lists servers directly under `mcp`; V2 under `mcp.servers`. A file shows which one
            // it is; a new file gets V2's shape.
            let v1 = read_optional(&file)?.as_deref().is_some_and(opencode_v1);
            let path: &[&str] = if v1 { &["mcp", "qx"] } else { &["mcp", "servers", "qx"] };
            Ok(vec![
                Change {
                    client: c,
                    file: file.clone(),
                    edit: edit_json_file(
                        &file,
                        path,
                        |existing| if v1 { opencode_entry_v1(exe, existing) } else { opencode_entry(exe, existing) },
                        a,
                    ),
                },
            ])
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
        Client::Pi => Ok(pi::edit_pi(pi::pi_version(), p, exe, a)
            .into_iter()
            .map(|(file, edit)| Change { client: c, file, edit })
            .collect()),
        Client::Cursor => {
            let file = cursor_file(p);
            let config = cursor_config(p);
            Ok(vec![
                Change {
                    client: c,
                    file: file.clone(),
                    edit: edit_json_file(&file, &["mcpServers", "qx"], |existing| cursor_entry(exe, existing), a),
                },
                Change { client: c, edit: edit_cursor_statusline(&config, exe, a), file: config },
            ])
        }
        Client::Omp => {
            let file = p.omp_agent_dir.join("mcp.json");
            let hook = omp_extension(p);
            Ok(vec![
                Change {
                    client: c,
                    file: file.clone(),
                    edit: edit_json_file(&file, &["mcpServers", "qx"], |existing| omp_entry(exe, existing), a),
                },
                Change { client: c, edit: edit_generated(&hook, fill_exe(OMP_EXTENSION, exe), a), file: hook },
            ])
        }
        Client::Generic => Err("generic is not a client that can be configured".into()),
    }
}

/// omp's `bash` hook: omp mounts MCP tools under `xd://`, so its own shell is where qx pays off.
const OMP_EXTENSION: &str = include_str!("../../integrations/omp/qx.js");

/// The Claude Code status line: chains the user's own one and appends `qx gain --line`.
const STATUSLINE_SCRIPT: &str = include_str!("../../integrations/statusline/qx-statusline.js");

fn omp_extension(p: &Paths) -> PathBuf {
    p.omp_agent_dir.join("extensions").join("qx.js")
}

/// A file qx owns whole: written as `wanted`, removed as a whole (empty text, see `apply`).
fn edit_generated(file: &Path, wanted: String, a: Action) -> Edit {
    match (a, std::fs::read_to_string(file)) {
        (Action::Install, Ok(current)) if current == wanted => Edit::Unchanged,
        (Action::Install, _) => Edit::Changed(wanted),
        (Action::Remove, Ok(_)) => Edit::Changed(String::new()),
        (Action::Remove, Err(_)) => Edit::Unchanged,
    }
}

/// `template` with `__QX_EXE__` replaced by `exe`, escaped for a JS string literal.
fn fill_exe(template: &str, exe: &Path) -> String {
    let escaped = serde_json::to_string(&exe.to_string_lossy()).unwrap_or_default();
    template.replace("__QX_EXE__", escaped.trim_matches('"'))
}

/// Whether `content` is `template` as `fill_exe` makes it, whatever exe path it names.
fn generated_from(template: &str, content: &str) -> bool {
    let parts: Vec<&str> = template.split("__QX_EXE__").collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    content.len() >= first.len() + last.len() && content.starts_with(first) && content.ends_with(last)
}

/// omp's own `mcp.json` outranks the `~/.claude.json` entry it also imports. `timeout` is in ms:
/// omp's 30 s default would cut a `run` that waits its full ~100 s.
fn omp_entry(exe: &Path, existing: Option<&Value>) -> Value {
    overlay(existing, json!({
        "command": exe.to_string_lossy(),
        "args": ["mcp"],
        "env": merge_env(existing, "omp", &[]),
        "timeout": 600000,
    }))
}

/// Cursor's global MCP config; it reads `mcpServers` like Claude Code.
fn cursor_file(p: &Paths) -> PathBuf {
    p.home.join(".cursor").join("mcp.json")
}

fn cursor_entry(exe: &Path, existing: Option<&Value>) -> Value {
    overlay(existing, json!({
        "command": exe.to_string_lossy(),
        "args": ["mcp"],
        "env": merge_env(existing, "cursor", &[]),
    }))
}

fn claude_file(p: &Paths) -> PathBuf {
    p.claude_config_dir.as_ref().unwrap_or(&p.home).join(".claude.json")
}

/// The hooks live in the user settings, not next to the MCP servers in `.claude.json`.
fn claude_settings(p: &Paths) -> PathBuf {
    claude_dir(p).join("settings.json")
}

/// Where Claude Code keeps its settings, and where qx puts the status line script beside them.
fn claude_dir(p: &Paths) -> PathBuf {
    p.claude_config_dir.clone().unwrap_or_else(|| p.home.join(".claude"))
}

const STATUS_LINE: &[&str] = &["statusLine"];

/// Claude runs this through Git Bash on Windows, so the script path takes forward slashes. It starts
/// from the user's previous `statusLine` object, so `padding` and the like survive; only `type` and
/// `command` are qx's. No `refreshInterval`: qx forcing one would rerun the user's script on a timer.
fn claude_statusline_value(script: &Path, previous: Option<&Value>) -> Value {
    let script = script.to_string_lossy().replace('\\', "/");
    let mut value = previous.filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
    value["type"] = json!("command");
    value["command"] = json!(format!("node \"{script}\""));
    value
}

/// PowerShell rejects a quoted program name followed by arguments, so only a path with a space is quoted.
fn cursor_statusline_value(exe: &Path) -> Value {
    let exe = exe.to_string_lossy();
    let exe = if exe.contains(char::is_whitespace) { format!("\"{exe}\"") } else { exe.into_owned() };
    json!({
        "type": "command",
        "command": format!("{exe} gain --line"),
        "updateIntervalMs": 10000,
        "timeoutMs": 2000,
    })
}

/// A statusLine value qx wrote: Claude's `node ".../qx-statusline.js"` or Cursor's `qx gain --line`
/// (the exe quoted when its path has a space).
fn is_qx_statusline(v: &Value) -> bool {
    let Some(cmd) = v["command"].as_str() else { return false };
    if cmd.contains("qx-statusline.js") {
        return true;
    }
    let Some(exe) = cmd.strip_suffix(" gain --line") else { return false };
    let exe = exe.strip_prefix('"').and_then(|e| e.strip_suffix('"')).unwrap_or(exe);
    Path::new(exe).file_stem().is_some_and(|s| s.eq_ignore_ascii_case("qx"))
}

/// What the `statusLine` member of a settings text holds.
enum Slot {
    Free,
    Value(std::ops::Range<usize>, Value),
    /// The member exists but its text does not parse (or the document itself does not).
    Unreadable,
}

fn statusline_slot(text: &str) -> Slot {
    match jsonc::member(text, STATUS_LINE) {
        Ok(None) => Slot::Free,
        Ok(Some(range)) => match serde_json::from_str(&jsonc::to_strict_json(&text[range.clone()])) {
            Ok(value) => Slot::Value(range, value),
            Err(_) => Slot::Unreadable,
        },
        Err(_) => Slot::Unreadable,
    }
}

/// A blank file is an empty document (see `jsonc`), though it is not valid JSON.
fn is_blank(text: &str) -> bool {
    text.trim_start_matches('\u{feff}').trim().is_empty()
}

/// The sidecar: what `statusLine` held before qx took the slot. `raw` is that value's own source
/// text, so removing qx gives back its bytes (comments, indentation) and not a re-print.
fn render_sidecar(previous: &Value, raw: Option<&str>) -> String {
    let mut doc = json!({"previous": previous});
    if let Some(raw) = raw {
        doc["raw"] = json!(raw);
    }
    format!("{}\n", serde_json::to_string_pretty(&doc).unwrap_or_default())
}

/// The previous value and its source text, or `None` when there is no usable sidecar.
fn parse_sidecar(text: Option<&str>) -> Option<(Value, Option<String>)> {
    let doc = serde_json::from_str::<Value>(text.filter(|t| is_sidecar(t))?).ok()?;
    Some((doc["previous"].clone(), doc["raw"].as_str().map(str::to_string)))
}

/// Whether `text` is a sidecar as `render_sidecar` writes it.
fn is_sidecar(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|doc| doc.as_object().map(|m| m.contains_key("previous") && m.keys().all(|k| k == "previous" || k == "raw")))
        .unwrap_or(false)
}

/// What the sidecar must hold after this run; `None` leaves the file as it is. A slot that already
/// holds qx's own value keeps whatever sidecar there is (even a missing or corrupt one): qx's value
/// is never stored as the one to restore, and "nothing before" is never guessed when settings.json
/// could not be read.
fn claude_sidecar(settings: Result<Option<&str>, ()>, sidecar: Option<&str>, a: Action) -> Option<String> {
    let none = || Some(render_sidecar(&Value::Null, None));
    // The file is gone: the sidecar goes with it, or records that nothing was there before.
    let Some(text) = settings.ok()? else { return if a == Action::Install { none() } else { Some(String::new()) } };
    if !is_blank(text) && parse_strict(text).is_none() {
        return None;
    }
    match (statusline_slot(text), a) {
        (Slot::Unreadable, _) => None,
        (Slot::Free, Action::Install) => none(),
        (Slot::Free, Action::Remove) => Some(String::new()),
        (Slot::Value(_, v), Action::Install) if is_qx_statusline(&v) => None,
        (Slot::Value(range, v), Action::Install) => Some(render_sidecar(&v, Some(&text[range]))),
        // qx's statusLine can only be restored from a usable sidecar, which then goes with it.
        (Slot::Value(_, v), Action::Remove) if is_qx_statusline(&v) => parse_sidecar(sidecar).map(|_| String::new()),
        (Slot::Value(..), Action::Remove) => Some(String::new()),
    }
}

/// `current` with `statusLine` pointed at qx's script (Install) or given back what it held (Remove).
fn claude_statusline_text(current: &str, script: &Path, sidecar: Option<&str>, a: Action) -> Result<String, String> {
    let slot = statusline_slot(current);
    match a {
        Action::Install => {
            // The base for qx's value is what the user had: the sidecar's record when qx holds the slot
            // (a slot of qx's own keeps nothing of an older qx but its record), else the slot itself.
            let base = match &slot {
                Slot::Unreadable => {
                    return Err("the statusLine entry cannot be parsed, so qx will not overwrite it: edit it by hand".into());
                }
                Slot::Free => None,
                Slot::Value(_, v) if is_qx_statusline(v) => Some(parse_sidecar(sidecar).map_or_else(|| v.clone(), |(previous, _)| previous)),
                Slot::Value(_, v) => Some(v.clone()),
            };
            let ours = claude_statusline_value(script, base.as_ref());
            match slot {
                Slot::Value(_, v) if v == ours => Ok(current.to_string()),
                _ => jsonc::set_member(current, STATUS_LINE, &ours),
            }
        }
        Action::Remove => {
            // A slot the user has since taken over for something else is theirs.
            let Slot::Value(range, v) = slot else {
                return Ok(current.to_string());
            };
            if !is_qx_statusline(&v) {
                return Ok(current.to_string());
            }
            // Without the record there is no telling what to give back, and deleting the key could
            // destroy the user's old status line.
            let Some((previous, raw)) = parse_sidecar(sidecar) else {
                return Err("the previous statusLine is unknown (sidecar missing): edit settings.json by hand".into());
            };
            if previous.is_null() {
                return jsonc::remove_member(current, STATUS_LINE).and_then(|text| jsonc::prune_empty(&text, &[]));
            }
            match raw.filter(|r| parse_strict(r).as_ref() == Some(&previous)) {
                Some(raw) => Ok(format!("{}{}{}", &current[..range.start], raw, &current[range.end..])),
                None => jsonc::set_member(current, STATUS_LINE, &previous),
            }
        }
    }
}

/// Claude's settings with both qx edits in one write: the PreToolUse hook and the status line.
fn edit_claude_settings(file: &Path, exe: &Path, script: &Path, sidecar: Option<&str>, line: bool, a: Action) -> Edit {
    let original = match read_optional(file) {
        Ok(text) => text,
        Err(e) => return Edit::Refuse(e),
    };
    let current = match (&original, a) {
        (Some(text), _) => text.clone(),
        (None, Action::Remove) => return Edit::Unchanged,
        (None, Action::Install) => "{}\n".to_string(),
    };
    // The status line goes first: the hook edit then tidies an emptied document down to `{}`.
    let staged = if line {
        match claude_statusline_text(&current, script, sidecar, a) {
            Ok(text) => text,
            Err(e) => return Edit::Refuse(format!("{}: {e}", file.display())),
        }
    } else {
        current
    };
    match claude_hook_edit(file, &staged, exe, a) {
        Edit::Unchanged if original.as_deref() == Some(staged.as_str()) => Edit::Unchanged,
        Edit::Unchanged if parse_strict(&staged).is_some() => Edit::Changed(staged),
        Edit::Unchanged => Edit::Refuse(format!("the edit would leave {} unparseable; edit it by hand", file.display())),
        other => other,
    }
}

fn cursor_config(p: &Paths) -> PathBuf {
    p.home.join(".cursor").join("cli-config.json")
}

/// cursor-agent has one `statusLine` slot: qx takes it when free and leaves another tool's alone.
/// The file is cursor-agent's own, written on its first run: qx never creates it (a stub may be
/// rejected), so without one there is nothing to edit.
fn edit_cursor_statusline(file: &Path, exe: &Path, a: Action) -> Edit {
    let text = match read_optional(file) {
        Ok(Some(text)) => text,
        Ok(None) => return Edit::Unchanged,
        Err(e) => return Edit::Refuse(e),
    };
    let foreign = matches!(statusline_slot(&text), Slot::Value(_, v) if !is_qx_statusline(&v));
    if foreign {
        return Edit::Unchanged;
    }
    edit_json_file(file, STATUS_LINE, |_| cursor_statusline_value(exe), a)
}

const PRE_TOOL_USE: &[&str] = &["hooks", "PreToolUse"];

/// The PreToolUse group that sends Claude's own Bash and PowerShell commands through qx.
/// The exec form (`args`) starts qx without a shell, so the hook costs one process start.
fn claude_hook(exe: &Path) -> Value {
    json!({
        "matcher": "Bash|PowerShell",
        "hooks": [{"type": "command", "command": exe.to_string_lossy(), "args": ["hook", "claude"], "timeout": 10}],
    })
}

/// A `qx hook claude` command, from this build or an older one.
fn is_qx_command(hook: &Value) -> bool {
    let qx = Path::new(hook["command"].as_str().unwrap_or_default())
        .file_stem()
        .is_some_and(|s| s.eq_ignore_ascii_case("qx"));
    qx && hook["args"] == json!(["hook", "claude"])
}

/// A group that holds only a `qx hook claude` entry.
fn is_qx_hook(group: &Value) -> bool {
    matches!(group["hooks"].as_array().map(Vec::as_slice), Some([hook]) if is_qx_command(hook))
}

/// A group that holds a `qx hook claude` entry next to hooks of the user's own.
fn is_mixed_qx_group(group: &Value) -> bool {
    !is_qx_hook(group) && group["hooks"].as_array().is_some_and(|hooks| hooks.iter().any(is_qx_command))
}

/// The group with its qx hooks pointed at `exe` (Install) or taken out (Remove).
fn mixed_group_edit(group: &Value, exe: &Path, a: Action) -> Value {
    let mut group = group.clone();
    if let Some(hooks) = group["hooks"].as_array_mut() {
        match a {
            Action::Install => {
                for hook in hooks.iter_mut().filter(|h| is_qx_command(h)) {
                    hook["command"] = json!(exe.to_string_lossy());
                }
            }
            Action::Remove => hooks.retain(|h| !is_qx_command(h)),
        }
    }
    group
}

/// `current` with the qx hook installed or removed. A group that also holds the user's hooks
/// is edited in place, never joined by a second qx group.
fn claude_hook_text(current: &str, groups: &[Value], exe: &Path, a: Action) -> Result<String, String> {
    let mut text = current.to_string();
    for (i, group) in groups.iter().enumerate().filter(|(_, g)| is_mixed_qx_group(g)) {
        let edited = mixed_group_edit(group, exe, a);
        if edited != *group {
            text = jsonc::replace_element(&text, PRE_TOOL_USE, i, &edited)?;
        }
    }
    text = jsonc::remove_elements(&text, PRE_TOOL_USE, is_qx_hook)?;
    match a {
        Action::Install if groups.iter().any(is_mixed_qx_group) => Ok(text),
        Action::Install => jsonc::push_element(&text, PRE_TOOL_USE, &claude_hook(exe)),
        // Only a file qx changed is tidied: an empty list the user left there stays.
        Action::Remove if text == current => Ok(text),
        Action::Remove => jsonc::prune_empty(&text, PRE_TOOL_USE),
    }
}

/// The hook part of the settings edit alone, read from disk; the hook tests drive it directly.
#[cfg(test)]
fn edit_claude_hook(file: &Path, exe: &Path, a: Action) -> Edit {
    let current = match read_optional(file) {
        Ok(Some(text)) => text,
        Ok(None) if a == Action::Remove => return Edit::Unchanged,
        Ok(None) => "{}
".to_string(),
        Err(e) => return Edit::Refuse(e),
    };
    claude_hook_edit(file, &current, exe, a)
}

/// The hook edit on `current`, the text of `file` (named for messages) as it now stands.
fn claude_hook_edit(file: &Path, current: &str, exe: &Path, a: Action) -> Edit {
    let groups = match jsonc::elements(current, PRE_TOOL_USE) {
        Ok(groups) => groups,
        Err(e) => return Edit::Refuse(format!("{}: {e}", file.display())),
    };
    let ours: Vec<&Value> = groups.iter().filter(|g| is_qx_hook(g)).collect();
    let current_hook = claude_hook(exe);
    let settled = ours == [&current_hook]
        && groups.iter().filter(|g| is_mixed_qx_group(g)).all(|g| mixed_group_edit(g, exe, a) == *g);
    if a == Action::Install && settled {
        return Edit::Unchanged;
    }
    match claude_hook_text(current, &groups, exe, a) {
        Ok(text) if text == current => Edit::Unchanged,
        Ok(text) if parse_strict(&text).is_some() => Edit::Changed(text),
        Ok(_) => Edit::Refuse(format!("the edit would leave {} unparseable; edit it by hand", file.display())),
        Err(e) => Edit::Refuse(format!("{}: {e}", file.display())),
    }
}

/// Whichever OpenCode file already holds qx, else the first that exists, else `opencode.json`.
fn opencode_file(p: &Paths) -> PathBuf {
    let dir = p.home.join(".config").join("opencode");
    let json = dir.join("opencode.json");
    let jsonc = dir.join("opencode.jsonc");
    for file in [&json, &jsonc] {
        if let Ok(text) = std::fs::read_to_string(file) {
            if has_qx(&text) {
                return file.clone();
            }
        }
    }
    for file in [&json, &jsonc] {
        if file.exists() {
            return file.clone();
        }
    }
    json
}

fn has_qx(text: &str) -> bool {
    [&["mcp", "servers", "qx"][..], &["mcp", "qx"][..]]
        .iter()
        .any(|path| jsonc::member(text, path).ok().flatten().is_some())
}

/// An OpenCode V1 config: `mcp` holds servers itself (objects with a `type` or `command`), and no
/// V2 `mcp.servers` map. (In V1 a server could be named `servers`; it then has a `type`.)
fn opencode_v1(text: &str) -> bool {
    let Some(v) = parse_strict(text) else { return false };
    let Some(mcp) = v.get("mcp").and_then(Value::as_object) else { return false };
    let server = |s: &Value| s.get("type").is_some() || s.get("command").is_some();
    match mcp.get("servers") {
        Some(s) if !server(s) => false,
        _ => mcp.values().any(server),
    }
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
            Action::Install => match jsonc::set_member("{}", path, &entry(None)) {
                Ok(text) => Edit::Changed(text),
                Err(e) => Edit::Refuse(format!("{}: {e}", file.display())),
            },
        },
        Some(current) => {
            let existing = jsonc::member(current, path)
                .ok()
                .flatten()
                .and_then(|range| serde_json::from_str(&jsonc::to_strict_json(&current[range])).ok());
            let updated = match a {
                Action::Install => jsonc::set_member(current, path, &entry(existing.as_ref())),
                // Only a file qx changed is tidied: an empty section the user left there stays.
                Action::Remove => jsonc::remove_member(current, path).and_then(|text| {
                    if text == current {
                        Ok(text)
                    } else {
                        jsonc::prune_empty(&text, &path[..path.len() - 1])
                    }
                }),
            };
            match updated {
                Ok(text) if text == current => Edit::Unchanged,
                Ok(text) => match parse_strict(&text) {
                    // A config the client cannot read is worse than no change at all.
                    Some(_) => Edit::Changed(text),
                    None => Edit::Refuse(format!(
                        "the edit would leave {} unparseable; edit it by hand",
                        file.display()
                    )),
                },
                Err(e) => Edit::Refuse(format!("{}: {e}", file.display())),
            }
        }
    }
}

/// The document as strict JSON, or `None` when the edit broke it.
fn parse_strict(text: &str) -> Option<Value> {
    serde_json::from_str(&jsonc::to_strict_json(text)).ok()
}

/// `key` is merged: a user's own keys survive, ours overwrite. Clients disagree on the
/// name (`env` on Claude Code and pi, `environment` on OpenCode).
fn merge_env(existing: Option<&Value>, client: &str, extra: &[(String, Value)]) -> Value {
    let mut env = serde_json::Map::new();
    for key in ["env", "environment"] {
        if let Some(Value::Object(map)) = existing.and_then(|e| e.get(key)) {
            for (k, v) in map {
                env.insert(k.clone(), v.clone());
            }
        }
    }
    for (k, v) in extra {
        env.insert(k.clone(), v.clone());
    }
    env.insert("QX_CLIENT".to_string(), Value::String(client.to_string()));
    Value::Object(env)
}

/// The user's entry with ours laid over it: keys qx does not own (`enabled`, `cwd`, comments' siblings)
/// survive a re-run.
fn overlay(existing: Option<&Value>, ours: Value) -> Value {
    let mut entry = existing.and_then(Value::as_object).cloned().unwrap_or_default();
    if let Value::Object(ours) = ours {
        entry.extend(ours);
    }
    Value::Object(entry)
}

fn claude_entry(exe: &Path, existing: Option<&Value>) -> Value {
    overlay(existing, json!({
        "type": "stdio",
        "command": exe.to_string_lossy(),
        "args": ["mcp"],
        "env": merge_env(existing, "claude-code", &[]),
    }))
}

/// OpenCode V1's local server: no `codemode`, and `timeout` there is the tool-listing timeout, so
/// none is written (a user's own stays).
fn opencode_entry_v1(exe: &Path, existing: Option<&Value>) -> Value {
    let mut entry = overlay(existing, json!({
        "type": "local",
        "command": [exe.to_string_lossy(), "mcp"],
        "environment": merge_env(existing, "opencode", &[]),
    }));
    // A user who disabled the server keeps it disabled.
    entry["enabled"] = existing.and_then(|e| e.get("enabled")).cloned().unwrap_or(json!(true));
    entry
}

fn opencode_entry(exe: &Path, existing: Option<&Value>) -> Value {
    let mut entry = overlay(existing, json!({
        "type": "local",
        "command": [exe.to_string_lossy(), "mcp"],
        "environment": merge_env(existing, "opencode", &[]),
        // Without this, OpenCode hides the qx tools behind its own `execute` tool.
        "codemode": false,
        "timeout": { "execution": 600000 },
    }));
    // A user's own timeout object may carry other keys; keep them.
    if let Some(t) = existing.and_then(|e| e.get("timeout")).and_then(Value::as_object) {
        let mut timeouts = t.clone();
        timeouts.insert("execution".into(), json!(600000));
        entry["timeout"] = Value::Object(timeouts);
    }
    entry
}

/// Write one planned change: back up, write a temp file, rename over the original.
/// Empty text (or a bare `{}`) removes the file — no config format qx writes is valid and empty, and
/// removing an extension means removing its files.
pub fn apply(ch: &Change) -> std::io::Result<()> {
    let Edit::Changed(text) = &ch.edit else {
        return Ok(());
    };
    // A symlinked config (dotfiles) is edited where it points: the link stays a link.
    let target = write_target(&ch.file);
    if !text.is_empty() {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // Removing a file keeps the first backup too: the file may be one the user edited.
    if let Ok(existing) = std::fs::read(&ch.file) {
        let backup = ch.file.with_extension(format!(
            "{}qx-bak",
            ch.file
                .extension()
                .map(|e| format!("{}.", e.to_string_lossy()))
                .unwrap_or_default()
        ));
        // The first backup is the user's own file; a later run must not replace it with qx's edit.
        if !backup.exists() && !leaves_nothing_of_the_users(ch.client, text, &existing) {
            std::fs::copy(&ch.file, &backup)?;
        }
    }
    // A JSON config qx emptied to `{}` goes too: the client reads a missing file as `{}`. A link is
    // not deleted: the real file (the dotfiles copy) is the one that must lose the qx entry.
    let is_link = target != ch.file;
    if !is_link && (text.is_empty() || text.trim_start_matches('\u{feff}').trim() == "{}") {
        return match std::fs::remove_file(&ch.file) {
            Ok(()) => {
                if ch.client == Client::Pi {
                    drop_empty_extension_dirs(&ch.file);
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };
    }
    let tmp = target.with_extension(format!("qx-tmp-{}", std::process::id()));
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        use std::io::Write;
        f.write_all(text.as_bytes())?;
        // On disk before the rename, so a crash leaves the old file or the whole new one.
        f.sync_all()
    });
    written.and_then(|()| std::fs::rename(&tmp, &target)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// `extensions/qx` and `extensions` go when the last file in them does; `remove_dir` only ever
/// removes an empty folder.
fn drop_empty_extension_dirs(file: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir.filter(|d| d.file_name().is_some_and(|n| n == "qx" || n == "extensions")) {
        if std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

/// Where a write to `file` lands: the target of a symlink, else the file itself.
fn write_target(file: &Path) -> PathBuf {
    let is_link = std::fs::symlink_metadata(file).is_ok_and(|m| m.file_type().is_symlink());
    if !is_link {
        return file.to_path_buf();
    }
    std::fs::canonicalize(file)
        .or_else(|_| std::fs::read_link(file).map(|l| file.parent().unwrap_or(Path::new("")).join(l)))
        .unwrap_or_else(|_| file.to_path_buf())
}

/// A file qx made, and now empties, holds nothing worth a `.qx-bak`: what qx itself writes, down to
/// the last key. (pi's extension files are compared with what qx generates; a config must hold only
/// qx's entry, so one with the user's own `env`, `timeout` or other key is backed up.)
fn leaves_nothing_of_the_users(client: Client, text: &str, existing: &[u8]) -> bool {
    let Ok(existing) = std::str::from_utf8(existing) else {
        return false;
    };
    if client == Client::Pi && text.is_empty() {
        return pi::is_generated(existing);
    }
    if text.is_empty() {
        // The widget files are qx's whole: a script or sidecar qx wrote holds nothing of the user's.
        match client {
            Client::Omp if generated_from(OMP_EXTENSION, existing) => return true,
            Client::ClaudeCode if generated_from(STATUSLINE_SCRIPT, existing) || is_sidecar(existing) => return true,
            _ => {}
        }
    }
    let bare = text.trim_start_matches('\u{feff}').trim();
    (bare.is_empty() || bare == "{}")
        && match client {
            Client::ClaudeCode | Client::OpenCode | Client::Pi | Client::Cursor | Client::Omp => parse_strict(existing).is_some_and(|doc| only_qx_wrote(&doc)),
            _ => only_qx_lines(client, existing),
        }
}

/// TOML and YAML: every line is one a fresh install writes (the `command` line names the exe).
fn only_qx_lines(client: Client, existing: &str) -> bool {
    let exe = Path::new("qx");
    let fresh = match client {
        Client::Codex => toml::edit_codex(None, exe, Action::Install),
        Client::Dsh => yaml::edit_dsh(None, exe, Action::Install),
        Client::Hermes => yaml::edit_hermes(None, exe, Action::Install),
        _ => return false,
    };
    let Edit::Changed(fresh) = fresh else {
        return false;
    };
    let known: std::collections::HashSet<&str> = fresh.lines().map(str::trim).collect();
    existing
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .all(|l| l.is_empty() || l.starts_with("command") || known.contains(l))
}

/// A document that is one chain of single-key objects down to a qx server entry or qx hook group.
fn only_qx_wrote(doc: &Value) -> bool {
    match doc {
        // Every member must be qx's: its server entry, its status line, or a chain down to its hook.
        Value::Object(m) if !m.is_empty() => m.iter().all(|(key, value)| match key.as_str() {
            "qx" => m.len() == 1 && is_pristine_entry(value),
            "statusLine" => is_qx_statusline(value),
            _ => only_qx_wrote(value),
        }),
        Value::Array(groups) if groups.len() == 1 => {
            let exe = groups[0]["hooks"][0]["command"].as_str().unwrap_or_default();
            groups[0] == claude_hook(Path::new(exe))
        }
        _ => false,
    }
}

/// The server entry as `claude_entry` / `opencode_entry` write it, apart from the exe path: no extra
/// key, `QX_CLIENT` the only variable, the default timeout.
fn is_pristine_entry(entry: &Value) -> bool {
    let Some(m) = entry.as_object() else {
        return false;
    };
    let only_client = |v: &Value| v.as_object().is_some_and(|env| env.keys().all(|k| k == "QX_CLIENT"));
    m.iter().all(|(k, v)| match k.as_str() {
        "type" | "command" => true,
        "args" => *v == json!(["mcp"]),
        "env" | "environment" => only_client(v),
        "enabled" => *v == json!(true),
        "codemode" => *v == json!(false),
        "timeout" => *v == json!({ "execution": 600000 }) || *v == json!(600) || *v == json!(600000),
        "exposure" => *v == json!("direct"),
        _ => false,
    })
}

/// Two spellings of one file: slashes and case do not matter on Windows, nor do `..` or links.
pub fn same_path(a: &str, b: &Path) -> bool {
    let norm = |s: &str| s.replace('/', "\\").to_ascii_lowercase();
    norm(a) == norm(&b.to_string_lossy()) || crate::deploy::same_path(Path::new(a), b)
}

pub struct Status {
    pub client: Client,
    pub file: Option<PathBuf>,
    pub command: Option<String>,
    pub notes: Vec<String>,
}

pub fn status(c: Client, p: &Paths) -> Status {
    status_with(c, p, crate::resolve::find_on_path("node").is_some())
}

/// `status`, with whether `node` resolves on PATH given (see `plan_with`).
fn status_with(c: Client, p: &Paths, node: bool) -> Status {
    let mut notes = Vec::new();
    let (file, command) = match c {
        Client::ClaudeCode => {
            let file = claude_file(p);
            let command = read_json_command(&file, &["mcpServers", "qx", "command"]);
            let hook = std::fs::read_to_string(claude_settings(p))
                .ok()
                .and_then(|text| jsonc::elements(&text, PRE_TOOL_USE).ok())
                .and_then(|groups| {
                    groups
                        .iter()
                        .flat_map(|g| g["hooks"].as_array().into_iter().flatten())
                        .find(|h| is_qx_command(h))
                        .cloned()
                });
            match hook.as_ref().and_then(|h| h["command"].as_str()) {
                None => notes.push("no `qx hook claude` PreToolUse hook: Claude's own Bash/PowerShell bypass qx".into()),
                Some(cmd) if !Path::new(cmd).is_file() => {
                    notes.push(format!("the PreToolUse hook points at a missing binary ({cmd}): run `qx setup claude`"));
                }
                Some(cmd) => {
                    if let Some(want) = crate::deploy::canonical().filter(|w| !same_path(cmd, w)) {
                        notes.push(format!("the PreToolUse hook runs {cmd}, not {}: run `qx setup claude`", want.display()));
                    }
                }
            }
            if command.is_some() {
                let script = claude_dir(p).join("qx-statusline.js");
                let line = std::fs::read_to_string(claude_settings(p)).ok().and_then(|t| match statusline_slot(&t) {
                    Slot::Value(_, v) => Some(v),
                    _ => None,
                });
                match line {
                    Some(v) if is_qx_statusline(&v) && !script.is_file() => {
                        notes.push(format!("the statusLine runs {} which is missing: run `qx setup claude`", script.display()));
                    }
                    Some(v) if is_qx_statusline(&v) => {}
                    _ if !node => notes.push("node not found on PATH: statusLine not installed".into()),
                    _ => notes.push("no qx statusLine in settings.json: the savings line is not shown; run `qx setup claude`".into()),
                }
            }
            (Some(file), command)
        }
        Client::OpenCode => {
            let file = opencode_file(p);
            let v1 = std::fs::read_to_string(&file).ok().as_deref().is_some_and(opencode_v1);
            let at: &[&str] = if v1 { &["mcp", "qx"] } else { &["mcp", "servers", "qx"] };
            let command = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| {
                    let range = jsonc::member(&text, &[at, &["command"]].concat()).ok()??;
                    let value: Value =
                        serde_json::from_str(&jsonc::to_strict_json(&text[range])).ok()?;
                    Some(match value {
                        Value::Array(items) => items.first()?.as_str()?.to_string(),
                        other => other.as_str()?.to_string(),
                    })
                });
            // V1 has no Code Mode to hide the tools behind.
            if let Some(text) = std::fs::read_to_string(&file).ok().filter(|_| !v1) {
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
            let mcp = p.home.join(".pi").join("agent").join("mcp.json");
            let extension = p
                .home
                .join(".pi")
                .join("agent")
                .join("extensions")
                .join("qx.js");
            let mut command = read_json_command(&mcp, &["mcpServers", "qx", "command"]);
            let mut file = mcp;
            match pi::pi_version() {
                None => notes.push("pi not found on PATH".into()),
                Some(version) if version < (0, 99) => {
                    // No MCP client in this pi: the extension is the registration.
                    if let Ok(text) = std::fs::read_to_string(&extension) {
                        command = extension_exe(&text);
                        file = extension;
                    } else {
                        notes.push(
                            "pi has no MCP client here: run `qx setup pi` to install the qx extension"
                                .into(),
                        );
                    }
                }
                Some(_) => {}
            }
            (Some(file), command)
        }
        Client::Cursor => {
            let file = cursor_file(p);
            let command = read_json_command(&file, &["mcpServers", "qx", "command"]);
            if command.is_some() {
                let text = std::fs::read_to_string(cursor_config(p)).ok();
                match text.as_deref().map(statusline_slot) {
                    None => notes.push("no cli-config.json yet (cursor-agent writes it on first run): run `qx setup cursor` again after it exists to show the savings line".into()),
                    Some(Slot::Value(_, v)) => {
                        if !is_qx_statusline(&v) {
                            notes.push("cli-config.json statusLine belongs to another tool: qx leaves it, so the savings line is not shown".into());
                        }
                    }
                    Some(_) => notes.push("no qx statusLine in cli-config.json: the savings line is not shown; run `qx setup cursor`".into()),
                }
            }
            (Some(file), command)
        }
        Client::Omp => {
            let file = p.omp_agent_dir.join("mcp.json");
            let command = read_json_command(&file, &["mcpServers", "qx", "command"]);
            match std::fs::read_to_string(omp_extension(p)).ok().and_then(|t| extension_exe(&t)) {
                None => notes.push("no qx bash hook in extensions/qx.js: omp's own bash bypasses qx".into()),
                Some(hook) if command.as_deref().is_some_and(|c| !same_path(&hook, Path::new(c))) => {
                    notes.push(format!("the bash hook runs {hook}, the MCP entry another qx: run `qx setup omp`"));
                }
                Some(_) => {}
            }
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

/// The exe the installed pi extension was generated for (`const EXE = "…";`).
fn extension_exe(text: &str) -> Option<String> {
    let start = text.find("const EXE = \"")? + "const EXE = ".len();
    let rest = &text[start..];
    let end = rest.find('\n')?;
    let literal = rest[..end].trim().trim_end_matches(';');
    serde_json::from_str::<String>(literal).ok()
}

fn read_json_command(file: &Path, path: &[&str]) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let range = jsonc::member(&text, path).ok()??;
    serde_json::from_str(&jsonc::to_strict_json(&text[range])).ok()
}

/// Whether a client is installed for this user: its config file or directory exists (pi: the
/// `pi` command answers). Explicitly named clients are configured either way.
fn installed(c: Client, p: &Paths) -> bool {
    match c {
        Client::ClaudeCode => claude_file(p).is_file() || claude_settings(p).parent().is_some_and(Path::is_dir),
        Client::OpenCode => p.home.join(".config").join("opencode").is_dir(),
        Client::Codex => p.codex_home.is_dir(),
        Client::Dsh => p.dsh_home.is_dir(),
        Client::Hermes => p.hermes_home.is_dir(),
        Client::Pi => pi::pi_version().is_some(),
        Client::Cursor => p.home.join(".cursor").is_dir(),
        Client::Omp => p.omp_agent_dir.is_dir(),
        Client::Generic => false,
    }
}

pub fn run(args: &[String]) -> i32 {
    let mut action = Action::Install;
    let mut dry = false;
    let mut all = false;
    let mut clients: Vec<Client> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--dry-run" => dry = true,
            "--remove" => action = Action::Remove,
            "all" => all = true,
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
    let paths = Paths::from_env();
    if paths.home.as_os_str().is_empty() {
        // Every client config lives under the home directory; a relative path would land in the cwd.
        eprintln!("qx setup: neither USERPROFILE nor HOME is set, so the client configs cannot be found");
        return 2;
    }
    if all || clients.is_empty() {
        // `all` means every client this machine has: a config for a tool that is not installed
        // is clutter, and pi's absence is not a failure.
        for c in Client::ALL {
            if installed(c, &paths) {
                clients.push(c);
            } else {
                println!("{}: skipped: not installed", c.key());
            }
        }
    }
    let exe = match crate::deploy::canonical() {
        Some(path) => path,
        None => {
            eprintln!("qx setup: no cargo bin directory (set CARGO_HOME or USERPROFILE)");
            return 2;
        }
    };
    // Registering a binary that is not there leaves every client with a dead entry.
    if action == Action::Install && !exe.is_file() {
        eprintln!("qx setup: {} does not exist: run `qx deploy` first", exe.display());
        if !dry {
            return 2;
        }
    }
    if configure(clients, &paths, &exe, action, dry) {
        1
    } else {
        0
    }
}

/// Plan and apply `action` for each client; true when anything was refused or could not be written.
fn configure(clients: Vec<Client>, paths: &Paths, exe: &Path, action: Action, dry: bool) -> bool {
    let mut refused = false;
    let mut dsh_seen = false;
    let mut dsh_refused = false;
    for c in clients {
        let changes = match plan(c, paths, exe, action) {
            Ok(changes) => changes,
            Err(e) => {
                refused = true;
                println!("{}: refused: {e}", c.key());
                continue;
            }
        };
        if changes.is_empty() {
            println!("{}: unchanged", c.key());
            continue;
        }
        for change in changes {
            let key = change.client.key();
            if change.client == Client::Dsh {
                dsh_seen = true;
                if matches!(change.edit, Edit::Refuse(_)) {
                    dsh_refused = true;
                }
            }
            match &change.edit {
                Edit::Unchanged => println!("{key}: unchanged ({})", change.file.display()),
                Edit::Refuse(reason) => {
                    refused = true;
                    println!("{key}: refused: {reason}");
                }
                Edit::Changed(_) if dry => println!("{key}: would update {}", change.file.display()),
                Edit::Changed(_) => match apply(&change) {
                    Ok(()) => println!("{key}: updated {}", change.file.display()),
                    Err(e) => {
                        refused = true;
                        println!("{key}: cannot write {}: {e}", change.file.display());
                    }
                },
            }
        }
    }
    // Never on a dry run (that would delete files from a preview), and never when the dsh
    // entry could not be read: the copies it names may still be the ones in use.
    if dsh_seen && !dsh_refused && !dry && action == Action::Install {
        // dsh used to need a versioned copy per build; now one canonical binary is enough,
        // and every stale copy is a different qx under a client.
        let removed = yaml::prune_dsh_binaries(&paths.dsh_home);
        if !removed.is_empty() {
            println!(
                "dsh: removed {} unreferenced qx cop(ies) from {}: {}",
                removed.len(),
                paths.dsh_home.join("bin").display(),
                removed.join(", ")
            );
        }
    }
    refused
}

#[cfg(test)]
mod tests {
    use super::*;

    // The tests do not depend on whether this machine has node on PATH.
    fn plan(c: Client, p: &Paths, exe: &Path, a: Action) -> Result<Vec<Change>, String> {
        plan_with(c, p, exe, a, true)
    }

    fn status(c: Client, p: &Paths) -> Status {
        status_with(c, p, true)
    }

    fn paths(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("qx-setup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Paths {
            home: root.clone(),
            dsh_home: root.join("dsh"),
            hermes_home: root.join("hermes"),
            codex_home: root.join("codex"),
            claude_config_dir: None,
            omp_agent_dir: root.join(".omp").join("agent"),
        }
    }

    fn changed(edit: &Edit) -> Option<&str> {
        match edit {
            Edit::Changed(text) => Some(text),
            _ => None,
        }
    }

    #[test]
    fn claude_config_dir_moves_both_claude_files() {
        let dir = PathBuf::from(r"D:\cfg");
        let p = Paths::from_lookup(|k| match k {
            "USERPROFILE" => Some(r"C:\u".into()),
            "CLAUDE_CONFIG_DIR" => Some(dir.clone().into_os_string()),
            _ => None,
        });
        assert_eq!(claude_file(&p), dir.join(".claude.json"));
        assert_eq!(claude_settings(&p), dir.join("settings.json"));
        let q = Paths::from_lookup(|k| (k == "USERPROFILE").then(|| r"C:\u".into()));
        assert_eq!(claude_file(&q), PathBuf::from(r"C:\u\.claude.json"));
        assert_eq!(claude_settings(&q), PathBuf::from(r"C:\u\.claude\settings.json"));
    }

    #[test]
    fn the_first_backup_is_the_users_own_file() {
        let p = paths("backup");
        let file = p.home.join("config.toml");
        std::fs::write(&file, "mine").unwrap();
        for text in ["first edit", "second edit"] {
            let ch = Change { client: Client::Codex, file: file.clone(), edit: Edit::Changed(text.into()) };
            apply(&ch).unwrap();
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "second edit");
        assert_eq!(std::fs::read_to_string(p.home.join("config.toml.qx-bak")).unwrap(), "mine");
    }

    #[test]
    fn json_entries_keep_the_keys_qx_does_not_own() {
        let exe = Path::new(r"C:\q\qx.exe");
        let old = json!({"enabled": false, "cwd": "D:\\w", "command": ["old", "mcp"], "environment": {"K": "v"}});
        let v2 = opencode_entry(exe, Some(&old));
        assert_eq!((&v2["enabled"], &v2["cwd"], &v2["environment"]["K"]), (&json!(false), &json!("D:\\w"), &json!("v")));
        assert_eq!(v2["command"][0], exe.to_string_lossy().as_ref());
        let v1 = opencode_entry_v1(exe, Some(&old));
        assert_eq!((&v1["enabled"], &v1["cwd"]), (&json!(false), &json!("D:\\w")));
        assert_eq!(opencode_entry_v1(exe, None)["enabled"], json!(true));
        let claude = claude_entry(exe, Some(&json!({"cwd": "D:\\w", "env": {"K": "v"}})));
        assert_eq!((&claude["cwd"], &claude["env"]["K"]), (&json!("D:\\w"), &json!("v")));
    }

    #[test]
    fn doctor_notes_a_hook_that_points_at_a_missing_binary() {
        let p = paths("hookmissing");
        let settings = claude_settings(&p);
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        let ghost = p.home.join("gone").join("qx.exe");
        let text = json!({"hooks": {"PreToolUse": [claude_hook(&ghost)]}}).to_string();
        std::fs::write(&settings, text).unwrap();
        let notes = status(Client::ClaudeCode, &p).notes;
        assert!(notes.iter().any(|n| n.contains("missing binary")), "{notes:?}");
        assert!(same_path("C:/A/qx.EXE", Path::new("c:\\a\\qx.exe")));
    }

    #[test]
    fn same_path_sees_through_dot_dot() {
        let dir = std::env::temp_dir().join(format!("qx-samepath-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let exe = dir.join("bin").join("qx.exe");
        std::fs::write(&exe, b"x").unwrap();
        let dotted = dir.join("bin").join("..").join("bin").join("qx.exe");
        assert!(same_path(&dotted.to_string_lossy(), &exe));
        let _ = std::fs::remove_dir_all(&dir);
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
    fn claude_hook_joins_the_others_and_replaces_only_its_own() {
        let p = paths("claude-hook");
        let file = claude_settings(&p);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let rtk = r#"{"matcher": "Bash", "hooks": [{"type": "command", "command": "rtk hook claude"}]}"#;
        let stale = r#"{"matcher": "Bash", "hooks": [{"type": "command", "command": "C:/old/qx.exe", "args": ["hook", "claude"]}]}"#;
        std::fs::write(&file, format!("{{
  \"hooks\": {{
    \"PreToolUse\": [
      {rtk},
      {stale}
    ]
  }},
  \"model\": \"x\"
}}
")).unwrap();
        let exe = Path::new(r"C:
ew\qx.exe");
        let text = changed(&edit_claude_hook(&file, exe, Action::Install)).expect("an update").to_string();
        assert!(text.contains(rtk), "other hooks keep their bytes: {text}");
        let groups = jsonc::elements(&text, PRE_TOOL_USE).unwrap();
        assert_eq!(groups.len(), 2, "the stale qx hook is replaced, not duplicated");
        assert_eq!(groups[1], claude_hook(exe));
        assert_eq!(parse_strict(&text).unwrap()["model"], "x");

        std::fs::write(&file, &text).unwrap();
        assert!(matches!(edit_claude_hook(&file, exe, Action::Install), Edit::Unchanged));
        let removed = changed(&edit_claude_hook(&file, exe, Action::Remove)).expect("a removal").to_string();
        assert_eq!(jsonc::elements(&removed, PRE_TOOL_USE).unwrap().len(), 1);
        assert!(removed.contains(rtk));

        // No settings file yet: one is created holding just the hook.
        std::fs::remove_file(&file).unwrap();
        let fresh = changed(&edit_claude_hook(&file, exe, Action::Install)).expect("a new file").to_string();
        assert_eq!(jsonc::elements(&fresh, PRE_TOOL_USE).unwrap(), vec![claude_hook(exe)]);
        assert!(matches!(edit_claude_hook(&file, exe, Action::Remove), Edit::Unchanged));
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn opencode_sets_codemode_false_in_the_file_that_has_qx() {
        let p = paths("opencode");
        let dir = p.home.join(".config").join("opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("opencode.json"),
            "{\n  // hand-edited\n  \"mcp\": {\"servers\": {\"qx\": {\"type\": \"local\", \"environment\": {\"QX_GUARD\": \"safe\"}}}},\n}\n",
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
        // OpenCode calls the map `environment`; a user's own keys must survive.
        assert_eq!(v["mcp"]["servers"]["qx"]["environment"]["QX_GUARD"], "safe");
        assert_eq!(v["mcp"]["servers"]["qx"]["command"][1], "mcp");
        assert!(text.contains("// hand-edited"), "the comment must survive");
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn opencode_v1_configs_get_the_v1_shape() {
        // OpenCode V1 lists servers directly under `mcp` (no `servers`, no `codemode`, numeric timeout).
        let p = paths("opencode-v1");
        let dir = p.home.join(".config").join("opencode");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("opencode.json");
        let exe = Path::new(r"C:\q\qx.exe");
        for start in [
            r#"{"mcp": {"qx": {"type": "local", "command": ["x"], "environment": {"QX_GUARD": "safe"}}}}"#,
            r#"{"mcp": {"other": {"type": "local", "command": ["o"]}}}"#,
        ] {
            std::fs::write(&file, start).unwrap();
            let text = changed(&plan(Client::OpenCode, &p, exe, Action::Install).unwrap()[0].edit)
                .expect("an install")
                .to_string();
            let v: Value = serde_json::from_str(&jsonc::to_strict_json(&text)).unwrap();
            assert_eq!(v["mcp"]["qx"]["command"][0], r"C:\q\qx.exe", "{text}");
            assert_eq!(v["mcp"]["qx"]["environment"]["QX_CLIENT"], "opencode", "{text}");
            assert!(v["mcp"].get("servers").is_none() && v["mcp"]["qx"].get("codemode").is_none(), "{text}");
            if start.contains("QX_GUARD") {
                assert_eq!(v["mcp"]["qx"]["environment"]["QX_GUARD"], "safe", "{text}");
            }
            std::fs::write(&file, &text).unwrap();
            assert!(matches!(plan(Client::OpenCode, &p, exe, Action::Install).unwrap()[0].edit, Edit::Unchanged));
            let removed = changed(&plan(Client::OpenCode, &p, exe, Action::Remove).unwrap()[0].edit)
                .expect("a removal")
                .to_string();
            assert!(!removed.contains("qx.exe"), "{removed}");
        }
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn removing_a_file_keeps_the_users_copy_as_a_backup() {
        let p = paths("apply-remove");
        std::fs::create_dir_all(&p.home).unwrap();
        let file = p.home.join("qx.js");
        std::fs::write(&file, "// the user's own edit\n").unwrap();
        let ch = Change { client: Client::Pi, file: file.clone(), edit: Edit::Changed(String::new()) };
        apply(&ch).unwrap();
        assert!(!file.exists());
        assert_eq!(std::fs::read_to_string(p.home.join("qx.js.qx-bak")).unwrap(), "// the user's own edit\n");
        // A file that is already gone is not an error.
        apply(&ch).unwrap();
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn only_installed_clients_count_as_installed() {
        let p = paths("installed");
        std::fs::create_dir_all(&p.codex_home).unwrap();
        std::fs::write(p.home.join(".claude.json"), "{}").unwrap();
        assert!(installed(Client::Codex, &p) && installed(Client::ClaudeCode, &p));
        for c in [Client::OpenCode, Client::Dsh, Client::Hermes] {
            assert!(!installed(c, &p), "{}", c.key());
        }
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn home_falls_back_to_home_when_userprofile_is_unset() {
        let env = |k: &str| (k == "HOME").then(|| std::ffi::OsString::from(r"C:\h"));
        assert_eq!(Paths::from_lookup(env).home, Path::new(r"C:\h"));
        assert!(Paths::from_lookup(|_| None).home.as_os_str().is_empty());
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

    #[test]
    fn remove_on_a_config_without_the_parent_key_is_unchanged_not_refused() {
        let p = paths("remove-noparent");
        std::fs::write(claude_file(&p), "{\"a\": 1}\n").unwrap();
        let changes = plan(Client::ClaudeCode, &p, Path::new(r"C:\q\qx.exe"), Action::Remove).unwrap();
        assert!(matches!(changes[0].edit, Edit::Unchanged), "a missing parent must not refuse the removal");
        let _ = std::fs::remove_dir_all(&p.home);
    }

    // ---- batch 7: setup data-loss and tidiness (SU2, SU5-SU8, end to end) ----

    fn q() -> &'static Path {
        Path::new(r"C:\q\qx.exe")
    }

    /// Install, install again (nothing may change), remove, remove again, all through the real plan and apply.
    fn cycle(plan_for: &dyn Fn(Action) -> Vec<Change>) {
        for action in [Action::Install, Action::Remove] {
            for ch in plan_for(action) {
                if let Edit::Refuse(r) = &ch.edit {
                    panic!("{action:?} refused {}: {r}", ch.file.display());
                }
                apply(&ch).unwrap();
            }
            let again = plan_for(action);
            let names: Vec<String> = again.iter().map(|c| c.file.display().to_string()).collect();
            assert!(again.iter().all(|c| matches!(c.edit, Edit::Unchanged)), "{action:?} twice must change nothing: {names:?}");
        }
    }

    fn put(file: &Path, text: &str) {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }

    /// Every `.qx-bak` and `qx-tmp` file left under `dir`.
    fn leftovers(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let path = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                if path.is_dir() {
                    stack.push(path);
                } else if name.contains(".qx-bak") || name.contains("qx-tmp") {
                    out.push(path.display().to_string());
                }
            }
        }
        out
    }

    fn qx_hook_commands(settings: &str) -> Vec<String> {
        jsonc::elements(settings, PRE_TOOL_USE)
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
            .filter(|h| h["args"] == json!(["hook", "claude"]))
            .map(|h| h["command"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    fn pi_changes(version: (u32, u32), p: &Paths, a: Action) -> Vec<Change> {
        pi::edit_pi(Some(version), p, q(), a)
            .into_iter()
            .map(|(file, edit)| Change { client: Client::Pi, file, edit })
            .collect()
    }

    #[test]
    fn apply_writes_through_a_symlink_and_keeps_the_link() {
        let p = paths("symlink");
        let real = p.home.join("dotfiles").join("settings.json");
        put(&real, "{}\n");
        let link = p.home.join("settings.json");
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&real, &link);
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&real, &link);
        if made.is_err() {
            println!("skip: this account may not create symlinks");
            return;
        }
        let ch = Change { client: Client::ClaudeCode, file: link.clone(), edit: Edit::Changed("{\"a\": 1}\n".into()) };
        apply(&ch).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link must stay a link");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "{\"a\": 1}\n", "the target holds the edit");
        assert_eq!(std::fs::read_to_string(p.home.join("settings.json.qx-bak")).unwrap(), "{}\n");
        // Removing empties the file the link points at; the link stays, so the dotfiles copy loses the entry.
        let gone = Change { client: Client::ClaudeCode, file: link.clone(), edit: Edit::Changed("{}\n".into()) };
        apply(&gone).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "removing keeps the link");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "{}\n", "the target holds the emptied text");
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn removing_a_customised_entry_keeps_a_backup() {
        let p = paths("rt-claude-custom");
        let file = claude_file(&p);
        let custom = "{\"mcpServers\":{\"qx\":{\"type\":\"stdio\",\"command\":\"C:/q/qx.exe\",\"args\":[\"mcp\"],\"env\":{\"QX_CLIENT\":\"claude-code\",\"MY\":\"1\"},\"timeout\":5}}}";
        put(&file, custom);
        for ch in plan(Client::ClaudeCode, &p, q(), Action::Remove).unwrap() {
            apply(&ch).unwrap();
        }
        assert!(!file.exists(), "the emptied file goes");
        assert_eq!(std::fs::read_to_string(p.home.join(".claude.json.qx-bak")).unwrap(), custom, "the user's own env and timeout stay recoverable");
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn a_client_whose_plan_fails_is_a_refusal_not_a_skip() {
        let p = paths("planerr");
        std::fs::create_dir_all(&p.codex_home).unwrap();
        // UTF-16 with a BOM: the file cannot be read as text, so `plan` returns Err.
        let bytes: Vec<u8> = vec![0xFF, 0xFE, b'a', 0, b'=', 0, b'1', 0];
        std::fs::write(p.codex_home.join("config.toml"), &bytes).unwrap();
        assert!(configure(vec![Client::Codex], &p, q(), Action::Install, true), "the exit code must say it was refused");
        assert_eq!(std::fs::read(p.codex_home.join("config.toml")).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn blank_settings_files_are_empty_documents_not_refusals() {
        for (i, blank) in ["", "  \r\n", "\u{feff}", "\u{feff}\n"].into_iter().enumerate() {
            let p = paths(&format!("blank{i}"));
            put(&claude_file(&p), blank);
            put(&claude_settings(&p), blank);
            let changes = plan(Client::ClaudeCode, &p, q(), Action::Install).unwrap();
            assert_eq!(changes.len(), 4);
            for ch in &changes {
                let text = changed(&ch.edit).unwrap_or_else(|| panic!("{} {blank:?} must install: {:?}", ch.file.display(), ch.edit));
                if ch.file.extension().is_some_and(|e| e == "json") {
                    assert!(parse_strict(text).is_some(), "{text}");
                }
                apply(ch).unwrap();
            }
            let settings = std::fs::read_to_string(claude_settings(&p)).unwrap();
            assert_eq!(qx_hook_commands(&settings), vec![q().to_string_lossy().to_string()]);
            let servers = std::fs::read_to_string(claude_file(&p)).unwrap();
            assert_eq!(parse_strict(&servers).unwrap()["mcpServers"]["qx"]["args"][0], "mcp");
            // Removing from a blank file is a no-op, not an error. The widget script and its sidecar
            // written above are still there, so their removal is a real change.
            put(&claude_file(&p), blank);
            put(&claude_settings(&p), blank);
            for ch in plan(Client::ClaudeCode, &p, q(), Action::Remove).unwrap() {
                if ch.file == claude_file(&p) || ch.file == claude_settings(&p) {
                    assert!(matches!(ch.edit, Edit::Unchanged), "{blank:?}: {:?}", ch.edit);
                }
            }
            let _ = std::fs::remove_dir_all(&p.home);
        }
    }

    #[test]
    fn a_group_with_the_qx_hook_and_another_hook_gets_no_second_qx_hook() {
        let p = paths("mixedhook");
        let file = claude_settings(&p);
        let exe_text = q().to_string_lossy().replace('\\', "\\\\");
        let mixed = format!(
            "{{\"hooks\":{{\"PreToolUse\":[{{\"matcher\":\"Bash\",\"hooks\":[{{\"type\":\"command\",\"command\":\"{exe_text}\",\"args\":[\"hook\",\"claude\"]}},{{\"type\":\"command\",\"command\":\"echo mine\"}}]}}]}}}}"
        );
        put(&file, &mixed);
        assert!(matches!(edit_claude_hook(&file, q(), Action::Install), Edit::Unchanged), "the hook is already there");
        let newer = Path::new(r"D:\new\qx.exe");
        let text = changed(&edit_claude_hook(&file, newer, Action::Install)).expect("the stale command is updated").to_string();
        assert_eq!(qx_hook_commands(&text), vec![r"D:\new\qx.exe".to_string()], "one qx hook, pointing at the new exe: {text}");
        assert!(text.contains("echo mine"));
        put(&file, &text);
        assert!(matches!(edit_claude_hook(&file, newer, Action::Install), Edit::Unchanged));
        let removed = changed(&edit_claude_hook(&file, newer, Action::Remove)).expect("a removal").to_string();
        assert!(qx_hook_commands(&removed).is_empty(), "{removed}");
        assert!(removed.contains("echo mine"), "the user's own hook stays: {removed}");
        assert!(parse_strict(&removed).is_some());
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn claude_install_then_remove_gives_back_the_original_bytes() {
        let p = paths("rt-claude");
        let json = "{\n  \"numStartups\": 3,\n  \"mcpServers\": {\n    \"other\": {\"command\": \"x\"}\n  }\n}\n";
        let settings = "{\n  \"model\": \"opus\"\n}\n";
        put(&claude_file(&p), json);
        put(&claude_settings(&p), settings);
        cycle(&|a| plan(Client::ClaudeCode, &p, q(), a).unwrap());
        assert_eq!(std::fs::read_to_string(claude_file(&p)).unwrap(), json);
        assert_eq!(std::fs::read_to_string(claude_settings(&p)).unwrap(), settings);
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn files_qx_created_end_empty_and_leave_no_backup() {
        let p = paths("rt-claude-new");
        std::fs::create_dir_all(claude_settings(&p).parent().unwrap()).unwrap();
        cycle(&|a| plan(Client::ClaudeCode, &p, q(), a).unwrap());
        for file in [claude_file(&p), claude_settings(&p)] {
            if let Ok(text) = std::fs::read_to_string(&file) {
                assert_eq!(parse_strict(&text).unwrap(), json!({}), "{}: {text:?}", file.display());
            }
        }
        assert_eq!(leftovers(&p.home), Vec::<String>::new(), "qx's own text is no one's backup");
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn crlf_json_stays_crlf_and_a_comment_stays_put() {
        let p = paths("rt-crlf");
        let settings = "{\r\n  \"model\": \"opus\" // keep\r\n}\r\n";
        put(&claude_settings(&p), settings);
        for ch in plan(Client::ClaudeCode, &p, q(), Action::Install).unwrap() {
            apply(&ch).unwrap();
        }
        let installed = std::fs::read_to_string(claude_settings(&p)).unwrap();
        assert!(!installed.replace("\r\n", "").contains('\n'), "a bare LF went into a CRLF file: {installed:?}");
        assert!(installed.contains("\"model\": \"opus\", // keep\r\n"), "the comment stays on its line: {installed:?}");
        assert!(parse_strict(&installed).is_some());
        for ch in plan(Client::ClaudeCode, &p, q(), Action::Remove).unwrap() {
            apply(&ch).unwrap();
        }
        assert_eq!(std::fs::read_to_string(claude_settings(&p)).unwrap(), settings);
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn a_new_json_member_gets_a_comma_on_the_line_before_and_its_own_closing_brace() {
        let p = paths("fmt");
        put(&claude_file(&p), "{\n  \"a\": 1\n}\n");
        let text = changed(&plan(Client::ClaudeCode, &p, q(), Action::Install).unwrap()[0].edit).unwrap().to_string();
        assert!(text.starts_with("{\n  \"a\": 1,\n  \"mcpServers\": {\n"), "{text}");
        assert!(text.ends_with("\n  }\n}\n"), "{text}");
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn opencode_install_then_remove_gives_back_the_original_bytes() {
        let p = paths("rt-opencode");
        let file = p.home.join(".config").join("opencode").join("opencode.json");
        let original = "{\n  \"theme\": \"dark\"\n}\n";
        put(&file, original);
        cycle(&|a| plan(Client::OpenCode, &p, q(), a).unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn codex_install_then_remove_gives_back_the_original_bytes() {
        let p = paths("rt-codex");
        let file = p.codex_home.join("config.toml");
        let original = "model = \"o3\"\n\n[projects.\"c:/x\"]\ntrust_level = \"trusted\"\n";
        put(&file, original);
        cycle(&|a| plan(Client::Codex, &p, q(), a).unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn dsh_install_then_remove_gives_back_the_original_bytes() {
        let p = paths("rt-dsh");
        let file = p.dsh_home.join("cordis.patch.yml");
        let original = "- insert:\n    - id: mine\n      name: '@x/y'\n      config:\n        serverName: mine\n";
        put(&file, original);
        cycle(&|a| plan(Client::Dsh, &p, q(), a).unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn hermes_install_then_remove_gives_back_the_original_bytes() {
        for (tag, original) in [
            ("a", "model:\n  default: x\n"),
            ("b", "model:\n  default: x\nmcp_servers:\n  other:\n    command: x\n"),
        ] {
            let p = paths(&format!("rt-hermes-{tag}"));
            let file = p.hermes_home.join("config.yaml");
            put(&file, original);
            cycle(&|a| plan(Client::Hermes, &p, q(), a).unwrap());
            assert_eq!(std::fs::read_to_string(&file).unwrap(), original, "{tag}");
            let _ = std::fs::remove_dir_all(&p.home);
        }
    }

    #[test]
    fn pi_old_extension_installs_again_unchanged_and_leaves_nothing_behind() {
        let p = paths("rt-pi-old");
        cycle(&|a| pi_changes((0, 85), &p, a));
        assert_eq!(leftovers(&p.home), Vec::<String>::new(), "qx made these files, so it keeps no copy of them");
        assert!(!p.home.join(".pi").join("agent").join("extensions").exists(), "the empty folders qx made go too");
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn pi_new_mcp_json_gives_back_the_original_bytes() {
        let p = paths("rt-pi-new");
        let file = p.home.join(".pi").join("agent").join("mcp.json");
        let original = "{\n  \"mcpServers\": {\n    \"other\": {\"command\": \"x\"}\n  }\n}\n";
        put(&file, original);
        cycle(&|a| pi_changes((1, 3), &p, a));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        let _ = std::fs::remove_dir_all(&p.home);
        let fresh = paths("rt-pi-new2");
        cycle(&|a| pi_changes((1, 3), &fresh, a));
        assert_eq!(leftovers(&fresh.home), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&fresh.home);
    }

    // ---- end to end: real plan + apply in a temp home ----

    fn all_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                all_files(&path, out);
            } else {
                out.push(path);
            }
        }
    }

    fn apply_all(changes: Vec<Change>) -> usize {
        let mut n = 0;
        for ch in changes {
            assert!(!matches!(ch.edit, Edit::Refuse(_)), "refused: {}", ch.file.display());
            if matches!(ch.edit, Edit::Changed(_)) {
                n += 1;
                apply(&ch).unwrap();
            }
        }
        n
    }

    /// Install, install again (nothing changes), remove: a seeded home gets its own bytes back,
    /// an empty one keeps no trace of qx and no backup.
    fn roundtrip(
        tag: &str,
        seeds: impl Fn(&Paths) -> Vec<(PathBuf, &'static str)>,
        plan_for: impl Fn(&Paths, Action) -> Vec<Change>,
    ) {
        let p = paths(tag);
        let seeds = seeds(&p);
        for (file, text) in &seeds {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
        assert!(apply_all(plan_for(&p, Action::Install)) > 0, "{tag}: install changed nothing");
        assert_eq!(apply_all(plan_for(&p, Action::Install)), 0, "{tag}: a second install must change nothing");
        assert!(apply_all(plan_for(&p, Action::Remove)) > 0, "{tag}: remove changed nothing");
        for (file, text) in &seeds {
            assert_eq!(&std::fs::read_to_string(file).unwrap(), text, "{tag}: {} is not the original", file.display());
        }
        assert_eq!(apply_all(plan_for(&p, Action::Remove)), 0, "{tag}: a second remove must change nothing");
        let mut files = Vec::new();
        all_files(&p.home, &mut files);
        // A home with no config gets none back: not even a `{}` or a backup.
        assert!(!seeds.is_empty() || files.is_empty(), "{tag}: files left in an empty home: {files:?}");
        let _ = std::fs::remove_dir_all(&p.home);
    }

    const E2E_EXE: &str = r"C:\q\qx.exe";

    fn real_plan(c: Client) -> impl Fn(&Paths, Action) -> Vec<Change> {
        move |p, a| plan(c, p, Path::new(E2E_EXE), a).unwrap()
    }

    #[test]
    fn e2e_claude_code_roundtrip() {
        let c = |p: &Paths| claude_file(p);
        let s = |p: &Paths| claude_settings(p);
        roundtrip("e2e-claude-fresh", |_| vec![], real_plan(Client::ClaudeCode));
        roundtrip(
            "e2e-claude-full",
            |p| {
                vec![
                    (c(p), "{\n  \"numStartups\": 3,\n  \"mcpServers\": {\n    \"other\": {\n      \"command\": \"x\"\n    }\n  }\n}\n"),
                    (s(p), "{\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"PreToolUse\": [\n      {\n        \"matcher\": \"Edit\",\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"y\"\n          }\n        ]\n      }\n    ]\n  }\n}\n"),
                ]
            },
            real_plan(Client::ClaudeCode),
        );
        roundtrip(
            "e2e-claude-bare",
            |p| vec![(c(p), "{\n  \"numStartups\": 3\n}\n"), (s(p), "{\n  \"model\": \"opus\"\n}\n")],
            real_plan(Client::ClaudeCode),
        );
        roundtrip(
            "e2e-claude-crlf",
            |p| vec![(c(p), "{\r\n  \"numStartups\": 3\r\n}\r\n"), (s(p), "{\r\n  \"model\": \"opus\"\r\n}\r\n")],
            real_plan(Client::ClaudeCode),
        );
    }

    #[test]
    fn e2e_opencode_roundtrip() {
        let f = |p: &Paths| opencode_file(p);
        roundtrip("e2e-opencode-fresh", |_| vec![], real_plan(Client::OpenCode));
        roundtrip(
            "e2e-opencode-bare",
            |p| vec![(f(p), "{\n  \"theme\": \"dark\"\n}\n")],
            real_plan(Client::OpenCode),
        );
        roundtrip(
            "e2e-opencode-servers",
            |p| vec![(f(p), "{\n  \"mcp\": {\n    \"servers\": {\n      \"other\": {\n        \"command\": [\"x\"]\n      }\n    }\n  }\n}\n")],
            real_plan(Client::OpenCode),
        );
    }

    #[test]
    fn e2e_cursor_roundtrip() {
        let f = |p: &Paths| p.home.join(".cursor").join("mcp.json");
        roundtrip("e2e-cursor-fresh", |_| vec![], real_plan(Client::Cursor));
        roundtrip(
            "e2e-cursor-other",
            |p| vec![(f(p), "{
  \"mcpServers\": {
    \"other\": {
      \"command\": \"x\"
    }
  }
}
")],
            real_plan(Client::Cursor),
        );
    }

    fn claude_script(p: &Paths) -> PathBuf {
        claude_dir(p).join("qx-statusline.js")
    }

    fn claude_sidecar_file(p: &Paths) -> PathBuf {
        claude_dir(p).join("qx-statusline.json")
    }

    #[test]
    fn claude_chains_a_foreign_status_line_and_remove_restores_it_byte_for_byte() {
        let p = paths("sl-foreign");
        // Odd spacing on purpose: the restored value is the user's own text, not a re-print.
        let foreign = "{\n  \"model\": \"opus\",\n  \"statusLine\": {\n    \"type\": \"command\",   \"command\": \"echo mine\"\n  }\n}\n";
        put(&claude_settings(&p), foreign);
        apply_all(real_plan(Client::ClaudeCode)(&p, Action::Install));

        let installed = parse_strict(&std::fs::read_to_string(claude_settings(&p)).unwrap()).unwrap();
        let script = claude_script(&p).to_string_lossy().replace('\\', "/");
        assert_eq!(installed["statusLine"]["command"], format!("node \"{script}\""));
        assert_eq!(installed["statusLine"]["type"], "command");
        assert!(installed["statusLine"].get("refreshInterval").is_none(), "qx must not force a refresh timer");
        let sidecar: Value = serde_json::from_str(&std::fs::read_to_string(claude_sidecar_file(&p)).unwrap()).unwrap();
        assert_eq!(sidecar["previous"], json!({"type": "command", "command": "echo mine"}));
        assert_eq!(
            std::fs::read_to_string(claude_script(&p)).unwrap(),
            fill_exe(STATUSLINE_SCRIPT, Path::new(E2E_EXE))
        );
        assert!(status(Client::ClaudeCode, &p).notes.iter().all(|n| !n.contains("statusLine")), "{:?}", status(Client::ClaudeCode, &p).notes);

        assert_eq!(apply_all(real_plan(Client::ClaudeCode)(&p, Action::Install)), 0, "a second install changes nothing");
        apply_all(real_plan(Client::ClaudeCode)(&p, Action::Remove));
        assert_eq!(std::fs::read_to_string(claude_settings(&p)).unwrap(), foreign);
        assert!(!claude_script(&p).exists() && !claude_sidecar_file(&p).exists());
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn claude_reinstall_with_another_exe_keeps_the_first_previous() {
        let p = paths("sl-twice");
        put(&claude_settings(&p), "{\"statusLine\": {\"type\": \"command\", \"command\": \"echo mine\"}}\n");
        apply_all(real_plan(Client::ClaudeCode)(&p, Action::Install));
        let sidecar = std::fs::read_to_string(claude_sidecar_file(&p)).unwrap();
        let settings = std::fs::read_to_string(claude_settings(&p)).unwrap();

        let newer = Path::new(r"D:\new\qx.exe");
        let changes = plan(Client::ClaudeCode, &p, newer, Action::Install).unwrap();
        // Only the hook and the script name the exe; qx's own statusLine is never stored as the one to restore.
        let sidecar_edit = &changes.iter().find(|c| c.file == claude_sidecar_file(&p)).unwrap().edit;
        assert!(matches!(sidecar_edit, Edit::Unchanged), "{sidecar_edit:?}");
        for ch in changes {
            apply(&ch).unwrap();
        }
        assert_eq!(std::fs::read_to_string(claude_sidecar_file(&p)).unwrap(), sidecar);
        let after = parse_strict(&std::fs::read_to_string(claude_settings(&p)).unwrap()).unwrap();
        assert_eq!(after["statusLine"], parse_strict(&settings).unwrap()["statusLine"]);
        assert_eq!(apply_all(plan(Client::ClaudeCode, &p, newer, Action::Install).unwrap()), 0);
        apply_all(plan(Client::ClaudeCode, &p, newer, Action::Remove).unwrap());
        assert_eq!(
            parse_strict(&std::fs::read_to_string(claude_settings(&p)).unwrap()).unwrap(),
            json!({"statusLine": {"type": "command", "command": "echo mine"}})
        );
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn claude_without_a_status_line_ends_without_one() {
        let p = paths("sl-none");
        let original = "{\n  \"model\": \"opus\"\n}\n";
        put(&claude_settings(&p), original);
        apply_all(real_plan(Client::ClaudeCode)(&p, Action::Install));
        let sidecar: Value = serde_json::from_str(&std::fs::read_to_string(claude_sidecar_file(&p)).unwrap()).unwrap();
        assert_eq!(sidecar["previous"], Value::Null);
        assert!(parse_strict(&std::fs::read_to_string(claude_settings(&p)).unwrap()).unwrap().get("statusLine").is_some());
        apply_all(real_plan(Client::ClaudeCode)(&p, Action::Remove));
        let restored = std::fs::read_to_string(claude_settings(&p)).unwrap();
        assert_eq!(restored, original);
        assert!(parse_strict(&restored).unwrap().get("statusLine").is_none());
        assert!(!claude_script(&p).exists() && !claude_sidecar_file(&p).exists());
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn cursor_takes_a_free_status_line_slot_and_leaves_a_foreign_one() {
        let p = paths("sl-cursor");
        let config = cursor_config(&p);
        // cursor-agent writes its own config on first run: without one qx has nothing to edit.
        apply_all(real_plan(Client::Cursor)(&p, Action::Install));
        assert!(!config.exists(), "qx never creates cli-config.json");
        assert!(matches!(edit_cursor_statusline(&config, Path::new(E2E_EXE), Action::Remove), Edit::Unchanged));
        put(&config, "{}\n");
        apply_all(real_plan(Client::Cursor)(&p, Action::Install));
        let v = parse_strict(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(v["statusLine"], cursor_statusline_value(Path::new(E2E_EXE)));
        assert_eq!(v["statusLine"]["command"], format!("{E2E_EXE} gain --line"));
        assert_eq!(apply_all(real_plan(Client::Cursor)(&p, Action::Install)), 0);
        apply_all(real_plan(Client::Cursor)(&p, Action::Remove));
        assert!(!config.exists(), "a config qx emptied goes");
        let _ = std::fs::remove_dir_all(&p.home);

        let p = paths("sl-cursor-foreign");
        let config = cursor_config(&p);
        let foreign = "{\n  \"statusLine\": {\"type\": \"command\", \"command\": \"mine\"}\n}\n";
        put(&config, foreign);
        apply_all(real_plan(Client::Cursor)(&p, Action::Install));
        assert_eq!(std::fs::read_to_string(&config).unwrap(), foreign, "another tool's slot is left alone");
        let notes = status(Client::Cursor, &p).notes;
        assert!(notes.iter().any(|n| n.contains("another tool")), "{notes:?}");
        apply_all(real_plan(Client::Cursor)(&p, Action::Remove));
        assert_eq!(std::fs::read_to_string(&config).unwrap(), foreign, "remove does not delete what is not qx's");
        let _ = std::fs::remove_dir_all(&p.home);
    }

    fn edit_for<'a>(changes: &'a [Change], file: &Path) -> &'a Edit {
        &changes.iter().find(|c| c.file == file).expect("a change for the file").edit
    }

    #[test]
    fn claude_keeps_a_corrupt_or_missing_sidecar_while_qx_holds_the_slot() {
        let p = paths("sl-corrupt");
        put(&claude_settings(&p), "{\"statusLine\": {\"type\": \"command\", \"command\": \"echo mine\"}}\n");
        apply_all(real_plan(Client::ClaudeCode)(&p, Action::Install));
        let sidecar = claude_sidecar_file(&p);

        std::fs::write(&sidecar, "{ not json").unwrap();
        let changes = real_plan(Client::ClaudeCode)(&p, Action::Install);
        assert!(matches!(edit_for(&changes, &sidecar), Edit::Unchanged));
        apply_all(changes);
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "{ not json", "no fresh null record over a corrupt one");

        std::fs::remove_file(&sidecar).unwrap();
        apply_all(real_plan(Client::ClaudeCode)(&p, Action::Install));
        assert!(!sidecar.exists(), "no invented `previous: null` when the record is gone");
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn claude_remove_without_the_sidecar_keeps_the_status_line_and_its_script() {
        let p = paths("sl-nosidecar");
        put(&claude_settings(&p), "{\"statusLine\": {\"type\": \"command\", \"command\": \"echo mine\"}}\n");
        apply_all(real_plan(Client::ClaudeCode)(&p, Action::Install));
        let installed = std::fs::read_to_string(claude_settings(&p)).unwrap();
        for sidecar_text in [None, Some("{ not json")] {
            match sidecar_text {
                None => std::fs::remove_file(claude_sidecar_file(&p)).unwrap(),
                Some(text) => std::fs::write(claude_sidecar_file(&p), text).unwrap(),
            }
            let changes = real_plan(Client::ClaudeCode)(&p, Action::Remove);
            match edit_for(&changes, &claude_settings(&p)) {
                Edit::Refuse(why) => assert!(why.contains("previous statusLine is unknown"), "{why}"),
                other => panic!("the settings edit must refuse: {other:?}"),
            }
            for ch in &changes {
                apply(ch).unwrap();
            }
            assert_eq!(std::fs::read_to_string(claude_settings(&p)).unwrap(), installed, "{sidecar_text:?}: the slot is left alone");
            assert!(claude_script(&p).exists(), "{sidecar_text:?}: the script the slot runs stays");
            assert_eq!(std::fs::read_to_string(claude_sidecar_file(&p)).ok().as_deref(), sidecar_text);
        }
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn claude_unparseable_settings_leave_the_sidecar_alone() {
        let p = paths("sl-unparseable");
        let sidecar = claude_sidecar_file(&p);
        for settings in ["{ \"model\": ", "{\"statusLine\": nope}"] {
            put(&claude_settings(&p), settings);
            put(&sidecar, "{\"previous\": {\"type\": \"command\", \"command\": \"echo mine\"}}\n");
            let changes = plan(Client::ClaudeCode, &p, q(), Action::Install).unwrap();
            assert!(matches!(edit_for(&changes, &sidecar), Edit::Unchanged), "{settings}");
            assert!(matches!(edit_for(&changes, &claude_settings(&p)), Edit::Refuse(_)), "{settings}");
        }
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn claude_status_line_keeps_the_previous_keys_and_forces_no_refresh() {
        let p = paths("sl-padding");
        let foreign = "{\"statusLine\": {\"type\": \"command\", \"command\": \"echo mine\", \"padding\": 0}}\n";
        put(&claude_settings(&p), foreign);
        apply_all(real_plan(Client::ClaudeCode)(&p, Action::Install));
        let installed = parse_strict(&std::fs::read_to_string(claude_settings(&p)).unwrap()).unwrap();
        assert_eq!(installed["statusLine"]["padding"], 0);
        assert_eq!(installed["statusLine"]["type"], "command");
        assert!(installed["statusLine"]["command"].as_str().unwrap().contains("qx-statusline.js"));
        assert!(installed["statusLine"].get("refreshInterval").is_none());
        assert_eq!(apply_all(real_plan(Client::ClaudeCode)(&p, Action::Install)), 0, "a second install changes nothing");
        apply_all(real_plan(Client::ClaudeCode)(&p, Action::Remove));
        assert_eq!(std::fs::read_to_string(claude_settings(&p)).unwrap(), foreign);
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn claude_without_node_installs_no_status_line() {
        let p = paths("sl-nonode");
        let foreign = "{\"statusLine\": {\"type\": \"command\", \"command\": \"echo mine\"}}\n";
        put(&claude_settings(&p), foreign);
        for ch in plan_with(Client::ClaudeCode, &p, Path::new(E2E_EXE), Action::Install, false).unwrap() {
            apply(&ch).unwrap();
        }
        let after = parse_strict(&std::fs::read_to_string(claude_settings(&p)).unwrap()).unwrap();
        assert_eq!(after["statusLine"], json!({"type": "command", "command": "echo mine"}));
        assert!(!claude_script(&p).exists() && !claude_sidecar_file(&p).exists());
        let notes = status_with(Client::ClaudeCode, &p, false).notes;
        assert!(notes.iter().any(|n| n.contains("node not found on PATH")), "{notes:?}");
        let _ = std::fs::remove_dir_all(&p.home);
    }

    #[test]
    fn cursor_quotes_the_exe_only_when_its_path_has_whitespace() {
        let plain = cursor_statusline_value(Path::new(E2E_EXE));
        assert_eq!(plain["command"], format!("{E2E_EXE} gain --line"));
        assert!(is_qx_statusline(&plain));
        let spaced = cursor_statusline_value(Path::new(r"C:\Program Files\qx\qx.exe"));
        assert_eq!(spaced["command"], r#""C:\Program Files\qx\qx.exe" gain --line"#);
        assert!(is_qx_statusline(&spaced));
    }
    #[test]
    fn e2e_omp_roundtrip() {
        let f = |p: &Paths| p.omp_agent_dir.join("mcp.json");
        roundtrip("e2e-omp-fresh", |_| vec![], real_plan(Client::Omp));
        roundtrip(
            "e2e-omp-other",
            |p| vec![(f(p), "{\n  \"mcpServers\": {\n    \"other\": {\n      \"command\": \"x\"\n    }\n  }\n}\n")],
            real_plan(Client::Omp),
        );
        // The bash hook names the installed exe, and status reads it back.
        let p = paths("e2e-omp-hook");
        apply_all(real_plan(Client::Omp)(&p, Action::Install));
        let hook = std::fs::read_to_string(omp_extension(&p)).unwrap();
        assert_eq!(extension_exe(&hook).as_deref(), Some(E2E_EXE), "{hook}");
        assert!(status(Client::Omp, &p).notes.is_empty(), "{:?}", status(Client::Omp, &p).notes);
        std::fs::write(omp_extension(&p), "// the user's own\n").unwrap();
        apply_all(real_plan(Client::Omp)(&p, Action::Remove));
        assert!(!omp_extension(&p).exists());
        assert!(p.omp_agent_dir.join("extensions").join("qx.js.qx-bak").is_file(), "an edited hook is kept");
        let _ = std::fs::remove_dir_all(&p.home);
        let p = Paths::from_lookup(|k| match k {
            "USERPROFILE" => Some(r"C:\h".into()),
            "PI_CONFIG_DIR" => Some(".pix".into()),
            _ => None,
        });
        assert_eq!(p.omp_agent_dir, Path::new(r"C:\h\.pix\agent"));
        let p = Paths::from_lookup(|k| (k == "PI_CODING_AGENT_DIR").then(|| r"D:\a".into()));
        assert_eq!(p.omp_agent_dir, Path::new(r"D:\a"));
    }

    #[test]
    fn e2e_codex_roundtrip() {
        let f = |p: &Paths| p.codex_home.join("config.toml");
        roundtrip("e2e-codex-fresh", |_| vec![], real_plan(Client::Codex));
        roundtrip(
            "e2e-codex-table",
            |p| vec![(f(p), "model = \"o3\"\n\n[mcp_servers.other]\ncommand = \"x\"\n")],
            real_plan(Client::Codex),
        );
        roundtrip("e2e-codex-bare", |p| vec![(f(p), "model = \"o3\"\n")], real_plan(Client::Codex));
        roundtrip(
            "e2e-codex-bom-crlf",
            |p| vec![(f(p), "\u{feff}model = \"o3\"\r\n")],
            real_plan(Client::Codex),
        );
    }

    #[test]
    fn e2e_dsh_roundtrip() {
        let f = |p: &Paths| p.dsh_home.join("cordis.patch.yml");
        roundtrip("e2e-dsh-fresh", |_| vec![], real_plan(Client::Dsh));
        roundtrip(
            "e2e-dsh-other",
            |p| vec![(f(p), "- insert:\n    - id: mcp-other\n      name: b\n")],
            real_plan(Client::Dsh),
        );
    }

    #[test]
    fn e2e_hermes_roundtrip() {
        let f = |p: &Paths| p.hermes_home.join("config.yaml");
        roundtrip("e2e-hermes-fresh", |_| vec![], real_plan(Client::Hermes));
        roundtrip(
            "e2e-hermes-other",
            |p| vec![(f(p), "model: x\nmcp_servers:\n  other:\n    command: o\n")],
            real_plan(Client::Hermes),
        );
        roundtrip("e2e-hermes-bare", |p| vec![(f(p), "model: x\n")], real_plan(Client::Hermes));
    }

    #[test]
    fn e2e_pi_roundtrip() {
        let exe = Path::new(E2E_EXE);
        let plan_for = |version: (u32, u32)| {
            move |p: &Paths, a| {
                pi::edit_pi(Some(version), p, exe, a)
                    .into_iter()
                    .map(|(file, edit)| Change { client: Client::Pi, file, edit })
                    .collect()
            }
        };
        // 0.85: three generated files, and the folders made for them go with them.
        roundtrip("e2e-pi-old", |_| vec![], plan_for((0, 85)));
        let p = paths("e2e-pi-old-dirs");
        apply_all(plan_for((0, 85))(&p, Action::Install));
        apply_all(plan_for((0, 85))(&p, Action::Remove));
        assert!(!p.home.join(".pi").join("agent").join("extensions").exists());
        let _ = std::fs::remove_dir_all(&p.home);
        roundtrip("e2e-pi-new-fresh", |_| vec![], plan_for((1, 3)));
        roundtrip(
            "e2e-pi-new-other",
            |p| vec![(p.home.join(".pi").join("agent").join("mcp.json"), "{\n  \"mcpServers\": {\n    \"other\": {\n      \"command\": \"x\"\n    }\n  }\n}\n")],
            plan_for((1, 3)),
        );
    }
}
