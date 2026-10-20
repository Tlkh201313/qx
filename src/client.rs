//! Per-client wording, tool ids and structured-output policy.
//!
//! Every CLI that speaks MCP names its own tools differently and surfaces results
//! differently: Codex shows the model only `structuredContent`, Hermes shows the
//! text and the JSON (so `stdout` appears twice), dsh, Claude Code and Hermes reject
//! a result that carries an `outputSchema` but no `structuredContent`. The server
//! therefore asks this module what to print instead of assuming dsh.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Client {
    Dsh,
    ClaudeCode,
    Codex,
    OpenCode,
    Hermes,
    Pi,
    Generic,
}

/// The native tool a client has, so a qx message can tell the model where to go next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Native {
    Shell,
    Read,
    Write,
    Edit,
    Glob,
    Grep,
}

/// How a client surfaces a tool result.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Structured {
    /// Today's behavior: `outputSchema` plus `structuredContent` with `stdout`/`stderr`.
    Full,
    /// `structuredContent` without `stdout` and `stderr` (the text already carries them).
    Meta,
    /// No `outputSchema` in `tools/list`, no `structuredContent` in results.
    Off,
}

impl Native {
    /// The name qx uses in its own messages (`read`, `write`, …), matched back to a tool.
    pub fn from_name(name: &str) -> Option<Native> {
        Some(match name {
            "shell" => Native::Shell,
            "read" => Native::Read,
            "write" => Native::Write,
            "edit" => Native::Edit,
            "glob" => Native::Glob,
            "grep" => Native::Grep,
            _ => return None,
        })
    }

    fn index(self) -> u8 {
        match self {
            Native::Shell => 0,
            Native::Read => 1,
            Native::Write => 2,
            Native::Edit => 3,
            Native::Glob => 4,
            Native::Grep => 5,
        }
    }
}

impl Client {
    // Used by `qx setup` to walk every client it can register.
    #[allow(dead_code)]
    pub const ALL: [Client; 6] = [
        Client::Dsh,
        Client::ClaudeCode,
        Client::Codex,
        Client::OpenCode,
        Client::Hermes,
        Client::Pi,
    ];

    // Used by `qx setup` to name each client in its own config.
    #[allow(dead_code)]
    pub fn key(self) -> &'static str {
        match self {
            Client::Dsh => "dsh",
            Client::ClaudeCode => "claude-code",
            Client::Codex => "codex",
            Client::OpenCode => "opencode",
            Client::Hermes => "hermes",
            Client::Pi => "pi",
            Client::Generic => "generic",
        }
    }

    pub fn from_key(key: &str) -> Option<Client> {
        Some(match key.trim().to_ascii_lowercase().as_str() {
            "dsh" => Client::Dsh,
            "claude-code" | "claude" => Client::ClaudeCode,
            "codex" => Client::Codex,
            "opencode" | "open-code" => Client::OpenCode,
            "hermes" => Client::Hermes,
            "pi" => Client::Pi,
            "generic" => Client::Generic,
            _ => return None,
        })
    }

    /// `QX_CLIENT`, then the exact `clientInfo.name`, then `CLAUDECODE=1`, then generic.
    /// Hermes reports the SDK default `mcp`, which is not distinctive, so it needs `QX_CLIENT`.
    pub fn detect(client_info_name: Option<&str>, env: impl Fn(&str) -> Option<String>) -> Client {
        if let Some(c) = env("QX_CLIENT").as_deref().and_then(Client::from_key) {
            return c;
        }
        if let Some(name) = client_info_name {
            let named = match name {
                "dsh-mcp-client" => Some(Client::Dsh),
                "claude-code" => Some(Client::ClaudeCode),
                "codex-mcp-client" => Some(Client::Codex),
                "opencode" => Some(Client::OpenCode),
                "pi" => Some(Client::Pi),
                _ => None,
            };
            if let Some(c) = named {
                return c;
            }
        }
        if env("CLAUDECODE").is_some() {
            return Client::ClaudeCode;
        }
        Client::Generic
    }

    /// How this client's model reaches the tool: OpenCode renames it, others use the
    /// server-qualified id, and a client qx cannot name gets a phrase instead.
    pub fn tool_id(self, tool: &str) -> String {
        match self {
            Client::OpenCode => format!("qx_{tool}"),
            Client::Generic => format!("the qx `{tool}` tool"),
            _ => format!("mcp__qx__{tool}"),
        }
    }

    /// The backticked name of the client's own tool, safe to drop into a sentence
    /// shaped `use the native {} tool`. The generic client names no tool, so it gets
    /// the bare role (`write`) and the sentence stays true.
    pub fn native(self, n: Native) -> &'static str {
        let named = match self {
            Client::Dsh => ["`pwsh`", "`read`", "`write`", "`edit`", "`glob`", "`grep`"],
            Client::ClaudeCode => ["`Bash`", "`Read`", "`Write`", "`Edit`", "`Glob`", "`Grep`"],
            Client::Codex => [
                "`exec_command`",
                "`exec_command`",
                "`apply_patch`",
                "`apply_patch`",
                "`exec_command`",
                "`exec_command`",
            ],
            Client::OpenCode => ["`shell`", "`read`", "`write`", "`edit`", "`glob`", "`grep`"],
            Client::Hermes => [
                "`terminal`",
                "`read_file`",
                "`write_file`",
                "`patch`",
                "`search_files`",
                "`search_files`",
            ],
            Client::Pi => ["`bash`", "`read`", "`write`", "`edit`", "`bash`", "`bash`"],
            Client::Generic => ["shell", "read", "write", "edit", "glob", "grep"],
        };
        named[n.index() as usize]
    }

    pub fn structured(self) -> Structured {
        match self {
            Client::ClaudeCode | Client::Codex => Structured::Off,
            Client::Hermes => Structured::Meta,
            Client::Dsh | Client::OpenCode | Client::Pi | Client::Generic => Structured::Full,
        }
    }

    /// Server instructions, kept under 1024 bytes: Codex reads the first 512, Claude
    /// Code truncates at 4096, dsh fails above 32 KiB, and all three are paid per request.
    pub fn instructions(self) -> String {
        let (shell, read, write) = (
            self.native(Native::Shell),
            self.native(Native::Read),
            self.native(Native::Write),
        );
        format!(
            "qx is the preferred tool layer for this session. Call {} for commands: prefer \
             `program` + `args`, pass the session `cwd`; for env vars use \
             `command: \"NAME=v prog args\"` instead of a shell wrapper, and keep \
             `mode: \"compact\"`. Call {} for filesystem work: \
             `op=read|write|edit|glob|grep|list|stat`, always with `cwd`. qx fs is \
             workspace-confined, and write/edit need a prior qx read. Prefer qx for read, glob \
             and literal grep. On a qx error, or for regex, images or sandbox escalation, fall \
             back to {shell} and {read}/{write}. If a `run` output is truncated, read its \
             `full_output` spool instead of re-running.",
            self.tool_id("run"),
            self.tool_id("fs"),
        )
    }

    fn index(self) -> u8 {
        match self {
            Client::Dsh => 0,
            Client::ClaudeCode => 1,
            Client::Codex => 2,
            Client::OpenCode => 3,
            Client::Hermes => 4,
            Client::Pi => 5,
            Client::Generic => 6,
        }
    }

    fn from_index(i: u8) -> Client {
        match i {
            0 => Client::Dsh,
            1 => Client::ClaudeCode,
            2 => Client::Codex,
            3 => Client::OpenCode,
            4 => Client::Hermes,
            5 => Client::Pi,
            _ => Client::Generic,
        }
    }
}

/// The current session's client. Set from `QX_CLIENT` at startup and from
/// `clientInfo.name` on `initialize`; one process serves one session.
static CURRENT: AtomicU8 = AtomicU8::new(6);

thread_local! {
    /// A per-thread override for tests: the global is process-wide and tests run in parallel.
    static SCOPED: std::cell::Cell<Option<Client>> = const { std::cell::Cell::new(None) };
}

pub fn set_current(c: Client) {
    CURRENT.store(c.index(), Ordering::Relaxed);
}

pub fn current() -> Client {
    match SCOPED.with(|c| c.get()) {
        Some(c) => c,
        None => Client::from_index(CURRENT.load(Ordering::Relaxed)),
    }
}

#[cfg(test)]
pub fn scoped<T>(c: Client, f: impl FnOnce() -> T) -> T {
    SCOPED.with(|cell| {
        let previous = cell.replace(Some(c));
        let out = f();
        cell.set(previous);
        out
    })
}

/// Serializes the tests that change the process-wide client: anything running on a
/// spawned thread reads the global, so a test that sets it must not overlap one that
/// spawns workers.
#[cfg(test)]
pub fn lock_global() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_prefers_env_then_client_info_then_claudecode() {
        let none = |_: &str| None;
        assert_eq!(Client::detect(Some("codex-mcp-client"), none), Client::Codex);
        assert_eq!(Client::detect(Some("dsh-mcp-client"), none), Client::Dsh);
        assert_eq!(Client::detect(Some("mcp"), none), Client::Generic);
        assert_eq!(
            Client::detect(Some("mcp"), |k| (k == "QX_CLIENT").then(|| "hermes".into())),
            Client::Hermes
        );
        assert_eq!(
            Client::detect(None, |k| (k == "CLAUDECODE").then(|| "1".into())),
            Client::ClaudeCode
        );
        assert_eq!(Client::from_key("claude-code"), Some(Client::ClaudeCode));
        assert_eq!(Client::from_key("nope"), None);
    }

    #[test]
    fn instructions_name_this_clients_tools_and_fit_every_cap() {
        for c in Client::ALL.into_iter().chain([Client::Generic]) {
            let t = c.instructions();
            assert!(t.len() < 1024, "{c:?} instructions are {} bytes", t.len());
            let head = &t[..t.len().min(512)];
            assert!(
                head.contains(&c.tool_id("run")) && head.contains(&c.tool_id("fs")),
                "{c:?} names its tools only past the 512-byte cap"
            );
            assert!(
                t.contains("cwd") && t.contains("program") && t.contains("fall back"),
                "{c:?}"
            );
            if c != Client::Dsh {
                assert!(!t.contains("dsh"), "{c:?} instructions mention dsh");
            }
        }
        assert_eq!(Client::OpenCode.tool_id("run"), "qx_run");
        assert!(Client::Codex.instructions().contains("apply_patch"));
    }

    #[test]
    fn every_native_name_is_usable_inside_the_sentence() {
        for c in Client::ALL.into_iter().chain([Client::Generic]) {
            for n in [
                Native::Shell,
                Native::Read,
                Native::Write,
                Native::Edit,
                Native::Glob,
                Native::Grep,
            ] {
                let name = c.native(n);
                assert!(!name.is_empty(), "{c:?} {n:?}");
                assert!(!name.contains('`') || name.starts_with('`'), "{c:?} {n:?}: {name}");
            }
        }
        assert_eq!(Client::Dsh.native(Native::Write), "`write`");
        assert_eq!(Client::Codex.native(Native::Write), "`apply_patch`");
        assert_eq!(Client::Generic.native(Native::Write), "write");
    }

    #[test]
    fn setting_the_client_is_global_and_scoped_is_not() {
        let _global = lock_global();
        let previous = current();
        set_current(Client::Pi);
        assert_eq!(current(), Client::Pi);
        scoped(Client::Hermes, || {
            assert_eq!(current(), Client::Hermes);
            scoped(Client::Codex, || assert_eq!(current(), Client::Codex));
            assert_eq!(current(), Client::Hermes);
        });
        assert_eq!(current(), Client::Pi);
        set_current(previous);
    }
}
