# qx — fast local command executor

`qx` is a small Rust CLI + MCP server that runs local commands with the fewest
possible Windows process creations. It exists because on a machine with several
real-time antivirus scanners, **spawning a process is the expensive part** — not
the command itself.

## What it does

1. **In-process built-ins** — 31 common commands run inside qx with **zero process
   creation**: `pwd echo true false sleep clear cls ls dir cat type head tail wc
   mkdir rm del cp copy mv move touch which where basename dirname realpath whoami
   hostname seq`.
   Unknown flags or wildcards fall back to the real executable, so nothing breaks.
   `rm` refuses drive roots, your home directory, and the current directory.
2. **Direct exec** — `qx git status` calls `git.exe` directly. No `bash`, no login
   profile, no `cmd.exe` in between. Node targets also get a persistent
   `NODE_COMPILE_CACHE` (Node 22+), which cuts repeat CLI startup by ~20%.
   `QX_NODE_CACHE=off` disables it.
3. **Shim bypass** — resolves the real binary behind Windows launchers:
   - `Git\cmd\*.exe` → `Git\mingw64\bin\*.exe`
   - `npm.cmd` / `npx.cmd` / `pnpm.cmd` / `yarn.cmd` → `node.exe` + JS entrypoint
   - `pip.exe` / `pytest.exe` / `black.exe` / `uvicorn.exe` / `flake8.exe`
     → `python.exe -m <module>` when the module is installed beside that
     interpreter. This removes the console-script launcher process.
   - **npm run/test/start fast path** - reads the nearest `package.json` and runs
     the script directly, skipping npm's ~100 ms JS CLI startup. Local
     `node_modules\.bin` dirs are on PATH, lifecycle env is set, and stdout is
     the script's own (no npm banner). Anything unusual - workspaces, pre/post
     scripts, `script-shell`, relevant `npm_config_*` overrides, scripts that
     read `npm_*` vars, or flags/args outside a strict allowlist - falls back to
     real npm.
   - **npx local-bin fast path** - `npx <declared local tool>` runs the local bin
     directly instead of spawning npx; anything that might download falls back.
   - **generic `.cmd` parser** — reads any npm/Scoop-style shim, extracts the
     `%~dp0` target, and runs `node + script` or the target `.exe` directly.
     This covers every npm-installed CLI: eslint, prettier, jest, vitest, next,
     vite, playwright, turbo, nx, prisma, tsc, tsx, and anything else.
   - `.ps1` → `powershell -NoProfile -ExecutionPolicy Bypass -File`
4. **Shell-free command strings** — `command: "ls -la"` is tokenized and run
   directly. Only real shell syntax (pipes, redirects, quotes, globs, variables)
   spawns a shell.
5. **Output compaction for LLM readers** — strips ANSI, collapses repeated
   lines, keeps head + tail of large output, and **rescues error/warning lines
   from the omitted middle** so a buried failure is never lost. Default 200
   lines (80 head / 60 tail). Language-aware profiles shrink common noisy
   commands further:
   - `cargo test` / `pytest` / `jest` / `vitest` → failures + final result
   - `cargo build` / `tsc` / `vite` / `webpack` → errors/warnings + summary
   - `git status` / `git diff` → branch, changed files, hunks
   - `rg` / `grep` → grouped matches
   - Python tracebacks and Node stacks → traceback + exception, not logs
   - JSON → minified when it saves bytes
   `--raw` / MCP `mode: "full"` always returns the untouched text.
6. **MCP server** - `qx mcp` speaks MCP over stdio. Claude Code starts it once and
   never spawns anything per built-in command. Results carry structured fields
   (`exit_code`, `duration_ms`, `truncated`, `redactions`, `guard_warning`,
   `profile`, `omitted_lines`).
   Up to 4 tool calls execute in parallel (`QX_MCP_WORKERS`), so independent
   commands do not queue behind each other.
7. **Diagnostics** - `qx doctor` measures process-creation latency and names the
   antivirus products fighting over every spawn.
8. **Bounded, clean execution** - captured output is capped at 16 MiB per stream,
   timeouts terminate the whole process tree, and children receive a
   deduplicated PATH with a valid PATHEXT. That prevents the Windows
   environment-block truncation that makes `cmd /c tool` answer
   "not recognized". Built-in `head`, `tail`, and `wc` stream files instead of
   loading them whole; `cat` marks truncation at 32 MiB.

Set `QX_NO_BUILTINS=1` to disable all built-ins and always exec the real binary.

## Trust & safety (why it is safe to auto-approve)

qx is meant to be allow-listed in Claude Code (`Bash(qx:*)`, `mcp__qx__*`). That
removes Claude's own confirmation prompt, so qx enforces its own gate before
every command — including built-ins and shell wrappers.

**Deny** (CLI exit 126, MCP `isError`):
- `rm`/`del`/`rd` of drive roots, `C:\Windows`, Program Files, your home, the
  current directory, or a parent of it
- `git push --force` (override with `QX_ALLOW_FORCE_PUSH=1`)
- `curl ... | sh`, `wget ... | bash`, `Invoke-Expression`, encoded PowerShell
- SQL `DROP`, `TRUNCATE`, or `DELETE`/`UPDATE` without `WHERE`
- disk/boot changes: `format`, `diskpart`, `bcdedit`, `vssadmin delete`, `cipher /w`
- registry/service/scheduled-task deletion, local-admin creation, force-killing
  critical Windows processes

**Warn + audit** (still runs): `git reset --hard`, `git clean -f`, branch
deletion, wildcard deletion, deleting outside the workspace, package publish,
inline code that spawns or deletes.

**Shell wrappers are inspected recursively** - the payload of `cmd /c`,
`powershell -Command`, and `bash -c` is analyzed as if typed directly.
`env`, `xargs`, `timeout`, and `nohup` are peeled back too, so
`env rm -rf /` is judged as `rm -rf /`. Paths are expanded before checking:
`~`, `%VAR%`, `$env:VAR`, and `$HOME` all resolve to the real location. Any
attempt to set `QX_GUARD=off` inside a command is denied.

**Credential egress** is scoped to actual network clients and inline network
code. A commit message that merely mentions `.env` or `curl` is not blocked;
`cat .env | curl -d @-` is.

**Secret redaction** - API keys, tokens, passwords, and private-key blocks become
`***` in captured and MCP output. A human at a TTY sees raw output unless
`QX_REDACT=1`.

**Audit** - every warning or block is appended to
`%LOCALAPPDATA%\qx\audit.jsonl`; review with `qx audit`. Set `QX_AUDIT=1` to log
every command. The file rotates to `audit.jsonl.1` at 5 MiB.

```powershell
qx check "rm -rf C:\"      # DENY
qx check "git status"      # ALLOW
qx check "git push --force" # DENY
qx audit 20
```

`QX_GUARD=strict` turns warnings into blocks. `QX_GUARD=off` disables the gate —
only with the user's explicit decision. `QX_ALLOW_ROOTS=C:\a;C:\b` marks extra
directories where deletion is routine.

## Build & install

```powershell
cd C:\Users\USER\Projects\qx
cargo build --release
Copy-Item target\release\qx.exe C:\Users\USER\.cargo\bin\qx.exe -Force
```

## Usage

```powershell
qx git status
qx cargo test --quiet
qx python -m pytest -q
qx npm run build                        # skips npm's CLI startup
qx npx tsc --noEmit                     # local bin, no npx process
qx -c "npm run build 2>&1 | tail -30"   # shell string when you need pipes
qx --raw <cmd>                          # no compaction
qx --stream <cmd>                       # live output (default on a TTY)
qx which npm                            # show the real binary / shim bypass
qx doctor                               # why is this machine slow?
QX_STATS=1 qx <cmd>                     # record savings
qx gain                                 # savings report
```

Flags: `--max-lines`, `--head`, `--tail`, `--timeout SECS`, `--cwd DIR`,
`--shell cmd|bash`, `--capture`.

Exit code and stdout/stderr are passed through unchanged (compaction aside).

## MCP setup (Claude Code)

```powershell
claude mcp add --scope user qx "C:\Users\USER\.cargo\bin\qx.exe" mcp
```

The server exposes one tool, `run`:

| field | meaning |
|---|---|
| `program` + `args` | direct exec, fastest |
| `command` | full shell line for pipes / redirects / `&&` |
| `shell` | `auto` (default), `cmd`, `bash` |
| `cwd` | working directory |
| `timeout_ms` | default 120000 |
| `mode` | `compact` (default) or `full` |

Agents should prefer `program` + `args`. It removes one process creation per call.

## Measured on this machine (AV active)

| command | time |
|---|---|
| `qx git --version` | **0.1-0.3 s** |
| `qx npm run <script>` | **~0.08-0.13 s** |
| `npm run <script>` (PowerShell) | 0.4-0.6 s |
| `bash -c "git --version"` | 3.0 s |
| `rtk git --version` | 3.7 s |
| `bash -lc "git --version"` (WSL + profile) | **63 s** |
| `qx` MCP built-in (`ls`, `wc`, `cat`) | **0-1 ms** |
| `qx` MCP command (`git status`) | 0.2-1.9 s |
| 3 MCP calls in flight (0.6 s each) | **0.7 s total** |

The remaining time is the operating system scanning each new process. A CLI
call to `qx` still pays for spawning `qx.exe` itself; the MCP server does not.
That is why built-ins are effectively instant through MCP. An oversized PATH can
also add seconds of failed lookups - qx deduplicates it before every spawn.

## The real bottleneck

`qx doctor` on the machine this was built for:

```
[SLOW] cold spawn (cmd /d /c exit): 593 ms avg
[!] antivirus installs detected: Avast, Malwarebytes, Bitdefender agent
```

Four security stacks (including Windows Defender) were scanning every process
creation. Fixing that is worth more than any CLI:

1. Keep **one** real-time antivirus; uninstall or fully disable the others.
2. Add folder exclusions for your code directories, `~/.cargo`, and `node_modules`.
3. Never use `bash -lc` — a login profile turns 2 s into 60 s.
4. Route agent commands through `qx` / `qx mcp`.

## Design notes

- Zero async runtime, zero wrapper daemon - `std::process` only.
- One dependency (`serde_json`) for the MCP protocol.
- Capture mode reads stdout and stderr on separate threads, so large output can
  never deadlock the child. Each stream is capped at 16 MiB; the excess is
  drained, not buffered.
- Timeout terminates a Windows Job Object plus `taskkill /T`, then waits a
  bounded 2 s - a surviving grandchild can never hang qx.
- MCP runs up to 4 commands concurrently with bounded backpressure; JSON-RPC
  replies are matched by id, so out-of-order completion is safe.
- Child processes get a rebuilt environment: PATH is deduplicated and PATHEXT is
  restored when the inherited block is malformed or oversized.
- `cmd.exe /c` receives the command as raw command-line text. Letting Rust
  quote it turned inner quotes into `\"`, so `node -e "..."` silently did
  nothing; the raw form also keeps quoted executable paths working.
- npm/npx acceleration is deliberately fail-closed: if the package shape or
  arguments are not fully understood, qx runs real npm/npx instead of guessing.
- Stats are opt-in (`QX_STATS=1`) so the fast path stays free of file I/O.

## Tests

```powershell
cargo test --release
```
