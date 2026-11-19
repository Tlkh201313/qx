---
title: "qx wrapping adds no measurable wait to cargo, npm, node or python; Cursor is blocked by its own allowlist, not by qx"
author: Tlkh201313
date: 2026-11-08
branch: review/full-codebase
cost: { minutes: 25, tokens: 257445 }
id: l-5053d1af
scope: repo
status: active
src: agent
touches:
  - docs/next-step-vibecoders.md
  - src/exec.rs
  - src/npm.rs
  - src/python.rs
---

## Decided
Do not chase wrapper startup for cargo, rustc, npm, node, python. Measured 2026-11-08 on the release build, median of 7: qx adds -7 to +9 ms over running the program directly (cargo --version 24 vs 31 ms, node -e 1 41 vs 32, python -c 1 72 vs 66, npm --version 174 vs 221). The qx mcp server idles at 6 MB working set and peaks near 10 MB.

## Tried and ruled out
Looking for lock waits and window hits in 2026-10-24..11-08 transcripts: zero "Blocking waiting for file lock" results; cargo via qx is 39 min of wall time in total (test 55 min of that is the suite itself, median 3.1 s, p90 21 s). The waits left are the programs' own run time (node and npx p90 40-60 s, 30 window hits for node), which qx cannot shorten without changing what the command does.

## Watch out
Cursor is not wired to qx: ~/.cursor/mcp.json has no qx entry and ~/.cursor/cli-config.json has approvalMode "allowlist" with only Shell(ls) allowed, so a Cursor agent that reaches for cargo through its own shell tool waits for approval. This is inferred from config, not seen in a Cursor log. qx has no `setup cursor` (docs/next-step-vibecoders.md lists it as planned); Cursor's mcp.json uses the same `mcpServers` shape the pi setup already edits.
In the code: src/exec.rs, src/npm.rs, src/python.rs hold the launch paths that were measured.
