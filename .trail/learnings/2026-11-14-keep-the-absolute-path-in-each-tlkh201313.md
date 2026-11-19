---
title: Keep the absolute path in each file-index entry; rebuilding it per visit made glob and grep slower
author: Tlkh201313
date: 2026-11-14
branch: docs/readme-rewrite
cost: { minutes: 1097, tokens: 9581539 }
id: l-54bbf35c
scope: repo
status: active
src: agent
touches:
  - Map.md
  - src/builtin.rs
  - src/exec.rs
  - src/fsops.rs
  - src/guard.rs
  - src/main.rs
  - src/mcp.rs
  - src/redact.rs
  - src/setup/mod.rs
  - src/setup/yaml.rs
---

## Decided
Leave the in-memory file index storing both the absolute path and the relative path for every file. Dropping the absolute copy saves memory only in repos near the 200k-file cap; in a normal repo it saves under a megabyte.

## Tried and ruled out
Removed the absolute path from each index entry and rebuilt it (index root + relative path, slashes flipped) every time a search visits a file. With two PathBuf pushes per file, glob and grep got about 0.8 ms slower; with one reused string buffer, a `**/*.md` glob still went from about 0.5 ms to 0.75 ms. Speed is the top goal, so reverted. A lazy path (built only for files that match) would need the visit callback to change; not tried.

## Watch out
Timing `run` lines with `target\release\qx.exe` gives false failures: bash pipelines die with `NtCreateDirectoryObject 0xC0000022` (exit 127) because of the Low integrity label on `target\`. Copy the exe out first, then run `scripts/bench_mcp.py`, and compare old and new back to back; this machine's timings drift by 2x between runs.

In the code: src/index.rs (`Entry.path`), src/fsops.rs (`walk_snapshot`), scripts/bench_mcp.py, Gatcha.md.
