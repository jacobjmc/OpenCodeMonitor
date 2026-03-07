---
shaping: true
---

# REST Backend Status and Parity

OpenCodeMonitor uses `opencode serve` (HTTP REST + SSE) as the backend transport.
All OpenCode-to-CodexMonitor protocol translation remains in Rust so the frontend continues to consume CodexMonitor-shaped events.

## Current Backend Shape

- One shared `opencode serve` process
- Managed server binds explicitly to `127.0.0.1`
- Managed server prefers the monitor port and falls back to a free localhost port on conflict
- Workspace scoping via `?directory=<workspace_path>` on REST requests
- One SSE subscription on `/global/event`
- REST and SSE requests honor `OPENCODE_SERVER_PASSWORD` / `OPENCODE_SERVER_USERNAME` when present
- Managed server health is re-checked before reuse and restarted in place if the child died
- Rust translation layer maps OpenCode SSE events into CodexMonitor frontend event shapes
- Frontend remains transport-agnostic

## Implemented in Backend Core

These features are implemented against OpenCode REST in `src-tauri/src/shared/codex_core.rs`:

- Thread/session lifecycle: start, list, resume, interrupt
- Message send via `/session/:id/prompt_async`
- Image attachments via REST `file` parts (`data:` URLs or fetched public URLs)
- Model/provider discovery via `/config/providers`
- Collaboration mode list via `/agent`
- Thread fork via `/session/:id/fork`
- Thread rename via `PATCH /session/:id` (`title`)
- Thread archive via `PATCH /session/:id` (`time.archived`)
- MCP status list via `/mcp`
- Skills list via `/skill`
- Review start via `/session/:id/command` using OpenCode built-in `review` command

## Feature Parity Status (CodexMonitor UI vs OpenCode)

### Compatible (native OpenCode support)

- Fork thread
- Archive thread
- Rename thread
- MCP server status
- Skills listing
- Review (via OpenCode command API)

### Partial / Translation-based

- Review delivery modes: OpenCode has a built-in `review` command, but CodexMonitor's `start_review` API is translated to `session.command`. Detached review uses a forked thread.
- Thread archive visibility: OpenCode session archive state is respected in backend listing, and frontend may also still apply local hidden-session behavior.

### Intentionally Out of Scope / Removed from Product UX

- `turn_steer` (Codex-style turn steering; no direct OpenCode REST endpoint)
- `apps_list` (Codex app catalog shape does not map cleanly to OpenCode `agent`/`command`/`skill`/MCP concepts)
- `account_rate_limits` (no equivalent OpenCode account/rate-limit API endpoint)
- `codex_login` (Codex-specific concept; OpenCode uses provider auth and OAuth/API-key flows)

## Next Parity Work

1. Add/expand tests for REST parity adapters (MCP status mapping, skill mapping, review target translation).
2. Continue parity polish on features that have direct OpenCode equivalents.

## Architecture Invariants (still required)

- All OpenCode protocol translation stays in Rust.
- Frontend thread reducer receives CodexMonitor-shaped events.
- Shared backend behavior lives in `src-tauri/src/shared/*` first.
- App and daemon remain thin adapters over shared core logic.

## File Anchors

- Event translation: `src-tauri/src/backend/event_translator.rs`
- Shared protocol methods: `src-tauri/src/shared/codex_core.rs`
- REST process + SSE routing: `src-tauri/src/backend/app_server.rs`
