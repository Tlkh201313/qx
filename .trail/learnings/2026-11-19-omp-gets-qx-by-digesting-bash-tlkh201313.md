---
title: "omp gets qx by digesting bash results, never by replacing or rewriting bash"
author: Tlkh201313
date: 2026-11-19
branch: docs/readme-rewrite
id: l-9d7d45b8
scope: repo
status: active
replaces: l-7b928e18
src: agent
touches:
  - integrations/omp/qx.js
  - "src/main.rs#squeeze"
  - src/hook.rs
---

## Decided
omp's native `bash` runs the model's exact command; `integrations/omp/qx.js` digests the result in `tool_result` by piping the body to `qx squeeze --label <command> --code <exit> --cwd <dir> --max-ratio 70`, keeping omp's trailing notices (`Wall time: …`, `Command exited with code N`, `[…]` lines) verbatim. omp's approval rules, the transcript and the model's command all stay untouched; squeeze also records the command in qx history for the compaction feed. History replay is limited to passing test/lint runs (`cargo test|check|clippy`, `pytest`, `go test|vet`, `npm test`, `dotnet test`), with the fingerprint taken before the run and the stored reply always sent.

## Tried and ruled out
- Re-registering `bash` and delegating through `ctx.invokeTool`: the extension tool gets omp's default `exec` approval only (`ToolInfo` has no `approval`, `invokeNativeTool` skips the gate), so user deny/prompt patterns and the critical-command prompts (`rm -rf /`, even in yolo) silently stopped applying.
- `tool_call` input rewrite to `qx …` (`qx hook omp`, now deleted): the rewritten line is baked into the transcript and user allow rules stop matching.
- Squeezing omp's native grep/glob results: their output is a heading tree, not `rg`/`find` text, so the digest never fired.
- Replaying cheap read commands (`ls`, `cat`, `git …`): two git spawns cost ~70 ms per call, and ignored or outside files, refs and untracked-dir edits made replays stale.

## Watch out
- An exe under `target/` cannot write the Temp spool on this machine; lossy digests then fall back to the full text, so benchmarks run from `target/` silently skip them. Measure with a copy in `~/.cargo/bin`.
- gold_eval scores facts present in omp's raw view (reads are capped at 300 lines); facts outside that view say nothing about qx.
