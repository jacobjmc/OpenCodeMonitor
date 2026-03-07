# OpenCode Monitor

OpenCode Monitor is a desktop app for monitoring and interacting with [OpenCode](https://github.com/anomalyco/opencode) agents across multiple workspaces.

It is forked from [CodexMonitor](https://github.com/Dimillian/CodexMonitor) by Dimillian, adapted to use OpenCode's REST API + SSE backend while preserving CodexMonitor-shaped frontend event contracts.

OpenCode Monitor is an independent community project and is not affiliated with or endorsed by the OpenCode team.

## Status

**Active development** — core REST/SSE support is live for thread/session lifecycle, event translation, messaging, model discovery, approvals, and image attachments. Remaining work is parity polish and OpenCode-specific UX cleanup.

## Install

Download the latest packaged build from [GitHub Releases](https://github.com/jacobjmc/OpenCodeMonitor/releases).

Current release targets:

- macOS Apple Silicon
- Windows x64
- Linux x64 / arm64 (`AppImage` and `rpm`)

## Requirements

### 1) OpenCode CLI (required)

OpenCode Monitor uses your local `opencode` CLI installation and manages its own local `opencode serve` process automatically. You do not need to start the server yourself.

Install OpenCode first:

```bash
# Recommended (macOS/Linux)
brew install anomalyco/tap/opencode

# Or npm
npm i -g opencode-ai@latest
```

Then verify the CLI:

```bash
opencode --version
```

### 2) Local tooling for development only

- Node.js 20+
- npm 10+
- Rust stable toolchain (`cargo`)

## First Run

1. Install the `opencode` CLI and confirm `opencode --version` works in your terminal.

2. Launch OpenCode Monitor.

3. Open `Settings -> OpenCode` if you want to verify the managed server status. The app starts and monitors its own local OpenCode server, which defaults to `http://127.0.0.1:14096`.

4. Add a workspace and start a thread.

## Development

To run from source:

```bash
npm install
npm run tauri:dev
```

## Architecture

- **Frontend**: React 19 + Vite + TypeScript
- **Backend**: Tauri 2 (Rust)
- **Protocol**: OpenCode REST API + SSE, translated in Rust to CodexMonitor-shaped frontend events

## Validation

```bash
npm run typecheck
npm run test
cd src-tauri && cargo check
cd src-tauri && cargo test
```

## Release Build

```bash
npm run tauri:build
```

## Repo Guides

- `docs/codebase-map.md` — task-oriented file map
- `docs/shaping/rest-api-migration.md` — backend architecture and parity notes
- `docs/app-server-events.md` — frontend event contract

## Credits & Support

OpenCode Monitor is built on top of [CodexMonitor](https://github.com/Dimillian/CodexMonitor) by [Thomas Ricouard](https://github.com/Dimillian).

**Support the original author:**

- [Sponsor Thomas on GitHub](https://github.com/sponsors/Dimillian)

**Support this fork:**

- [Buy me a coffee](https://buymeacoffee.com/jacobjmc)

## License

MIT — see [LICENSE](LICENSE)
