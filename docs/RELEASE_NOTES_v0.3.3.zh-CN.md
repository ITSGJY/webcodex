# WebCodex 0.3.3

[English](RELEASE_NOTES_v0.3.3.md) | [简体中文](RELEASE_NOTES_v0.3.3.zh-CN.md)

WebCodex 0.3.3 新增原生 Windows x64 client/Runner 支持和 MCP 2026-07-28
stateless tools core，同时让单机临时分享和 ChatGPT OAuth MCP discovery 更容易使用。

## 主要更新

- **原生 Windows x64 client + Runner。** `webcodex` 与 `webcodex-runner`
  现在支持 Windows 原生仓库操作、命令执行、process-tree 清理、路径处理、Git、
  validation jobs 和 LSP navigation。Windows 的支持部署方式是让 Windows Runner
  连接远端 Linux WebCodex Server。
- **MCP 2026-07-28 stateless tools core。** 现代 MCP client 可以通过
  `server/discover` 协商，并直接调用 `tools/list` / `tools/call`，不再依赖 legacy
  initialize/session lifecycle；现有 MCP 2025-06-18 路径继续保留给兼容的旧 client。
- **临时分享本地项目。** `webcodex share` 可以启动隔离的本地 Server + Runner，
  创建临时 Project Connector credential，并通过 Cloudflare Quick Tunnel 暴露
  `/mcp`，短期使用时不要求 Cloudflare 账号。
- **ChatGPT OAuth MCP discovery 兼容。** OAuth discovery 现在能够处理 ChatGPT
  MCP client 期望的 metadata flow，同时保持 WebCodex 现有 authorization boundary。
- **更强的 release provenance。** Windows 原生打包会核对 tag commit、clean build
  identity、三 binary 的共同 build metadata 和 archive 内容，只有通过后才能作为
  可发布 artifact。

## Windows 支持范围

Windows x64 支持 CLI 和 Runner，用于操作本地 Windows 仓库并连接远端 Linux
Server。

0.3.3 在 Windows 上暂不支持长期运行的本地 WebCodex Server、`webcodex share`、
`webcodex agent install`、persistent shell、SSH resource、config hot reload、
AppContainer sandbox、ARM64 和 UNC project root。Windows archive 中的
`webcodex-server.exe` 仅用于保持三 binary npm artifact contract，不表示 Windows
Server service 已受支持。

Windows 机器重启后，需要重新运行同一条 `webcodex connect ...`，或执行
`webcodex agent start --profile <name>` 恢复 hosted Runner；本版本不提供登录/开机
自动启动。

## MCP 兼容性

2026-07-28 transport path 是 stateless 的：每个请求携带自己的 protocol metadata
和标准 MCP headers，WebCodex 不会为该路径创建或依赖 `Mcp-Session-Id`。持久工作
仍通过显式 task / Workflow Session 等 application handle 表达。

MCP 2025-06-18 initialize/session 行为继续保留。缓存 tool schema 的 client 在升级后
应刷新 schema。

## 升级说明

1. 从同一个不可变 v0.3.3 revision 一起升级 `webcodex`、`webcodex-server` 和
   `webcodex-runner`。
2. 确认所有 binary 都报告 `0.3.3`、同一个具体 commit，并且 `dirty=false`。
3. 刷新缓存的 MCP schema，让 client 重新发现 2026-07-28 transport 行为和当前
   tool surface。
4. Windows 使用 CLI/Runner 连接远端 Linux Server；不要把 artifact 中附带的
   `webcodex-server.exe` 当作受支持的 Windows service。

## Binary 打包

计划发布的 artifacts：

- `webcodex-v0.3.3-linux-x64.tar.gz`
- `webcodex-v0.3.3-linux-arm64.tar.gz`
- `webcodex-v0.3.3-darwin-arm64.tar.gz`
- `webcodex-v0.3.3-win32-x64.tar.gz`

每个 artifact 都必须在对应 native release host 上从同一个不可变 `v0.3.3` tag
构建，并包含 `webcodex`、`webcodex-server` 和 `webcodex-runner`。真实 SHA-256
只会在 exact release assets 完成构建和验证后写入；在这些 checksum 替换 release
manifest placeholder 之前，不得发布 npm package。

## 已知限制

- 本版本不发布 macOS x64、Windows ARM64 和其他 targets。
- Windows 支持 client/Runner，不支持长期运行的本地 Server 或 OS service-install
  路径。
- Docker image 仍然是 server-only；持有仓库的机器仍需要 Runner。
- 已连接 client 可以在配置边界内修改文件和执行命令；请使用版本控制、可恢复备份
  和权限合适的 OS 用户。

## 发布验证

release candidate 必须通过 formatting、workspace 编译与测试、focused MCP/runtime
coverage、Windows native validation、npm self-tests、artifact-to-npm install smoke、
release binary identity/provenance、Markdown 本地链接检查和 clean-worktree review。
最终可发布 artifacts 必须从 immutable tag 在各自 native release host 上重新构建。
