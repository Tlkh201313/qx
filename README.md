# qx — fast local command executor

`qx` is a small Rust CLI + MCP server that runs local commands with the fewest
possible Windows process creations. It exists because on a machine with several
real-time antivirus scanners, **spawning a process is the expensive part** — not
the command itself.

## What it does

1. **In-process built-ins** — 31 common commands run inside qx with **zero process
   creation**: `pwd echo true false sleep clear cls ls dir cat type head tail wc grep
   mkdir rm del cp copy mv move touch which basename dirname realpath whoami
   hostname seq`.
   Anything a built-in does not reproduce exactly goes to the real tool instead:
   unknown flags, wildcards, `/x` arguments (cmd switches like `dir /b`, MSYS paths
   like `/tmp/x`), `tail -n +N`, `basename f .txt`, `echo -e`, `ls -lh`, `seq 1e3`,
   `sleep` over 10 s (a built-in cannot be killed by the call's timeout), `touch`
   on a read-only file, and `cp -r` over a tree holding a symlink or junction.
   `grep` answers `-i -n -l -r -F --include=*.ext` with literal patterns and hands
   every other shape (regex metacharacters, `-c`, `-v`, stdin, binary or over-32 MiB
   files, more than 20k files) to the real `grep`, so the line list is never guessed.
   A recursive scan visits the whole tree the way GNU grep does, `.git` and
   `node_modules` included; `fs grep` is the call that skips them.
   `dir type del erase copy move` run through `cmd.exe` when no executable of that
   name exists, or when a `/switch` is given. `where` is always the real
   `where.exe`; `which` prints the first PATH hit.
   `rm` refuses drive roots, your home directory, and the current directory.
   When a built-in declines and no `name.exe` is on PATH, qx resolves the name
   from Git for Windows' `usr\bin` (`wc`, `grep`, `sed`, `awk`, `find`, `sort`,
   `cp`, `mkdir`, …), so
   `wc -l a.txt b.txt` or a POSIX line that reached `cmd` still answers instead of
   failing with "command not found"; only the coreutils Git actually ships are
   eligible, and the error message says when a declined built-in had nowhere to go.
   Two names need care: System32 ships a `find.exe` and a `sort.exe` that take
   different switches, so `find src -name x` and `sort -n` pick Git's binaries
   while a switch-less `sort`/`find` keeps whatever PATH resolves first.
2. **Direct exec** — `qx git status` calls `git.exe` directly. No `bash`, no login
   profile, no `cmd.exe` in between. Node targets also get a persistent
   `NODE_COMPILE_CACHE` (Node 22+), which cuts repeat CLI startup by ~20%.
   `QX_NODE_CACHE=off` disables it.
   A quoted argument does not force a shell: `qx -c 'grep -n "fn main" src'` is
   tokenized here (double quotes grouped and stripped, exactly what both cmd and
   Git Bash do to them) and still runs in-process. Single quotes, `$ % ! ^ * ?`,
   operators, or an escape keep the shell.
   Python startup is mostly `.pth` hooks. By default qx-launched Python skips
   `pip_system_certs` (which imports all of pip to inject system certs) and
   pynvml's warning-only finder, and keeps system-cert injection through a lazy
   truststore shim (`tls`, ~3x faster start). `QX_PYTHON_FAST=1` is fastest but
   disables that injection; `QX_PYTHON_FAST=off` runs the hooks untouched.
3. **Shim bypass** — resolves the real binary behind Windows launchers:
   - `Git\cmd\*.exe` → `Git\mingw64\bin\*.exe` (the child's PATH gets
     `Git\usr\bin` and `Git\mingw64\bin` first, so hooks that need `sh` still run)
   - `npm.cmd` / `npx.cmd` / `pnpm.cmd` / `yarn.cmd` → `node.exe` + JS entrypoint
   - **any** pip/distlib or uv console-script launcher (`pytest.exe`, `black.exe`,
     `hermes.exe`, …) → `python.exe <launcher.exe>`. Those launchers are a stub
     with a stored zip appended; running the interpreter on the launcher is
     exactly what the launcher itself does after one more process creation, so
     `sys.argv[0]` and `sys.path[0]` still are the launcher. The interpreter
     comes from the launcher's shebang line, so it works for stubs in any
     `Scripts` directory, venvs included. No allow-list, no module guess.
   - a venv's `Scripts\python.exe` → the base interpreter with
     `__PYVENV_LAUNCHER__` set, which is the same session (`sys.prefix`,
     `sys.executable` and `sys.path` are unchanged) with one process fewer.
     Skipped when `PYTHONHOME` or `__PYVENV_LAUNCHER__` is already set, when the
     venv is older than 3.11, or with `QX_PYTHON_VENV=off`.
   - **npm run/test/start fast path** - reads the nearest `package.json` and runs
     the script directly, skipping npm's ~100 ms JS CLI startup. Local
     `node_modules\.bin` dirs are on PATH, lifecycle env is set, and stdout is
     the script's own (no npm banner). `pre<name>` / `post<name>` hooks run in
     npm's order, each with its own `npm_lifecycle_*` env, stopping at the first
     failure; `npm_command`, `npm_config_user_agent`, `npm_execpath` and `NODE`
     match real npm (not exported: the resolved `npm_config_*` values, `COLOR`).
     Anything unusual - workspaces, `script-shell`, relevant `npm_config_*`
     overrides, scripts that read `npm_*` vars, or flags/args outside a strict
     allowlist - falls back to real npm. Each step also skips `cmd.exe`: when the
     script is a plain command (no shell syntax, no `cmd` internal, and its
     program resolves to a real executable) qx execs it directly, unwrapping a
     stock `.bin` shim to `node`. `a && b`, `echo hi`, an unknown program, or a
     shim that contains script logic keep npm's shell.
   - **npx local-bin fast path** - `npx <declared local tool>` runs the local bin
     directly instead of spawning npx; anything that might download falls back.
   - **pnpm run/test/start fast path** - same idea for pnpm, which pays even more
     startup than npm (~500 ms vs ~60 ms here). It only fires for a project that
     pins itself (`packageManager: pnpm@<version>`), has no `pnpm-workspace.yaml`
     in the root or any ancestor, no `workspaces` key, no `pre`/`post` script,
     and no `.npmrc` key pnpm would honor differently (`shell-emulator`,
     `enable-pre-post-scripts`, and the shared list below). Scripts see
     `PNPM_SCRIPT_SRC_DIR`, `npm_package_json`, `INIT_CWD`, `npm_execpath`,
     `npm_node_execpath`, `NODE`, and - when node's version is known -
     `npm_config_user_agent = pnpm/<version> …`. Everything else goes to pnpm.
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
   - a **cmdlet as `program`** (`Get-ChildItem src`) runs through PowerShell with
     each argument passed as a single-quoted literal, so `$var`, quotes and
     redirection inside arguments are never re-evaluated.
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
   antivirus products fighting over every spawn. `QX_TRACE=1` prints the phase
   timings of a run (spawn, wait, kill window, taskkill, reap) to stderr, so
   "why did that take a second" has an answer instead of a guess.
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
target\release\qx.exe deploy
```

`deploy` copies the running build to the canonical `%CARGO_HOME%\bin\qx.exe`
(else `%USERPROFILE%\.cargo\bin\qx.exe`), and is safe to run while MCP servers
are using it: Windows lets a running image be renamed, so the old copy is moved
aside to `qx.exe.old-<ms>` and the new one takes its place. Running clients keep
the old file until they exit; `qx deploy` reports the aside path, and the next
deploy sweeps it. `--to <path>` installs somewhere else, `--dry-run` says what
it would do without writing. One canonical binary and one path in every client
config is what keeps clients from drifting onto stale builds.

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
`--shell cmd|bash|powershell`, `--capture`. `auto` picks the shell that can parse
the line: PowerShell for `$x = 1`, `[T]::Method()`, `try { }`, `& $exe`, `@'…'@`
and `Verb-Noun` cmdlets (no profile, UTF-8 output, exit code preserved); Git Bash
when a pipeline or redirect runs a POSIX utility (`ls cat head tail wc grep sed awk
uniq xargs …`; not `find` or `sort`, which Windows has too); otherwise cmd. A cmd
line over 8 KiB fails with a message naming the limit. PowerShell 5.1 has no
`&&`/`||`, so those stay on cmd.

Children never prompt: stdin is empty, PowerShell runs `-NonInteractive` (so
`Read-Host` fails fast instead of hanging), and git gets `GIT_EDITOR=true`,
`GIT_SEQUENCE_EDITOR=true` and `GIT_TERMINAL_PROMPT=0`. Elevation requests
(`Start-Process -Verb RunAs`, `runas`, `gsudo`) are blocked — an agent cannot
answer a UAC prompt.

Exit code and stdout/stderr are passed through unchanged (compaction aside).

## MCP setup

### One command: `qx setup`

```powershell
cargo build --release; target\release\qx.exe deploy
qx setup all              # or: qx setup claude-code opencode
qx doctor                 # [install] and [clients] show what each CLI points at
```

`qx setup` edits each client's own config in place and nothing else: every byte
outside the qx entry is preserved, so comments, key order and trailing commas in
a hand-edited file survive. It backs the old file up next to itself as
`<name>.<ext>.qx-bak`, writes a temp file and renames it over the original, so a
crash cannot leave half a config. When a file's shape is not one qx recognises
(a legacy key, a flow-style table, an unexpected nesting) it refuses that client
and prints the snippet to paste instead. `--dry-run` prints what would change;
`--remove` deletes the qx entry and leaves everything else.

Each client editor keeps the `env` keys the existing entry already had and sets
`QX_CLIENT` so the server knows which tool names and output shape to use.

### Claude Code

```powershell
claude mcp add --scope user qx "C:\Users\USER\.cargo\bin\qx.exe" mcp
```

### Codex

`qx setup codex` writes `[mcp_servers.qx]` into `%CODEX_HOME%\config.toml`
(default `~\.codex\config.toml`), with `startup_timeout_sec = 20` and
`tool_timeout_sec = 600`, plus a `[mcp_servers.qx.env]` subtable holding
`QX_CLIENT = "codex"` and any keys you already had. Codex shows the model only
`structuredContent`, so qx omits `stdout` duplication from the text view for
this client. The block is edited line by line, so comments and every other table
in the file survive; a flow-style `mcp_servers = { … }` table is refused with a
paste-ready block instead.

### OpenCode

```powershell
opencode mcp add qx --global -- "C:\Users\USER\.cargo\bin\qx.exe" mcp
```

This writes `mcp.servers.qx` to the global config. Keep
`"qx_*": "allow"` in the permission block so the agent never stops for a qx
approval. Restart OpenCode after replacing `qx.exe`: a running MCP process
holds the old executable image until it is restarted.

### DeepSeek Harness (dsh)

`qx setup dsh` rewrites the `mcp-qx` item in `$DSH_HOME/cordis.patch.yml`:
`config.command` becomes the canonical `qx.exe` (with `/` separators, as dsh
expects) and `config.env` gains `QX_CLIENT: dsh`, keeping the keys you already
had. Every other line stays byte-identical; a flow-style `env: { … }` is refused
with the block to paste. After a successful dsh install it also deletes
`$DSH_HOME\bin\qx*.exe` copies that no `cordis.patch.yml` under `$DSH_HOME`
references — one canonical binary is enough now that `qx deploy` can replace a
file that a running server still holds.

Doing it by hand looks like this:

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
          QX_CLIENT: dsh
          QX_PYTHON_FAST: tls
        toolCallTimeoutMs: 300000
        failOnStartupError: true
```

The model sees `mcp__qx__run` and `mcp__qx__fs`. qx also sends MCP server
instructions, so dsh injects the routing rule into the system prompt
automatically: qx first for commands, read/glob/literal grep and guarded
mutations; native dsh tools on any qx error, for regex/images, or for sandbox
escalation. Restart dsh after changing the `command` path.

### Hermes

`qx setup hermes` writes a `qx:` child under `mcp_servers:` in
`%HERMES_HOME%\config.yaml` (default `%LOCALAPPDATA%\hermes\config.yaml`):

```yaml
mcp_servers:
  qx:
    command: 'C:\Users\USER\.cargo\bin\qx.exe'
    args: [mcp]
    env:
      QX_CLIENT: hermes
    timeout: 600
    connect_timeout: 60
```

Other servers and every other top-level key are left byte-identical. Hermes
reports the SDK's default `mcp` as its client name, so `QX_CLIENT: hermes` is
what selects its profile: qx then keeps the text output and puts only metadata
in `structuredContent` (Hermes prints both copies). Two things to know: Hermes
ignores MCP server instructions, so the routing rule has to live in its own
prompt, and if `tool_search` is set to `auto` the qx tools may not be exposed at
all — `qx doctor` flags that.

### pi

`qx setup pi` reads pi's own `package.json` version and picks the right half:

- **pi 0.99 and later** - its built-in MCP client: `~\.pi\agent\mcp.json` gets
  `mcpServers.qx` with `command`, `args: ["mcp"]`, `env.QX_CLIENT: "pi"`,
  `exposure: "direct"` and `timeout: 600`.
- **pi before 0.99** (0.85 is what ships today) - pi has no MCP support at all,
  so the setup writes a small extension: `~\.pi\agent\extensions\qx.js`, plus
  `qx\bridge.js` and a `package.json` marking that directory as ESM. The bridge
  speaks stdio JSON-RPC to `qx mcp` (spawned lazily, respawned if it dies, an
  aborted call sends `notifications/cancelled`) and the extension registers
  `mcp__qx__run` and `mcp__qx__fs` with the parameter shapes qx expects.

`node integrations/pi/smoke.mjs <path-to-qx.exe>` exercises the bridge against a
real server and prints `pi bridge ok`.

### The `run` tool

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

0.7 numbers, median of 5 through `Measure-Command`, against the 0.6 build that
was on `PATH`:

| what | 0.6 | 0.7 | why |
|---|---|---|---|
| `qx --raw python -c pass` | 539 ms | **105 ms** | one process: the stale 0.6 copy predated the site-hook fix |
| `qx --raw pytest --version` | 718 ms | **311 ms** | any pip/uv launcher runs as `python.exe <launcher>` |
| `qx --raw hermes --version` (venv console script) | 26 134 ms | **571 ms** | launcher + venv redirect, both one process |
| `qx pnpm run <script>` | 569 ms | **67 ms** | pnpm fast path for a pinned, non-workspace project |
| `qx npm run <plain script>` | 92 ms | **60 ms** | the script step no longer spawns `cmd.exe` |
| `qx mcp` exit with a 7 s call running | 6 817 ms | **377 ms** | EOF cancels the call instead of waiting for it |

| command | time |
|---|---|
| `qx git --version` | **0.1-0.3 s** |
| `qx npm run <script>` | **~0.06-0.13 s** |
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
