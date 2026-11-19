# qx — Claude hub

Fast Windows command executor + MCP server (Rust). Global rules (qx/rtk/fusion) load from `~/.claude/CLAUDE.md`; not repeated here.

## Read on demand (these files are NOT auto-loaded; open only the one you need)
| When | Open |
|---|---|
| Before exploring code | `Map.md`, then `trail ask "<task>" --source` / codegraph |
| Before build, test, bench, deploy, or when something weird fails | `Gatcha.md` |
| Before finishing, or after being corrected | `Mistake.md` |
| Session start / end | `Handoff.md` (TEMP, may be wiped) |
| Before deciding direction or scope | `Goal.md` |

## One home per fact (no copies)
| Fact | Home |
|---|---|
| Layout, feature→file | `Map.md` |
| Trap, limit, machine quirk | `Gatcha.md` |
| Agent error + rule | `Mistake.md` |
| In-flight state | `Handoff.md` |
| Direction, success measures | `Goal.md` |
| Decision / ruled-out approach | `.trail/learnings/` (`trail note`) |
| User preferences, machine/tool history, cross-repo | memory: `~/.claude/projects/C--Users-USER-projects-qx/memory/MEMORY.md` (index, one line each) |

Memory points to repo files; repo files do not point to memory. If a memory entry becomes a repo fact (applies to anyone working here), move it to the matching MD and delete the memory.

## Rules that are not obvious
- Edit loop: `cargo test --no-run` (dev profile, opt-level 2: ~2 s rebuild, ~17 s suite; release is 60-73 s per build). `cargo build --release` only for the CLI/e2e/deploy exe (`target\release`). Test: copy the test exe out of `target\` (Gatcha.md). Lint = CI: `cargo clippy --release --all-targets --locked -- -D warnings`.
- No commit, push or deploy unless the user asks. Other agents edit this tree: `git diff` first.
- Guard blocks: report, never rephrase or `QX_GUARD=off`. Probe with `qx check`.
- New or moved feature → update `Map.md` in the same change.
- Output hidden by hooks → write to a file and Read it.

## Keep it small
Each MD: one line per item, delete what code or tests now enforce, newest first where dated. This file stays under ~40 lines; it routes, it does not explain.
