# Map — repo layout, where each feature lives

Read this BEFORE exploring. Paths relative to repo root. For symbols/callers: `trail ask`, `trail callers <sym>`, `codegraph_explore`. Line counts left out on purpose (they rot).

qx = Rust 2021, single binary `qx.exe`, deps only `regex` + `serde_json`. Windows-first. CLI + MCP stdio server (`run`, `fs`) that runs commands fast, guards them, compacts output.

## Entry points (src/main.rs dispatch)
| CLI | Goes to |
|---|---|
| `qx <cmd>` / `-c "<line>"` / `proxy` | `exec.rs` via `run_cli` (flags `--raw --head --tail --timeout --deadline --cwd --shell`; `flag_num`: bad value exits 2, `--timeout` takes 30s/5m/1h) |
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
| Safety gate: deny/warn rules, path expansion (8.3 names, `~user`, admin shares, cygdrive, braces, streams), launcher peeling, `for`/`forfiles` bodies per item, inline perl/ruby/php/python/node code, home data folders | `guard.rs` |
| Secret redaction of command output (key=value, tokens, Authorization schemes, .netrc, cookies, URL users); `fs` returns files unredacted | `redact.rs` |
| Output compaction (ANSI, dedupe, head+tail, rescue buried errors) | `compact.rs` |
| Command-aware digests (git status/diff, tests, builds, grep, JSON) — runs before compact; a red run keeps unknown column-0 and failure-word lines in the room left; long grep hits keep head+tail and spool | `digest.rs` |
| In-process built-ins (ls cat grep sed rm cp …, decline → real exe; `sed -n A,Bp` streams files past `READ_CAP`) | `builtin.rs` |
| Process spawn, shell fallback, capture, timeout, job window; Windows paths sent to bash get `/` (`slash_windows_paths`), and a POSIX tool given a `*` glob goes to bash (`posix_tool_backslash_glob`); env prefix per chain step (`chain_starts`, `assignment_word`); `PATH=` prefix resolves over that PATH (`resolve::resolve_in`); `timeout N`/`rmdir /s`/`find dir` routing (`posix_flags`); `.cmd` shim refuses a newline argument | `exec.rs` |
| Find real exe, skip shims (Git cmd, npm.cmd, pip launchers) | `resolve.rs` |
| `npm run/test/start` without npm CLI | `npm.rs` |
| Python launchers/venv in one process | `python.rs` |
| Python fast-TLS stub (`CERTS_TLS_STUB`, `PYTHON_FAST_TLS_DIR`) | `exec.rs` |
| MCP server: `run` + `fs` tools, jobs, limits; `run` keys (`RUN_KEYS`, unknown key is an error), `env` object, `kill_on_timeout`, protocol versions (`PROTOCOL_VERSIONS`), fs calls cancellable (`cancellable`), job result kept for every poll, kill says "stopped on request" | `mcp.rs` |
| Per-client tool names/wording (Claude, Codex, OpenCode, dsh, Hermes, pi) | `client.rs` |
| `fs` tool ops: read/write/edit/glob/grep/list/stat, `FS_*` errors, atomic writes; lean (frameless) replies for Claude Code/Codex (`lean_read`); 49k reply cap (`CLIENT_REPLY_BYTES`, also drives `run` stream caps in `mcp.rs` `stream_caps`); scratch dir (`scratch_root`); grep context (streamed window, `context_lines`); write/edit refuse `guard::write_protected` paths (`FS_PROTECTED`); glob retries with `[` literal; git runs with `core.fsmonitor=false` | `fsops.rs` |
| Live workspace file list for glob/grep (ReadDirectoryChangesW); folders git ignores are left out (`git_ignored_dirs` in `fsops.rs`, kept in `Snapshot.skipped`; a new folder asks `git check-ignore`, 16 per tree); a `.gitignore`/`info/exclude` change or an 8.3 alias rebuilds it; files named like skipped folders are indexed | `index.rs` |
| Unknown `fs` option keys named in an `[qx] ignored (not fs options)` line (`KNOWN_KEYS`) | `fsops.rs` |
| Warm start of PowerShell, tool lookups, file cache | `warmup.rs` |
| Claude PreToolUse hook (routes Bash/PowerShell through qx); chained captured stages share one end time (`Budget`, `--deadline`) | `hook.rs` |
| Register qx in each client's config, byte-preserving; writes through symlinks (`write_target`); qx hook inside a user's hook group edited in place (`is_mixed_qx_group`); Remove prunes containers qx emptied (`jsonc::prune_empty`) and deletes a config left as `{}` (`apply`); no `.qx-bak` of qx's own files (`leaves_nothing_of_the_users`, `pi::is_generated`); dsh prune names what it deletes; e2e round trips per client (`e2e_*_roundtrip`) | `setup/mod.rs` (+ `jsonc.rs`, `toml.rs` Codex, `yaml.rs` dsh/Hermes, `pi.rs`) |
| One canonical installed exe; old copy deleted at once when free; `sweep_aside` also removes `.new-*` copies created over 10 min ago | `deploy.rs` |
| Slow-command diagnosis | `doctor.rs` |
| Hostile-text fuzz (no panic, no lost error line for any exit code) | `fuzz.rs` |

Tests live inline (`#[cfg(test)] mod tests` per file), no `tests/` dir. Most: guard, digest, exec, fsops.

## Other dirs
| Path | What |
|---|---|
| `README.md` | User docs: what it does, trust/safety, per-client MCP setup, measurements, design notes |
| `integrations/pi/` | pi extension bridge (`qx.js` schema mirrors the server's `run`/`fs` keys, tested in `bridge.test.mjs`; `smoke.mjs`), run by CI with node 22 |
| `.github/workflows/ci.yml` | windows-latest: clippy `-D warnings`, tests in release and dev profile (`QX_REQUIRE_ORACLE=1`), pi tests, bench gated at 10 ms in-process median (`QX_BENCH_MAX_INPROC_MS`), MSRV 1.82 build |
| `docs/` | `review/` full-review report, `superpowers/{specs,plans}` design + plans, `next-step-vibecoders.md` |
| `.trail/learnings/` | decisions, ruled-out paths (trail). `.trail/cache/` = generated per-file cards, don't edit |
| `.worktrees/`, `.superpowers/`, `scratch/`, `target/`, `target-dev/` | agent worktrees, SDD artifacts, scratch (gitignored), build output. Not source |

## Where state lives outside the repo
- Installed exe: `~/.cargo/bin/qx.exe` (what the live MCP server runs). Audit log: `%LOCALAPPDATA%\qx\audit.jsonl` (written by `guard.rs`, read by `qx audit`). Python stub: `%LOCALAPPDATA%\qx\python-fast-tls-vN`.
- Claude memory: see CLAUDE.md.

## Keep fresh
New file or moved feature → edit its row here in the same change. A wrong row costs more than a missing one.
