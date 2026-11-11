# Gatcha — traps in this repo and on this machine

Not-obvious things that bite. Each line: trap → what to do. Read before build/test/bench/deploy. Add a line when something wastes >10 min. A trap fixed in code (and tested) gets deleted here. Agent behaviour errors go in Mistake.md, not here.

## Build / test
- Test exe in `target\` gets a Low Mandatory Level label (seen 2026-10-24, still true 2026-11-02) → every file create fails `os error 5`, ~92 false test failures. Copy it out and run from repo root: `cp target/release/deps/qx-<hash>.exe <scratch>/probe/qxt.exe && <scratch>/probe/qxt.exe --test-threads=4`. Don't create test files named `nul.*`/`con.*` (Windows device names).
- `cargo test --release` does NOT relink `target\release\qx.exe` → e2e checks after it run a stale exe. `cargo build --release` first.
- Free RAM ~1 GB: full `cargo test` in background gets killed. Use `-j 1`; run single tests from the built exe.
- Last full run from a copied exe (2026-11-11, dev profile): 535 passed, 0 failed, 2 ignored, ~22 s. Run it with cwd = repo root (the `builtin` tests read repo files). More failures than that on a clean tree = the `target\` label above or a stuck warm-PowerShell test, not your change. CI sets `QX_REQUIRE_ORACLE=1` (GNU grep/sed parity tests must run, not skip).
- rustc overflows its stack on `guard.rs` (exit 0xc00000fd) → build with `RUST_MIN_STACK=268435456`.
- Worktrees build the same test exe name (`qx-<hash>.exe`) → a shared `target` runs another tree's code. Give each tree its own `CARGO_TARGET_DIR` and check `--list` names the test you added.
- PATH differs: Git Bash puts Git `usr\bin` before System32, MCP server sees plain Windows PATH (`sort`, `find` differ). Run PATH-sensitive tests under both.
- Other agents edit this tree too, and a commit of theirs can sweep in your uncommitted edits. `git diff` before building on it; diff HEAD against your tested copy.

## Live server / deploy
- Live MCP `qx` runs the OLD `~/.cargo/bin/qx.exe` until the client restarts. Never overwrite it while serving; use `qx deploy` from a copy outside `target\`. New code: test with `target\release\qx.exe` CLI.
- Rollback = `~/.cargo/bin/qx.exe.old-<ts>` (swept by next deploy). Keep a durable copy first.

## Python stub
- Edit to the TLS stub text → bump `N` in `PYTHON_FAST_TLS_DIR` (`python-fast-tls-vN`, `src/exec.rs`), or old stubs stay cached.
- `from pip._vendor import truststore` must stay FIRST in `CERTS_TLS_STUB` (`src/exec.rs`; unit test asserts) or pip hits `RecursionError`. Verify with `pip download --no-cache-dir --no-deps six==1.16.0`; `pip index versions` can pass from cache.

## Measuring
- Bench: `python scripts/bench_mcp.py <abs exe> <repo>`; targets in `Goal.md`. Python MCP bench: never `Popen(bufsize=0)` (1 byte/syscall, fakes +1 ms/KB). Pass an absolute exe path.
- Machine speed swings 2-10x (AV, RAM). A/B back to back in one run, same inputs, before calling a regression.
- Comparing hook speed between two builds: put each as `qx.exe` first on PATH; one not on PATH bails early and looks 2x faster.

## Tooling that hides output
- Claude Code hooks elide output ("sections elided", "body elided"), even for `.txt` copies. For exact bytes: write to a file, then `awk '{printf "%d| %s\n", NR, $0}' f | sed -n 'A,Bp' > out.log` and Read `out.log`.
- Git Bash: `\\` typed in a command reaches bash as one `\`. In patch scripts build escapes with `chr(92)` and check with `od -c`. Native Python can't open `/c/Users/...`; use `C:/Users/...`.

## qx fs limits (open, do not claim otherwise)
- Writes/edits outside cwd refused (`FS_OUTSIDE_WORKSPACE`), e.g. `~\.claude` → use native Write/Edit. Exception: `%TEMP%\claude\...` (the session scratchpad) is allowed (`scratch_root`). `write` over an existing file needs a prior qx `read`; `edit` does not (its unique exact match is the check).
- Reader can briefly see an edited file missing (ENOENT ~9% under a hammer loop): `ReplaceFileW` kept to preserve ADS/ACLs.
- Walk stops at `MAX_WALK_DEPTH` (64) with no notice. Names with unpaired surrogates created after server start are missing from glob/grep. `read` of invalid UTF-8 shows U+FFFD silently (edit/write refuse `FS_ENCODING`).
- fs grep is a regex (invalid regex falls back to text with a note; `literal:true` forces text), skips `node_modules target dist build vendor .git …` and, in a git repo, every folder git ignores (named in the "not searched" note; `all=true` includes them); shell `grep` built-in does not skip them.

## Guard
- Probe with `qx check "<cmd>"` only. Never run a destructive command at a real path (see Mistake.md 2026-10-19). Don't set `QX_GUARD=off`; report `[qx guard] BLOCKED` to the user.
