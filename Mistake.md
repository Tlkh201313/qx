# Mistake — what agents got wrong here, and the rule that prevents it

Newest first. One entry = date | what happened | rule. Add one when you (or the user) catch an error that a rule would have prevented. Delete an entry once a test or guard enforces it in code. Environment traps without a wrong action go in Gatcha.md.

- 2026-10-25 | `fs grep ignore_case` was silently ignored: the `fast_regex` pre-filter dropped the case flag (found in the ultra audit, fixed) | Every option flag needs a test that flips it on a fixture where the answer differs.
- 2026-10-24 | A 12 s Read looked like a regression; it was machine load | A/B back to back in one run and re-apply to confirm. Time tools solo: parallel batches inflate durations.
- 2026-10-23 | Ran e2e check after `cargo test`; exe was stale, hook "looked broken" | `cargo build --release` before any e2e.
- 2026-10-20 | Benchmarked MCP with `Popen(bufsize=0)`: reported a 20 ms cost, real ~2 ms | Never unbuffered; cross-check with a direct spawn in the same run.
- 2026-10-19 | Ran OLD `qx.exe` with `rm -rf ..\x40\Windows` (real path) to show a guard bypass; only non-admin saved C:\Windows | Guard probes use `qx check`, or nonexistent leaf paths verified absent. Never a real system path, even old-vs-new.
- 2026-10-19 | Python TLS stub imported standalone `truststore` first → pip `RecursionError`, hidden by pip cache | Keep vendored import first; test with `--no-cache-dir`.
