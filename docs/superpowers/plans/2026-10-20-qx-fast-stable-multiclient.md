# qx 0.7 — faster npm/Python, steadier MCP, every agent CLI — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpower-subagent-driven-development (recommended) or superpower-executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the measured npm/Python spawn and startup waste, make `qx mcp` shut down and back-pressure predictably, and register qx correctly in dsh, Claude Code, Codex, OpenCode, Hermes and pi with one command.

**Architecture:** Keep qx's shape (std-only Rust, `serde_json` the only dependency, fail closed to today's behavior). New focused modules: `client.rs` (per-client wording and output policy), `python.rs` (launcher/venv resolution), `deploy.rs` (canonical install), `setup/` (byte-preserving config editors per format). Existing modules change only at the seams named in each task.

**Tech Stack:** Rust 2021, `serde_json` 1, Windows 11 (`cfg(windows)` tests skip with a `skip:` line when a tool is absent), one plain-JS pi extension.

**Spec:** `docs/superpowers/specs/2026-10-20-qx-fast-stable-multiclient-design.md` (diagnosis D1–D13, requirements R1–R13).

## Global Constraints

- Dependencies: `serde_json = "1"` only. No `toml`, `yaml`, `indexmap`, async runtime, or serde_json `preserve_order` (R11).
- Every new fast path is fail-closed: an unrecognized shape returns `None` and the old path runs unchanged.
- New env switches, exactly: `QX_PYTHON_VENV=off`, `QX_MCP_QUEUE` (default 64, clamp 0..=1024), `QX_CLIENT` (`dsh|claude-code|codex|opencode|hermes|pi|generic`).
- Canonical binary: `%CARGO_HOME%\bin\qx.exe`, else `%USERPROFILE%\.cargo\bin\qx.exe` (R1).
- Tests touching real homes are forbidden: every setup/deploy function takes explicit paths; tests use `std::env::temp_dir()` dirs named `qx-<tag>-<pid>`.
- Run builds/tests through qx `run` (MCP) or an unsandboxed shell. The dsh `pwsh` sandbox is read-only, and Git's msys binaries fail inside it with "couldn't create signal pipe".
- Each task ends with `cargo test --release` fully green, its README paragraph updated, and one commit.
- Server instructions stay < 1024 bytes and their first 512 bytes stand alone (Codex reads 512, Claude Code truncates at 4096, dsh fails > 32 KiB).

## Review Focus

1. **A console-script launcher whose shebang interpreter was deleted or moved** (stale venv) must keep the launcher, not fail differently. Owned by Task 4: `stale_shebang_keeps_the_launcher`.
2. **qx started from inside an activated venv or with `PYTHONHOME` set** must not redirect venv Pythons. Owned by Task 4: `venv_redirect_skips_when_pythonhome_or_launcher_env_set`.
3. **A client closing stdin while calls are still queued** must never start the queued calls. Owned by Task 1: `eof_cancels_running_and_queued_calls_promptly`.
4. **An npm script naming a cmd internal, or a `.bin` shim containing script logic**, must keep `cmd.exe`. Owned by Task 5: `shell_syntax_cmd_internals_and_logic_shims_keep_cmd`.
5. **Hand-edited configs** (JSONC comments, trailing commas, flow-style YAML, inline TOML tables) must have every byte outside the qx entry preserved, or be refused with a paste-ready snippet. Owned by Tasks 8–10: `*_preserves_unrelated_bytes`, `*_refuses_unknown_shapes`.

## Client matrix (research 2026-10-20; drives Tasks 2 and 8–11)

| key | clientInfo.name | model tool id | native shell / read / write / edit / glob / grep | structured policy | config (format) | qx entry essentials |
|---|---|---|---|---|---|---|
| `dsh` | `dsh-mcp-client` | `mcp__qx__run` | `pwsh` / `read` / `write` / `edit` / `glob` / `grep` | Full | `%DSH_HOME%\cordis.patch.yml` (YAML list, item `id: mcp-qx`) | `command`, `args: [mcp]`, `env.QX_CLIENT: dsh`, `toolCallTimeoutMs: 300000`, `failOnStartupError: true` |
| `claude-code` | `claude-code` (also `CLAUDECODE=1`) | `mcp__qx__run` | `Bash` / `Read` / `Write` / `Edit` / `Glob` / `Grep` | Off | `%USERPROFILE%\.claude.json` `mcpServers.qx` (JSON) | `{"type":"stdio","command":…,"args":["mcp"],"env":{"QX_CLIENT":"claude-code"}}` |
| `codex` | `codex-mcp-client` | `mcp__qx__run` | `exec_command` / `exec_command` / `apply_patch` / `apply_patch` / `exec_command` / `exec_command` | Off | `%CODEX_HOME%` or `%USERPROFILE%\.codex\config.toml` `[mcp_servers.qx]` (TOML) | `command`, `args = ["mcp"]`, `startup_timeout_sec = 20`, `tool_timeout_sec = 600`, `[mcp_servers.qx.env] QX_CLIENT = "codex"` |
| `opencode` | `opencode` | `qx_run` | `shell` / `read` / `write` / `edit` / `glob` / `grep` | Full | `%USERPROFILE%\.config\opencode\opencode.json` or `.jsonc` `mcp.servers.qx` (JSONC) | `{"type":"local","command":[…,"mcp"],"environment":{"QX_CLIENT":"opencode"},"codemode":false,"timeout":{"execution":600000}}` |
| `hermes` | `mcp` (SDK default; not distinctive) | `mcp__qx__run` | `terminal` / `read_file` / `write_file` / `patch` / `search_files` / `search_files` | Meta | `%HERMES_HOME%` or `%LOCALAPPDATA%\hermes\config.yaml` `mcp_servers.qx` (YAML) | `command`, `args: [mcp]`, `env: {QX_CLIENT: hermes}`, `timeout: 600`, `connect_timeout: 60` |
| `pi` | `pi` (≥ 0.99 built-in) | `mcp__qx__run` | `bash` / `read` / `write` / `edit` / `bash` / `bash` | Full | ≥ 0.99: `%USERPROFILE%\.pi\agent\mcp.json` `mcpServers.qx`; < 0.99 (installed 0.85.1): extension `%USERPROFILE%\.pi\agent\extensions\qx.js` | mcp.json: `{"command":…,"args":["mcp"],"env":{"QX_CLIENT":"pi"},"exposure":"direct","timeout":600}` |
| `generic` | anything else | "the qx `run` tool" | "your native shell / read / write / edit / glob / grep tool" | Full | — | — |

Structured policies: **Full** is today's behavior (outputSchema plus structuredContent with `stdout`/`stderr`). **Meta** is structuredContent without `stdout`/`stderr`. **Off** means no `outputSchema` in `tools/list` and no `structuredContent` key in results.

Detection order: `QX_CLIENT` env, then exact clientInfo.name, then `CLAUDECODE=1`, then `generic`.

---

### Task 0: Baseline the uncommitted 0.6 work

**Files:** none edited (commit only).

- [ ] **Step 1: Run the full suite**

Run: `cargo test --release`
Expected: every `test result:` line says `ok. … 0 failed`. If any test fails, stop and report; do not start Task 1 on a red tree.

- [ ] **Step 2: Commit the review patches as the baseline**

```bash
git add Cargo.toml README.md src docs
git commit -m "qx 0.6.1: review patches (fs tool, fuzz tests, guard/redact/digest updates) + 0.7 spec and plan"
```

---

### Task 1: MCP server — route by method, bounded admission, bounded shutdown (D8, D9, R8)

**Files:**
- Modify: `src/mcp.rs` (`serve` at lines 15-66, `Inflight::begin` at 88-105, `respond` at 182-194)
- Test: `src/mcp.rs` `mod tests`

**Interfaces:**
- Produces: `pub fn serve() -> i32` (unchanged signature), which calls `serve_io(stdin.lock(), stdout(), Limits::from_env())`.
- Produces: `struct Limits { workers: usize, queue: usize, drain: Duration }`. `Limits::from_env()`: `QX_MCP_WORKERS` default 4, clamp 1..=16; `QX_MCP_QUEUE` default 64, clamp 0..=1024; drain 3 s.
- Produces: `fn serve_io<R: BufRead, W: Write + Send + 'static>(reader: R, out: W, limits: Limits) -> i32`.
- Produces: `#[derive(Debug, PartialEq)] enum Route { Inline, Worker }` and `fn route(req: &Value) -> Route`. Worker iff an object with `method == "tools/call"` and an `id`, or an array containing one.
- Produces: `fn cancel_all()`, which sets `static SHUTDOWN: AtomicBool` and every `INFLIGHT` flag.
- Produces: `fn respond_value(req: &Value) -> Option<Value>`. `respond(line)` stays as a parse-then-delegate wrapper (`-32700` on bad JSON) so the existing tests keep working.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn route_is_by_method_not_by_text() {
    assert_eq!(route(&json!({"jsonrpc":"2.0","id":"tools/call","method":"ping"})), Route::Inline);
    assert_eq!(route(&json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1,"reason":"tools/call"}})), Route::Inline);
    assert_eq!(route(&json!({"jsonrpc":"2.0","method":"tools/call","params":{}})), Route::Inline); // notification: never run
    assert_eq!(route(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{}})), Route::Worker);
}
#[test]
fn unknown_methods_like_server_discover_get_method_not_found() { // dsh probes a sibling qx with this first
    let r = respond(r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}"#).unwrap();
    assert_eq!(r.pointer("/error/code"), Some(&json!(-32601)));
}
#[cfg(windows)] #[test]
fn eof_cancels_running_and_queued_calls_promptly() {
    // input: initialize, id 1 = ping -n 30 127.0.0.1, id 2 = same; then EOF. Limits{workers:1, queue:4, drain:3s}
    // assert serve_io returns in < 6 s; reply id 1 has structuredContent.timed_out == true;
    // reply id 2 text contains "qx is shutting down" and isError == true (it never started).
}
#[cfg(windows)] #[test]
fn calls_beyond_the_limit_get_busy() {
    // Limits{workers:1, queue:0, drain:3s}; input: initialize, id 1 = ping -n 30, id 2 = pwd; EOF
    // reply id 2: isError == true, text contains "qx busy", structuredContent.source == "busy"
}
```

The writer for `serve_io` in tests is `#[derive(Clone)] struct Sink(Arc<Mutex<Vec<u8>>>)` implementing `Write`. Replies are parsed per line and looked up by `id`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --release mcp::tests::`
Expected: compile errors: `cannot find function route`, `serve_io`, `Limits`.

- [ ] **Step 3: Implement routing, admission and shutdown in `src/mcp.rs`**

- Parse each line once on the reader.
- `Route::Inline` replies go straight to the writer. `Route::Worker` increments `active` first, and if `active > workers + queue` it replies busy immediately: `"qx busy: {n} calls already running or queued (limit {workers}+{queue}); retry shortly"`.
- After `gate.acquire()`, a worker that sees `SHUTDOWN` replies `"qx is shutting down; the call was not run"` (isError) without spawning.
- `Inflight::begin` starts its flag at `true` when `SHUTDOWN` is set.
- On EOF: `cancel_all()`, wait until `active == 0` or `drain` elapses, return 0. Children die with qx's job object.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --release mcp::tests::`
Expected: PASS, including the pre-existing cancel/ping/malformed-request tests.

- [ ] **Step 5: Update README §6 (MCP server: queue limit, shutdown, `QX_MCP_QUEUE`) and commit**

```bash
git add src/mcp.rs README.md
git commit -m "mcp: route by method, bound queued calls, cancel in-flight work on EOF"
```

---

### Task 2: Client profiles — right tool names and output policy per CLI (D10, D12, R9, R12)

**Files:**
- Create: `src/client.rs`
- Modify: `src/main.rs` (`mod client;`); `src/mcp.rs` (`initialize`, `run_schema`, `fs_schema`, `tool_result`, `MCP_INSTRUCTIONS` removed); `src/fsops.rs` (`FsError::reply` at 73-91 and every message containing `native dsh`)
- Test: `src/client.rs` `mod tests`, plus the updated `src/fsops.rs` test that asserted `native dsh \`write\``

**Interfaces:**
- Produces, in `src/client.rs`:
  - `#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum Client { Dsh, ClaudeCode, Codex, OpenCode, Hermes, Pi, Generic }`
  - `pub enum Native { Shell, Read, Write, Edit, Glob, Grep }`
  - `pub enum Structured { Full, Meta, Off }`
  - `impl Client`:
    - `pub const ALL: [Client; 6]` (all but Generic)
    - `pub fn key(self) -> &'static str` and `pub fn from_key(&str) -> Option<Client>`
    - `pub fn detect(client_info_name: Option<&str>, env: impl Fn(&str) -> Option<String>) -> Client`
    - `pub fn tool_id(self, tool: &str) -> String`
    - `pub fn native(self, n: Native) -> &'static str`
    - `pub fn structured(self) -> Structured`
    - `pub fn instructions(self) -> String`
  - `pub fn set_current(Client)` and `pub fn current() -> Client`. Backed by an `AtomicU8`; before `initialize` it is `QX_CLIENT` or Generic.
  - `#[cfg(test)] pub fn scoped<T>(c: Client, f: impl FnOnce() -> T) -> T`, a thread-local override so parallel tests never race on the global.
- All values come from the Client matrix above; `native()` returns the backticked name, e.g. ``"`apply_patch`"``.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn detection_prefers_env_then_client_info_then_claudecode() {
    let none = |_: &str| None;
    assert_eq!(Client::detect(Some("codex-mcp-client"), none), Client::Codex);
    assert_eq!(Client::detect(Some("dsh-mcp-client"), none), Client::Dsh);
    assert_eq!(Client::detect(Some("mcp"), none), Client::Generic);
    assert_eq!(Client::detect(Some("mcp"), |k| (k == "QX_CLIENT").then(|| "hermes".into())), Client::Hermes);
    assert_eq!(Client::detect(None, |k| (k == "CLAUDECODE").then(|| "1".into())), Client::ClaudeCode);
}
#[test]
fn instructions_name_this_clients_tools_and_fit_every_cap() {
    for c in Client::ALL.into_iter().chain([Client::Generic]) {
        let t = c.instructions();
        assert!(t.len() < 1024, "{c:?}");
        let head = &t[..t.len().min(512)];
        assert!(head.contains(&c.tool_id("run")) && head.contains(&c.tool_id("fs")), "{c:?}");
        assert!(t.contains("cwd") && t.contains("program") && t.contains("fall back"), "{c:?}");
        if c != Client::Dsh { assert!(!t.contains("dsh"), "{c:?}"); }
    }
    assert_eq!(Client::OpenCode.tool_id("run"), "qx_run");
    assert!(Client::Codex.instructions().contains("apply_patch"));
}
#[test]
fn output_policy_follows_the_matrix() {
    // tools/list under scoped(Codex) and scoped(ClaudeCode): no tool has "outputSchema";
    // a run call under scoped(Codex) has no "structuredContent" key; under scoped(Hermes) it has
    // structuredContent.exit_code but no stdout; under scoped(Dsh) it has structuredContent.stdout.
}
```

In `src/fsops.rs`, change the existing assertion `r.text.contains("native dsh \`write\`")` to run under `client::scoped(Client::Dsh, …)` and expect ``"native `write`"``. Add the same call under `scoped(Client::Codex, …)` expecting ``"`apply_patch`"``.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --release client:: fsops::`
Expected: compile error `unresolved module client`.

- [ ] **Step 3: Implement `src/client.rs`, then wire it in**

- `initialize`: `client::set_current(Client::detect(params.clientInfo.name, |k| std::env::var(k).ok()))`; `instructions: current().instructions()`.
- `run_schema()` and `fs_schema()` build their descriptions with `native()` and drop `outputSchema` when `structured() == Off`.
- `tool_result` applies the policy: Off removes `structuredContent`; Meta removes `stdout` and `stderr`.
- In `fsops.rs`, every "native dsh `X` tool" phrase becomes ``format!("native {} tool", client::current().native(Native::X))``. The structured `fallback` field keeps its purpose word (`read`, `write`, …).
- The Dsh instructions keep today's sentence about sandbox escalation and images; other clients say "If a qx call errors, or needs images or regex, fall back to …".

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --release`
Expected: PASS (the whole suite; fsops message tests included).

- [ ] **Step 5: Update README §6 (per-client wording, `QX_CLIENT`, output policy table) and commit**

```bash
git add src/client.rs src/main.rs src/mcp.rs src/fsops.rs README.md
git commit -m "mcp: per-client tool names, native fallbacks and structured-output policy"
```

---

### Task 3: Built-ins that decline fall back to Git's coreutils (D7, R7)

**Files:**
- Modify: `src/resolve.rs` (`resolve` at 44-66, plus a new fn); `src/exec.rs` (`git_root` at 510-515, `Target::Missing` arm at 501)
- Test: `src/resolve.rs`, `src/mcp.rs` tests

**Interfaces:**
- Produces: `pub fn git_coreutil(name: &str) -> Option<PathBuf>` in `resolve.rs`.
  - Only these names: `basename cat cp dirname echo false head ls mkdir mv pwd realpath rm seq sleep tail touch true wc which` (ASCII case-insensitive).
  - Git root: the nearest ancestor (up to 3 levels) of `find_on_path("git")` that holds `usr\bin`; else the root of `git_bash()`. Cached in a `OnceLock`.
  - Returns `<root>\usr\bin\<name>.exe` when it is a file.
- Produces: `pub fn not_found(program: &str) -> String` in `exec.rs`.
  - For a built-in name: `"command not found: {p} (the qx built-in does not support these arguments, and no {p}.exe is on PATH or in Git for Windows usr\\bin)"`.
  - Otherwise: `"command not found: {p}"`.
- `resolve()`: when the PATH lookup misses, it tries `git_coreutil` and classifies the hit.
- `git_root(cmd)` also recognizes `<root>\usr\bin\x.exe`, so the existing PATH prepend applies.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn coreutil_fallback_is_limited_to_coreutils() {
    assert!(git_coreutil("git").is_none() && git_coreutil("notepad").is_none());
}
#[test] fn not_found_names_the_builtin_limit() {
    assert!(crate::exec::not_found("wc").contains("qx built-in"));
    assert_eq!(crate::exec::not_found("zzz-nope"), "command not found: zzz-nope");
}
#[cfg(windows)] #[test] fn a_declined_builtin_reaches_git_coreutils() { // mcp tests; skip if git_coreutil("wc") is None
    // cwd with a.txt "1\n" and b.txt "1\n2\n"; call {"program":"wc","args":["-l","a.txt","b.txt"],"cwd":d}
    // assert exit_code 0, stdout contains "total" and "3"
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --release coreutil not_found declined_builtin`
Expected: compile error `cannot find function git_coreutil`.

- [ ] **Step 3: Implement `git_coreutil`, the `resolve` fallback, `not_found`, and the `git_root` `usr\bin` case**

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --release`
Expected: PASS.

- [ ] **Step 5: Update README §1 (fallback to Git `usr\bin`) and commit**

```bash
git add src/resolve.rs src/exec.rs src/mcp.rs README.md
git commit -m "builtins: fall back to Git for Windows coreutils when no binary is on PATH"
```

---

### Task 4: Python — any console-script launcher and any venv in one process (D3, D4, R3, R4)

**Files:**
- Create: `src/python.rs`
- Modify: `src/resolve.rs` (`classify` at 478-510: replace `python_console_script` + `python_minor`, deleting both and their test at 1015-1029); `src/exec.rs` (Direct arm at 434-472: venv redirect, delete the `prefix_m`/`PYTHONSAFEPATH` lines 446 and 461-464); `src/main.rs` (`mod python;`, plus `which` printing ` [venv → <base>]`)
- Test: `src/python.rs` `mod tests`

**Interfaces:**
- Produces: `pub fn launcher_interpreter(exe: &Path) -> Option<PathBuf>`.
  - Runs only when the parent dir is named `Scripts` (ASCII case-insensitive) and the file is ≤ 2 MiB.
  - Returns the absolute existing `python.exe` the launcher would run. Never `pythonw.exe`, never a relative path.
- Produces: `fn shebang_interpreter(bytes: &[u8]) -> Option<String>`, the pure parser.
- Produces: `pub struct VenvRedirect { pub base: PathBuf, pub launcher: PathBuf }` and `pub fn venv_redirect(python: &Path) -> Option<VenvRedirect>`, which delegates to `fn venv_redirect_with(python: &Path, env: impl Fn(&str) -> Option<String>) -> Option<VenvRedirect>`.
- Produces: `fn parse_pyvenv(text: &str) -> Option<(PathBuf, (u32, u32))>`. Reads `home` plus `version_info`, or failing that `version`.
- `classify`: `launcher_interpreter(&p)` → `Target::Direct { exe: interpreter, prefix: vec![p] }`. This is exact launcher semantics: Python runs the `.exe` as a zipapp, so `sys.argv[0]` is the launcher path and `sys.path[0]` is the launcher.
- Direct arm: when `venv_redirect(&exe)` is Some, spawn `base` and set `__PYVENV_LAUNCHER__=launcher`; `apply_python_fast` still applies.

`shebang_interpreter` algorithm (the signature does not determine it). Fail closed at every step:
1. Find the last `PK\x05\x06`. Read entries (u16 @+10, must be 1), cd_size (u32 @+12) and cd_offset (u32 @+16).
2. Compute `zip_start = eocd - cd_size - cd_offset`. It must be ≥ 0 and hold `PK\x03\x04`.
3. In the local header, the method (u16 @+8) must be 0 and the name (len u16 @+26, extra u16 @+28) must be `__main__.py`. The data follows, sized by u32 @+18.
4. If the data starts with `#!`, its first line is the shebang (uv). Otherwise take the last `#!` within the 1 KiB before `zip_start`, whose line ends at `zip_start` (pip/distlib).
5. Trim `\r\n`, strip one pair of `"` quotes, and require a drive-absolute path whose file name is `python.exe`.

venv conditions:
- `QX_PYTHON_VENV` ≠ `off`, and neither `PYTHONHOME` nor `__PYVENV_LAUNCHER__` is set in qx's environment.
- The file name is `python.exe`, its parent is `Scripts`, and `<parent.parent>\pyvenv.cfg` parses to version ≥ 3.11.
- `<home>\python.exe` is a file.

- [ ] **Step 1: Write the failing tests**

```rust
// helper: fn stored_zip(name: &str, data: &[u8]) -> Vec<u8> — local header, data, central dir, EOCD; offsets relative to zip start, crc 0
#[test] fn pip_layout() {
    let mut f = b"MZstub".to_vec(); f.extend(b"#!C:\\Py312\\python.exe\r\n");
    f.extend(stored_zip("__main__.py", b"import sys\nfrom pytest import console_main\n"));
    assert_eq!(shebang_interpreter(&f).as_deref(), Some("C:\\Py312\\python.exe"));
}
#[test] fn uv_layout_with_trailing_resources() {
    let mut f = b"MZstub".to_vec();
    f.extend(stored_zip("__main__.py", b"#!C:\\v\\Scripts\\python.exe\n# -*- coding: utf-8 -*-\nimport sys\n"));
    f.extend(b"<?xml version=\"1.0\"?><assembly/>PADDINGXXPADDING");
    assert_eq!(shebang_interpreter(&f).as_deref(), Some("C:\\v\\Scripts\\python.exe"));
}
#[test] fn quoted_shebang_is_unquoted() { /* #!"C:\Program Files\Py\python.exe" -> C:\Program Files\Py\python.exe */ }
#[test] fn rejects_unknown_shapes() { /* two entries; method 8; entry "x.py"; pythonw.exe; "#!python.exe"; no EOCD -> None each */ }
#[test] fn stale_shebang_keeps_the_launcher() {
    // temp <d>\Scripts\tool.exe = pip layout naming <d>\gone\python.exe (absent)
    // launcher_interpreter == None; resolve::resolve(<path>, None) is Target::Direct{exe: tool.exe, prefix: []}
}
#[test] fn parses_pyvenv_variants() {
    assert_eq!(parse_pyvenv("home = C:\\H\nversion_info = 3.12.1.final.0\n"), Some((PathBuf::from("C:\\H"), (3, 12))));
    assert_eq!(parse_pyvenv("home = C:\\H\nuv = 0.11.21\nversion_info = 3.11.9\n").unwrap().1, (3, 11));
    assert_eq!(parse_pyvenv("home = C:\\H\nversion = 3.10.4\n").unwrap().1, (3, 10));
}
#[test] fn venv_redirect_skips_when_pythonhome_or_launcher_env_set() {
    // fixture: <v>\Scripts\python.exe, <v>\pyvenv.cfg (home=<h>, 3.12), <h>\python.exe
    // with env none -> Some{base: <h>\python.exe, launcher: <v>\Scripts\python.exe}
    // env PYTHONHOME set -> None; env __PYVENV_LAUNCHER__ set -> None; QX_PYTHON_VENV=off -> None; version 3.10 -> None
}
#[cfg(windows)] #[test] fn venv_redirect_matches_the_redirector() {
    // skip unless `python -m venv --without-pip <tmp>` succeeds
    // PROBE = "import sys;print(sys.prefix);print(sys.executable);print(sys.path)"
    // stdout of <tmp>\Scripts\python.exe -c PROBE == stdout of exec::build_command_at(<that path>, ["-c", PROBE], None)
    // and that Command's program is the base interpreter, not <tmp>\Scripts\python.exe
}
#[cfg(windows)] #[test] fn a_real_pip_launcher_runs_as_one_process() {
    // skip unless resolve::find_on_path("pytest") is a Scripts\pytest.exe
    // resolve("pytest") is Direct{exe: ...python.exe, prefix: [..\pytest.exe]}; running it with --version exits 0
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --release python::`
Expected: compile error `unresolved module python`.

- [ ] **Step 3: Implement `src/python.rs`, swap `classify` to it, add the venv redirect to the Direct arm, and extend `which`**

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --release`
Expected: PASS. `target\release\qx.exe which pytest` prints `pytest -> …\python.exe …\Scripts\pytest.exe`. `target\release\qx.exe which hermes` prints the base Python 3.11 with `[venv → …]`.

- [ ] **Step 5: Update README §3 (replace the 14-name list with launcher/venv rules, `QX_PYTHON_VENV`) and commit**

```bash
git add src/python.rs src/resolve.rs src/exec.rs src/main.rs README.md
git commit -m "python: run any pip/uv launcher and any venv interpreter as one process"
```

---

### Task 5: npm scripts without `cmd.exe` (D5, R5)

**Files:**
- Modify: `src/exec.rs` (extract the `match resolve::resolve(...)` body of `build_command_at`, lines 434-502, into `command_for`); `src/resolve.rs` (new `resolve_in`); `src/npm.rs` (`commands` at 57-74, `bin_path` at 405-432)
- Test: `src/npm.rs` `mod tests`, `src/mcp.rs` tests

**Interfaces:**
- Produces: `pub fn command_for(program: &str, target: Target, args: &[String]) -> Result<Command, String>` in `exec.rs`. `build_command_at` now calls it, so behavior is unchanged.
- Produces: `pub fn resolve_in(program: &str, first: &[PathBuf], cwd: Option<&Path>) -> Target` in `resolve.rs`. Tries `find_in_dir` over `first` (no cache), then `resolve(program, cwd)`.
- Produces in `npm.rs`:
  - `fn local_bins(root: &Path) -> Vec<PathBuf>`, the `node_modules\.bin` dirs from root upward, factored out of `bin_path`.
  - `const CMD_INTERNALS: &[&str]` = `assoc break call cd chcp chdir cls color copy date del dir echo endlocal erase exit for ftype goto if md mkdir mklink move path pause popd prompt pushd rd rem ren rename rmdir set setlocal shift start time title type ver verify vol`.
  - `fn direct_step(f: &Fast, s: &Step) -> Option<Command>`: requires `exec::split_simple(&s.command)` to be Some, a program not in `CMD_INTERNALS`, and `resolve_in(prog, &local_bins(&f.cwd), Some(&f.cwd))` to be `Target::Direct`.
  - `fn decorate(c: &mut Command, f: &Fast, s: &Step)` applies today's cwd, PATH, env, lifecycle, child-default and node-cache settings to both kinds of command.
- `commands()`: each step tries `direct_step`, else today's `build_shell(…, "cmd")`.

- [ ] **Step 1: Write the failing tests**

```rust
const STOCK_SHIM: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\..\\tool\\cli.js\" %*\r\n";
#[test] fn a_plain_script_runs_its_local_bin_without_cmd() { // skip if find_on_path("node") is None
    // root/package.json {"scripts":{"build":"tool --flag"}}; node_modules/.bin/tool.cmd = STOCK_SHIM; node_modules/tool/cli.js
    // let c = &commands(&fast(&["run".into(),"build".into()], Some(&root)).unwrap()).unwrap()[0];
    // file name of c.get_program() == "node.exe"; args contain a path ending "cli.js" and "--flag"
}
#[test] fn shell_syntax_cmd_internals_and_logic_shims_keep_cmd() {
    // scripts: "a":"tool && tool", "b":"echo hi", "c":"no-such-tool-zz x", "d":"logic" where .bin/logic.cmd sets a variable
    // each first command's program file name == "cmd.exe"
}
#[test] fn qx_npm_run_plain_script_end_to_end() { // mcp tests; skip without node
    // package.json {"scripts":{"v":"node --version"}}; call {"program":"npm","args":["run","v"],"cwd":d}
    // exit_code 0, stdout starts with "v", source == "direct"
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --release npm::tests:: qx_npm_run_plain`
Expected: `a_plain_script_runs_its_local_bin_without_cmd` FAILS (program is `cmd.exe`).

- [ ] **Step 3: Implement `command_for`, `resolve_in`, `local_bins`, `direct_step` and `decorate`**

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --release`
Expected: PASS (existing pre/post-hook and npmrc tests included).

- [ ] **Step 5: Update README §3 npm bullet (scripts run without cmd.exe when plain) and commit**

```bash
git add src/exec.rs src/resolve.rs src/npm.rs src/mcp.rs README.md
git commit -m "npm: run plain scripts directly through the unwrapped local bin"
```

---

### Task 6: `pnpm run/test/start` fast path (D6, R6)

**Files:**
- Modify: `src/npm.rs` (`direct` at 78-91, `fast` at 161-251, `identity` at 256-283, `npmrc_overrides` at 368-403)
- Test: `src/npm.rs` `mod tests`

**Interfaces:**
- Consumes: Task 5's `commands()`/`decorate`.
- Produces: `pub fn is_pnpm(program: &str) -> bool` (`pnpm`, `pnpm.cmd`, `pnpm.exe`).
- Produces: `pub fn fast_pnpm(args: &[String], cwd: Option<&Path>) -> Option<Fast>`. `fast()` (npm) stays; both share `fn fast_with(pm: Pm, args, cwd)` with `enum Pm { Npm, Pnpm { version: String } }`.
- `direct()`: `if is_pnpm(program) { return commands(&fast_pnpm(args, cwd)?).ok(); }`

pnpm conditions (all required, else None):
- Same `QX_NPM_FAST` and env denylist as npm; `parse(args)` is Some.
- `packageManager` is `pnpm@<ver>[+…]`; the version is the text between `@` and `+`.
- No `pnpm-workspace.yaml` in the root or any ancestor, and no `workspaces` key.
- No `pre<name>` or `post<name>` script.
- `npmrc_overrides` gets the extra keys `shell-emulator` and `enable-pre-post-scripts` (shared list).

pnpm env:
- `npm_lifecycle_event` and `npm_lifecycle_script` (via `decorate`).
- `npm_package_name`, `npm_package_version`, `npm_package_json`, `INIT_CWD`, and `PNPM_SCRIPT_SRC_DIR=<root>`.
- `npm_config_user_agent = "pnpm/{ver} npm/? node/{node} win32 x64"`.
- `npm_execpath` = the resolved pnpm entry (`resolve("pnpm")` Direct `prefix[0]`).
- `npm_node_execpath` and `NODE` = node.
- No `npm_command` or `npm_config_local_prefix`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn pnpm_fast_path_requires_a_pinned_pnpm() {
    // package.json {"packageManager":"pnpm@9.1.0+sha512.abc","scripts":{"build":"tool"}}
    // let f = fast_pnpm(&["run".into(),"build".into()], Some(&d)).unwrap();
    // f.env contains ("PNPM_SCRIPT_SRC_DIR", d); if node is found, npm_config_user_agent starts_with "pnpm/9.1.0 "
}
#[test] fn pnpm_falls_back_on_workspaces_hooks_and_unpinned() {
    // no packageManager -> None; "npm@10.0.0" -> None; scripts.prebuild present -> None;
    // pnpm-workspace.yaml in the parent dir -> None; .npmrc "shell-emulator=true" -> None
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --release pnpm_`
Expected: compile error `cannot find function fast_pnpm`.

- [ ] **Step 3: Implement `Pm`, `fast_with`, `fast_pnpm`, `is_pnpm` and the pnpm identity env**

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --release`
Expected: PASS.

- [ ] **Step 5: Update README §3 (pnpm bullet with its fail-closed rules) and commit**

```bash
git add src/npm.rs README.md
git commit -m "npm: pnpm run/test/start fast path for pinned, non-workspace projects"
```

---

### Task 7: `qx deploy` — one canonical binary, replaceable while running (D1, R1)

**Files:**
- Create: `src/deploy.rs`
- Modify: `src/main.rs` (`mod deploy;`, `"deploy" => deploy::run(&args[1..])` in `dispatch`, help text)
- Test: `src/deploy.rs` `mod tests`

**Interfaces:**
- Produces: `pub fn canonical() -> Option<PathBuf>`, per Global Constraints.
- Produces: `#[derive(Debug, PartialEq)] pub enum Installed { Unchanged, Replaced { aside: Option<PathBuf> } }`.
- Produces: `pub fn install(src: &Path, dst: &Path) -> std::io::Result<Installed>`.
  - Unchanged when the canonical paths are equal or `same_bytes`.
  - Otherwise: copy to `<dst>.new-<pid>`; rename an existing dst to `<dst>.old-<unix_ms>`; rename new → dst. If the last rename fails, rename the aside back.
- Produces: `pub fn same_bytes(a: &Path, b: &Path) -> std::io::Result<bool>`, comparing sizes, then streaming in 64 KiB chunks.
- Produces: `pub fn sweep_aside(dst: &Path) -> usize`. Deletes `<file name>.old-*` siblings, ignores in-use failures, and returns the count removed.
- Produces: `pub fn run(args: &[String]) -> i32`. Flags `--to <path>`, `--dry-run`; source is `std::env::current_exe()`. Prints one of:
  - `qx deploy: up to date (<dst>)`
  - `qx deploy: installed <dst> (previous copy kept as <aside> until running clients exit)`
  - the planned action, for `--dry-run`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn install_copies_then_is_idempotent() { /* src "A" -> dst absent: Replaced{aside:None}, dst == "A"; again: Unchanged */ }
#[test] fn dry_run_writes_nothing() { /* run(["--to", dst, "--dry-run"]) == 0 and dst still absent */ }
#[cfg(windows)] #[test] fn install_replaces_a_running_exe() {
    // copy C:\Windows\System32\PING.EXE to <d>\qx.exe; spawn it "-n 6 127.0.0.1"
    // install(<copy of whoami.exe>, <d>\qx.exe) == Replaced{aside: Some(p)} with p existing; child.try_wait() is Ok(None)
    // kill + wait child; sweep_aside(<d>\qx.exe) == 1; p no longer exists; <d>\qx.exe has whoami's bytes
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --release deploy::`
Expected: compile error `unresolved module deploy`.

- [ ] **Step 3: Implement `src/deploy.rs` and the dispatch arm**

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --release`
Expected: PASS.

- [ ] **Step 5: Replace README "Build & install" with `cargo build --release; target\release\qx.exe deploy` and commit**

```bash
git add src/deploy.rs src/main.rs README.md
git commit -m "deploy: install the canonical qx.exe, replacing it even while MCP servers run"
```

---

### Task 8: `qx setup` framework, byte-preserving JSON(C) editor, Claude Code + OpenCode, doctor `[clients]` (D11, D13, R2, R10, R13)

**Files:**
- Create: `src/setup/mod.rs`, `src/setup/jsonc.rs`
- Modify: `src/main.rs` (`mod setup;`, `"setup" => setup::run(&args[1..])`); `src/doctor.rs` (`run` at 21: add `[install]` and `[clients]` sections)
- Test: `src/setup/jsonc.rs`, `src/setup/mod.rs` `mod tests`

**Interfaces:**
- Consumes: `client::Client` and `Client::key()` (Task 2); `deploy::canonical()` and `deploy::same_bytes()` (Task 7).
- Produces in `setup/mod.rs`:
  - `pub struct Paths { pub home, pub local_app_data, pub dsh_home, pub hermes_home, pub codex_home: PathBuf }`. `Paths::from_env()` defaults: `DSH_HOME` → `home\.dsh`; `HERMES_HOME` → `local_app_data\hermes`; `CODEX_HOME` → `home\.codex`.
  - `#[derive(Clone, Copy, PartialEq)] pub enum Action { Install, Remove }`
  - `pub enum Edit { Unchanged, Changed(String), Refuse(String) }`. `Refuse` carries the reason plus the paste-ready snippet.
  - `pub struct Change { pub client: Client, pub file: PathBuf, pub edit: Edit }`
  - `pub fn plan(c: Client, p: &Paths, exe: &Path, a: Action) -> Result<Vec<Change>, String>`. One `Change` per file; pi's extension path yields two.
  - `pub fn apply(ch: &Change) -> std::io::Result<()>`. Only for `Changed`: create parent dirs, copy the old file to `<file>.qx-bak`, write `<file>.qx-tmp-<pid>`, then rename over the file.
  - `pub struct Status { pub client: Client, pub file: Option<PathBuf>, pub command: Option<String>, pub notes: Vec<String> }` and `pub fn status(c: Client, p: &Paths) -> Status`
  - `pub fn run(args: &[String]) -> i32`, for `qx setup [all|<key>…] [--dry-run] [--remove]`. Prints `<key>: unchanged|updated <file>|refused: <reason>` per client; exits 1 if any refusal.
  - Shared rule: each client editor reads the existing qx entry's env map, keeps its keys, and sets `QX_CLIENT`.
- Produces in `setup/jsonc.rs` (JSONC-aware: strings and escapes, `//` and `/* */` comments, trailing commas):
  - `pub fn member(text: &str, path: &[&str]) -> Result<Option<std::ops::Range<usize>>, String>`, the byte range of the value.
  - `pub fn set_member(text: &str, path: &[&str], value: &serde_json::Value) -> Result<String, String>`. Replaces the value, or inserts the member, creating missing parent objects. An inserted member goes on its own line at the sibling indentation (else parent + 2), pretty-printed and re-indented.
  - `pub fn remove_member(text: &str, path: &[&str]) -> Result<String, String>`
  - `pub fn to_strict_json(text: &str) -> String`, which drops comments and trailing commas. Every edit's output must re-parse through `serde_json` after this.
- Claude Code: file `home\.claude.json`, path `["mcpServers","qx"]`, value per the matrix.
- OpenCode file choice:
  - Use whichever of `opencode.json`/`opencode.jsonc` already has `mcp.servers.qx`; else the first that exists; else create `opencode.json`.
  - Path `["mcp","servers","qx"]`; value per the matrix.
  - Refuse when that `mcp` object holds a legacy entry `mcp.qx` that has `type`.
- doctor `[install]`: the canonical path, whether it is `same_bytes` with the running exe, and the count of stale `dsh_home\bin\qx*.exe` files.
- doctor `[clients]`: one `status` line per `Client::ALL`, plus notes:
  - the command differs from canonical;
  - OpenCode has both files, or `codemode` is not false;
  - `~/.claude.json` has project-scoped qx entries pointing elsewhere;
  - pi < 0.99 needs the extension;
  - Hermes has `tool_search` set to `auto`.

- [ ] **Step 1: Write the failing tests**

```rust
// jsonc.rs
#[test] fn set_member_preserves_unrelated_bytes() {
    let src = "{\n  // keep me\n  \"a\": 1, /* and me */\n  \"mcp\": {\"servers\": {\"other\": {\"x\": 1},}},\n}\n";
    let out = set_member(src, &["mcp","servers","qx"], &json!({"type":"local"})).unwrap();
    assert!(out.starts_with("{\n  // keep me\n  \"a\": 1, /* and me */\n"));
    assert!(out.contains("\"other\": {\"x\": 1}"));
    let v: Value = serde_json::from_str(&to_strict_json(&out)).unwrap();
    assert_eq!(v["mcp"]["servers"]["qx"]["type"], "local");
}
#[test] fn set_member_creates_parents_and_replaces_in_place() {
    // "{}" + ["mcpServers","qx"] -> parses with mcpServers.qx; replacing an existing qx value leaves the text before and after its range identical
}
#[test] fn malformed_text_is_an_error() { /* "{\"a\": " -> Err */ }
// mod.rs
#[test] fn claude_install_is_idempotent_and_keeps_env() {
    // .claude.json with mcpServers.qx {command: old, env: {QX_GUARD: "safe"}} -> Changed; the new entry has command = exe,
    // env == {QX_GUARD: "safe", QX_CLIENT: "claude-code"}; planning again on the result -> Unchanged
}
#[test] fn opencode_sets_codemode_false_in_the_file_that_has_qx() {
    // both opencode.json (with mcp.servers.qx) and opencode.jsonc exist -> Change.file is opencode.json; codemode == false; timeout.execution == 600000
}
#[test] fn opencode_refuses_unknown_shapes() { /* "mcp": {"qx": {"type":"local", ...}} legacy entry -> Edit::Refuse containing "\"servers\"" */ }
#[test] fn remove_deletes_only_the_qx_entry() { /* Remove on claude + opencode fixtures; other servers byte-identical */ }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --release setup::`
Expected: compile error `unresolved module setup`.

- [ ] **Step 3: Implement `jsonc.rs`, the framework, the two JSON clients and the doctor sections**

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --release`
Expected: PASS. `target\release\qx.exe doctor` shows `[clients]`, with claude-code and opencode flagged as pointing at a non-canonical binary.

- [ ] **Step 5: Add README "MCP setup → qx setup" (usage, files touched, `.qx-bak`) and commit**

```bash
git add src/setup src/main.rs src/doctor.rs README.md
git commit -m "setup: byte-preserving config editor; Claude Code and OpenCode; doctor reports clients and drift"
```

---

### Task 9: Codex (`config.toml`)

**Files:**
- Create: `src/setup/toml.rs`
- Modify: `src/setup/mod.rs` (route `Client::Codex`)
- Test: `src/setup/toml.rs` `mod tests`

**Interfaces:**
- Produces: `pub fn edit_codex(text: Option<&str>, exe: &Path, a: Action) -> Edit`.
- Produces: `pub fn codex_command(text: &str) -> Option<String>` (for `status`).
- Block rules, line-based:
  - The qx block starts at a header line equal to `[mcp_servers.qx]` or `[mcp_servers."qx"]`, after trim.
  - It runs to the line before the next `[` header that is not `[mcp_servers.qx.` (subtables such as `.env` belong to the block).
  - Install replaces the block, or appends it after one blank line, with exactly the matrix keys. Paths are TOML literal strings (`'…'`); a path containing `'` uses a basic string with `\\` escapes.
  - Existing `[mcp_servers.qx.env]` `KEY = "value"` lines are kept.
- Refuse when the text has `mcp_servers = {`, or `qx = {` inside a `[mcp_servers]` table.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn codex_appends_and_preserves_unrelated_bytes() {
    let src = "# >>> caveman\nmodel = \"x\"\n\n[mcp_servers.caveman]\ncommand = 'c'\n# --- end\n";
    let Edit::Changed(out) = edit_codex(Some(src), Path::new(r"C:\q\qx.exe"), Action::Install) else { panic!() };
    assert!(out.starts_with(src));
    assert!(out.contains("[mcp_servers.qx]\ncommand = 'C:\\q\\qx.exe'\nargs = [\"mcp\"]\nstartup_timeout_sec = 20\ntool_timeout_sec = 600\n"));
    assert!(out.contains("[mcp_servers.qx.env]\nQX_CLIENT = \"codex\"\n"));
}
#[test] fn codex_replaces_its_block_and_keeps_env() { /* existing block with env QX_GUARD = "safe" + a later [profiles.x] table -> both kept, command updated; second run Unchanged */ }
#[test] fn codex_refuses_unknown_shapes() { /* "mcp_servers = { qx = { command = 'x' } }" -> Refuse */ }
#[test] fn codex_remove() { /* removes [mcp_servers.qx] and [mcp_servers.qx.env] only */ }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --release setup::toml::`
Expected: compile error `cannot find function edit_codex`.

- [ ] **Step 3: Implement `src/setup/toml.rs` and route Codex**

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --release`
Expected: PASS.

- [ ] **Step 5: Add the Codex line to README setup section and commit**

```bash
git add src/setup README.md
git commit -m "setup: Codex config.toml"
```

---

### Task 10: dsh (`cordis.patch.yml`) and Hermes (`config.yaml`), plus `.dsh\bin` cleanup

**Files:**
- Create: `src/setup/yaml.rs`
- Modify: `src/setup/mod.rs` (route `Client::Dsh`, `Client::Hermes`; after a successful dsh apply, delete `dsh_home\bin\qx*.exe` that no `cordis.patch.yml` under `dsh_home` references, ignoring in-use failures)
- Test: `src/setup/yaml.rs` `mod tests`

**Interfaces:**
- Produces: `fn block_end(lines: &[&str], start: usize, indent: usize) -> usize`. Returns the first later non-blank, non-comment line with indent ≤ `indent`.
- Produces: `pub fn edit_dsh(text: Option<&str>, exe: &Path, a: Action) -> Edit`.
  - The item is the line whose trimmed form is `- id: mcp-qx`.
  - Install replaces that item's `command:` value with `'<exe with / separators>'`.
  - It sets `QX_CLIENT: dsh` in its block `env:` (replacing an existing line, else inserting the first child), and leaves every other line alone.
  - With no item, it appends a new top-level `- insert:` entry holding the full matrix item.
  - Refuse on a flow-style non-empty `env: {…}` or a missing `config:`.
- Produces: `pub fn edit_hermes(text: Option<&str>, exe: &Path, a: Action) -> Edit`.
  - Uses the top-level `mcp_servers:` (indent 0); child indent comes from its first child, else 2.
  - Replaces the child `qx:` block, or appends after the last child, or appends `mcp_servers:` at EOF.
  - Generated block, with exe as a single-quoted scalar: `qx:` / `command: '<exe>'` / `args: [mcp]` / `env:` / `QX_CLIENT: hermes` / `timeout: 600` / `connect_timeout: 60`.
  - Existing child keys under `qx.env` are kept.
  - Refuse on `mcp_servers: {` with content, or tabs in indentation.
- Produces: `pub fn dsh_command(text: &str) -> Option<String>` and `pub fn hermes_command(text: &str) -> Option<String>` (for `status`).

- [ ] **Step 1: Write the failing tests**

```rust
const DSH: &str = "# Global dsh patch\n- insert:\n    - id: mcp-qx\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: qx\n        transport: stdio\n        command: 'C:/Users/USER/.dsh/bin/qx-75ce61d91cba.exe'\n        args:\n          - mcp\n        env:\n          QX_PYTHON_FAST: tls\n        toolCallTimeoutMs: 300000\n        failOnStartupError: true\n";
#[test] fn dsh_repoints_command_and_tags_env_only() {
    let Edit::Changed(out) = edit_dsh(Some(DSH), Path::new(r"C:\Users\USER\.cargo\bin\qx.exe"), Action::Install) else { panic!() };
    assert!(out.contains("        command: 'C:/Users/USER/.cargo/bin/qx.exe'\n"));
    assert!(out.contains("        env:\n          QX_CLIENT: dsh\n          QX_PYTHON_FAST: tls\n"));
    assert_eq!(out.lines().count(), DSH.lines().count() + 1);
}
#[test] fn dsh_appends_when_absent_and_refuses_flow_env() { /* "" -> contains "- id: mcp-qx"; "env: {A: b}" -> Refuse */ }
#[test] fn hermes_adds_qx_beside_other_servers_preserving_bytes() {
    // "model: x\nmcp_servers:\n  codegraph:\n    command: cg\n    timeout: 120\nplatform_toolsets:\n  cli: [mcp-codegraph]\n"
    // -> codegraph block and platform_toolsets byte-identical; "  qx:\n    command: 'C:\\q\\qx.exe'\n    args: [mcp]\n" present; second run Unchanged
}
#[test] fn hermes_refuses_unknown_shapes() { /* "mcp_servers: {codegraph: {command: cg}}" -> Refuse; tab indentation -> Refuse */ }
#[test] fn yaml_remove() { /* dsh: drops the mcp-qx item; hermes: drops the qx child only */ }
#[test] fn dsh_cleanup_deletes_only_unreferenced_copies() {
    // dsh_home/bin has qx.exe, qx-aaa.exe, qx-bbb.exe; cordis.patch.yml references qx-bbb.exe -> after cleanup only qx-bbb.exe remains
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --release setup::yaml::`
Expected: compile error `cannot find function edit_dsh`.

- [ ] **Step 3: Implement `src/setup/yaml.rs`, route both clients, add the dsh cleanup**

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --release`
Expected: PASS.

- [ ] **Step 5: Add the dsh/Hermes lines to the README setup section (note: Hermes ignores server instructions; tool_search may hide qx) and commit**

```bash
git add src/setup README.md
git commit -m "setup: dsh cordis.patch.yml and Hermes config.yaml; prune stale dsh qx copies"
```

---

### Task 11: pi — built-in MCP when ≥ 0.99, a qx extension otherwise

**Files:**
- Create: `integrations/pi/qx.js`, `integrations/pi/qx/bridge.js`, `integrations/pi/smoke.mjs`, `src/setup/pi.rs`
- Modify: `src/setup/mod.rs` (route `Client::Pi`)
- Test: `src/setup/pi.rs` `mod tests`; `node integrations/pi/smoke.mjs`

**Interfaces:**
- Produces: `pub fn pi_version(p: &Paths) -> Option<(u32, u32)>`. Reads `package.json` `version` from the package dir reached by `resolve::resolve("pi")`'s Direct `prefix[0]` (the walk-up to the dir that holds `package.json` with name `@earendil-works/pi-coding-agent`).
- Produces: `pub fn edit_pi(version: Option<(u32, u32)>, p: &Paths, exe: &Path, a: Action) -> Vec<(PathBuf, Edit)>`.
  - ≥ (0,99): `home\.pi\agent\mcp.json` via `jsonc::set_member(["mcpServers","qx"], matrix value)`.
  - Otherwise: two files, with `__QX_EXE__` replaced by the JSON-string-escaped exe:
    - `home\.pi\agent\extensions\qx.js` = `include_str!("../../integrations/pi/qx.js")`
    - `home\.pi\agent\extensions\qx\bridge.js` = `include_str!("../../integrations/pi/qx/bridge.js")`
  - `None` version: Refuse "pi not found".
- `integrations/pi/qx/bridge.js`:
  - `export class QxBridge { constructor(exe: string, env?: object); call(tool: "run"|"fs", args: object, signal?: AbortSignal): Promise<{text: string, isError: boolean, structured: object|undefined}>; close(): void }`
  - Spawns `exe mcp` lazily once, with `QX_CLIENT=pi`. Sends `initialize` with `clientInfo {name:"pi", version:"qx-bridge"}`, then `notifications/initialized`.
  - Newline-delimited JSON-RPC with incrementing ids and a pending map. An abort sends `notifications/cancelled`. Child exit rejects all pending calls; the next call respawns.
- `integrations/pi/qx.js`:
  - Imports `{ Type } from "typebox"` statically (the same pattern as the working `caveman-native.js`), and `QxBridge` from `./qx/bridge.js`.
  - `registerTool` for `mcp__qx__run` and `mcp__qx__fs`, with `Type.Object` parameters mirroring `run_schema`/`fs_schema` input properties. Descriptions come from Task 2's Pi wording.
  - `execute`: `isError` → `throw new Error(text)`; else return `{ content: [{type:"text", text}], details: structured ?? {} }`.
- `integrations/pi/smoke.mjs <exe>`: calls `run {program:"pwd"}` and `fs {op:"stat", cwd:".", path:"."}`, asserts neither is an error, prints `pi bridge ok`, exits 0.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn pi_new_versions_get_mcp_json() { /* edit_pi(Some((1,3)), tmp, exe, Install): one Change for .pi/agent/mcp.json with exposure "direct", timeout 600, env.QX_CLIENT "pi" */ }
#[test] fn pi_old_versions_get_the_extension() {
    // edit_pi(Some((0,85)), ...): Changes for extensions/qx.js and extensions/qx/bridge.js; qx.js contains "registerTool" and "mcp__qx__run";
    // bridge.js contains the escaped exe ("C:\\\\q\\\\qx.exe") and no "__QX_EXE__"; second run -> Unchanged
}
#[test] fn pi_missing_is_refused() { /* edit_pi(None, ...) -> single Refuse containing "pi not found" */ }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --release setup::pi::`
Expected: compile error `cannot find function edit_pi`.

- [ ] **Step 3: Write `bridge.js`, `qx.js`, `smoke.mjs`, and implement `src/setup/pi.rs`**

- [ ] **Step 4: Verify both halves**

Run: `cargo test --release` → PASS.
Run: `cargo build --release; node integrations/pi/smoke.mjs target\release\qx.exe` → prints `pi bridge ok`, exit 0.

- [ ] **Step 5: Add the pi line to README setup section and commit**

```bash
git add integrations src/setup README.md
git commit -m "setup: pi via built-in MCP (>=0.99) or a qx bridge extension"
```

---

### Task 12: Release 0.7.0 and roll out on this machine

**Files:**
- Modify: `Cargo.toml` (`version = "0.7.0"`); `README.md` ("Measured on this machine" table refreshed with the numbers below)

- [ ] **Step 1: Bump the version, build, deploy**

Run: `cargo test --release; cargo build --release; target\release\qx.exe deploy`
Expected: tests PASS; then `qx deploy: installed C:\Users\USER\.cargo\bin\qx.exe …`; `qx --version` prints `qx 0.7.0`.

- [ ] **Step 2: Preview, then apply client registration**

Run: `qx setup all --dry-run`, review every planned change with the user, then `qx setup all`.
Expected:
- `updated` for claude-code, codex, opencode, hermes, dsh, pi (or `unchanged`); no `refused`.
- `.qx-bak` files exist beside each edited config.
- the unreferenced `.dsh\bin\qx*.exe` copies are gone (any still in use are reported).

- [ ] **Step 3: Doctor shows no drift**

Run: `qx doctor`
Expected: `[install]` canonical matches the running exe and reports 0 stale copies. `[clients]` lists all six as registered → `C:\Users\USER\.cargo\bin\qx.exe` (dsh after restart).

- [ ] **Step 4: Restart each client and confirm the connection**

Run in turn:
- `claude mcp list`, `codex mcp list`, `opencode mcp list`, `hermes mcp list`: each shows `qx` connected/enabled.
- Restart dsh: the `mcp__qx__run` tool is available, and its instructions name `pwsh`.
- Start pi: `mcp__qx__run` appears in its tool list.

Expected: qx connected in all six. On a pi load error naming `typebox` or `./qx/bridge.js`, stop and report it; do not edit the extension blind.

- [ ] **Step 5: Re-measure against the diagnosis**

Run, each through PowerShell `Measure-Command`, median of 7:
- `qx --raw python -c pass`
- `qx --raw pytest --version`
- `qx which hermes`
- the MCP EOF-with-running-call script from the spec

Expected:
- `python -c pass` ≤ 120 ms (was 539)
- `pytest --version` ≤ 350 ms (was 718 raw)
- `which hermes` shows one base-python process with `[venv → …]`
- the MCP server exits ≤ 3.5 s after EOF (was 7.1 s with a 7 s call; unbounded before)

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock README.md
git commit -m "qx 0.7.0: measured speedups, qx deploy, qx setup for six agent CLIs"
```
