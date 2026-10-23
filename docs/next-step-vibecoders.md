# Next step: qx for every vibecoder

Goal: anyone who codes through an agent (Claude Code, Codex, Cursor, OpenCode, ...)
installs qx in one step and spends fewer tokens and less waiting, while the agent
sees every fact it needs. Never trade correctness for savings.

## Where qx stands (2026-10-22)

- Works: in-process built-ins, direct exec, shim bypass, digests with a spool of
  the full output, guard, redaction, MCP server, `qx setup` for six clients, and the
  Claude Code PreToolUse hook (`qx hook claude`).
- Hook replay over 40,069 distinct real agent commands (2026-10-23): 27,623 rewritten
  (68.9%), down from 71% after the hook stopped touching `$var` arguments of MSYS
  tools, `which`/`check` (qx subcommands), and lines with `eval`/`source`/functions.
  Hook cost: ~13 ms per call, ~48 ms when it asks rtk first (rtk capped at 3 s).
- Hook and permissions: outside `bypassPermissions` the hook rewrites only when every
  allow rule answers the line the same before and after, so `Bash(qx:*)` cannot turn a
  `git push` prompt into a pass. With qx allow-listed this means few rewrites in default
  mode. Open: a hook answer that keeps the prompt (`permissionDecision: "ask"`) would
  let it rewrite there too, but only once it is known which lines Claude Code itself
  passes without a prompt, or every `cat` would start asking (item 5 below).
- Test suite: 255 pass, 0 fail. Linux `cargo check --all-targets`: clean, no warnings.
  All 27,521 Bash rewrites of the replay still parse (`bash -n`).
- Gaps for a stranger: build from source only; Windows-tuned docs and numbers;
  savings are opt-in (`QX_STATS=1`) and counted in bytes; no setup for Cursor,
  Windsurf, Gemini CLI, Cline or VS Code; macOS never built.
- Guard on Linux/macOS: home is protected (via `HOME`), but system directories are
  only the Windows ones. A Unix list needs care: macOS temp lives under `/var/folders`
  and Homebrew under `/usr/local`, so "deny everything under `/var` or `/usr`" breaks
  normal work. Build it with the CI matrix (step 1), where it can be tested.

## What a vibecoder needs, in order

1. **Install in one line, undo in one line.** Prebuilt binaries per platform on
   GitHub releases (Windows x64/arm64, macOS universal, Linux x64/arm64, musl), plus
   `cargo install qx` and a thin `npx qx-install` that downloads the right binary.
   Then `qx setup` with no arguments detects every installed client and wires it;
   `qx setup --remove` restores the `.qx-bak` copies. Nothing else on the machine changes.
2. **See the saving without trusting a claim.** Count by default, locally only:
   bytes and estimated tokens in vs out per call, time saved vs a shell spawn, and
   how many cut outputs were later read from the spool (a cut that is often re-read
   is a cut that lost something). `qx gain` prints it; nothing leaves the machine.
3. **Quality the user can check.** Ship the replay corpus method as `qx eval`:
   run a folder of recorded command outputs through the digests and report the
   reduction plus a "kept" score (every error line, failing test name, file:line and
   final summary present in the digest). A digest that drops one of those fails CI.
   Publish the numbers per digest; a digest without a passing score stays off.
4. **Cover the clients they use.** Cursor (`~/.cursor/mcp.json`), Windsurf, Gemini
   CLI, Cline/Roo and VS Code (`.vscode/mcp.json`) are JSON MCP configs: the existing
   `jsonc` editor already writes them. Add each with its config test. For clients
   with a hook API (Claude Code today, Codex/Gemini when stable), add `qx hook <client>`.
5. **Make the hook the default path for Claude Code.** The model need not remember
   `qx`; deny/ask rules keep working, and a hook that fails or doubts prints nothing,
   so the original command runs. Add `QX_HOOK=off` as the kill
   switch and say it in `qx doctor`.
6. **Portable by design, Windows-fast where it matters.** On macOS/Linux spawns are
   cheap, so the pitch there is digests, guard and redaction. Run the test suite in
   CI on all three OSes; keep Windows-only speed paths behind `cfg(windows)` as now.

## Order of work (each step ships alone)

| step | change | proof it worked |
|---|---|---|
| 1 | CI: test on Windows, Linux, macOS; release workflow builds binaries | green matrix, release assets |
| 2 | default-on local stats in tokens, `qx gain` shows them | `qx gain` on a fresh install |
| 3 | `qx eval` + kept-score gate per digest | CI fails on a seeded lossy digest |
| 4 | setup for Cursor, Windsurf, Gemini CLI, Cline, VS Code | config round-trip tests |
| 5 | `npx qx-install` and `qx setup` auto-detect | clean VM: one line to working agent |
| 6 | README rewritten for strangers: 3-line quick start first, Windows tuning later | someone new installs without help |

## Rules that do not bend

- Lossless where it counts: errors, failing tests, paths with line numbers, the final
  summary, and exit codes always reach the model; everything cut is in the spool.
- Opt-out of every rewrite (`--raw`, `mode:"full"`, `QX_HOOK=off`) stays one switch.
- No telemetry leaves the machine. No account, no server.
- The guard is never weakened to raise a savings number.

## Not now

- A hosted dashboard, a plugin store listing, or per-model token pricing tables:
  local `qx gain` answers the question first.
- Rewriting rtk's job: when rtk is installed, qx builds on its rewrite instead of
  competing with it.
