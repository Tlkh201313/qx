# qx — fast local command executor

`qx` is a small Rust CLI + MCP server that runs local commands with the fewest
possible Windows process creations. It exists because on a machine with several
real-time antivirus scanners, **spawning a process is the expensive part** — not
the command itself.

## What it does

1. **In-process built-ins** — 30 common commands run inside qx with **zero process
   creation**: `pwd echo true false sleep clear cls ls dir cat type head tail wc
   mkdir rm del cp copy mv move touch which basename dirname realpath whoami
   hostname seq`.
   Anything a built-in does not reproduce exactly goes to the real tool instead:
   unknown flags, wildcards, `/x` arguments (cmd switches like `dir /b`, MSYS paths
   like `/tmp/x`), `tail -n +N`, `basename f .txt`, `echo -e`, `ls -lh`, `seq 1e3`,
   `sleep` over 10 s (a built-in cannot be killed by the call's timeout), `touch`
   on a read-only file, and `cp -r` over a tree holding a symlink or junction.
   `dir type del erase copy move` run through `cmd.exe` when no executable of that
   name exists, or when a `/switch` is given. `where` is always the real
   `where.exe`; `which` prints the first PATH hit.
   `rm` refuses drive roots, your home directory, and the current directory.
2. **Direct exec** — `qx git status` calls `git.exe` directly. No `bash`, no login
   profile, no `cmd.exe` in between. Node targets also get a persistent
   `NODE_COMPILE_CACHE` (Node 22+), which cuts repeat CLI startup by ~20%.
   `QX_NODE_CACHE=off` disables it.
   Python startup is mostly `.pth` hooks. By default qx-launched Python skips
   `pip_system_certs` (which imports all of pip to inject system certs) and
   pynvml's warning-only finder, and keeps system-cert injection through a lazy
   truststore shim (`tls`, ~3x faster start). `QX_PYTHON_FAST=1` is fastest but
   disables that injection; `QX_PYTHON_FAST=off` runs the hooks untouched.
3. **Shim bypass** — resolves the real binary behind Windows launchers:
   - `Git\cmd\*.exe` → `Git\mingw64\bin\*.exe` (the child's PATH gets
     `Git\usr\bin` and `Git\mingw64\bin` first, so hooks that need `sh` still run)
   - `npm.cmd` / `npx.cmd` / `pnpm.cmd` / `yarn.cmd` → `node.exe` + JS entrypoint
   - `pip.exe` / `pytest.exe` / `black.exe` / `uvicorn.exe` / `flake8.exe`
     → `python.exe -m <module>` when the module is installed beside that
     interpreter and the interpreter is Python 3.11+. This removes the
     console-script launcher process; qx sets `PYTHONSAFEPATH=1` so the working
     directory stays off `sys.path`, as with the launcher (a repo's own
     `platform.py` or `types.py` cannot shadow the stdlib). Older Pythons keep
     the launcher.
   - **npm run/test/start fast path** - reads the nearest `package.json` and runs
     the script directly, skipping npm's ~100 ms JS CLI startup. Local
     `node_modules\.bin` dirs are on PATH, lifecycle env is set, and stdout is
     the script's own (no npm banner). `pre<name>` / `post<name>` hooks run in
     npm's order, each with its own `npm_lifecycle_*` env, stopping at the first
     failure; `npm_command`, `npm_config_user_agent`, `npm_execpath` and `NODE`
     match real npm (not exported: the resolved `npm_config_*` values, `COLOR`).
     Anything unusual - workspaces, `script-shell`, relevant `npm_config_*`
     overrides, scripts that read `npm_*` vars, or flags/args outside a strict
     allowlist - falls back to real npm.
   - **npx local-bin fast path** - `npx <declared local tool>` runs the local bin
     directly instead of spawning npx; anything that might download falls back.
   - **generic `.cmd` parser** — reads a stock npm/Scoop-style shim, extracts the
     `%~dp0` target, and runs `node + script` or the target `.exe` directly.
     This covers every npm-installed CLI: eslint, prettier, jest, vitest, next,
     vite, playwright, turbo, nx, prisma, tsc, tsx, and anything else. Only the
     exact stock shapes are unwrapped: a `.cmd` that also sets a variable, `cd`s,
     branches, or passes node flags is script logic, so it runs through `cmd.exe`
     as written.
   - **explicit paths** (`./node_modules/.bin/tsc`, `bin\x.cmd`) resolve against
     the call's `cwd`, try PATHEXT before the bare name, and never run an
     extensionless file (a `sh` script, os error 193).
   - `.ps1` → `powershell -NoProfile -ExecutionPolicy Bypass -File`
   - `program: powershell` gets `-NoProfile` added (a profile costs 300+ ms and
     can print into the output); `QX_PS_PROFILE=1` keeps it.
4. **Shell-free command strings** — `command: "ls -la"` is tokenized and run
   directly. Only real shell syntax (pipes, redirects, quotes, globs, variables)
   spawns a shell. Leading `NAME=value` words (`CI=1 npm test`,
   `RUST_BACKTRACE=1 cargo test`, `A="x y" make`) become the child's environment
   instead of failing in `cmd.exe`; the guard still judges the whole line. Values
   with `$`, `%`, backticks or operators stay with the shell. The prefix goes to
   that child (and to built-ins) only: qx's own environment, including its `QX_*`
   switches, is never modified.
5. **Output compaction for LLM readers** — strips ANSI, collapses repeated
   lines, keeps head + tail of large output, and **rescues error/warning lines
   from the omitted middle** so a buried failure is never lost. Default 200
   lines (80 head / 60 tail). Language-aware profiles shrink common noisy
   commands further:
   - `cargo test` / `pytest` / `jest` / `vitest` → failures + final result
   - `cargo build` / `tsc` / `vite` / `webpack` → errors/warnings + summary
   - `git status` / `git diff` → branch, changed files, hunks
   - `go test -v` → failing tests with their log lines, no `=== RUN` chatter
   - `rg` / `grep` → matches grouped per file (25 per file, 300 overall; a capped
     file still shows its true count, and no later file is dropped). `rg` without
     line numbers works too
   - `find` / `git ls-files` / `rg -l` / `rg --files` → lossless
     `dir/{a,b,c}` grouping
   - Python tracebacks and Node stacks → traceback + exception, not logs
   - JSON → minified when it saves bytes
   The profile follows the real command, not the wrapper: `cd app && CI=1 npx tsc
   2>&1 | tail`, `time uv run pytest`, `pnpm exec eslint .` are digested as `tsc`,
   `pytest`, `eslint`. Build/test/traceback digests also drop the working
   directory from located paths (3+ hits, with a one-line note).
   A digest steps aside, leaving only plain compaction, when it cannot be
   faithful: the command asked for another format (`--json`, `--message-format=json`,
   `--junit`, `-v`/`--verbose`, and `--nocapture`/`-s` on a passing run);
   `git status -s`/`--porcelain` or any line it does not know; `git log` with
   `-p`, `--date`, `--summary` and similar; combined or conflict `git diff`; unittest
   output; a pipeline whose later stages are not `head`/`tail`/`tee`/`cat`. Digests
   keep `npm warn` lines, webpack `WARNING in` blocks, pytest/vitest summary lines
   and a Python exception whatever its class is called. A compacted reply also
   reports `truncated` when a single over-long line was cut.
   Over budget, runs of 8+ lines that differ only in digits (progress counters)
   keep their first and last line plus a count; error lines are never collapsed.
   **Nothing cut is gone for good:** when output is truncated, the full redacted
   text is saved to `%TEMP%\qx-spool\` (newest 40 kept) and the reply ends with
   `[qx] full output: <path>` (also `full_output` in the structured fields; the
   CLI prints it on stderr), so the agent can grep it instead of re-running the
   command. Noise-only digests (`git status`, grouped path lists) are not spooled.
   `--raw` / MCP `mode: "full"` always returns the untouched text.
6. **MCP server** - `qx mcp` speaks MCP over stdio and exposes `run` (commands)
   plus `fs` (read, write, edit, glob, literal grep, list, stat). Claude Code
   starts it once and never spawns anything per built-in command. Results carry
   structured fields (`exit_code`, `duration_ms`, `truncated`, `full_output`,
   `redactions`, `guard_warning`, `profile`, `omitted_lines`); a field at its
   default (`truncated: false`, `redactions: 0`, `full_output: null`, empty
   `stderr`, ...) is left out, while `exit_code` and `stdout` are always there.
   Up to 4 tool calls execute in parallel (`QX_MCP_WORKERS`), so independent
   commands do not queue behind each other. `fs` greps scan files across
   `QX_FS_THREADS` workers (default CPUs/2, capped at 8). Notifications
   (no `id`) are never answered or run;
   `notifications/cancelled` stops the matching running call (it reports as
   timed out), also while the call still waits for a worker slot; ping and
   cancel are answered even when every slot is busy. `args` must be an array of strings (anything else is an error,
   not a different command) and `timeout_ms` is clamped to 1 s-1 h.
7. **Diagnostics** - `qx doctor` measures process-creation latency and names the
   antivirus products fighting over every spawn.
8. **Bounded, clean execution** - captured output is capped at 16 MiB per stream,
   timeouts terminate the whole process tree (also for `--timeout` on a live
   stream), a Windows job object kills that tree if qx itself dies, a piped CLI
   stdin is passed on to the child (MCP children get none), and children receive a
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
- `rm`/`del`/`rd` of drive roots, `C:\Windows`, Program Files, your home (or a
  parent such as `C:\Users`), the current directory, or a parent of it; the same
  paths as `mv`/`move` sources, `C:\*`, and `find <root> -delete` / `-exec rm`
- `wsl --unregister`
- `git push --force` (override with `QX_ALLOW_FORCE_PUSH=1`)
- `curl ... | sh`, `wget ... | bash`, `Invoke-Expression`, encoded PowerShell
- SQL `DROP`, `TRUNCATE`, or `DELETE`/`UPDATE` without `WHERE`
- disk/boot changes: `format`, `diskpart`, `bcdedit`, `vssadmin delete`, `cipher /w`
- registry/service/scheduled-task deletion, local-admin creation, force-killing
  critical Windows processes

**Warn + audit** (still runs): `git reset --hard`, `git clean -f`, branch
deletion, wildcard deletion, `find -delete` / `-exec rm`, deleting outside the
workspace, package publish, inline code that spawns or deletes.

**Shell wrappers are inspected recursively** - the payload of `cmd /c`,
`powershell -Command`, and `bash -c` is analyzed as if typed directly.
`env`, `xargs`, `timeout`, `nohup`, `sudo`, `doas`, `wsl`, `command`, and `exec`
are peeled back too, so `sudo rm -rf /` is judged as `rm -rf /`. Paths are
expanded before checking: `~`, `%VAR%`, `$env:VAR`, and `$HOME` all resolve to
the real location, and relative paths (`..\..\Windows`) resolve against the
working directory even when no `cwd` is given. Any attempt to set `QX_GUARD`,
`QX_REDACT` or `QX_ALLOW_*` inside a command is denied.

**Built-in file tools refuse to nest a directory in itself** (`cp -r d d/sub`,
`mv d d/sub`), and `mv` only falls back to copy+delete across volumes, so a
locked or non-empty target errors instead of merging trees. A `cwd` that is not
a directory is an error rather than silently ignored. The MCP server answers
malformed input (bad JSON, invalid UTF-8, batches) and survives a panicking
handler.

**qx `fs` mutations are workspace-confined**: every path is resolved against
the required `cwd`, symlink/junction escapes are refused, `.git`/`.hg`/`.svn`
metadata is read-only, and write/edit require a prior qx read of the same
version. Existing files are replaced through a temporary file in the same
directory followed by an atomic rename (and an fsync, matching dsh's local
backend); new files use a no-replace publication. qx cannot enforce dsh's
sandbox modes, so the native dsh `read`/`write`/`edit` tools remain
authoritative for sandbox escalation, images, and any path outside `cwd`. qx
grep is literal-only; native grep owns regex.

**Credential egress** is scoped to actual network clients and inline network
code. A commit message that merely mentions `.env` or `curl` is not blocked;
`cat .env | curl -d @-` is.

**Secret redaction** - API keys, tokens, passwords, and private-key blocks become
`***` in captured and MCP output (a quoted value is masked whole, spaces and
`;` included; PGP key blocks too). Values that are plainly code or prose
(`std::env::var(`, `Option<String>`, `pub token: String`, `error: invalid
secret: expected 32 bytes`) are left readable. A human at a TTY sees raw output
unless `QX_REDACT=1`.

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
`--shell cmd|bash|powershell`, `--capture`. `auto` picks PowerShell when a command
starts with a `Verb-Noun` cmdlet or uses `$env:` (no profile, UTF-8 output),
bash for POSIX syntax, else cmd. PowerShell 5.1 has no `&&`/`||`.

Exit code and stdout/stderr are passed through unchanged (compaction aside).

## MCP setup

### Claude Code

```powershell
claude mcp add --scope user qx "C:\Users\USER\.cargo\bin\qx.exe" mcp
```

### OpenCode

```powershell
opencode mcp add qx --global -- "C:\Users\USER\.cargo\bin\qx.exe" mcp
```

This writes `mcp.servers.qx` to the global config. Keep
`"qx_*": "allow"` in the permission block so the agent never stops for a qx
approval. Restart OpenCode after replacing `qx.exe`: a running MCP process
holds the old executable image until it is restarted.

### DeepSeek Harness (dsh)

Add qx to the global `$DSH_HOME/cordis.patch.yml` (or a profile's
`cordis.patch.yml`):

```yaml
- insert:
    - id: mcp-qx
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        serverName: qx
        transport: stdio
        command: C:/Users/USER/.cargo/bin/qx.exe
        args: [mcp]
        env:
          QX_PYTHON_FAST: tls
        toolCallTimeoutMs: 300000
        failOnStartupError: true
```

The model sees `mcp__qx__run` and `mcp__qx__fs`. qx also sends MCP server
instructions, so dsh injects the routing rule into the system prompt
automatically: qx first for commands, read/glob/literal grep and guarded
mutations; native dsh tools on any qx error, for regex/images, or for sandbox
escalation. On Windows a running stdio server holds the old executable image,
so keep a versioned copy under `$DSH_HOME/bin` and point `command` at it; a
rebuild can then replace `target\release\qx.exe` without a sharing violation.
Restart dsh after changing the `command` path.

`run`:

| field | meaning |
|---|---|
| `program` + `args` | direct exec, fastest |
| `command` | full shell line for pipes / redirects / `&&` |
| `shell` | `auto` (default), `cmd`, `bash` |
| `cwd` | working directory |
| `timeout_ms` | default 120000 |
| `mode` | `compact` (default) or `full` |

`fs` (always pass `cwd`):

| `op` | key fields | behavior |
|---|---|---|
| `read` | `path` (or `file_path`), `offset?`, `limit?` | line-numbered UTF-8 window; 50 KiB / 2000 lines; observes the file for a later qx write/edit |
| `write` | `path`, `content` | create, or replace after a qx read; parents created; atomic |
| `edit` | `path`, `old_string`, `new_string`, `replace_all?` | literal replace after a qx read; unique unless `replace_all`; UTF-8 only (UTF-16 / legacy code pages get `FS_ENCODING` and are left untouched) |
| `glob` | `pattern`, `path?` | `*`, `**`, `?`, `[class]`, `{a,b}`; newest first; generated dirs skipped |
| `grep` | `pattern`, `path?`, `include?`, `ignore_case?` | literal; `path:line: text`; scans files in parallel |
| `list` / `stat` | `path?` | directory entries / one path's type, size, mtime |

Agents should prefer `program` + `args` for commands. It removes one process
creation per call. Structured results also carry `stdout` and `stderr`, so Code
Mode clients that only surface `structuredContent` still receive command output.
For `fs`, structured results carry counts, truncation flags, skips, and
`duration_ms`; errors carry a stable `FS_*` code and a `fallback` hint naming
the native dsh tool.

### Limits and shutdown

The server runs at most 4 commands at once (`QX_MCP_WORKERS`, clamped 1–16) and
queues at most 64 more beyond that (`QX_MCP_QUEUE`, clamped 0–1024). A call past
the limit is refused immediately with `qx busy: N calls already running or
queued (limit 4+64); retry shortly`, so a runaway client cannot queue unbounded
work. Requests are routed by their JSON-RPC method, not by text: only a call
carrying an `id` takes a worker slot, so a `tools/call` notification never
starts a command.

When the client closes stdin the server stops rather than wait: running
commands are cancelled (their reply carries `timed_out: true`), calls still
waiting for a slot are refused with `qx is shutting down; the call was not run`,
and the server returns within about 3 s. Child processes die with qx's job object.

### Clients

qx writes its messages for the client it is talking to. Detection order is
`QX_CLIENT`, then `clientInfo.name`, then `CLAUDECODE=1`, then generic:

| client | detected by | tools qx names as the fallback | output shape |
|---|---|---|---|
| dsh | `dsh-mcp-client` | `pwsh`, `read`, `write` | text + `structuredContent` |
| Claude Code | `claude-code`, `CLAUDECODE=1` | `Bash`, `Read`, `Write` | text only (no `outputSchema`, no `structuredContent`) |
| Codex | `codex-mcp-client` | `exec_command`, `apply_patch` | text only |
| OpenCode | `opencode` | `shell`, `read`, `write` | text + `structuredContent` |
| Hermes | `QX_CLIENT=hermes` (it reports the SDK default `mcp`) | `terminal`, `read_file`, `write_file`, `patch` | text + metadata, without a second copy of `stdout` |
| pi | `pi` | `bash`, `read`, `write` | text + `structuredContent` |
| generic | anything else | `shell`, `read`, `write` | text + `structuredContent` |

The server instructions name the tools with that client's ids (`mcp__qx__run`, or
`qx_run` on OpenCode) and stay under 1 KiB, because Codex reads only the first
512 bytes of them. Set `QX_CLIENT` in the server environment when a client's
self-report is ambiguous; that is the only reliable way to reach the Hermes
profile.

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
| `qx fs read` (21 KiB source) | **0.6-1.0 ms** |
| `qx fs glob` `**/*.rs` (default dirs) | **0.8-1.2 ms** |
| `qx fs grep` literal in qx repo | **2.3-4.0 ms** |
| packaged `rg` glob/grep, same repo | 30-49 ms |
| `qx fs write` 100 KiB after read | **2.2-4.0 ms** |
| `qx fs edit` 100 KiB `replace_all` | **2.0-3.6 ms** |

These are warm, end-to-end MCP medians on the 28-thread machine above. qx glob
and grep skip `node_modules`, `target`, and similar generated trees during
recursive walks; pass one of those directories as `path` to search it directly
(qx then walks from that root), or let the native dsh glob/grep tools search the
whole ignored tree in about 30-47 ms. qx deliberately does not carry a slower
`all` switch just to match that. qx `read` is at parity with
dsh's in-process read, and qx write/edit win because they skip the JS diff
presentation while keeping fsync and atomic publication.

The remaining time is the operating system scanning each new process. A CLI
call to `qx` still pays for spawning `qx.exe` itself; the MCP server does not.
That is why built-ins are effectively instant through MCP. An oversized PATH can
also add seconds of failed lookups - qx deduplicates it before every spawn.

## The real bottleneck

`qx doctor` on the machine this was built for:

```
[SLOW] cold spawn (cmd /d /c exit): 593 ms median
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
