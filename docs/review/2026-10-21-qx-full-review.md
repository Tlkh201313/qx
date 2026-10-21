# qx full-codebase review - 2026-10-21

Method: baseline `cargo test --release` 225/226 (the 1 failure is msys `NtCreateDirectoryObject 0xC0000022` from the DSH sandbox, not code); clippy 9 warnings; 8 parallel read-only reviewers over 7 module groups, findings re-checked against the code where patched (each fix starts with a failing test).

Totals: 91 findings - 8 critical, 44 important, 39 minor. Disposition: Tn = patched in plan task n (docs/superpowers/plans/2026-10-21-qx-review-patches.md); deferred = recorded only.

| id | sev | location | comment | disposition |
|---|---|---|---|---|
| exec-1 | critical | src/exec.rs:653 | cmd_quote leaves a bare '"' unquoted: flips cmd quote parity, later '&' runs as a command (guard bypass) | T4 |
| guard-1 | critical | src/guard.rs:1347 | `2>&1` splits the pipeline: `curl url 2>&1 / sh` and `cat .env 2>&1 / curl -d @-` are allowed | T5 |
| guard-2 | critical | src/guard.rs:1342 | cmd.exe has no single quotes: `echo it's & rd /s /q C:\Windows` hides everything after the apostrophe | T5 |
| guard-3 | critical | src/guard.rs:576 | Command words not normalized for shell escapes: `@rd`, `r^d`, `C:\Win^dows`, PowerShell backtick continuation | T5 |
| guard-4 | critical | src/guard.rs:389 | Commands inside `{ }`, `try`, `foreach`, `ForEach-Object`, `if exist`, `for ... do` are never judged | T5 |
| guard-5 | critical | src/guard.rs:616 | `cmd /c` payload re-joined without quotes: `cmd /c rd /s /q "C:\Program Files"` is only a warning | T5 |
| npm-python-main-1 | critical | src/npm.rs:162 | fast_npx accepts path-like names: escapes node_modules\.bin and runs arbitrary exe invisible to the guard | T1 |
| text-pipeline-1 | critical | src/redact.rs:7 | KEYS list misses camelCase/no-underscore secret names (SecretAccessKey, secretKey, privateKey, accessKey) | T2 |
| builtin-resolve-1 | important | src/resolve.rs:186 | Bare names with .cmd/.bat (npm.cmd) never found on PATH | T9 |
| builtin-resolve-2 | important | src/builtin.rs:1146 | cp -r src/. dst nests instead of merging | T8 |
| builtin-resolve-3 | important | src/resolve.rs:368 | rustup shortcut ignores RUSTUP_TOOLCHAIN= env prefix | T9 (rustup env prefix finished in the follow-up commit) |
| builtin-resolve-4 | important | src/builtin.rs:971 | rm dangerous() checks process cwd not call cwd | T8 |
| builtin-resolve-5 | important | src/resolve.rs:751 | generic_shim drops quoted script arg / prefers node over exe | T9 |
| builtin-resolve-6 | important | src/resolve.rs:815 | git_bash() only knows Program Files paths | T9 |
| exec-2 | important | src/exec.rs:923 | qx's own inheritable stdin/stdout pipe handles leak into every child and surviving daemon | deferred - needs Win32 FFI + integration harness; risk > gain now |
| exec-3 | important | src/exec.rs:1018 | Output decoding is UTF-8-only; OEM/ANSI code-page output (cmd, ipconfig, tree, dir, where) becomes U+FFFD | deferred - OEM decoding shares decode() with file reads; needs a separate process-output path |
| exec-4 | important | src/exec.rs:893 | Timeout/cancel path runs taskkill.exe synchronously, unbounded, before the instant Job terminate | deferred - not unit-testable; Job terminate already follows |
| exec-5 | important | src/exec.rs:687 | Reader threads outlive the call and keep buffering into the Capture after snapshot() emptied it | T4 |
| exec-6 | important | src/exec.rs:471 | program:"cmd" with /c uses Rust's \" quoting; only build_shell/cmd_exe uses the raw form the README says is required | T4 |
| exec-7 | important | src/exec.rs:923 | Concurrent spawns from MCP workers cross-inherit each other's pipe write ends (no spawn serialization) | deferred - same as exec-2; not deterministic |
| fsops-1 | important | src/fsops.rs:1745 | glob **/name matches mid-name (xname) | T7 |
| fsops-2 | important | src/fsops.rs:872 | write over non-UTF-8/UTF-16/CRLF/BOM file silently re-encodes | T7 |
| fsops-3 | important | src/fsops.rs:982 | edit CRLF adaptation only when old has newline; mixed endings unmatchable | T7 |
| guard-10 | important | src/guard.rs:106 | Unbounded wrapper/substitution recursion plus O(n^2) vec copies: long `sudo sudo ...` or `$($(...` aborts the server | T5 |
| guard-11 | important | src/guard.rs:318 | Documented 'force-killing critical Windows processes' only covers `taskkill /f`; Stop-Process/kill unchecked | T5 |
| guard-6 | important | src/guard.rs:1228 | Unresolved `$var`/`%var%` delete targets are treated as in-workspace paths; `$home` and plain-Windows `$HOME` unresolved | T5 (partial) |
| guard-7 | important | src/guard.rs:521 | substitutions() flips `single` on an apostrophe inside double quotes, hiding every later `$(...)` | T5 |
| guard-8 | important | src/guard.rs:830 | `find -L / -delete` (or -H/-P) only warns: starting-point scan stops at the first dash option | T5 |
| guard-9 | important | src/guard.rs:778 | Launchers `start`, `Start-Process`, `call`, `Invoke-Command` not peeled; xargs -a and sudo --user mis-parsed | T5 (partial) |
| mcp-client-1 | important | src/mcp.rs:556 | Call cancelled while queued/at EOF still starts (builtins run fully, programs spawn) | T6 |
| npm-python-main-2 | important | src/npm.rs:257 | Relative --cwd (CLI) breaks the npm/pnpm/npx fast paths: root '' , relative PATH/INIT_CWD, relative shim script paths | T1 |
| npm-python-main-3 | important | src/npm.rs:120 | decorate() overwrites PATH after command_for added Git usr\bin/mingw64\bin, so scripts that run git lose sh for hooks | T1 |
| setup-deploy-doctor-1 | important | src/setup/jsonc.rs:61 | remove_member returns Err when a parent object is absent, so --remove refuses and exits 1 on a no-op | T10 |
| setup-deploy-doctor-10 | important | integrations/pi/qx/bridge.js:77 | fail() keeps the partial stdout buffer and handlers are not child-scoped: a mid-line death wedges the bridge forever | T13 |
| setup-deploy-doctor-11 | important | src/doctor.rs:207 | Drift report has false positives it cannot clear and misses overriding entries (R2); README promises a tool_search check that does not exist | deferred - partial later; tool_search claim: doc fix |
| setup-deploy-doctor-2 | important | src/setup/jsonc.rs:78 | remove_member deletes the comment lines that follow the removed member's comma | T10 |
| setup-deploy-doctor-3 | important | src/setup/toml.rs:123 | Existing qx env keys written inline / flow-style are silently dropped on reinstall (R13) | T11 |
| setup-deploy-doctor-4 | important | src/setup/toml.rs:31 | Block range swallows trailing comments/blank lines that belong to the NEXT table (TOML, Hermes, dsh) | T11 |
| setup-deploy-doctor-5 | important | src/setup/yaml.rs:92 | dsh --remove deletes the shared `- insert:` wrapper when mcp-qx is the first of several items | T12 |
| setup-deploy-doctor-6 | important | src/setup/pi.rs:115 | Any read error on pi mcp.json is treated as 'file absent' and Install overwrites it with a fresh document | T13 |
| setup-deploy-doctor-7 | important | src/setup/toml.rs:13 | Header detection is exact-string: `[mcp_servers.qx] # note` or spaced headers are missed, a duplicate table is appended | T11 |
| setup-deploy-doctor-8 | important | src/setup/yaml.rs:353 | YAML editors rewrite every CRLF file to LF (R13: every byte outside the entry preserved) | T12 |
| setup-deploy-doctor-9 | important | integrations/pi/qx/bridge.js:89 | No 'error' handler on child.stdin: EPIPE / failed spawn becomes an uncaught exception in the pi process | T13 |
| text-pipeline-10 | important | src/redact.rs:98 | A BEGIN PRIVATE KEY header inside code or grep output swallows the next 200 lines permanently | T2 |
| text-pipeline-2 | important | src/redact.rs:184 | JSON embedded in a JSON string (backslash-escaped quotes) leaks every secret value | T2 |
| text-pipeline-3 | important | src/redact.rs:191 | Space-separated flags with a prefixed name (--auth-token x, --db-password x) are not masked | T2 |
| text-pipeline-4 | important | src/redact.rs:216 | Quadratic scans in mask_key_values: repeated key hits and whitespace runs in a long line | T2 |
| text-pipeline-5 | important | src/redact.rs:134 | URL password containing '@' (or & ; , /) is only partly masked | T2 |
| text-pipeline-6 | important | src/digest.rs:1034 | grep digest mis-splits bare rg/grep output when the content contains ':digits:' (e.g. hh:mm:ss) | T3 |
| text-pipeline-7 | important | src/mcp.rs:551 | Digest label keeps only the first 4 args, so --nocapture/-s/--json later in argv is missed and test output is dropped | T3 |
| text-pipeline-8 | important | src/digest.rs:635 | git status --ignored: ignored files are reported as untracked | T3 |
| text-pipeline-9 | important | src/mcp.rs:508 | fs tool results (read/grep/list) bypass redaction while run output is redacted | deferred - ruling: fs read must stay verbatim for edit; not redacted (documented) |
| builtin-resolve-10 | minor | src/builtin.rs:773 | head/tail UTF-16 per-line decoding | deferred |
| builtin-resolve-11 | minor | src/builtin.rs:1259 | touch dir Access denied | deferred |
| builtin-resolve-12 | minor | src/builtin.rs:1219 | mv across volumes leaves partial tree | deferred |
| builtin-resolve-13 | minor | src/resolve.rs:567 | PATHEXT .js/.vbs/.msc classified Direct | deferred |
| builtin-resolve-7 | minor | src/builtin.rs:1409 | seq fractional step drops last element | deferred |
| builtin-resolve-8 | minor | src/builtin.rs:335 | ls file operand/-R/early return output loss | deferred |
| builtin-resolve-9 | minor | src/builtin.rs:1351 | realpath UNC, basename/dirname roots | deferred |
| exec-10 | minor | src/exec.rs:318 | write_atomic temp name uses only the PID, so MCP worker threads race on the same temp file | deferred |
| exec-8 | minor | src/exec.rs:691 | Capture keeps only the first 16 MiB; the tail (final error/summary) of huge output is lost for good | deferred |
| exec-9 | minor | src/exec.rs:970 | run_chain spawns the next step with an exhausted budget and drops earlier output on a mid-chain spawn error | deferred |
| fsops-10 | minor | src/fsops.rs:1340 | grep skips UTF-16 BOM files as binary | deferred |
| fsops-11 | minor | src/fsops.rs:1428 | list of empty dir returns empty text | deferred |
| fsops-12 | minor | src/fsops.rs:1809 | No tests for confinement/encoding paths | deferred |
| fsops-4 | minor | src/fsops.rs:333 | write creates parent dirs before VCS-protection check | deferred |
| fsops-5 | minor | src/fsops.rs:1124 | grep include rejects brace globs | deferred |
| fsops-6 | minor | src/fsops.rs:1525 | glob/grep silently skip build/vendor dirs with no notice | deferred |
| fsops-7 | minor | src/fsops.rs:241 | No Windows reserved-name/ADS validation on write | deferred |
| fsops-8 | minor | src/fsops.rs:595 | atomic replace drops file metadata | deferred |
| fsops-9 | minor | src/fsops.rs:1119 | grep multi-line pattern gives No matches | deferred |
| guard-12 | minor | src/guard.rs:931 | README says SQL UPDATE without WHERE is denied; code only warns, and one WHERE masks other statements | deferred |
| mcp-client-2 | minor | src/mcp.rs:89 | Batch with tools/call: refusal replies skip non-call items, not an array | deferred |
| mcp-client-3 | minor | src/mcp.rs:523 | Wrong-typed cwd/timeout_ms/program silently defaulted (rm runs in qx cwd) | T6 |
| mcp-client-4 | minor | src/mcp.rs:737 | Builtins ignore timeout_ms and cancel flag | deferred |
| mcp-client-5 | minor | src/mcp.rs:59 | Input line length unbounded | deferred |
| mcp-client-6 | minor | src/mcp.rs:970 | Missing serve_io tests for R8/robustness | deferred |
| mcp-client-7 | minor | src/mcp.rs:103 | active slot released after reply written: spurious busy | deferred |
| mcp-client-8 | minor | src/client.rs:128 | CLAUDECODE with any value selects Claude profile | T14 |
| npm-python-main-10 | minor | src/npm.rs:303 | npm_ guard is case-sensitive and the .npmrc key check misses array keys, though Windows env is case-insensitive | deferred |
| npm-python-main-11 | minor | src/python.rs:149 | pyvenv.cfg home may be empty/relative: base.is_file() then resolves against qx's cwd | deferred |
| npm-python-main-12 | minor | src/python.rs:17 | Cached launcher resolution keeps a stale shebang interpreter for the life of an MCP server | deferred |
| npm-python-main-4 | minor | src/npm.rs:465 | npm_command exported as 'run' (real npm: 'run-script'); pnpm path exports none although pnpm sets run-script | deferred |
| npm-python-main-5 | minor | src/main.rs:289 | --timeout in stream mode restarts for every npm pre/main/post step instead of one shared budget | deferred |
| npm-python-main-6 | minor | src/main.rs:157 | Flags and arguments after -c "<cmd>" are silently dropped (e.g. --cwd runs in the wrong directory) | deferred |
| npm-python-main-7 | minor | src/main.rs:142 | Invalid numeric flag values silently become defaults and swallow the next token; --timeout 0 kills at once | deferred |
| npm-python-main-8 | minor | src/main.rs:56 | std::env::args() panics (exit 101) on a non-Unicode argument | deferred |
| npm-python-main-9 | minor | src/npm.rs:108 | direct_step resolves bare names unlike cmd.exe: cwd not searched first, and Git coreutils fallback makes scripts pass that npm would fail | deferred |
| setup-deploy-doctor-12 | minor | src/setup/jsonc.rs:150 | UTF-8 BOM makes every JSON(C) client config unusable ('not an object'); inserts also mix LF into CRLF files | deferred |
| text-pipeline-11 | minor | src/compact.rs:95 | Lone-CR progress frames form one huge line and the 4000-char head-only cut drops the final status/error | deferred |
| text-pipeline-12 | minor | src/digest.rs:122 | keep_stderr compacts stderr but discards the cut flag, so truncated stderr is never spooled | deferred |

## Execution record

Branch `review/patches` (worktree `.worktrees/review`), based on `daffb23`. 14 plan tasks plus Task 9b (rustup env prefix) and one final-review fix pass; every fix started with a test that failed (a compile failure counts only where the function did not exist yet). Final state: `cargo test --release` 292 passed, 0 failed; `cargo clippy --release --all-targets` 0 warnings.

### Final independent review (0 Critical, 7 Important, 10 Minor)
Reproduced by running a probe build, then fixed test-first in one pass (commit `final review fixes`):
- I1 guard work multiplied per nesting level - alternate_readings_do_not_multiply_the_work_per_nesting_level RED->GREEN (shared per-check budget of 2000 line readings, exhaustion denies), suite green.
- I2 quadratic redaction on code-classed values - values_classed_as_code_do_not_make_the_scan_quadratic RED->GREEN (classifier sees the first 256 bytes of a value).
- I3 prefixed private-key dumps leaked - a_private_key_dumped_with_a_per_line_prefix_is_still_hidden RED->GREEN (block opens when the header ends its line, or the next line's last token is key material).
- I4 toml remove refused for inline env - codex_remove_works_when_install_would_refuse_an_inline_env RED->GREEN (the inline-env refusal applies to Install only).
- I5 cmd control-flow - cmd_bodies_in_parentheses_and_comparisons_are_judged RED->GREEN (paren-aware splitting, six word comparisons, leading at-sign and paren peeled).
- I6 leading redirections - a_command_behind_a_leading_redirection_is_judged RED->GREEN (leading redirects peeled; the cmd reading also runs for lines containing an ampersand-greater-than).
- I7 Start-Process single-string ArgumentList - start_process_with_a_single_string_argument_list_is_peeled RED->GREEN.
- M5 (re-graded Important) cmd /c with separate arguments keeps variable expansion - cmd_c_with_separate_arguments_still_expands_variables RED->GREEN.

### Deferred minors from the final review
- M1 kill alias / positional Stop-Process pid; M2 apostrophe cmd-reading warns on quoted remote commands (strict mode denies); M3 write re-applies CRLF/BOM so removing them needs the native tool, LF-file new_string loses deliberate CRs; M4 write refuses a file with one stray invalid byte; M6 jsonc backward comma scan vs // comment; M7 URL userinfo now spans & (fail-closed false positive); M8 program:'' with command passes the not-both check; M9 bridge keeps old child after EPIPE; M10 echo a ^& rd ... literal ampersand denied.

### Rulings made while executing (decisions taken on the owner's behalf)
- Ruling: user asked for review -> comments -> patch in one instruction and chose inline execution, so the plan is executed without a separate plan-review pause — cost if wrong: patches land on a branch the user can discard.
- Ruling: baseline has 1 environmental failure (mcp::tests::a_declined_builtin_reaches_git_coreutils, msys NtCreateDirectoryObject 0xC0000022 in the DSH sandbox); green = no other failures — cost if wrong: a real regression in that test would be masked.
- Ruling: foreign commit b43ee68 (other actor, 00:45) fixed some setup/mcp findings on this branch mid-review; Tasks 6 and 10-13 re-verify each finding test-first against the new code (a test that already passes = finding already fixed, skipped) and every commit stages explicit paths only (untracked .bench/ .probe/ are not mine) - cost if wrong: a stale finding gets a redundant test.
- Ruling: another session is actively editing src/exec.rs in the main tree (uncommitted, build broken mid-refactor), so this plan now runs in git worktree .worktrees/review on branch review/patches (from a9ba8e2); main tree and its exec.rs are untouched - cost if wrong: Task 4 (exec.rs) must be rebased onto that session's exec.rs changes.
- Task 1: Ruling: absolutizing lives in npm::start_dir (all three entry points) instead of main.rs/mcp.rs - one place covers every caller - cost if wrong: none.
- Task 2: Ruling: URL userinfo keeps , ; | as terminators (only & and the first-@ stop were dropped) because dropping them would mask prose like 'url;mail=a@b' - a password containing , ; or | stays unmasked - cost if wrong: leak of such a password.
- Task 2: Ruling: PEM block mode opens only for a bare header line or when the next line is base64/Proc-Type; the header line itself is still replaced - cost if wrong: a quoted multi-line key whose next line is not base64 leaks its body.
- Task 3: Ruling: run_label extracted as a free fn so the label rule is unit-testable; plain args still stop at four, flags never do (cap 400 chars) - cost if wrong: a very flag-heavy command line gets a longer label.
- Ruling: main-tree session has uncommitted edits in exec.rs, guard.rs, fsops.rs, mcp.rs, main.rs (overlapping Tasks 4-7), so Tasks 8,9,10-14 run first and Tasks 4-7 run last, rebased onto whatever that session has committed by then - cost if wrong: Tasks 4-7 may need conflict resolution or re-verification.
- Task 8: Ruling: mv left unchanged - 'mv src/. dst' has no useful meaning (cannot move the cwd entry), so only cp -r got the contents form - cost if wrong: none.
- Task 9: Ruling: builtin-resolve-3 (rustup shortcut vs per-call RUSTUP_TOOLCHAIN= prefix) is moved to the final batch with Tasks 4-7 because the fix must reach the build_command_at call sites in exec.rs/mcp.rs/main.rs that another session is editing - cost if wrong: that finding stays open until then.
- Task 9: Ruling: runnable_bare accepts only .cmd/.bat (plus exe/com) when PATHEXT lists them, not every PATHEXT extension, so .js/.vbs/.msc names do not newly resolve to a Direct target that CreateProcess cannot start - cost if wrong: a name that already carries .ps1 stays unresolved.
- Task 10: Ruling: remove_member treats an absent or non-object parent as nothing-to-remove (Ok unchanged); a malformed document is still an error - cost if wrong: a config whose parent has an unexpected type is reported unchanged instead of refused.
- Ruling: mcp::tests::eof_cancels_running_and_queued_calls_promptly joins the known environmental flakes - it needs a live 'ping -n' child and this sandbox prints 'PING: transmit failed. General failure.', so the call can end before the test sees it in INFLIGHT; it passed alone 2 of 3 runs and none of my changes touch that path - cost if wrong: a real EOF-cancel regression would be waved through as a flake.
- Task 11: Ruling: an inline or dotted env in the qx table is refused with the paste snippet (R13) instead of being parsed and kept - parsing TOML inline tables by hand is the riskier edit - cost if wrong: such users must paste the snippet themselves.
- Task 12: Ruling: only the doctor false positive from setup-deploy-doctor-11 (doubled quotes in yaml_scalar) is fixed; scanning projects.*.mcpServers.qx for overriding entries and the README tool_search claim stay deferred - larger design and doc questions - cost if wrong: doctor can still miss an overriding per-project entry.
- Task 13: Ruling: no request timeout was added to the bridge (setup-deploy-doctor-10 suggested one) - the signal-based cancel already exists and a timeout would need a pi-side policy - cost if wrong: a hung qx child blocks a call until pi aborts it.
- Ruling: the other session committed daffb23 (route every line to the shell that can parse it; touches exec.rs guard.rs fsops.rs main.rs mcp.rs README) on review/full-codebase, so review/patches was rebased onto it cleanly (commit ids in Tasks 1-13 above are pre-rebase) and Tasks 4-7 plus the rustup item now re-verify against that code - cost if wrong: a finding that daffb23 already fixed shows up as a test that passes immediately and is skipped.
- Task 4: Ruling: exec-3 (OEM-codepage output decoding) is deferred - decode()/decode_owned() are shared with file reads, so an OEM fallback would garble Latin-1 files; it needs a separate process-output path - cost if wrong: cmd built-ins such as 'tree' still print U+FFFD on OEM consoles.
- Task 4: Ruling: exec-2/exec-4/exec-7 (inheritable pipe handles, taskkill ordering, concurrent spawn handle leaks) stay deferred - Win32 FFI that no unit test can pin deterministically - cost if wrong: a daemon can still pin the stdio pipes until it exits.
- Task 5: Ruling: cmd-vs-sh quoting is judged both ways whenever the line has an apostrophe (deny wins) instead of asking which shell will run it - the guard runs before the shell is chosen - cost if wrong: a line that is dangerous only under cmd's reading of an apostrophe is denied even when bash runs it.
- Task 5: Ruling: guard-6 beyond home-variable spellings (a delete target that is still an unresolved variable after expansion) and guard-12 (README says SQL UPDATE without WHERE is denied, code warns) stay deferred - the first would turn many legitimate temp-dir cleanups into warnings and needs a policy call, the second is a doc/behavior decision - cost if wrong: a delete whose target comes from an assigned variable on the same line still passes.
- Task 5: Ruling: a titled start command cannot tell the title from the program once the tokenizer drops quotes, so it is judged as program 'title' - cost if wrong: that one spelling still hides its payload.
- Task 6: Ruling: the cancelled-before-start check also fires when SHUTDOWN is set, which is the intended 'no command may start after the client left' rule; five existing tests that call tools now hold server_lock() so a neighbouring EOF test cannot cancel them - cost if wrong: none in production, tests serialize slightly more.
- Task 6: Ruling: program together with a non-empty command is now an error (the command used to be silently ignored); an empty command string next to a program is still fine - cost if wrong: a client that always sends both non-empty fields now gets an error naming the problem.
- Task 7: Ruling: write over a file that is not valid UTF-8 is refused with FS_ENCODING (fallback to the native write tool) rather than converted - the model only saw a lossy view - cost if wrong: such files need the native tool to be rewritten whole.
- Task 7: Ruling: the minor fsops findings (parent dirs created before the .git check, brace include for grep, silent skipped dirs, reserved names, metadata loss, grep on UTF-16, empty-dir list text) stay deferred - none loses data in the common case.
- Final: Ruling: M5 (cmd /c with several args turns %VAR% into %^VAR%) re-graded Minor -> Important by effect (output silently changes for echo %USERNAME%) and enters the fix pass - cost if wrong: one extra small fix.
- Final: Ruling: the guard work budget (2000 line readings per top-level check) makes an absurdly complex but harmless line a deny - cost if wrong: a generated script with thousands of command substitutions in one call is refused with 'too complex to judge safely'.

