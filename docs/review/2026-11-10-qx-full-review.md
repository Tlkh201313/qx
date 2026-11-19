# qx full review, 2026-11-10: findings, ratings and fix plan

## Context

Scope: a full review of qx at HEAD ec4be11 (v0.9.0, branch docs/readme-rewrite), every area rated red/amber/green
with tests as proof, then a plan for every fix. The review changed nothing in the repo: all probes ran
against a copy of the release exe in the session scratch folder, guard cases through `qx check` only, setup and
deploy only against scratch home folders.

I read every `src/` file, `src/setup/*`, `scripts/`, `integrations/pi/`, `.github/workflows/ci.yml` and the docs
myself. The Haiku scouts only mapped locations. Every finding below has a case I ran (input, then wrong result).
Items from the 11-08 and 11-09 reviews are not repeated. Ideas with no failing case were dropped (listed at the end).

Baseline before any change: 431 tests pass, 2 ignored, 6 runs all green (9.4 to 10.6 s); clippy clean;
in-process calls 0.2 to 1.4 ms (target under 2 ms); `git --version` through qx 19.4 ms against 26.1 ms bare
(target within 10 ms).

**Totals: 6 must-fix, 34 should-fix, 45 nice-to-fix.** Red areas: guard, shell choice, fs write, digest.

## Ratings (tests as proof)

| area | RAG | why | tests that prove it today | gap |
|---|---|---|---|---|
| guard (`guard.rs`) | RED | 4 bypasses reach a deny target: G1, G2, G3, G11 | 58 unit tests, fuzz target | no test for `/switch` words, 8.3 name of a new file, `for` loops, `~name`, admin shares, launchers, rename |
| exec: shell choice | RED | E1: a Windows path sent to Git Bash loses its backslashes; `rm -rf .\cache` deletes `.cache` | 38 tests in `exec.rs` | no test runs a backslash path through the bash route |
| exec: spawn, shims, env | AMBER | E2 (arguments dropped after a newline), E6 | shim quoting tests | newline in a `.cmd` argument; env prefix per chain step |
| fs write / edit | RED | FS7: `fs write` into `.ssh` and Startup works, the guard denies the same write | write/edit/observe tests | no test ties `fs` to `guard::write_protected` |
| fs read | GREEN | no finding | read window, encoding, NUL tests | 8 `FS_*` codes have no asserting test |
| fs grep / glob (walk) | AMBER | FS6 (a search runs a repo's `core.fsmonitor` command), FS3 (637 MB for one context line), FS2 | `search_skips_folders_git_ignores_and_says_so`, GNU parity tests | hostile repo config, context on a large file, file path + `**` include |
| index (`index.rs`) | AMBER | IX1, IX2, IX3: index and walk give different answers | 1 test | almost untested; no index-against-walk test |
| built-ins | AMBER | B1: `sed -n 1,3p` reads a 300 MB file whole | 29 tests, parity tests | large-file sed |
| digest | RED | DG1, DG5, DG6: a failing package, the real error line and the matched text are dropped (DG6 with no note) | 49 tests, fuzz target | no fixture for go multi-package, node:test, mocha, log line before a traceback, long hit |
| redact | AMBER | R1, R2 show secrets; R3 hides real paths | 28 tests, fuzz target | `pass`/`pwd` names, `\n` before a token, path-shaped prefixes |
| compact | GREEN | no finding | 18 tests, fuzz target | none |
| resolve | AMBER | RS1: `PATH=x tool` runs the wrong program | 17 tests | per-call PATH |
| mcp server | AMBER | M1 (a job result is lost to a second poll), FS8 (cancel does not reach `fs`) | 26 tests | two polls of one job; cancel of an fs call |
| hook | GREEN | 27 rewritten lines gave the same stdout, stderr and exit code in Git Bash | 11 tests | shared deadline (H1, nice) |
| CLI (`main.rs`) | AMBER | MN1: `--timeout 0.5` silently becomes 120 s | 3 tests | flag value errors |
| npm fast path | GREEN | 13 cases same as real npm | 14 tests | global npmrc `script-shell` not testable here |
| python launcher, client, warmup | GREEN | no finding | 10 + 5 + 0 tests | warmup has no test (low risk) |
| setup: Claude, OpenCode | AMBER | SU2 (symlinked config replaced by a plain file) | jsonc 11, mod 13 tests | symlink; empty file; no end-to-end run against a temp home |
| setup: Codex | AMBER | SU4, SU5 (broken or silently skipped config, narrow triggers) | toml 11 tests | BOM, multi-line string, UTF-16 |
| setup: Hermes, dsh | AMBER | SU3 (Hermes config left unparseable), SU1 (unrelated `qx*.exe` deleted) | yaml 14 tests | comment less indented than servers; prune name rule |
| setup: pi, deploy, doctor | GREEN | nice items only (SU8, DP1, D7) | pi 5, deploy 5, doctor 2 tests | leftover files |
| pi extension | AMBER | PI1: says grep is literal, server uses regex, real lines are "not found"; PI2 | 3 bridge tests, 1 smoke | schema against server; abort; initialize timeout |
| speed | GREEN | both Goal.md targets met | `scripts/bench_mcp.py` (printed in CI) | not gated: a 10x slowdown passes CI |
| tokens | GREEN | replies are lean; the losses are the digest items above | reply-size tests in `mcp.rs` | none |
| CI | AMBER | no end-to-end setup test, tests only in release profile, bench not gated | `ci.yml` | see batch 9 |
| docs | AMBER | 7 drift items (D1 to D7), pi text | none | none |

## Decisions taken in the plan (approved with it)

1. **G12, deletes of your own data.** `rm -rf ~/.ssh`, Documents, Desktop, AppData only warn today and still run.
   Plan: deny deletes of the home's well-known children (`.ssh`, Documents, Desktop, Downloads, Pictures, AppData,
   `.claude`, `.cargo`, `.config`) unless `QX_ALLOW_ROOTS` covers them. Deletes inside them stay a warning.
2. **Persistence writes.** Plan: warn (not deny) on writes to a PowerShell profile, `~/.bashrc`, `~/.profile`,
   `~/.gitconfig`; `schtasks /create` becomes a deny through G1.
3. **Redaction names.** Plan: add `pwd`, `pass` (only at a name boundary), `master_key`, `encryption_key`,
   `signing_key`. `bypass=` and `compass:` stay readable (tested).
4. **Unknown `run` arguments (M10).** Plan: accept `env` (object of strings) and answer any other unknown key with
   an error that lists the valid keys.

## How each batch is done

- `git diff` and `git status` first (other agents edit this tree). No commit, push or deploy unless you ask.
- Test first: add the failing test from the "test" column, see it fail, then fix.
- Edit loop `cargo test --no-run` (dev profile); run the suite from a copied test exe (Gatcha.md). Release build
  only for CLI and MCP checks.
- Guard cases only through `qx check`. Setup and deploy only against temp homes.
- A new or moved feature updates `Map.md` in the same change.

## Batch 0: the report (docs only)

Write `docs/review/2026-11-10-qx-full-review.md`: the ratings table, every finding with its case, the dropped
items, and the probe list. The machine name is written as `HOST\User`.

## Batch 1: must-fix (safety and data loss)

| id | where | failing case | fix | test |
|---|---|---|---|---|
| G1 | `guard.rs:1040` `first_word`, used at 411-416 | `schtasks /create /tn x /tr calc.exe /sc onlogon` is allowed; `schtasks create` is denied | `first_word` strips one leading `/` or `-` (also serves reg, sc, terraform) | `schtasks_with_slash_switches_is_denied` |
| G2 | `guard.rs:2062` `norm` | `echo x > C:\PROGRA~1\x.bat` allowed; the long spelling is denied | canonicalize the longest existing ancestor, re-append the rest | `short_name_of_a_new_file_is_still_protected` |
| G3 | `guard.rs:820` `strip_control`, 1305 `unresolved_var` | `for /d %i in (C:\Users\*) do rd /s /q %i` allowed; same for `forfiles /c "cmd /c del @path"` | bind the loop variable to each item of the set and judge the body per item; a set that cannot be listed is denied; forfiles: judge `/p` plus the payload | one test per case, plus a harmless loop stays allowed |
| G11 | `guard.rs:1928` `expand_home` | `rm -rf ~USER` allowed; Git Bash expands it to the home folder | `~name[/rest]` maps to the sibling of the home folder; unknown name is denied for a delete | `tilde_user_is_the_home_directory` |
| FS7 | `fsops.rs` write and edit path; `guard.rs:1111` `write_protected` | `fs {op:write, cwd:<home>, path:".ssh\authorized_keys"}` creates the file; the same write as a command is denied. Same for `Start Menu\Programs\Startup` | make `write_protected` `pub(crate)`; call it in `resolve_for_write` and the edit path; new code `FS_PROTECTED` | `.ssh`, Startup, system folder and `.git\hooks` refused; a normal path still writes |
| E1 | `exec.rs:864` `missing_posix_tool`, 964-968 `posix_syntax` | folder holds `.cache` and `cache`; `rm -rf .\cache 2>/dev/null` deletes `.cache`, exit 0. `cat C:\a\b.toml \| head -3` gives "No such file" with no exit code | before a line goes to bash, turn `\` into `/` in unquoted words that are Windows paths (`X:\`, `.\`, `..\`, `name\name`); quoted text and `\$ \" \space` are untouched. The guard then judges the same path bash runs | the five shapes from my notes (a to e), plus quoted and escaped text unchanged |

## Batch 2: guard, should-fix

| id | failing case (all by `qx check`) | fix |
|---|---|---|
| G4 | `rm -rf \\localhost\C$\Windows` (also `127.0.0.1`, `\\?\UNC\`, `//localhost/`) only warns; `echo x > \\localhost\C$\Windows\x.bat` is allowed | in `norm`: `\\{localhost,127.0.0.1,::1,.,own name}\X$\rest` becomes `X:\rest` |
| G5 | `/proc/cygdrive/c/Windows` only warns | `msys_to_windows` (1918) also strips `/proc/cygdrive` and `/cygdrive` |
| G6 | `ren C:\Windows Windows.old`, `Rename-Item C:\Windows x` allowed | add `ren`, `rename`, `rename-item`, `rni` to the move arm (232) |
| G7 | `stdbuf -o0 rm -rf /c/Windows`, `winpty`, `setsid`, `env -S`, `busybox`, `builtin command`, `r\m`, `$'rm'`, alias and function bodies: all allowed | peel those launchers in `unwrap_wrapper` (1376); unescape the command word; a line that defines an alias or function holding a delete is judged by its body |
| G8 | `perl -e "system('rm -rf /c/Windows')"`, `ruby -e` allowed; a `python -c` delete inside the workspace is denied by mistake | pull string literals out of inline code and judge each as a path and as a shell line; add perl, ruby, php |
| G9 | `dd if=/dev/zero of=\\.\PhysicalDrive0` allowed | `dd` arm: deny `of=` a raw device or a protected path |
| G10 | `rm -rf .git` in the workspace is silent | warn (history loss) |
| G11b | `rm -rf $LOCALAPPDATA` says "cannot resolve" though it is set; `/c/{Windows,Users}` not expanded; `C:\Windows::$INDEX_ALLOCATION` read as a variable | expand any set variable; expand braces; strip a stream suffix |
| G12 | see decision 1 | deny list of home children |
| G13 | `takeown /f C:\Windows /r /d y` only warns | deny on a protected path |

One test per row. Run the existing `ordinary_dev_work_is_not_blocked` list after each change so nothing normal
starts to be denied.

## Batch 3: replies keep every error and result (digest)

| id | where | failing case | fix | test |
|---|---|---|---|---|
| DG1 | `digest.rs:1011` `diagnostic`, 879 `fail_line` | real `go test ./...`: `FAIL example.com/m/c [build failed]` and `c_test.go:6:14: undefined: nope` are not in the reply. Real `node --test`: first failing test's name and the counts are gone | on a red run drop only `routine_line` matches and keep the rest inside the 240-line budget; also recognise `FAIL<TAB>`, `✖`, `ℹ` counts | fixtures: go multi-package, node:test, mocha |
| DG5 | `digest.rs:456` `traceback` | `ERROR:root:database refused connection…` logged before a Python traceback is dropped | keep non-frame stderr lines outside the traceback (bounded) | fixture with a log line before the traceback |
| DG6 | `digest.rs:1235` | `rg -n needle` on 1,530-char lines: the hit is cut at 500 chars, the matched word is not in the reply, no note, no spool | set `clipped`, keep head and tail of the line, print the spool path | long-line fixture asserts the note |
| DG4 | `digest.rs:289` `is_git_log`, 314 | `git log -2 -U1`, `--patch-with-stat`, `-g`, `--parents`: the patch or stat is gone, note says only "bodies dropped" | `git_log` returns None on any line shape it does not know | the four commands as fixtures |
| DG3 | `digest.rs:715-719` | `git status --show-stash`: "Your stash currently has 1 entry" listed as an untracked file | in untracked/ignored sections accept only tab-indented lines, else decline | fixture |

Also extend the digest fuzz target (`fuzz.rs`) with exit codes other than 1 and several error lines, with the rule
"every line that contains `error`, `FAIL` or `panicked` is in the reply or the reply names a spool file".

## Batch 4: search, index and large files

| id | where | failing case | fix | test |
|---|---|---|---|---|
| FS6 | `fsops.rs:3096` (the only place qx starts git) | repo with `core.fsmonitor` set to a command: `fs grep` runs that command (marker file written) | add `-c core.fsmonitor=false` to the call | same fixture, no marker |
| FS2 | `fsops.rs:2023-2035` | `grep path=src/one.rs include="**/*.rs"` says "No matches found." though the file matches | when `path` is a file, match `include` against its path relative to cwd | the case |
| FS3 | `fsops.rs:2109` | 300 MB log, one match, `context:1`: 637 MB peak (21 MB without context) | keep the before/after lines during the scan (ring buffer) | context on a streamed file gives the same lines; no whole-file read |
| B1 | `builtin.rs:1553` `cmd_sed` | `sed -n 1,3p` on a 300 MB file: 1.8 s and 322 MB (head: 2 ms, 7 MB) | stream lines and stop after the last wanted line | large-tail fixture with a time bound |
| IX1 | `index.rs:291-297` | a file named `vendor` or `dist` is invisible to the index; a deleted `tools/dist` file stays listed | apply the generated-folder name rule to directories only | both cases |
| IX2 | `index.rs` remove path | `del LONGFI~1.TXT` leaves the long name in the index | a removed name with `~` that matches nothing marks the index broken (rebuild) | the case |
| IX3 | `index.rs:331` `add_tree`, 397 | a git-ignored folder created after start is searched by the index, not by the walk | keep the ignore set with the index; a `.gitignore` change marks it broken | the case, plus a `.gitignore` edit |
| FS4 | `fsops.rs:1987` | `context:"1"` (a string) gives no context and no note | use `usize_arg` like every other number | the case |
| FS5 | glob | `app/[id]/*.tsx` finds nothing though the folder exists | when nothing matched and the literal folder exists, retry with brackets literal | the case |

Add one new test that covers the whole class: for a temp tree, run a list of file changes, then compare the
indexed server's glob and grep replies with a `QX_INDEX=off` walk; they must be equal.

## Batch 5: exec, resolve, CLI and the MCP server

| id | where | failing case | fix | test |
|---|---|---|---|---|
| E2 | `exec.rs:551-562` | `.cmd` program with args `["line1\nline2","--dry-run","z"]` receives only `line1`, exit 0 | refuse with a clear error (cmd.exe cannot carry a newline in an argument) | the case gives an error |
| E6 | `exec.rs:1890` `peel_env` | `NODE_ENV=test a && b`: both steps get the variable; bash gives it to the first only | peel per chain step | the case |
| RS1 | `resolve.rs:329`, `exec.rs` `build_command_env` | `PATH=<dir> python --version` runs the system python although the child gets the new PATH | when the call sets PATH, resolve with `resolve_in` over that PATH | unit test with two temp folders plus the MCP case |
| RS2 | shell choice before env peel | `PATH=a;b tool` is sent to PowerShell whole and fails | peel leading `NAME=value` words first | the case |
| MN1 | `main.rs:109` `parse_num` | `qx --timeout 0.5 …` and `--timeout 5m` silently mean 120 s; `--timeout --raw` eats `--raw` | exit 2 naming the bad value; accept `30s` and `5m` | CLI tests |
| M1 | `mcp.rs:646` | two polls of one finished job in one message: the second says "still running", the next says "no job" | keep the result in the record until it was reported once per poller; forget after | `two_polls_of_one_job_both_get_the_result` |
| M7 / FS8 | `mcp.rs:881` | `fs read \\192.0.2.1\share\x` blocks 21 s; a cancel at 1 s does nothing | register fs calls with `Inflight`; answer a cancel at once and let the worker finish alone | cancel of a slow fs call answers within 100 ms |
| M10 | `mcp.rs` run arguments | `env:{…}` or a typo such as `cwdd` is ignored with no word | see decision 4 | unknown key gives an error; `env` reaches the child |
| M2 | `mcp.rs:1287` | built-in `sleep 3` with `timeout_ms:1000` is killed at 1 s; a real program becomes a job | built-ins get the hard deadline when promotion is on | the case |
| M3 | job kill reply | a job stopped on request says "timed_out: true" | say "stopped on request" | the case |
| M4, M5, M6 | `mcp.rs:93-94`, 397, schema | output schema lacks `full_output` and `elapsed_ms`; `active` count read after the add; any protocol version is echoed | add the two fields; use the `fetch_add` result; answer with a version qx supports; document or remove `kill_on_timeout` and `QX_PROMOTE` (no doc and no test names them) | schema test |
| E3 | `exec.rs:483` | `timeout 2 git --version` runs Windows `timeout.exe` and fails | `timeout <n> <cmd>` runs the command under qx's own deadline | the case |
| E5a | `exec.rs:984` `ps_quote` | `Get-ChildItem @types -Name` lists the wrong folder (splatting) | quote a word that starts with `@` | the case |
| E7, E5b | program choice | `rmdir /s /q t3` runs Git's rmdir and fails; `find logs` runs Windows find | `rmdir`/`del`/`copy`/`move` with a `/x` flag go to cmd; `find <existing dir>` goes to POSIX find | the cases |
| H1 | `hook.rs:78` | `a && b` gives each stage the full 118 s, so the second stage's output is lost at Claude's 120 s limit | one shared deadline for the line (`--deadline`) | unit test on the rewrite, CLI test |

## Batch 6: redaction

| id | where | failing case (made-up values) | fix |
|---|---|---|---|
| R1 | `redact.rs:7-37` | `DB_PASS=`, `MYSQL_PWD=`, `Pwd=…;`, `"pwd":`, `RAILS_MASTER_KEY=` shown | decision 3 |
| R2 | `redact.rs:443` | a GitHub token after an escaped `\n` in a JSON line is shown | `\n`, `\t`, `\r` count as an edge |
| R3 | `redact.rs:488` | `i18n/sk-SK/LC_MESSAGES/app.po` becomes `i18n/sk-***`; `vendor/hf_transfer-0.1.6/…`, `docs/SG.Core…md` hidden too | body alphabet per prefix; short prefixes need a digit in the body |
| R4 | 245, 497 | `.netrc` `password x`, `Authorization: Token x`, `-Password x`, `Cookie:`, a token as URL user | handle each form |
| R5 | prefix table | `xapp-`, `xoxe-`, `GOCSPX-`, `hvs.`, `rk_test_`, `shpat_`, `AGE-SECRET-KEY-1` shown | add the prefixes |
| R6 | `redact.rs:379` | INI `password = hunter.two.three` is read as code and shown | apply the code test only when the line looks like code |

Tests: each line masked, each look-alike (`bypass=`, `compass:`, the four paths, `token = jwt.sign(...)`) untouched.
Also fix the wording in `doctor.rs:114` (D7): redaction covers command output, `fs` returns files as written.

## Batch 7: setup and deploy

| id | where | failing case (scratch home) | fix | test |
|---|---|---|---|---|
| SU1 | `setup/yaml.rs:460` | `<dsh>\bin` holds `qxl-viewer.exe` and `QXtool.exe`; `qx setup dsh` deletes both, no backup | match only `qx.exe` or `qx` followed by a non-letter; print each removed name | both files stay |
| SU2 | `setup/mod.rs:393-402` `apply` | `settings.json` is a symlink into a dotfiles folder; after setup it is a plain file and the dotfiles copy has no hook | when the file is a symlink, write beside its resolved target | symlink case (skipped when a link cannot be made) |
| SU3 | `setup/yaml.rs:261-265` and 402-406 | Hermes file with a comment indented less than the servers: the qx block is written at the wrong indent, the file no longer parses, qx says "updated" | one shared `child_indent` helper that skips noise lines | both cases; the qx line's indent equals the sibling's |
| SU4 | `setup/toml.rs` | Codex file with a BOM and the qx table on line 1 gets a second table; a multi-line string holding `[mcp_servers.qx]` is rewritten; `mcp_servers.qxother` is refused | strip and restore the BOM; track `"""` and `'''`; require `.qx.`, `.qx=` or `."qx"` | three cases |
| SU5 | `setup/mod.rs:616-621` | UTF-16 `config.toml`: "skipped", exit 0, nothing installed | a plan error counts as refused (exit 1) | the case |
| SU6 | `setup/jsonc.rs` `first_value` | empty `settings.json`: server registered, hook refused (half install) | treat an empty document as `{}` | the case |
| SU7 | `setup/mod.rs` `is_qx_hook` | a user group holding the qx hook plus another hook gets a second qx hook | detect the hook inside any group; update in place | the case |
| SU8 | remove path | install then remove leaves empty `"PreToolUse": [ ]`, LF in a CRLF file, `.qx-bak` copies of qx's own files | remove containers qx created; keep the file's line ending; no backup for files qx generated | byte-exact round trip for each client |
| DP1 | `deploy.rs` `sweep_aside` | every deploy leaves `qx.exe.old-<ms>` even when nothing runs; a stale `.new-<pid>` is never removed; `--to <folder>` says "Access is denied" | delete the aside copy at once when possible; sweep old `.new-*`; clear message for a folder | three cases |

Add `tests/` style end-to-end coverage: one test per client that runs the real `plan` + `apply` against a temp home,
then install twice (second run unchanged), then remove.

## Batch 8: pi extension

| id | where | failing case | fix | test |
|---|---|---|---|---|
| PI1 | `integrations/pi/qx.js:52`, 66 | text says grep is literal; `grep "arr[0]"` on a file holding `arr[0] = 1;` answers "No matches found." | say "regex; `literal:true` for plain text"; add `literal`, `all`, `paths`, `ops`, `edits` | node test: every key of the server's `fs` and `run` schema is in the extension schema |
| PI2 | `qx.js:14-37`, 66 | a long command answers "Follow: {job:"j1"}" but the schema has no `job`; text says edit needs a prior read (it does not) | add `background`, `job`, `wait_ms`, `kill`; correct the text | same test |
| PI3 | `bridge.js:45`, 140 | a child that never answers: the call hangs and an abort does nothing | race `start()` against the signal and a 10 s timer | bridge test with a silent child |
| PI4 | `bridge.js:135` | 200 calls on one signal leave 200 listeners; a later abort sends 200 cancel notices | remove the listener when the reply arrives | bridge test |

Not verified: whether a provider in strict-schema mode blocks the undeclared `job` key (needs a provider key).

## Batch 9: small items, docs, CI, lean

- Built-ins: B2 (`whoami` case), B3 (`mkdir p\q\r`), B4 (`touch` on a folder), B5 (`cp -r dir file` message),
  FS11 (edit of a read-only file takes 607 ms and does not say why), FS1 (batch cap message at the top).
- Guard nice items: G14 (no warning for `git stash clear`, `reflog expire`, `filter-branch`…), G15 (false alarms:
  `npm publish --dry-run`, `shutdown /a`, `bcdedit /enum`, deletes in `%TEMP%`), G16 (`rsync --delete`,
  `Clear-Content`, `at`), G17 (audit log has no test).
- Docs: D1 `README.md:610` says 50 KiB, code is 48,000 bytes. D2 `README.md:420` says 8 KiB, code is 8,000.
  `README.md:600` omits the `powershell` shell value. D4 `main.rs:734` says max 8 threads, code allows 32.
  D5 `Gatcha.md:9` says 363 tests, today 431. D6 `main.rs:738-742` help omits six built-ins. D7 above.
  `Map.md` for every moved or new feature.
- Bench: BN1 `scripts/bench_mcp.py` prints the 9th of 11 samples as "p90"; fix the index.
- CI (`.github/workflows/ci.yml`): add a dev-profile test run; run tests in the MSRV job; gate the bench on a loose
  limit (in-process median under 5 ms on the runner) so a large slowdown fails.
- Lean, last and optional: split `fsops.rs` (4,731 lines: arguments, root, observation, I/O, read, write, edit,
  glob, grep, list, walking) into `src/fs/` modules along its existing section comments, no behaviour change,
  suite as proof. I recommend doing this only after batches 1 to 4 so the fixes are not mixed into a file move.

## Dropped (no failing case, so not findings)

Unchecked Win32 return values (`exec.rs:1355`, 1370, `fsops.rs:879`); caches with no eviction (`resolve`, fs
content cache, observation map); a lost update when a client rewrites `.claude.json` during setup; test globals
shared between `serve_io` tests; global npmrc `script-shell`; actions pinned by tag in CI.

## Verification

1. After each batch: suite from a copied test exe (expect 431 plus the new tests, 0 failed), then
   `cargo clippy --release --all-targets --locked -- -D warnings`.
2. Batch 1 and 2: re-run my guard case files through `qx check` (the three case files plus the new ones); every
   must and should row flips to deny or warn as planned, and the ordinary-work list stays allowed.
3. Batches 1, 3, 4, 5: `cargo build --release`, copy the exe out of `target\`, then re-run the probe scripts that
   produced each finding against it over MCP; each "wrong result" line must change to the right one.
4. Batch 7: re-run the setup probe against scratch homes for all six clients; results must parse (PyYAML, tomllib,
   strict JSON) and install plus remove must give back the original bytes.
5. Batch 8: `node integrations/pi/bridge.test.mjs` and `smoke.mjs`.
6. Speed: `python scripts/bench_mcp.py` before and after batches 1, 3 and 4; in-process medians stay under 2 ms and
   spawned calls within 10 ms of the bare process.
7. No deploy. The live `~/.cargo/bin/qx.exe` is replaced only when the user asks for `qx deploy`.

## Probes

The probe scripts ran from the review session's scratch folder against a copy of the release exe (never the
installed one), over MCP with a real client name. Each case is restated in the tables above and in the notes
below, so the fixes' tests carry them into the repo.

| probe | what it drove |
|---|---|
| `qxs.py` | small MCP client used by the others (spawn, initialize, `run`, `fs`, jobs) |
| `guard_probe.py` + `guard_cases1-3.txt` | every guard case through `qx check` only |
| `mcp_conc.py` | jobs, polls, cancel, concurrency (M1, M7/FS8, M2, M3) |
| `e1.json`-`e3.json` | shell choice and spawn cases (E1, E2, E3) |
| `rs1.py`-`rs3.py` | per-call PATH and env words (RS1, RS2, E6) |
| `h1.py` | hook rewrite of chained lines (H1) |
| `d1.py`, `d2.py` | real failing go, node:test, mocha, python, git and search runs through the digest (DG1-DG6) |
| `r1.py` | redaction with made-up secrets and look-alike paths (R1-R6) |
| `fs2.py`, `fs4.py`, `ix1.py` | grep include on a file, context as a string, index against walk (FS2, FS4, IX1-IX3) |
| `b1.py` | `sed -n 1,3p` on a 300 MB file (B1) |
| `np1.py` | npm fast path against real npm (13 cases) |
| `su1.py`, `dp2.py` | `qx setup` for six clients and `qx deploy`, scratch homes only (SU1-SU8, DP1) |
| `pi1.py`, `pi3.py`, `pi2.mjs` | the pi extension text against the server, and the bridge with a silent child (PI1-PI4) |

## Appendix: review notes per area

The working notes, kept as written during the review (cases, causes, line numbers at ec4be11).

### Guard and MCP server (lead notes)

Baseline: suite 431 pass / 2 ignored in 6 runs (5 default, 1 at 8 threads), 9.4-10.6 s; clippy clean; in-process
built-ins 0.2-0.3 ms, fs 0.2-1.4 ms, `git --version` 19.4 ms, a PowerShell line 61 ms (p90 184), a bash pipeline
62 ms, initialize 3.5 ms. FS_* codes with no asserting test: FS_IO, FS_BAD_CWD, FS_SYMLINK, FS_NOT_FILE, FS_EXISTS,
FS_BINARY, FS_NO_MATCH, FS_UNSUPPORTED; the guard's BLOCKED and warning text has no test.

#### guard.rs — findings (all verdicts from `qx check`, dry run). WARN = the command RUNS with a note
(main.rs:233-241, mcp.rs:977-999); only DENY stops it.

G1 must — `schtasks /create|/delete|/change` is allowed. guard.rs:411-416 uses first_word() (guard.rs:1040) which
  skips only `-` words; real syntax is `/create`. case: `schtasks /create /tn x /tr calc.exe /sc onlogon` → ALLOW;
  `schtasks create /tn x` → DENY. Existing test only uses the slash-less form? (check test ~2440). fix: strip a
  leading `/` or `-` in first_word (used by reg, sc, schtasks, terraform). test: schtasks_with_slash_switches_is_denied.
G2 must — write into a system dir through an 8.3 name of a NEW file. norm() guard.rs:2062 canonicalizes only when the
  whole path exists, else clean(p) keeps `progra~1`. case: `echo x > C:\PROGRA~1\x.bat` → ALLOW;
  `echo x > "C:\Program Files\x.bat"` → DENY; `copy a.bat C:\PROGRA~1\newdir\a.bat` → ALLOW.
  fix: canonicalize the longest existing ancestor and re-append the rest. test: short_name_of_a_new_file_is_still_protected.
G3 must — cmd `for` loops hide delete targets. `for /d %i in (C:\Users\*) do rd /s /q %i` → ALLOW;
  `for /d %i in (C:\Windows) do rd /s /q %i` → ALLOW; `for %i in (C:\Windows\*) do del /q %i` → ALLOW.
  cause: strip_control (guard.rs:820) drops the `for .. in (..) do` header; `%i` is not `%name%` so unresolved_var
  (guard.rs:1305) is false and the target looks like cwd\%i. fix: bind the loop variable to each item of the set
  (judge the body once per item; unknown/wildcard set → deny like the pipe case "targets cannot be checked").
  Same for forfiles `/c "cmd /c del @path"` (ALLOW today): judge /p dir + payload.
G4 should — local admin share spelling of protected paths is only a warning. `rm -rf \\localhost\C$\Windows` → WARN,
  `\\127.0.0.1\C$\Windows` → WARN, `\\?\UNC\localhost\C$\Windows` → WARN, `//localhost/C$/Windows` → WARN,
  `\\localhost\C$\Users\USER` → WARN (home!). `ls //localhost/C$/Windows` works on this PC, so the path is real.
  cause: norm() strips only `\\?\`; no UNC→drive mapping. fix in norm(): `[\\?\UNC\|\\]{localhost|127.0.0.1|::1|.|%COMPUTERNAME%}\X$\rest` → `X:\rest`.
  (TODO re-probe the write case `echo x > \\localhost\C$\Windows\x.bat` through guard_probe.py; the bash probe lost a backslash.)
G5 should — `/proc/cygdrive/c/Windows` (real in Git Bash: ls works) → WARN "C:\proc\cygdrive\c\Windows".
  msys_to_windows guard.rs:1918 knows only `/c` and `/mnt/c`. `/cygdrive/c` does not exist in Git Bash (only Cygwin). fix: strip `/proc/cygdrive` and `/cygdrive` too.
G6 should — rename is not judged: `ren C:\Windows Windows.old` → ALLOW, `Rename-Item C:\Windows x` → ALLOW,
  `ren %USERPROFILE% x` → ALLOW, while `move C:\Windows C:\x` → DENY. guard.rs:232 list lacks ren/rename/rename-item/rni.
G7 should — transparent launchers not peeled: `stdbuf -o0 rm -rf /c/Windows`, `winpty rm -rf ..` (ships with Git for
  Windows), `setsid rm -rf ..`, `env -S "rm -rf /c/Windows"`, `busybox rm -rf C:\Windows`, `builtin command rm -rf ..` → all ALLOW.
  unwrap_wrapper guard.rs:1376. Also bash escapes in the command word: `r\m -rf /c/Windows` → ALLOW (base_name of
  `r\m` is `m`), `$'rm' -rf ..` → ALLOW, `$'\x72m'` → ALLOW; `alias x='rm -rf'; x /c/Windows` → ALLOW;
  `f(){ rm -rf "$@"; }; f /c/Windows` → ALLOW (token `f(){` hides the body).
G8 should — inline code: `perl -e "system('rm -rf /c/Windows')"` → ALLOW, `ruby -e "FileUtils.rm_rf('C:/Windows')"` → ALLOW
  (inline_code guard.rs:1101 knows only python/node). check_code guard.rs:1635 is keyword-only:
  `python -c "shutil.rmtree(r'C:\Users\USER')"` → WARN (home, raw string has one backslash) but
  `python -c "shutil.rmtree('C:/Users/USER/Projects/qx/target/tmp')"` → DENY (false deny inside the workspace);
  `python -c "os.system('rd /s /q C:\\Windows')"` → WARN only. fix: pull the string literals out and judge each as a
  path (protected()) and as a shell line (check_shell_mode); add perl/ruby/php -e/-r.
G9 should — `dd if=/dev/zero of=\\.\PhysicalDrive0` and `of=/dev/sda` → ALLOW (dd.exe ships with Git Bash; README says
  disk destruction is denied). No `dd` arm in check_mode. fix: deny `of=` a raw device, deny_if_protected(of=).
G10 should — deleting `.git` of the workspace is silent: `rm -rf .git` → ALLOW. fix: warn (history loss).
G11 should — bash `$NAME` only expands HOME/USERPROFILE/APPDATA (expand_dollar_env guard.rs:1968): `rm -rf $LOCALAPPDATA`
  → WARN "variable qx cannot resolve" though the var is set and bash will expand it. `rm -rf ${HOME:-/}` → WARN.
  `rm -rf ~user` → ALLOW (bash expands ~name to that user's home; TODO confirm `echo ~USER` in Git Bash).
  `rm -rf /c/{Windows,Users}` → WARN "C:\{Windows,Users}" (brace expansion not expanded).
  `rm -rf C:\Windows::$INDEX_ALLOCATION` → WARN (stream suffix read as a variable).
G12 should — deletes of user data are warn-only by design, but inconsistent: write_protected() (guard.rs:1111) denies
  WRITING into `~\.ssh`, yet `rm -rf ~/.ssh`, `rm -rf C:\Users\USER\Documents|Desktop|AppData|.claude`,
  `rd /s /q %APPDATA%` → WARN and run. Suggest deny for the home's well-known children (.ssh, Documents, Desktop,
  AppData, Downloads, Pictures, .claude, .cargo, .config) unless QX_ALLOW_ROOTS covers them (decided in the plan).
G13 should — takeown/icacls on a protected path → WARN only (guard.rs:431-432): `takeown /f C:\Windows /r /d y`.
G14 nice — git: `git push --force-with-lease` → ALLOW (test at guard.rs:3182 lists it; check if deliberate);
  no warn for `git stash clear|drop`, `git reflog expire --expire=now --all`, `git gc --prune=now`, `git filter-branch`,
  `git update-ref -d`, `git worktree remove --force`, `git tag -d`; `git restore file` / `git checkout file` not warned
  while `git checkout -- file` is. `git clean -fdx C:\Users\USER` → WARN only (path operand not judged).
G15 nice — false alarms: `npm publish --dry-run` → WARN; `pnpm publish`/`yarn publish` → ALLOW (inconsistent);
  `shutdown /a` (abort) → DENY; `bcdedit /enum` (read-only) → DENY; `rm -rf %TEMP%\foo`, `rm -rf /tmp/foo` → WARN
  "outside the workspace" (and `/tmp` is mapped to C:\tmp, in Git Bash it is %TEMP%); `python -c "print('rm -rf /')"` → WARN;
  check_code says "recursive deletion" for a plain `os.unlink`.
G16 nice — not judged at all: `rsync -a --delete x/ /c/Windows/` → ALLOW; `Clear-Content C:\Windows\System32\drivers\etc\hosts`
  → ALLOW (set-content is judged, clear-content/new-item -force are not); `at 00:00 cmd /c ..` → ALLOW.
G17 nice — audit log (guard.rs:2192): rotate keeps one old file, rename then append is not atomic across processes
  (two qx at the 5 MB edge can both rename; harmless loss of one generation). No test for record().
Not bugs (my case was wrong): `C:\WINDOW~1` does not exist (Windows has no short alias) → warn is right; PROGRA~1 of an
existing dir is denied. docker/kubectl warn is right. `sc delete`/`Stop-Service` cases could not be spawned by Python
(OS refused the exe name); probe them through a file later if needed.
G4+ confirmed via guard_probe: `echo x > \localhost\C$\Windows\x.bat` → ALLOW, `copy a.bat \localhost\C$\Windows\a.bat` → ALLOW (write_protected sees `unc\localhost\c$\windows`).
G11+ must — `rm -rf ~USER` → ALLOW; Git Bash `echo ~USER` prints /c/Users/USER, so this deletes the home directory with no warning. expand_home guard.rs:1928 knows only `~`, `~/`, `~\`. fix: `~name[/rest]` → the Users dir sibling (or deny unknown ~name).
G14: `git push --force-with-lease` allowed on purpose (test ordinary_dev_work_is_not_blocked guard.rs:3182) → not a finding. `sc stop X` → ALLOW while Stop-Service warns (nice).

#### mcp.rs — findings (proved with mcp_conc.py m1.json on S\probe\qx.exe)
M1 should — two polls of one job at once: the first takes the result (JobRec::wait uses guard.take(), mcp.rs:646) and forgets the job; the second waits its whole wait_ms and answers `job j1 still running (8s)` though ping ended at 3 s; the next poll says `no job j1`. case: background ping -n 4, then two {job:j1,wait_ms:8000} in one message. fix: keep the result in the record (clone/Arc) and let forget happen once; or wake waiters with a 'taken' state that renders 'already reported'. test: two_polls_of_one_job_both_get_the_result.
M2 nice — builtin commands are not promoted: {program:sleep,args:[3],timeout_ms:1000} → exit 124 'qx: stopped (cancelled or timed out)' at 1 s, while a real program (ping) is moved to a background job as the schema says. A builtin cp -r / rm -r stops half way. mcp.rs:1287 set_deadline(now+timeout) where timeout = window. fix: deadline = hard for builtins when promote, or say so in the schema.
M3 nice — a job stopped with {job,kill:true} is reported `exit_code: 124 / timed_out: true` (isError) — wording says timeout for a requested stop. fix: 'stopped on request'.
M4 nice — schema drift: `kill_on_timeout`, `QX_PROMOTE`, aliases workdir/timeoutMs/run_in_background are not in the schema (fine), but run description says default 120000/max 270000 while Claude Code's window is 100 s (SOFT_MS_CLAUDE mcp.rs:588) and clamp is 3,600,000 (mcp.rs:948). outputSchema lacks full_output, elapsed_ms. Comment at JOB_MAX_MS 'unless the call asked for less' is not what the code does (hard = JOB_MAX_MS always when promote).
M5 nice — `active.fetch_add(1); active.load()` (mcp.rs:93-94) reads a count other calls may have raised; use the fetch_add return. Busy reply for a batch answers non-call ids (ping) with a tool result.
M6 nice — initialize echoes any protocolVersion the client sends (mcp.rs:397) instead of answering with one qx supports.
M7 check-later — fs calls return before Inflight::begin (mcp.rs:881): a cancel never reaches a long fs grep/glob; look for a deadline in fsops.
M8 check-later — `run_direct(..).ok()` (mcp.rs:1036) falls back to the shell on Err: if npm chain ran step 1 and step 2 failed to START, the shell would run the whole thing again. Check npm::direct / run_chain.
M9 check-later (globals) — serve_io sets SHUTDOWN/SPAWN_GATE process-wide; two serve_io tests in parallel: one ending sets SHUTDOWN=true for the other. See scout 'Map globals'.
X1 should — `command: wc -l C:\Users\USER\Projects\qx\src\*.rs` → exit 1 `wc: 'C:UsersUSERProjectsqxsrc*.rs': No such file` : auto shell sends a line with a glob to Git Bash, which eats the backslashes of a Windows path. Pin the rule when reading exec.rs (shell pick) and builtin glob handling.
Not findings: running_reply with a 60 KB single line is clipped to 8 KB (ok); guard::record returns early on Allow (guard.rs:2194).

Docs drift D1-D7 was checked against the code and is listed in the pi, CI and docs section below.

### exec.rs findings (read 1-1982 in full; probes: tools/e1.json e2.json e3.json -> out/e1.txt e2.txt e3.txt; scratch dir probe/work)

E1 (MUST, data loss; merges X1) Windows path in a line that goes to Git Bash loses its backslashes.
  Route: pick_shell -> missing_posix_tool (exec.rs:864) / posix_syntax chained rule (967-968) send the line to bash as typed; bash eats `\`.
  posix_syntax's 'a Windows path means not POSIX' rule (exec.rs:964) covers only MARKERS.
  Cases (all MCP run `command`, claude-code client):
   a. cwd has `.cache\x.txt` and `cache\y.txt`; `rm -rf .\cache 2>/dev/null` -> reply '(no output)' exit 0; `.cache` is DELETED, `cache` still there. guard: ALLOW ok.
      Same shape: `rm -rf .\git 2>/dev/null` would delete `.git` (guard ALLOW; not run).
   b. `rm -f <abs>\logs\*.tmp` -> guard warning names the right path, reply '(no output)' exit 0, both .tmp files still there (silent no-op).
   c. `rm -rf out\*` / `rm -rf .\cache\*` -> exit 0, nothing deleted (`\*` becomes a literal star).
   d. `cat C:\Users\USER\Projects\qx\Cargo.toml | head -3` -> stderr `cat: 'C:UsersUSERProjectsqxCargo.toml': No such file`, NO exit code shown (pipe status of head = 0).
   e. `cat sub\a.txt | head -1`, `head -1 sub\a.txt; echo done`, `grep -c o sub\a.txt || echo none`, `wc -l C:\..\src\*.rs`, `ls <abs>\*.tmp`, `grep -c name C:\..\Cargo.toml > out.txt` -> all 'No such file' with the path squashed.
  Fix idea: before handing a line to bash, rewrite unquoted words that look like Windows paths (`X:\..`, `.\..`, `..\..`, `name\name`) to forward slashes; never touch `\` inside quotes or before a shell metachar that is not a path char (keep `\$`, `\"`, `\ `, `\n`?). Simplest safe rule: in an unquoted word, a `\` followed by an alphanumeric, `.`, `_`, `-`, `*`, `?`, `[` or `{` and preceded by a path char -> `/`. Tests: each case above.
  Guard side: guard judged `.\cache`, bash ran `.cache` => guard and executor disagree on the target. After the fix both see the same path.

E2 (SHOULD) `.cmd`/`.bat` program with an argument holding a newline: the argument is cut at the newline and EVERY later argument is dropped, exit 0.
  Case: `{program:".\\shim.cmd", args:["line1\nline2","--dry-run","z"]}` (shim = `python argv.py %*`) -> `["line1"]`. Direct `python argv.py` gets all 3.
  Where: command_for Target::Shim (exec.rs:551-562) builds a cmd line with cmd_quote and no newline check; cmd.exe stops at the first newline.
  Real shape: `npx`/`tsc.cmd`/`claude.cmd -p "multi\nline" --flag` -> the flag after the text vanishes.
  Fix: refuse (clear error: a .cmd program cannot take a newline in an argument; pass a file) - cmd.exe has no escape for a newline in an argument. Test: the case above gives an error, not a short run.
  Other special chars through the shim are right: `a&b x|y <z> ^caret 100% %USERNAME% say "hi" !bang! 'a b' '' tab back\\ end\"q` all arrive intact.

E3 (NICE) `timeout 2 git --version` (command or program form) runs Windows timeout.exe -> `ERROR: Invalid syntax. Default option is not allowed more than '1' time(s).` exit 1. The guard peels `timeout N cmd` as a POSIX launcher (qx check `timeout 5 rm -rf C:\Windows` -> DENY) so the two halves disagree. posix_flags (exec.rs:483) covers only find/sort.
  Fix: `timeout <number> <cmd...>` -> run <cmd> under qx's own deadline (or Git's timeout.exe). Test: the case prints the git version.

E5a (NICE) An argument starting with `@` to a PowerShell cmdlet is splatted. Case: cwd `nm` holding `@types\a.d.ts` and `other.txt`; `{program:"Get-ChildItem", args:["@types","-Name"]}` -> lists the cwd (`@types`, `other.txt`) instead of `a.d.ts`. `{program:"Get-Item",args:["@args"]}` -> 'missing mandatory parameter Path'. ps_quote (exec.rs:984) leaves `@x` bare. Fix: quote a word that starts with `@`. Test: both cases.

E5b (NICE) `find logs` (no flags) runs Windows find.exe -> `FIND: Parameter format not correct` exit 2. posix_flags needs a flag to pick POSIX find. Fix: `find <existing dir>` with no quoted string -> POSIX find. Low value; list as nice.

E6 (SHOULD-low) `VAR=x cmd1 && cmd2` / `VAR=x cmd1 ; cmd2`: the variable is applied to every step, POSIX applies it to cmd1 only.
  Case: `NODE_ENV=test python -c "import os;print(os.environ.get('NODE_ENV'))" && python -c "...same..."` -> `test` / `test` (bash prints `test` / `None`). Same with `;`.
  Real shape: `NODE_ENV=test npm test && npm run build` builds in test mode. Where: peel_env (exec.rs:1890) runs before the chain is split (run_chain 1805). Fix: peel per step. Test: the case.

E7 (NICE) `rmdir /s /q t3` (command and program form) runs Git's /usr/bin/rmdir: `failed to remove '/s'`, `'/q'`, `'t3': Directory not empty`, exit 1. `rd /s /q` goes to cmd and works. The guard reads `rmdir /s /q C:\Windows` as the cmd form (DENY). Fix: `rmdir`/`del`/`copy`/`move` with a `/x` flag -> cmd. Check resolve.rs for the cmd-internal list before writing the fix.

Not findings (checked): md5sum on `C:\..\[id]\a.txt` drive path works (E4 dropped). `pwsh` not installed here, cannot prove the 5.1 fallback. `-Include "*.tmp,*.txt"` as ONE arg is one string by contract. cmd internals as `program` (`ren`, `rd`, `ver`, `set`, `mklink`) -> 'command not found: ren' (4 ms): arguably by contract (`command` form works), list under docs: say so in the schema or route to cmd. M8 (chain re-run through the shell fallback) is NOT a finding: run_chain 1818-1824 reports a later step that cannot start as a failure.
Left unproved, low: write_atomic temp name per process not per thread (exec.rs:317, first run only); cmd_quote `%VAR%` inside a quoted arg (probe showed `%USERNAME%` arrives literal through a shim, so fine).

### fsops.rs (read 1-3427 in full). PROVEN results (probes: tools/fs_setup.py, tools/fs1.json -> out/fs1.txt, tools/fs2.py). Fixtures: probe/work/fs

FS6 SHOULD (security, PROVEN): a read-only search runs code from the searched repo. ask_git_for_ignored_dirs (fsops.rs:3092-3103) runs `git -C <cwd> ls-files -o -i --exclude-standard --directory`. Case: scratch repo with `git config core.fsmonitor "echo ran > MARKER.txt; false"`; `fs {op:grep, pattern:needle, cwd:<repo>}` -> match returned AND MARKER.txt written (138 ms). Root = the fs cwd (path below a non-repo cwd does not trigger: glob path=evilrepo from parent -> no marker). Fix: add `-c core.fsmonitor=false` (and GIT_CONFIG_NOSYSTEM not needed) to that git call; test = the same fixture asserts no marker. Check other `Command::new("git")` sites in src for the same.
FS3 SHOULD (load, PROVEN): grep context re-reads the whole file (fsops.rs:2109). 300 MB log, one match: context=0 -> 270 ms, peak working set 21 MB; context=1 -> 659 ms, peak 637 MB. At the 512 MiB limit about 1.1 GB, per file with a match. Fix: keep the before/after lines during the scan (ring buffer of N lines in scan_stream/scan_buffer) or re-read only a window by byte offset. Test: context on a streamed (>8 MiB) file gives the same lines and no whole-file read (unit: scan returns context).
FS2 SHOULD (wrong result, PROVEN): `grep path=src/one.rs include="**/*.rs"` -> 'No matches found.' (18 bytes) though the file has the match; include="*.rs" finds it; path=src + include="**/*.rs" finds it. Cause fsops.rs:2023-2035: rel of the file to itself is ''. Fix: when path is a file, match include against its path relative to cwd (or its name).
FS4 NICE (PROVEN): `context:"1"` (string) -> no context lines, no warning (55 bytes vs 88 with context:1). fsops.rs:1987 uses as_u64; use usize_arg like every other number.
FS5 NICE (PROVEN): glob `app/[id]/*.tsx` -> 'No files found.' while app/[id]/page.tsx exists; `app/[[]id]/*.tsx` and path=app/[id] + `*.tsx` find it. Standard glob class rule, but Next.js folders make it common. Fix: when nothing matched and the literal reading of the pattern's folder exists, retry with brackets literal (or say so in the note).
FS11 NICE (PROVEN): edit of a read-only file takes 607 ms to fail (atomic_replace retries os error 5 five times, fsops.rs:1066) and says 'cannot replace ...: Access is denied. (os error 5) ... fallback: use the native Edit tool' - does not say the file is read-only. Fix: check the read-only attribute in check_mutable and refuse at once with 'file is read-only'.
FS1 NICE (PROVEN, small): batch = read of a 48 KB file + 12 edits -> edits correctly skipped ('skipped: batch output cap reached'), nothing edited; but the reply is cut at 49000 of 49083 so the last op's line ('13/13 edit e12.txt') has no status. With 32 ops about 20 skip lines fall off. Fix: on cap, one line at the TOP: 'ops 2-13 not run (batch output cap)'. Safe direction, so only NICE.
FS7 MUST (security, PROVEN, tools/fs4.py): the fs tool skips a rule the guard enforces. guard.rs:1111 write_protected denies writes into system dirs, `\start menu\programs\startup`, `\.ssh\`, `\.git\hooks\`. Case (scratch folder fakehome): `qx check 'echo x > "<fakehome>\.ssh\authorized_keys"'` -> DENY 'refusing to write into the SSH directory'; `fs {op:write, cwd:<fakehome>, path:'.ssh\authorized_keys', content:'x'}` -> 'Created file', file exists. Same for 'Start Menu\Programs\Startup\a.cmd'. cwd is caller-chosen and qx is allow-listed, so the real ~/.ssh and Startup are one call away. fsops.rs has no reference to guard (mcp.rs:881 returns fs before the guard). Fix: make guard::write_protected pub(crate) and call it in resolve_for_write/resolve_existing for write+edit (code FS_PROTECTED); tests for .ssh, Startup, system dir, plus one that a normal path still writes. Guard cases6 also: `Set-Content $PROFILE ..`, `echo x >> ~/.bashrc`, `echo x > ~\.gitconfig`, `schtasks /create .. /sc onlogon` -> ALLOW (policy question P2: add PowerShell profile, shell rc files, schtasks /create to WARN?).
FS8 NICE (PROVEN, tools/fs3.json; detail for M7): `fs read \\192.0.2.1\share\x.txt` -> error after 21,049 ms; notifications/cancelled sent at 1 s had no effect; other calls were still answered (stat 2 ms). Reply then says 'fallback: use the native Read tool' which hangs the same way. Fix with M7 (fs calls honour cancel: answer the cancel at once and let the worker finish alone) - no deadline needed.
Side note: a client that writes requests and closes stdin at once gets 'qx is shutting down; the call was not run' for every queued call (python communicate()). Check against M-notes; probably by design (documented?).
FS9 NICE-weak: CONTENT cache 128 MiB never evicts (fsops.rs:2216). FS10 NICE-weak: observation MAP never shrinks (780).
Not findings: junction loops (skipped as symlinks); check_mutable on existing files in write (1439); fast_regex correctness; mixed line ending normalisation (by design).
Tests gap (scout h3): no test asserts FS_IO, FS_BAD_CWD, FS_SYMLINK, FS_NOT_FILE, FS_EXISTS, FS_BINARY, FS_NO_MATCH, FS_UNSUPPORTED.
Docs: main.rs:734 says QX_FS_THREADS max 8; code clamps 1..32 (fsops.rs:3170-3179).

### builtin.rs (read 1-2162 in full). Probe: tools/b1.py (fixtures probe/work/b). Helper for all later probes: tools/qxs.py (Srv, show, check, peak_mb).

B1 SHOULD (load, PROVEN): `sed -n 1,3p <300 MB file>` -> 1842 ms, server peak working set 322 MB (head -n 3 on the same file: 2 ms, 7 MB). cmd_sed (builtin.rs:1553) does fs::read of the whole file and only then checks `len > READ_CAP` (1554), then declines and the real sed runs. On a multi-GB log the server allocates the whole file. Fix: stream lines with BufReader and stop after line `to` (as read_head_tail does); check metadata().len() first for the NUL sniff window. Test: sed on a file over READ_CAP returns lines without reading past `to` (assert on a file with a short head and a huge tail; time/size bound).
B2 NICE (PROVEN): `whoami` builtin prints `HOST\User` in the case of the env vars; whoami.exe prints all lower case (`host\user`). Scripts that compare the string differ. Fix: lower-case both parts (builtin.rs:2104-2117).
B3 NICE (PROVEN): `mkdir p\q\r` -> exit 1 'The system cannot find the path specified' - cmd and PowerShell both create the parents; only POSIX mkdir does not. Fix: create parents when the operand is spelled with a backslash (Windows spelling), keep POSIX rule for `/`. (low value; decide in plan)
B4 NICE (PROVEN): `touch adir` (a directory) -> exit 1 'Access is denied'; real touch updates the folder time. Fix: return None for a directory operand (builtin.rs:1979).
B5 NICE (PROVEN): `cp -r srcdir afile.txt` -> 'The system cannot find the path specified (os error 3)'; GNU says cannot overwrite non-directory with directory. Cause copy_rec swallows AlreadyExists from create_dir when dst is a FILE (builtin.rs:1761). Fix: check dst is a dir after AlreadyExists.
Not findings (probed): rm -f on a read-only file and rm -rf on a tree with read-only files (top level and nested) all remove; rm -r on a junction removes the link only, target kept; rm on a junction without -r says 'is a directory (use -r)'; `del adir` refuses (cmd would prompt); `ls` of 301 entries through MCP = generic 80+60 cut with a `full output:` spool file (ls sampler is CLI-only); cat of 300 MB: 580 ms, 108 MB peak, cut at 32 MiB with note; wc -l 300 MB: 304 ms 8 MB; tail -n 2: 1 ms.
Not probed, judged fine by reading: grep builtin declines every shape it cannot answer exactly (regex chars, -R, -w, -o, two -e, binary hit, >32 MiB, >20,000 files, incomplete walk); head/tail windows; seq float handling; cp/mv into self; move_across temp+rename.

### index.rs findings (read in full; probe tools/ix1.py, outputs out/ix1.txt ix1d.txt ix1f.txt)
The index exists only for the server's start folder (warmup.rs:25 → fsops::prime → index::ensure). Each case below
compares the long-lived indexed server with a fresh `QX_INDEX=off` server on the same folder.

#### IX1 SHOULD (wrong search result) — a FILE named like a generated folder is invisible to the index, and stays after delete
index.rs:291-297 applies `skip_dir_name` to every path component, the file name included; the walk applies it to
directories only (fsops.rs:2978-2980).
- After start, create file `vendor` (text `NEEDLE_vendor_file`) in the root → `glob **/*` and `grep NEEDLE` do not
  show it; the walk shows it. Reply adds a false note `[qx] not searched: build, vendor (all=true includes them)`.
- File `tools/dist` exists at start, then is deleted → `glob tools/*` still lists `tools/dist`; walk: "No files found."
Names hit: build, dist, target, vendor, coverage, venv, node_modules, .cache … (fsops.rs:3048-3064). `build` and
`dist` scripts with no extension are common.
Fix: in `change`, test only the parent components by name; for the last component stat first and apply the name rule
only when it is a directory (for REMOVED: try the exact-file removal first, then the name rule). Test: the two cases.

#### IX2 SHOULD (wrong search result) — a delete through the 8.3 short name leaves the file in the index
`longfilename_alpha_beta.txt` (short `LONGFI~1.TXT`): `del LONGFI~1.TXT` → `glob *.txt` still lists
`longfilename_alpha_beta.txt`; the walk does not. (The kernel reports the name the caller used.) Append through the
short name: fine. Delete as `FOO.TXT` of `Foo.txt`: fine.
Fix: on REMOVED/RENAMED_OLD whose rel matches no file and no directory range, and whose last component contains `~`,
mark the index broken (rebuild); cheap and exact. Test: the case above.

#### IX3 SHOULD (index and walk disagree) — a git-ignored folder created after start is searched
Repo with `.gitignore` = `out/`. After start, write `out/gen.txt`; 33 s later `glob **/*.txt` and `grep NEEDLE` list
`out/gen.txt` from the index; the walk leaves it out and says `[qx] not searched: out`. The ignore list is read once
at build (index.rs:397 → walk_live → git_ignored_dirs) and add_tree (index.rs:331) never consults it; the same holds
when `.gitignore` itself changes. Effect: grep reads build output / old copies the walk is designed to skip (noise,
tokens), and results change with whether the index happens to be alive.
Fix: keep the ignore set with the index; in add_tree / change for a new top-level-or-nested directory ask
`git_ignored_dirs(root)` (cached 30 s) and skip + record it; a change to any `.gitignore` or `.git/info/exclude`
marks the index broken. Test: the case above; `.gitignore` edit case.

#### Checked, fine (no finding)
- Move a 20,000-file folder into a 40,000-file indexed root: first glob after it 222 ms once, then 2.4 ms (plain walk
  58 ms). `upsert` is O(n) per insert but the cost is small at these sizes; MAX_WALK_FILES is not re-checked in
  add_tree (index can pass 200k after a huge move; memory only, no wrong result) — NICE: drop the index past the cap.
- Case-variant delete, rename dir to/from a generated name, buffer overflow → broken → walk, junction → broken.
- `unsafe impl Send for Index` (index.rs:159): handles are only used under the per-index mutex; Pending is boxed so
  the kernel buffer does not move. Sound.
- Drop waits up to 2 s for the cancel packet when `issue()` failed in build (nothing pending) and leaks 256 KB:
  only on a failed watch (e.g. a share that refuses the request) and on the prime thread. Not shown by a case.
- UNC roots: a 256 KB buffer over SMB is documented to fail (64 KB limit) → index never builds → plain walk. Not
  tested (no share on this machine); result is only "no index".

### digest.rs findings (read in full, probed with tools/d1.py and d2.py; outputs in out/d_*.txt)

All probes: release exe in probe\, MCP `run`, scratch folders under probe\work\d.

#### DG1 SHOULD (output quality: an error is dropped) — failing test run keeps only lines it recognises
Root: `diagnostic` (digest.rs:1011) on a red run keeps `fail_line` blocks + `summary_line` and drops every other line. The
user's rule is the reverse: drop only noise. Three real cases:
- a) Go, real `go test ./...` (module with pkg a failing test, b ok, c test file that does not compile). Full text has
  `FAIL example.com/m/c [build failed]` and (stderr) `c\c_test.go:6:14: undefined: nope`. Compact reply has neither: it
  shows only TestAdd, `FAIL example.com/m/a`, `ok example.com/m/b`, "[qx] 4 diagnostic lines omitted". The reader sees one
  failing package, there are two. (`FAIL<TAB>pkg` is not matched: fail_line tests `"FAIL "` with a space, digest.rs:879.)
- b) Node built-in runner, real `npm test` (script `node --test`, 3 tests, 2 fail). Reply drops `✖ adds numbers` and
  `test at a.test.js:3:1` (name and place of the first failing test) and the counts `ℹ tests 3 / pass 1 / fail 2`. The
  second failing test's name survives only because it sits inside the first block's 30-line window. (`✖` U+2716 is not in
  fail_line; only `✕` and `×` are.)
- c) Same shape, not run (tool not installed): mocha `1) adds:` / `1 failing`, dotnet `Failed X [3 ms]`, mvn `[ERROR] Tests run:`.
Fix: on a red run (code != 0) invert the filter: drop only `routine_line` matches, keep everything else, hold to the
existing 240-line budget with spool. Add `FAIL\t`, bare `FAIL`, `✖`, `ℹ ` counts to the recognisers anyway. Tests: the
three texts above as fixtures (go multi-package, node:test, mocha) asserting the named lines are in the reply.

#### DG5 SHOULD (output quality: an error is dropped) — traceback digest drops stderr lines before the traceback
`python job.py` where job.py does `logging.error('database refused connection at 10.0.0.7:5432 (attempt 3 of 3)')` then
raises RuntimeError('job aborted'). Reply stderr = the traceback only; the `ERROR:root:database refused …` line (the real
cause) is gone. A spool path is printed. Cause: `traceback` (digest.rs:456) keeps a line only when in_py or a node
pattern matches; anything before `Traceback` is dropped. Node branch has the same structure (keeps only lines matching a
list, digest.rs:501-513).
Fix: keep non-frame stderr lines outside the traceback (bounded: last N=20 before it, or all when under the 120 budget);
the digest's job is to trim frames, not to drop log lines. Test: fixture with a logged error before the traceback.

#### DG6 SHOULD (output quality: a result is lost, no way back shown) — search digest cuts a hit at 500 chars silently
`rg -n needle_here .` over two files whose 4 lines each are ~1530 chars with `needle_here` at column ~1215. Reply: each
hit is `  1: var a0 = 1; xxxx…` (506 chars), the word that matched is NOT in the reply (has_match_word=False), no
`[qx] full output:` line, no note. Cause: digest.rs:1235 truncates to 500 chars but does not set `clipped`, and
`omitted` is 0, so `lossy()` is false and nothing is spooled.
Fix: set clipped when any hit is cut (so it is spooled and noted); better, cut around the match is impossible here (no
pattern known), so keep head 250 + tail 250 or leave long lines to `compact::MAX_LINE_CHARS` handling, which adds
`...[+N chars]` and a spool. Test: long-line fixture asserts clipped == true and the note.

#### DG4 SHOULD (output quality) — `git log` with a patch flag the list does not know loses the whole patch
`git log -2 -U1`, `git log -2 --patch-with-stat`, `git log -g -3`, `git log -2 --parents` → reply is 2-3 one-line commits
plus "[qx] git log condensed (7-char hashes, bodies dropped)". The patch / stat / reflog lines / parent hashes are gone
(spooled, but the note says only "bodies dropped"). Cause: `is_git_log` (digest.rs:289) is a deny-list of flags; `git_log`
(314) silently ignores any line that is not commit/Author/Date/indented.
Fix: make `git_log` return None when it meets a non-empty line it does not know (anything except `commit `, `Author:`,
`AuthorDate:`, `Commit:`, `CommitDate:`, `Date:`, `Merge:`, 4-space message, blank) and when `commit` carries more than
one hash. Shape test replaces the flag list. Tests: the four commands above as fixtures → None.

#### DG3 NICE (wrong result) — `git status --show-stash` lists the stash note as an untracked file
Scratch repo: 1 modified, 2 untracked, 1 stash. Reply: `untracked (3): new1.txt, new2.txt, Your stash currently has 1 entry`.
Cause: digest.rs:715-719 pushes every line while the section is untracked/ignored. Same for any trailing note
(`It took N seconds to enumerate untracked files…`, not reproduced).
Fix: in those sections accept only lines that were TAB-indented in the raw text; any other unknown line → return None
(as the code already does for the other sections). Test: fixture above.

#### Not findings (checked)
- JSON minify of a 111 KB pretty file: one line cut with `...[+47099 chars]` + spool path. Lossy but marked.
- `go test` with only a failing test, cargo/pytest/jest/vitest shapes: recognised.
- Unknown red shape with zero recognised lines → digest declines → generic compaction (good safety net; DG1 is the case
  where ONE line is recognised and the rest is dropped).
- `git show sha:file` (no hunks) → declines. Diff ≤400 lines shown whole. Clipped diff still names later files.
- `find -printf`, `git ls-files -s` through `paths`: no wrong grouping found.
- is_git_status flag test, is_search `ends_with(" grep")`: cannot produce a wrong reply (digest declines on shape).
- node traceback with one stderr line before it: digest declined (<10% saved), line kept. Same weakness as DG5 in code,
  not reproduced for node.

#### Tests gap
No fixture for: go multi-package, node:test, mocha, dotnet test, log line before a traceback, long grep hit, git log with
patch flags, status --show-stash.

### redact.rs + compact.rs findings (both read in full; probe tools/r1.py, output out/r1.txt)

Probe: a file of made-up values (each carries a LEAKnn marker) read with `run cat secrets.txt` through MCP.
Controls `API_KEY=…` and `ghp_…` are masked, so the probe itself works. Not in the 11-08 / 11-09 reviews (those list:
`#` in URL password, `curl -u`, PEM peek, --stream).

#### R1 SHOULD (redaction gap) — everyday password names in a .env are shown
`cat .env` (or `type`, `env`, `set`) with any of these lines returns the value unmasked:
`DB_PASS=…`, `SMTP_PASS=…`, `MYSQL_PWD=…` (MySQL's own variable), `Server=db1;Uid=sa;Pwd=…;` (ADO/ODBC string),
`"pwd": "…"`, `RAILS_MASTER_KEY=…`, `ENCRYPTION_KEY=…`.
Cause: KEYS (redact.rs:7-37) has password/passwd/secret/token/api_key… but not `pass`, `pwd`, `master_key`, `encryption_key`.
Fix: add `pwd`, `pass` (only at a name boundary: start or after `_`/`-`/`.`/`;`, so `bypass=`/`COMPASS=` stay), `master_key`,
`encryption_key`, `signing_key`. Tests: each line above masked; `bypass=abcdefgh`, `compass: north-east` untouched.

#### R2 SHOULD (redaction gap, known token shape) — a GitHub token after a `\n` escape is shown
`{"text":"first\nghp_<36 chars>"}` (a JSON log line / API reply with an escaped newline before the token) → shown in full.
Cause: mask_tokens `edge` (redact.rs:443) wants a non-alphanumeric byte before the prefix; the `n` of `\n` is alphanumeric.
Fix: also an edge when the two bytes before are `\` + one of n, t, r. Test: this line, plus `\tsk-…`.

#### R3 SHOULD (wrong result: a path is hidden) — paths that start like a token prefix are masked
`cat paths.txt` (same through find / git status / ls output) returns:
`i18n/sk-SK/LC_MESSAGES/app.po` → `i18n/sk-***`; `node_modules/sk-spinner/dist/sk-spinner.min.css` → `node_modules/sk-***`;
`vendor/hf_transfer-0.1.6/src/download.rs` → `vendor/hf_***`; `docs/SG.Core.Services.Messaging.Internal.md` → `docs/SG.***`.
"[qx] redacted 4 secret(s)". The agent cannot see which file it is (Slovak locale folders `sk-SK` are common).
Cause: is_token_byte (redact.rs:488) accepts `/` and `.` for every prefix, min length only.
Fix: body alphabet per prefix (sk-, hf_, glpat-, pypi-…: `[A-Za-z0-9_-]`; SG.: plus `.`; only ya29./eyJ/base64 keep `/+=.`) and
require at least one digit in the body for prefixes of ≤3 chars. Tests: the four paths untouched; real-shaped keys still masked.

#### R4 NICE (redaction gap) — credential formats without `=`/`:` between name and value
Shown unmasked: `machine api.example.com login bob password …` (.netrc), `Authorization: Token …` (only Bearer/Basic are
handled, redact.rs:497), `New-Thing -User bob -Password …` (flag form needs `--`, redact.rs:245),
`Cookie: sessionid=…`, `https://<PAT>@dev.azure.com/org/_git/repo` (token as URL user, no colon).
Fix: netrc: `password <word>` after `login <word>`; auth: any scheme word after `Authorization:`; single-dash flags
whose name is a KEY; URL userinfo with no colon and ≥20 token chars. Cookie: decide (policy), default mask after `Cookie:`/`Set-Cookie:`.

#### R5 NICE (redaction gap) — token prefixes not in the table
Shown: Slack `xapp-…` (and `xoxe-`), Google `GOCSPX-…`, Vault `hvs.…`, Stripe `rk_test_…`, Shopify `shpat_…`,
`AGE-SECRET-KEY-1…` (age/sops private key). Fix: add to PREFIXES + is_prefix_start (the sync test covers it).

#### R6 NICE (redaction gap) — INI `password = value` is judged as code
`password = $LEAK…`, `password = abc(secret)99`, `password = hunter.two.three` are shown (my.cnf / .ini / .conf style).
Cause: looks_like_expression (redact.rs:379) for the spaced `=` form. Fix: apply the code test for spaced `=` only when
the line also looks like code (has `let|const|var|val|:=`, ends with `;`, `)` or `{`, or the key is indented inside a
call); a bare `key = value` line is config. Needs care: keep `token = jwt.sign(...)` readable. Tests both ways.

#### Not findings
- `fs` read/grep do not redact: documented ruling (README:362, 10-21 review text-pipeline-9). Not repeated.
- UTF-16 file through `cmd /c type`, `cat`, PowerShell Get-Content: decoded and masked.
- guard audit log masks the command (guard.rs:2209).
- A lone `-----BEGIN PRIVATE KEY-----` line hides up to 200 following lines: fail closed, listed in the 10-21 review.
- compact.rs: head/tail + rescue, long-line cap with `...[+N chars]`, spool pruning (newest 40 + younger than 10 min),
  settle_frames, relative_paths: no wrong result found. `cat rep.csv` with 4 equal rows prints `0,0  (x4)` plus
  "[qx] collapsed 3 repeated line(s)": lossless and marked, by design.
- digest JSON line cut: marked (see f-digest.md).

### resolve.rs findings (read in full; probes tools/rs1.py rs2.py rs3.py, outputs out/rs*.txt)

#### RS1 SHOULD (wrong program runs) — a `PATH=` prefix does not change which program qx starts
Scratch dir binA holds `python.cmd` (prints FAKE-python-from-binA) and `zz_only_here.cmd`.
- `run command="PATH=<binA> python --version"` → `Python 3.12.6` (the system one). The child does get PATH=binA
  (`PATH=<binA> cmd /c echo %PATH%` prints binA), so the program and its environment disagree.
  Control: `set "PATH=<binA>;…" && python --version` through cmd prints FAKE-python-from-binA.
- Real use: `PATH=.venv\Scripts python -m pytest` runs the global interpreter, not the venv's.
Cause: resolve() reads PATH once per process (resolve.rs:329 OnceLock) and caches by name (resolve.rs:39); the call's
envs are never looked at. exec.rs:1877 already treats a PATH env as special for npm only.
Fix: when a call's envs carry PATH, resolve through `resolve_in(program, split_paths(that PATH), cwd)` (exists,
uncached) in build_command_env. Test: unit test with two temp dirs, plus the MCP case above.

#### RS2 NICE (a valid line fails) — `PATH=a;b tool` is sent to PowerShell whole
`run command="PATH=<binA>;C:\Windows\System32 zz_only_here"` → exit 1, PowerShell: "The term 'PATH=C:\…\binA' is not
recognized". Any Windows PATH value has `;`. Cause: the auto shell choice sees `;` (statement separator) before the
env prefix is peeled, and peel_env does not take the word. Fix: peel leading `NAME=value` words first; a `;` inside
that word is part of the value. Test: the line above prints A-tool-ran.

#### M10 NICE (mcp.rs; silent wrong result) — an argument `run` does not know is dropped without a word
`run {program:"cmd", args:["/c","echo","%ZZ_PROBE%"], env:{"ZZ_PROBE":"set"}}` → prints `%ZZ_PROBE%`, isError false,
no note. Same for a typo (`cwdd`, `timeout`). An agent that passes `env` (a common MCP exec argument) believes the
variable was set. Fix: either accept `env` (object of strings) on `run`, or answer with an error naming the unknown key
and the `VAR=x cmd` form. Recommended: accept `env` (it also gives program+args calls a way to set variables; today
only `command` can). Test: unknown key → error text lists valid keys; env reaches the child.
(My own earlier probe d2.py "QX_DIGEST=off in call env" was void for the same reason.)

#### Not findings (checked)
- git through de_trampoline (cmd\git.exe → mingw64\bin\git.exe): a pre-commit hook still finds sed and /usr/bin/ssh,
  same output as plain git.
- plain_shim / launch_shape / generic_shim: a shim with `set X=`, node flags or a fixed argument stays a Shim (cmd.exe).
- list_prefix: zeroed struct, handle closed once, non-"nothing here" errors fall back to stats.
- compute_path_dirs `pathext=` cut: ASCII lowercase keeps byte offsets.
- cache cap 512 without eviction: past the cap each call rescans PATH (ms), no wrong result.
- scout h4 "resolve CACHE cap 512": not a finding.

### hook.rs, main.rs, warmup.rs, client.rs (read in full except client 143-278; probe tools/h1.py, out/h1.txt)

#### hook.rs — evidence it is right
27 Bash lines rewritten by `qx hook claude` and run in Git Bash give the SAME stdout, stderr and exit code as the
original (heredoc, here-string, `< file`, `while read … done < in.txt`, quoting, globs, `~`, `/c/Windows`,
`--flag=/tmp/x`, exit codes, env prefix, loops, `if`, comments, multi-line, UTF-8, no trailing newline).
`timeout 2 python …` → `qx --timeout 2 …`: same stdout and rc=124, one extra stderr line `[qx] timed out after 2s` (fine).
`git status --short | wc -l` left as written. PowerShell: only plain native statements rewritten (`$x = …`, pipelines,
`Set-Alias`, `npm run dev` left alone). Default permission mode with no matching allow rule → no rewrite.

#### H1 NICE (output lost near the tool limit) — every captured stage gets the whole budget
`python a.py && python b.py` → `qx --timeout 118 python a.py && qx --timeout 118 python b.py`. If a.py takes 100 s,
b.py still has 118 s; Claude kills the tree at 120 s and b's captured output (qx prints only at exit) is lost — the
case the comment at hook.rs:78-79 says the budget exists to prevent. Shown by the rewrite only, not by a 2-minute run.
Fix: one shared deadline for the line (`--deadline <epoch-secs>`; each qx computes what is left). Test: unit test on
`plan` output + CLI test that `--deadline` in 1 s cuts a 5 s sleep and prints what it had.

#### MN1 SHOULD (wrong behaviour, confirmed) — a bad number for a flag is silently replaced
`qx --capture --timeout 0.5 python -c "import time; time.sleep(2); print('finished, no timeout')"` prints
`finished, no timeout`: parse_num (main.rs:109, used 173) falls back to 120 s for anything that is not an integer
(`0.5`, `5m`, `30s`). Same for --max-lines/--head/--tail. `qx --timeout --raw cmd` swallows `--raw` as the number.
Fix: exit 2 with "qx: --timeout needs a number of seconds (got '5m')"; accept `30s`/`5m` like hook.rs timeout_secs.
Test: CLI tests for 0.5, 5m, missing value.

#### D4 (help drift) main.rs:734 "QX_FS_THREADS=N … max 8" vs fsops.rs:3170-3179 explicit value clamp(1, 32).
#### D6 (help drift) main.rs:738-742 built-ins list lacks start-sleep get-content gc test-path write-output write-host
(all in builtin.rs NAMES 108-113).

#### Checked, fine
main: guard runs before built-ins; `-c` extra args rejected; bad --cwd rejected; write_out ignores broken pipe;
redact handles ANSI; stats are opt-in. warmup: prime_tools 5 s cap each, idle tick 10 s, unchecked Win32 returns are
benign (failure = no boost), timeBeginPeriod(1) never ended (process lifetime; costs a little battery, no wrong result).
client.rs 1-142: client kind in a process-wide AtomicU8 — relevant to test races (see tests using `scoped`/`lock_global`).

### client / npm / python / deploy / doctor (all read in full)

#### client.rs — no finding
`native`, `exit_is_error` (only QX_EXIT_ERROR=1), `structured`, `instructions` (< 1024 bytes), CURRENT AtomicU8,
thread-local SCOPED for tests, `lock_global` test mutex. Nothing wrong found.

#### npm.rs — GREEN, no finding (probe tools/np1.py, out/np1.txt)
`npm run` hello, args, fail (rc 7), pre/post chain, failing `pre` stops the chain, `%USERNAME%x`, `a^b`, `'a b'`,
`-- --x=1 y`, run from a sub-folder: all SAME as real npm (npm's banner lines excluded on purpose).
Not tested: a GLOBAL npmrc with `script-shell` (redirecting APPDATA did not move npm's `globalconfig`; the real file
was not touched). npm.rs reads project and home .npmrc only → possible gap, but no case, so no finding.
`plain_npm_env` (627-634) is test-only.

#### python.rs — GREEN, no finding
venv python `-c` with args, exit code 5, child of `sys.executable`: SAME. Only difference is qx's own stderr line
`[qx guard] warning: inline code spawns a subprocess` (expected).

#### deploy.rs — no failing case
Observations (NICE at most): `sweep_aside` removes only `qx.exe.old-*`; a `.new-PID` left by a crash is never swept.
Between the two renames there is a very short time with no `qx.exe`. To probe: `deploy --to <scratch>` twice, and
while a server runs from that path.

#### doctor.rs — D7 doc drift only
doctor.rs:114 prints "[redact] on for captured/MCP output" but `fs` replies are not redacted (README:362 says so).
The user's own `~/.claude/QX.md` has the same wording (their file: point it out, do not edit).

### setup (mod, jsonc, toml, yaml, pi) and deploy — all read in full
Probes: tools/su1.py (every home variable points into scratch; real configs never touched), tools/dp2.py.
Outputs: out/su1c.txt (claude), out/su1t.txt (codex, hermes), out/su1d.txt (dsh, pi, opencode, deploy), out/dp2.txt.

#### SU1 SHOULD (data loss) — `qx setup dsh` deletes unrelated programs from <dsh>\bin
setup_yaml.rs:460 matches `name.starts_with("qx") && name.ends_with(".exe")`.
Case (D4): `<dsh>\bin` holds `qxl-viewer.exe` and `QXtool.exe` (not qx builds). `qx setup dsh` prints
`dsh: removed 4 unreferenced qx cop(ies)`; both are gone, no backup. (The old copies had no fixed name: README only
said "keep a versioned copy under $DSH_HOME/bin", so the pattern is a guess.)
Fix: only `qx.exe` or `qx` followed by a non-letter (`qx-0.8.0.exe`, `qx_1.exe`, `qx.0.8.exe`); print each name removed.
Test: the D4 folder; `qxl-viewer.exe` and `QXtool.exe` must stay.

#### SU2 SHOULD (breaks the user's setup silently) — a symlinked config is replaced by a plain file
setup_mod.rs:393-402 writes a temp file and renames it over `ch.file`.
Case (C5): `~\.claude\settings.json` is a symlink to `dotfiles\settings.json`. After `qx setup claude` the link is a
regular file, and the dotfiles copy has no hook (so the next dotfiles sync drops the hook or the user's edits).
Same for every client file. Fix: in `apply`, if the file is a symlink, write beside its resolved target
(`std::fs::canonicalize`) and rename there. Test: symlink case (skip when the test cannot create a symlink).

#### SU3 SHOULD (broken client config) — Hermes: a comment or blank line less indented than the servers
setup_yaml.rs:261-265 takes the smallest indent of ANY line under `mcp_servers:`, comments and whitespace-only lines
included. Same code in hermes_command (402-406).
Case (Y2b): `mcp_servers:\n  # servers\n    other:\n        command: o\n` → qx block written at 2 spaces after a
4-space sibling → PyYAML: "expected <block end>, but found '<block mapping start>'". Hermes cannot read its config.
Case (Y2): same with a 2-space whitespace-only line and an existing 4-space `qx:` → a second `qx:` at 2 → broken.
qx prints "updated", exit 0. Fix: skip noise lines (`is_noise`) when computing child_indent, both places.
Test: both cases; result must parse (dev-dependency free: assert the qx line's indent equals the sibling's).

#### SU4 NICE (broken config, narrow trigger) — Codex config.toml
- T2: file starts with a BOM and `[mcp_servers.qx]` is line 1 → header not recognised (BOM glued to the line) → a
  second `[mcp_servers.qx]` is appended → "Cannot declare ('mcp_servers','qx') twice". Fix: strip the BOM before the
  line work and put it back.
- T12: a multi-line string (`text = """` … `"""`) in another table holds a line `[mcp_servers.qx]` → the string is
  rewritten as if it were the table (TOML broken); `--remove` then cuts the string and the keys after it.
  Fix: track `"""` / `'''` state in qx_tables, kept and refuses; inside a multi-line string nothing is a header.
- T10: `mcp_servers.qxother.command = 'o'` (another server, dotted keys) → refused although qx is not involved
  (setup_toml.rs:285 `starts_with("mcp_servers.qx")`). Fix: require `.qx.` / `.qx=` / `."qx"`.

#### SU5 NICE — exit code 0 when a config could not be read
T13: UTF-16 `config.toml` → `codex: skipped: … stream did not contain valid UTF-8`, exit 0, nothing installed.
Claude's settings.json in UTF-16 → `refused`, exit 1 (C4). setup_mod.rs:616-621 treats a plan error as "skipped".
Same path for dsh, hermes, opencode. Fix: a plan error counts as refused (exit 1).

#### SU6 NICE — empty settings.json is refused with "empty document"; half install
C2/C2b: a 0-byte or whitespace-only `settings.json` → `.claude.json` updated, `settings.json: empty document`, exit 1:
MCP server registered, no hook. Fix: treat an empty/whitespace document as `{}` (jsonc first_value or the two
callers). (Read-only settings.json gives the same half install with a clear message: fine.)

#### SU7 NICE — a user group that holds the qx hook plus another hook → second qx hook added
C8: group `[{qx hook claude}, {echo mine}]` → after install there are 2 `hook claude` entries (is_qx_hook wants a
group with exactly one hook). Every Bash call then runs the hook twice. Fix: when any group already holds a
`qx hook claude` entry whose command is the wanted exe, report Unchanged; when it names another exe, update that
entry in place.

#### SU8 NICE (cosmetic) — install + remove does not give the original file back; backups of qx's own files
- C1/O1: remove leaves `"PreToolUse": [\n      \n    ]`, `"servers": {\n      \n    }`, `mcpServers: {}`; Y1 leaves
  `mcp_servers:` (Hermes reads that as empty: checked in its source, `or {}`); T1b/T3/D2 leave an extra blank line.
- C3: LF lines written into a CRLF JSON file; a trailing `// keep` comment moves to its own line.
- O2/O3: new member formatting `}\n  ,\n    "qx": {…}}` (comma on its own line, closing brace glued on).
- P1/C1/T1/D1: `--remove` of things qx itself created leaves `.qx-bak` copies of qx's own text (3 in pi's
  extensions folder, plus the empty `extensions\qx\` folder). setup_mod.rs:381 says the first backup "is the
  user's own file"; here it is not. Fix: no backup for pi's generated files; remove empty containers qx created
  (empty array/object it added, the `extensions\qx` folder).

#### Checked, fine
- Claude: fresh install, BOM + comments + trailing commas kept (C3), UTF-16 refused, PreToolUse-as-object refused,
  3 MB `.claude.json` in 29 ms with user env and other servers kept (C6), second run byte-identical everywhere,
  not-deployed exe refused (C11), `--dry-run` writes nothing (C12).
- Codex T3: spaced/quoted header, user keys, multi-line arrays, tuned timeout, env, subtable all kept; CRLF kept.
- Hermes Y4 (user keys, env, tools children, comments, CRLF), Y5 `{}`, Y6 no final newline, Y8 two documents.
  Y3 quoted `"qx":` gets a second `qx:` (PyYAML takes the last; works) — not listed.
  Y7 `mcp_servers: ~` refused with the wrong reason ("flow-style mapping") — message only.
- dsh D1-D3: item among siblings kept in place, user env kept.
- OpenCode V1/V2/jsonc: valid results.
- Lost update when the client rewrites `.claude.json` between qx's read and rename: possible in theory (qx takes
  no lock), no case built → not a finding.

#### Deploy (dp2.txt) — NICE only
- DP1: every deploy that replaces a different build leaves `qx.exe.old-<ms>` behind even when nothing is running;
  the message says "kept … until running clients exit" but only the NEXT deploy removes it. A stale
  `qx.exe.new-<pid>` from a crashed deploy is never removed (deploy.rs sweep_aside matches `.old-` only).
  Fix: after the swap try to delete the aside copy at once (works when no process holds it) and say which happened;
  sweep `.new-*` older than a few minutes.
- `--to <existing folder>` → "Access is denied" (safe, unhelpful message).
- Replace while a server runs from the path: works, server stays alive, new file in place. Unknown flag and
  missing value: exit 2, nothing written.

### pi integration, fuzz, CI, bench script, docs drift — all read in full
Probes: tools/pi1.py, tools/pi3.py (server as client pi), tools/pi2.mjs (bridge.js with node). Outputs: out/pi1.txt, pi2.txt, pi3.txt.
Local pi is 0.85.1 (< 0.99), so `qx setup pi` installs this extension on this machine: it is live code.

#### PI1 SHOULD (wrong search result) — the pi tool text says grep is literal; the server searches a regex
integrations/pi/qx.js:52 `pattern: "glob: pattern. grep: literal text."`, :66 `grep (literal)`. No `literal` field in
FS_PARAMETERS. Server: regex by default (tools/list says `pattern REGEX … literal?`).
Case (pi3): a.js holds `arr[0] = 1;`, `total += price * 2;`, `const re = /^a$/;`, `foo.bar();`, `fooxbar();`.
- grep `arr[0]` → "No matches found." (the line is there)
- grep `price * 2` → "No matches found."
- grep `^a$` → "No matches found."
- grep `foo.bar()` → 2 matches (`foo.bar();` and `fooxbar();`)
A pi model told "literal" concludes the code is not there. Fix: say "regex; set literal:true for plain text" and add
`literal`, `all`, `paths`, `ops`, `edits` to FS_PARAMETERS (or generate both schemas from the server's tools/list in
a test so they cannot drift). Test (node, CI already runs node 22): every key of the server's `fs`/`run` inputSchema
is in the extension's schema; description contains "regex".

#### PI2 SHOULD — a long command becomes a job the pi schema cannot follow
qx.js:14-37 RUN_PARAMETERS has no `background`, `job`, `wait_ms`, `kill`. Case (pi1): `{program:python, args:[-c,
sleep 4], timeout_ms:1000}` → reply `[qx] still running after 1s; moved to background job j1 / Follow: {job:"j1",
wait_ms:270000}`. The schema pi shows the model has no `job`. pi-ai's validation does not strip unknown keys (so a
model that copies the hint may get through), but a provider in strict-schema mode cannot emit an undeclared key:
the job's output is then unreachable and the job runs up to 1 h. (Strict-mode part not run: needs a provider key.)
Also :31 "Default 120000." with no max; :63 built-in list misses grep, sed, etc. Same fix and test as PI1.
Also :66 "write/edit need a prior qx read" — server: edit needs no prior read (README:612). Costs a wasted read per edit.

#### PI3 NICE — bridge: a child that starts but never answers hangs every call, and abort does not help
bridge.js:45 `initialize` has no timeout; :140 `await this.start()` ignores `signal`.
Case (pi2): exe = a process that runs and prints nothing. `call()` still pending after 4 s with no signal; still
pending after an abort at 500 ms. Narrow trigger (qx answers initialize in ~3 ms), but then pi's Esc does nothing.
Fix: race start() against the signal and a 10 s timer; on timeout kill the child and reject.
#### PI4 NICE — one abort listener per call is never removed
bridge.js:135. Case (pi2): 200 normal calls with one signal → 200 listeners added, 0 removed; abort afterwards sends
200 `notifications/cancelled` for finished ids. Fix: remove the listener when the reply arrives.
(Depends on pi reusing a signal across calls of one turn; with a fresh signal per call it is only garbage.)
bridge.test.mjs covers: spawn failure, partial line after death, stale child event. smoke.mjs: one run + one stat.
Not covered: the two schemas, abort, initialize timeout.

#### Fuzz (src/fuzz.rs) — fine, one gap for the matrix
Fixed seeds, 4 targets (compact, redact, guard, digest): no panic, bounded output, idempotence checks. The digest
target always uses exit 1 and one `error:` line, so it cannot see DG1/DG5/DG6 (lost results). No fs or jsonc target.

#### CI (.github/workflows/ci.yml) — no failing case, so observations only (not findings)
- Tests run only in release profile; dev profile (the edit loop, opt-level 2, debug assertions) is never run in CI.
- MSRV 1.82 job builds, does not test. Actions pinned by tag, not SHA. Bench step is continue-on-error, never gated
  (Goal.md's speed targets are not enforced: a 10x slowdown passes CI).
- No test runs `qx setup` against a temp home end to end (unit tests on the editors only): SU1-SU3 were all reachable
  only that way.

#### Bench (scripts/bench_mcp.py) — NICE
- BN1: p90 index `rest[int(len(rest)*0.9)-1]` with 11 samples = the 9th of 11 (≈p82), so the printed "p90" is low.
- Reply read without matching its id (fine while calls are serial). Never fails: no threshold.

#### Docs drift (each checked against the code today)
- D1 README.md:610 "50 KiB / 2000 lines" — code: DEFAULT_READ_BYTES = 49_000 - 1_000 = 48,000 bytes (fsops.rs:38-40).
- D2 README.md:420 "a line over 8 KiB fails" — code: 8,000 (exec.rs:709); an 8,100-byte line fails under "8 KiB".
- D3 README.md:602 "max 270000" is right in effect (soft window, mcp.rs:585) — NOT a finding. README.md:600 lists
  shell values `auto, cmd, bash`; the schema also has `powershell`.
- D5 Gatcha.md:9 "363 passed, 1 ignored (2026-10-24)" — today 431 passed, 2 ignored.
- D7 doctor.rs:114 "[redact] on for captured/MCP output" — `fs` replies are MCP output and are not redacted
  (README.md:362 says so). Fix wording: "on for command output (run); fs returns file contents as written".
- pi extension text: see PI1/PI2.

#### Dropped (no failing case)
- h4: unchecked SetInformationJobObject / TerminateJobObject / ReleaseMutex returns (exec.rs:1355, 1370; fsops.rs:879).
- FS6 scope: `git` is spawned from one place only (fsops.rs:3096); no other call site to harden.

### Scout h4 (globals / unsafe) - map only, TO VERIFY MYSELF
- npm.rs:631 plain_npm_env removes env vars inside Once; 8 npm tests (npm.rs:653,706,736,762,810,826,880,894) + mcp.rs:1865 (before server_lock at 1866) call it with no lock. Judge when reading npm.rs: std::env::remove_var while other test threads read env = race (UB on some platforms; on Windows the CRT lock makes it safe-ish). Also: production removes env vars of the SERVER process? check.
- fsops::MAP (fsops.rs:780, `remember` 785) : inserts, no removal, no cap -> unbounded? judge in fsops.
- fsops::CONTENT (2216): cap refuses insert over MAX_CONTENT_BYTES, no eviction -> cache stops helping after full? judge.
- resolve::CACHE (resolve.rs:40): cap 512 refuses insert, stale entries stay (alive check at 52) -> judge.
- mcp::INFLIGHT: no cap (bounded by gate?).
- unsafe impl Send for Index (index.rs:159) 'used under the registry mutex only' -> check.
- Win32 unchecked returns: SetInformationJobObject (exec.rs:1355) -> if it fails KILL_ON_JOB_CLOSE is not set => orphan children? TerminateJobObject (1370) unchecked; ReleaseMutex fsops.rs:879; CreateMutexW no GetLastError.
- mcp globals guarded by server_lock in tests; client::CURRENT guarded by lock_global.
