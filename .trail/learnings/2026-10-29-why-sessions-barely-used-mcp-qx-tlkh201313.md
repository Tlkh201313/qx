---
title: Why sessions barely used mcp__qx__run (deferred behind ToolSearch) and the alwaysLoad…
author: Tlkh201313
date: 2026-10-29
id: l-47d865bf
scope: repo
status: active
touches:
  - src/mcp.rs
---

## Decided
Sessions since 2026-10-20 made 9,962 Bash calls vs 186 `mcp__qx__run` calls (<2%) despite QX.md and the server instructions: with `ENABLE_TOOL_SEARCH=auto` and big MCP servers (rea, Vercel, Supabase) loaded, Claude Code deferred qx's tools behind ToolSearch, so the model reached for Bash.

2026-10-24: `fit_schema` in `src/mcp.rs` adds `_meta: {"anthropic/alwaysLoad": true}` to `run` and `fs` for the Claude Code client only (documented in Claude Code's mcp docs; a server entry's `"alwaysLoad": false` overrides). tools/list for Claude Code is ~4.5 KB. Deployed to `~/.cargo/bin/qx.exe` the same day (old exe kept as scratchpad `qx-installed-before-alwaysload.exe`); it takes effect only when Claude Code restarts the qx server (new session or `/mcp` reconnect). Left uncommitted unless the user asks.

- Comparing hook speed between two qx builds: run each as `qx.exe` from a dir put first on PATH. A qx not on PATH bails out early in `qx_on_path` and looks 2x faster.
- Measured per command: Bash tool start ~100-125 ms, PowerShell tool ~0.5-0.8 s, qx run ~1-3 ms; the ~1 s hook floor applies to all of them (claude-hook-latency).

**Why:** instructions alone did not change tool choice; the tool has to be visible.

## Watch out
check `mcp__qx__run` usage counts in transcripts after the next sessions to see whether alwaysLoad moved the share.

_From Claude Code's memory of this repo, last updated Oct 24._
