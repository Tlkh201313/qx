# Map — repo layout, where each feature lives

Read this BEFORE exploring. Paths relative to repo root. For symbols/callers: `trail ask`, `trail callers <sym>`, `codegraph_explore`. Line counts left out on purpose (they rot).

qx = Rust 2021, single binary `qx.exe`, deps only `regex` + `serde_json`. Windows-first. CLI + MCP stdio server (`run`, `fs`) that runs commands fast, guards them, compacts output.

## Entry points (src/main.rs dispatch)
| CLI | Goes to |
|---|---|
| `qx <cmd>` / `-c "<line>"` / `proxy` | `exec.rs` via `run_cli` (flags `--raw --head --tail --timeout --cwd --shell`) |
| `qx mcp` / `serve` | `mcp.rs::serve` |
| `qx hook claude` | `hook.rs` |
| `qx setup [client]` | `setup/` |
| `qx deploy` | `deploy.rs` |
| `qx doctor` / `bench` | `doctor.rs` |
| `qx check "<cmd>"` / `policy` | guard dry run (`check_cmd` in main.rs → `guard.rs`) |
| `qx audit`, `gain`, `which` | main.rs |

## Features → files (src/)
| Feature | File |
|---|---|
| Safety gate: deny/warn rules, path expansion, shell-payload inspection | `guard.rs` |
| Secret redaction of output | `redact.rs` |
| Output compaction (ANSI, dedupe, head+tail, rescue buried errors) | `compact.rs` |
| Command-aware digests (git status/diff, tests, builds, grep, JSON) — runs before compact | `digest.rs` |
| In-process built-ins (ls cat grep sed rm cp …, decline → real exe) | `builtin.rs` |
| Process spawn, shell fallback, capture, timeout, job window | `exec.rs` |
| Find real exe, skip shims (Git cmd, npm.cmd, pip launchers) | `resolve.rs` |
| `npm run/test/start` without npm CLI | `npm.rs` |
| Python launchers/venv in one process | `python.rs` |
| Python fast-TLS stub (`CERTS_TLS_STUB`, `PYTHON_FAST_TLS_DIR`) | `exec.rs` |
| MCP server: `run` + `fs` tools, jobs, limits | `mcp.rs` |
| Per-client tool names/wording (Claude, Codex, OpenCode, dsh, Hermes, pi) | `client.rs` |
| `fs` tool ops: read/write/edit/glob/grep/list/stat, `FS_*` errors, atomic writes; lean (frameless) replies for Claude Code/Codex (`lean_read`); 49k reply cap (`CLIENT_REPLY_BYTES`, also drives `run` stream caps in `mcp.rs` `stream_caps`); scratch dir (`scratch_root`); grep context | `fsops.rs` |
| Live workspace file list for glob/grep (ReadDirectoryChangesW); folders git ignores are left out (`git_ignored_dirs` in `fsops.rs`, kept in `Snapshot.skipped`) | `index.rs` |
| Unknown `fs` option keys named in an `[qx] ignored (not fs options)` line (`KNOWN_KEYS`) | `fsops.rs` |
| Warm start of PowerShell, tool lookups, file cache | `warmup.rs` |
| Claude PreToolUse hook (routes Bash/PowerShell through qx) | `hook.rs` |
| Register qx in each client's config, byte-preserving | `setup/mod.rs` (+ `jsonc.rs`, `toml.rs` Codex, `yaml.rs` dsh/Hermes, `pi.rs`) |
| One canonical installed exe | `deploy.rs` |
| Slow-command diagnosis | `doctor.rs` |
| Hostile-text fuzz (no panic, no lost error) | `fuzz.rs` |

Tests live inline (`#[cfg(test)] mod tests` per file), no `tests/` dir. Most: guard, digest, exec, fsops.

## Other dirs
| Path | What |
|---|---|
| `README.md` | User docs: what it does, trust/safety, per-client MCP setup, measurements, design notes |
| `integrations/pi/` | pi extension bridge (`qx.js`, `bridge.test.mjs`, `smoke.mjs`), run by CI with node 22 |
| `.github/workflows/ci.yml` | windows-latest: clippy `-D warnings`, `cargo test --release --locked` (`QX_REQUIRE_ORACLE=1`), pi tests, MSRV 1.82 |
| `docs/` | `review/` full-review report, `superpowers/{specs,plans}` design + plans, `next-step-vibecoders.md` |
| `.trail/learnings/` | decisions, ruled-out paths (trail). `.trail/cache/` = generated per-file cards, don't edit |
| `.worktrees/`, `.superpowers/`, `scratch/`, `target/`, `target-dev/` | agent worktrees, SDD artifacts, scratch (gitignored), build output. Not source |

## Where state lives outside the repo
- Installed exe: `~/.cargo/bin/qx.exe` (what the live MCP server runs). Audit log: `%LOCALAPPDATA%\qx\audit.jsonl` (written by `guard.rs`, read by `qx audit`). Python stub: `%LOCALAPPDATA%\qx\python-fast-tls-vN`.
- Claude memory: see CLAUDE.md.

## Keep fresh
New file or moved feature → edit its row here in the same change. A wrong row costs more than a missing one.
