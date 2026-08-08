# WebCodex 0.3.3

[English](RELEASE_NOTES_v0.3.3.md) | [简体中文](RELEASE_NOTES_v0.3.3.zh-CN.md)

WebCodex 0.3.3 adds native Windows x64 client/Runner support and the MCP
2026-07-28 stateless tools core, while also making one-machine temporary sharing
and ChatGPT OAuth discovery easier to use.

## Highlights

- **Native Windows x64 client + Runner.** `webcodex` and `webcodex-runner` now
  support Windows-native repository work, command execution, process-tree
  cleanup, path handling, Git operations, validation jobs, and LSP navigation.
  The supported Windows deployment model connects the Windows Runner to a remote
  Linux WebCodex Server.
- **MCP 2026-07-28 stateless tools core.** Modern MCP clients can negotiate with
  `server/discover` and call `tools/list` / `tools/call` without the legacy
  initialize/session lifecycle. The existing MCP 2025-06-18 path remains for
  compatible legacy clients.
- **Temporary local sharing.** `webcodex share` can start an isolated local
  Server + Runner, create a temporary Project Connector credential, and expose
  `/mcp` through a Cloudflare Quick Tunnel for short-lived use without requiring
  a Cloudflare account.
- **ChatGPT OAuth MCP discovery compatibility.** OAuth discovery now handles the
  metadata flow expected by ChatGPT MCP clients while keeping WebCodex's
  existing authorization boundaries.
- **Stronger release provenance.** Native Windows packaging verifies the exact
  tagged commit, clean build identity, shared binary metadata, and archive
  contents before an artifact can be treated as publishable.

## Windows support scope

Windows x64 is supported for the CLI and Runner. It is intended for local
Windows repositories connecting to a remote Linux Server.

Not supported on Windows in 0.3.3: a long-running local WebCodex Server,
`webcodex share`, `webcodex agent install`, persistent shells, SSH resources,
config hot reload, AppContainer sandboxing, ARM64, or UNC project roots.
`webcodex-server.exe` remains in the Windows archive only to preserve the common
three-binary npm artifact contract.

After a Windows machine restart, resume a hosted Runner explicitly with the same
`webcodex connect ...` command or `webcodex agent start --profile <name>`.
Automatic startup at login/reboot is not part of this release.

## MCP compatibility

The 2026-07-28 path is stateless at the transport layer: each request carries its
protocol metadata and standard MCP headers, and WebCodex does not mint or depend
on `Mcp-Session-Id` for that path. Durable WebCodex work still uses explicit
application handles such as task and Workflow Session identifiers.

MCP 2025-06-18 initialization/session behavior remains available for existing
clients. Clients that cache tool schemas should refresh them after upgrading.

## Upgrade notes

1. Upgrade `webcodex`, `webcodex-server`, and `webcodex-runner` together from the
   same immutable v0.3.3 revision.
2. Verify that all installed binaries report `0.3.3`, the same concrete commit,
   and `dirty=false`.
3. Refresh cached MCP schemas so clients can discover the 2026-07-28 transport
   behavior and current tool surface.
4. On Windows, use the CLI/Runner path against a remote Linux Server; do not
   treat the packaged `webcodex-server.exe` as a supported Windows service.

## Binary packaging

The v0.3.3 release artifacts are:

- `webcodex-v0.3.3-linux-x64.tar.gz`
- `webcodex-v0.3.3-linux-arm64.tar.gz`
- `webcodex-v0.3.3-darwin-arm64.tar.gz`
- `webcodex-v0.3.3-win32-x64.tar.gz`

Each artifact was built natively from the exact immutable `v0.3.3` tag and
contains the three runtime binaries (`.exe` on Windows). The SHA-256 values of
the exact upload candidates are:

- `linux-x64`: `9b41648a2ca22a2919a47fd52db8a2e9c88b605b8afc9f378929922d3227ffa4`
- `linux-arm64`: `305eeca72321cca19632cecf9780dcb60a6719291e9ca76bb48a8b00924fb88c`
- `darwin-arm64`: `bd2ad416d21115248a0473afb186048151de7b407bbfd8eff6f1bb60d09429eb`
- `win32-x64`: `de44975c7abe5e3947bb486b2fe9172840dcbe3faec07633d8512b72efb790c2`

## Known limitations

- macOS x64, Windows ARM64, and other targets are not published by this release.
- Windows supports client/Runner operation, not the long-running local Server or
  OS service-install path.
- The Docker image remains server-only; repository machines still need a Runner.
- Connected clients can modify files and execute commands within configured
  boundaries. Use version control, recoverable backups, and appropriately
  scoped OS users.

## Release validation

The release candidate must pass formatting, workspace compilation and tests,
focused MCP/runtime coverage, native Windows validation, npm self-tests,
artifact-to-npm installation smoke, release binary identity/provenance checks,
Markdown local-link validation, and clean-worktree review. The final publishable
artifacts must be rebuilt from the immutable tag on their native release hosts.
