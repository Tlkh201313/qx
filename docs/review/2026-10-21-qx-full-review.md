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
| builtin-resolve-3 | important | src/resolve.rs:368 | rustup shortcut ignores RUSTUP_TOOLCHAIN= env prefix | T9 |
| builtin-resolve-4 | important | src/builtin.rs:971 | rm dangerous() checks process cwd not call cwd | T8 |
| builtin-resolve-5 | important | src/resolve.rs:751 | generic_shim drops quoted script arg / prefers node over exe | T9 |
| builtin-resolve-6 | important | src/resolve.rs:815 | git_bash() only knows Program Files paths | T9 |
| exec-2 | important | src/exec.rs:923 | qx's own inheritable stdin/stdout pipe handles leak into every child and surviving daemon | deferred - needs Win32 FFI + integration harness; risk > gain now |
| exec-3 | important | src/exec.rs:1018 | Output decoding is UTF-8-only; OEM/ANSI code-page output (cmd, ipconfig, tree, dir, where) becomes U+FFFD | T4 |
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

