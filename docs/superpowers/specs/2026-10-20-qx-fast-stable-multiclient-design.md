# qx 0.7 — faster npm/Python, a steadier MCP server, every agent CLI

Status: design approved for planning · Baseline: qx 0.6.0 (`e9082e3` + uncommitted review patches)

## 1. Diagnosis (measured 2026-10-20 on the target machine)

Machine: Windows 11, 28 threads, Avast + Malwarebytes + Bitdefender + Defender.
Process creation is currently healthy (`cmd /d /c exit` 24-35 ms); on a bad day
`qx doctor` has measured 593 ms per cold spawn, so every avoided process is worth
25-600 ms.

| # | finding | evidence | impact |
|---|---|---|---|
| D1 | **Deployment drift.** Five qx binaries exist; the one on PATH (`~/.cargo/bin/qx.exe`, Oct 19, 990 KB) predates the Python fix. dsh runs `~/.dsh/bin/qx-75ce61d91cba.exe`; four stale hashed copies sit beside it. | `qx --raw python -c pass`: stale **539 ms**, fresh build **91 ms**; stale trace still imports `pip._internal.network.session` (100 ms) | every CLI `qx python/pytest/pip` call pays ~450 ms; nothing tells the user |
| D2 | **Python `site` dominates startup.** Raw `python -c pass` 377 ms, `python -S` 33 ms. `pip_system_certs.pth` imports all of pip (≈250 ms); qx's tls stub fixes it (54 ms) | `-X importtime`: `site` 349 ms cumulative raw | fixed by the existing stub once D1 is fixed |
| D3 | **Console-script bypass is a 14-name allow-list and never fires in a venv.** `python_console_script` maps `pytest.exe` → `python -m pytest` only for listed names and only when `python.exe` is one level above `Scripts\`; venvs keep `Scripts\python.exe`. | `hermes.exe` (uv venv) = 3 processes: uv trampoline → venv redirector → base python. `pytest.exe` launcher 718 ms vs bypass 302 ms | uv/poetry/venv projects get no Python acceleration |
| D4 | **Venv `python.exe` is a redirector** that re-spawns the base interpreter. | venv `python -S` 40-49 ms vs base 24-27 ms; base + `__PYVENV_LAUNCHER__` gives identical `sys.prefix`, `sys.executable`, `sys.path` (3.11.9 verified) | one extra spawn on every venv Python call |
| D5 | **npm fast path still spawns `cmd.exe` for every script**, and `cmd` then runs the tool's `.cmd` shim before `node.exe`. | `npm run build` (`tsc -p .`) = cmd.exe + node.exe; qx already unwraps those shims for direct calls | one avoidable spawn per npm script/hook |
| D6 | **No pnpm fast path.** `pnpm --version` via corepack 260 ms vs `node -e 0` 45 ms. | measured | ~215 ms per `pnpm run` |
| D7 | **Built-in fallback to a missing binary fails.** `wc -l a b` (two files) declines the built-in, looks for `wc.exe` on PATH, finds none: `command not found: wc`. Git for Windows ships it in `usr\bin`, which is not on PATH. | reproduced in this session | spurious failures for `wc`, `ls -lh`, `tail -n +N`, … |
| D8 | **MCP shutdown waits for in-flight calls with no bound.** On stdin EOF the server sleeps until every running command ends (timeout cap 1 h). | init + `ping -n 8` then EOF: server lived 7 098 ms | orphaned `qx mcp` processes pin the old exe image after the client exits |
| D9 | **MCP dispatch is a substring test** (`line.contains("tools/call")`) and every `tools/call` spawns an unbounded OS thread before the gate. | `src/mcp.rs:40,52` | a cancel whose text mentions `tools/call` queues behind busy workers; a burst of calls spawns a thread each |
| D10 | **Server text is dsh-only.** Instructions name `mcp__qx__run`; ~15 fs fallback messages say "the native dsh tool". | `src/mcp.rs:13`, `src/fsops.rs:75-77` + 14 messages | Codex/OpenCode/Hermes/pi models get wrong tool names |
| D11 | **No installer for any client.** README documents Claude Code, OpenCode, dsh by hand; Codex, Hermes, pi are absent. | README "MCP setup" | the user's six CLIs need six manual edits; drift (D1) follows |

| D12 | **structuredContent handling differs per client.** Codex shows the model *only* `structuredContent` (JSON-escaped stdout, qx's text notes lost); Hermes shows text *and* structuredContent (stdout twice); dsh/OpenCode/pi show `content`; Claude Code reports conflict. | client research 2026-10-20 | double tokens (Hermes), worse formatting (Codex) |
| D13 | **Existing registrations are half-working.** OpenCode's `mcp.servers.qx` lacks `codemode:false`, so the model only reaches qx through `execute`; Claude Code and OpenCode point at the stale binary (D1); Codex, Hermes, pi are unregistered; pi 0.85.1 has no MCP at all. | `opencode.json`, `.claude.json`, `.codex/config.toml`, `%LOCALAPPDATA%\hermes\config.yaml` | qx unused or slow in four of six CLIs |

What is already fast and stays as is: MCP startup 44 ms, built-ins ≈0 ms/call,
`git --version` 58 ms direct (18 ms/call effective at 4 workers), npm `run`
fast path skips npm's own ~80 ms JS start.

## 2. Requirements

R1. One canonical install path (`%USERPROFILE%\.cargo\bin\qx.exe`). `qx deploy`
    replaces it even while MCP servers run it (rename-aside), is a no-op when
    identical, and removes stale aside files it can delete.
R2. `qx doctor` reports drift: every qx binary a client config points to that
    differs from the canonical one, and stale `~/.dsh/bin/qx-*.exe` copies.
R3. Any pip/distlib or uv console-script launcher is run as
    `<interpreter> <launcher.exe> args` (exact launcher semantics, one process),
    replacing the allow-list. Unrecognized layouts keep the launcher.
R4. A venv interpreter (`<venv>\Scripts\python.exe` with `pyvenv.cfg`,
    version ≥ 3.11) is run as `<home>\python.exe` with
    `__PYVENV_LAUNCHER__=<venv>\Scripts\python.exe`. `QX_PYTHON_VENV=off`
    disables it.
R5. npm fast-path scripts with no shell syntax and a program that resolves to a
    direct target (local `.bin` first) run without `cmd.exe`; shell syntax and
    cmd internals keep `cmd.exe`.
R6. `pnpm run/test/start` gets the same fast path, fail-closed: requires
    `packageManager: "pnpm@<ver>"`, no `pnpm-workspace.yaml`, no pre/post
    script for that name, no denylisted `.npmrc` key.
R7. When a built-in declines and the program is not on PATH, a coreutils name
    resolves to Git for Windows `usr\bin\<name>.exe`; if that is missing too,
    the error names the built-in limit.
R8. MCP: parse once and dispatch by method; cancel/ping/initialize never queue
    behind work; at most `QX_MCP_QUEUE` (default 64) calls wait, extra calls get
    a `busy` tool error; on stdin EOF all in-flight calls are cancelled and the
    process exits within 3 s.
R9. Client profiles: detect the client from `initialize.params.clientInfo.name`
    (override `QX_CLIENT`); instructions, tool descriptions and fs fallback
    messages name that client's qx tool ids and native tools. Unknown clients get
    neutral wording (no "dsh").
R10. `qx setup <client…|all> [--dry-run] [--remove]` registers `qx mcp` for dsh,
    Claude Code, Codex, OpenCode, Hermes and pi, with `QX_CLIENT` set, a tool
    timeout ≥ 300 s, a `.qx-bak` backup, and an atomic write. Idempotent.
R11. Dependencies stay `serde_json` only; no async runtime.
R12. Per-client structured output: Claude Code and Codex get no `outputSchema`
    and no `structuredContent` (the model reads the text content); Hermes gets
    structuredContent without `stdout`/`stderr`; dsh, OpenCode, pi and unknown
    clients keep today's full structuredContent.
R13. Config edits change only the qx entry: every byte outside it is preserved
    (comments, key order, formatting); existing `env` keys in the qx entry are
    kept; an unrecognized shape is refused with the snippet to paste, never
    rewritten.

## 3. Out of scope

Lazy stubs for editable-install finders (31 ms here, mostly pathlib/re that real
programs import anyway); a `batch` run op (all six clients issue parallel tool
calls); yarn and bun fast paths (not installed; bun is native-fast).

## 4. Client facts

See the plan's "Client matrix" section (from the 2026-10-20 research pass).
