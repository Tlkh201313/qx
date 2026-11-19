# Goal

## North star (user, 2026-10-19)
Everything at ultra speed, highest quality, fewer LLM tokens by clearing only noise, never losing content or importance.

## Success measures (agreed by the user, 2026-11-09)
Measured with `python scripts/bench_mcp.py <abs path to qx.exe> <repo> [client]` (read-only calls over stdio, client `claude-code`). CI prints the table on every run; it does not fail on timings, because shared runners are noisy.

| Measure | Target | Baseline 2026-11-09 (release exe, this PC) |
|---|---|---|
| In-process call (built-in `run`, every `fs` op on this repo) | median under 2 ms | 0.2 to 1.7 ms (worst: `fs grep "let "` over `src`, 13 KB reply) |
| Spawned call (`git`, `cargo`, `node`) | within 10 ms of the bare process | `git --version` 22.9 ms through qx, 27.2 ms bare from Python; `git status` 26.4 vs 34.2; `cargo --version` 13.0 vs 33.3 |
| Search reply carries signal only | no match from a git-ignored folder, and the "not searched" note names them | enforced by test `search_skips_folders_git_ignores_and_says_so`; `fs grep "fn main"` here is 1,902 bytes, all in tracked files |
| Compaction never loses an error or a result | zero lost errors | enforced by `src/fuzz.rs` and the `compact`/`digest` tests |
| Suite | deterministic, 5 runs green at default threads | 431 pass, 0 fail, 2 ignored; also green at 8 threads |

Notes:
- The user's first proposal for the search reply was "under 1 KB for a whole-repo search". That number moves with the repo (more real text containing the query), so the enforced measure is the signal-only rule above; the byte count stays in the table for tracking.
- Slow rows, for reference, not targets: PowerShell line 40 ms, bash pipeline 140 ms (Git Bash start), `python -c pass` 61 ms. They are the cost of the other program or shell, not of qx.

## In scope now
Reply size and speed for `run` and `fs` on Windows; clients already set up (`qx setup`): Claude Code first.

## Out of scope
A built-in `fetch` op (advice: not now; it adds a network path and a new class of security risk). Running `a && b` and `a | b` inside qx: measured 2026-11-09, 15% of calls and about 9 ms each, not worth it (see `docs/review/2026-11-09-qx-quality-plan.md`).

## Open questions for the user
- Which clients matter most after Claude Code (Codex, OpenCode, dsh, Hermes, pi)? It decides where setup and reply-limit work goes.

## How agents use this
Check a change against the north star before building it. If it adds latency, drops information, or costs more tokens, say so. Run the bench before and after a change that touches a hot path.
