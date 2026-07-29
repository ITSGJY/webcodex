# E2E Validation

[English](E2E_VALIDATION.md) | [简体中文](E2E_VALIDATION.zh-CN.md)

E2E validation script 用于在不依赖 ChatGPT UI 的情况下，本地验证 WebCodex server、agent、GPT Actions schema 和 MCP endpoint。

## 检查内容

典型 validation 应确认：

- `/openapi.json` 有效，并包含预期 GPT Actions operations。
- `runtime_status` 返回 `service=webcodex`。
- `listAgents` 显示 online agent 和 redacted policy summary。
- `listProjects` 显示 agent-backed project ids。
- 只读 project tools 可用。
- Mutation tools 只对 disposable projects 生效。
- MCP tools/list 与预期 runtime tool surface 匹配。
- 部署后的 artifact transfer smoke 覆盖 bounded runtime discovery、chunked
  artifact upload/readback、cleanup，以及安全 smoke project 的 git clean 状态。

## 部署 artifact transfer smoke

部署后需要集中验证 artifact transfer 和 runtime discovery 时，使用
`scripts/smoke_artifact_transfer.sh`。默认模式只打印 checklist，不读取 token
变量，也不会访问 server：

```bash
bash scripts/smoke_artifact_transfer.sh
```

显式 active mode 需要 public URL 和可用于 GPT/MCP 的 token：

```bash
WEBCODEX_SMOKE_RUN=1 \
WEBCODEX_PUBLIC_URL="https://webcodex.example.com" \
WEBCODEX_TOKEN="<wc_pat_or_allowed_shared_key>" \
bash scripts/smoke_artifact_transfer.sh
```

Smoke project 应是 disposable、agent-backed 的 git repository，例如
`agent:special:webcodex-smoke`。不要把 `wc_agent_*` 用于 GPT Actions、MCP
或这个 smoke script；该 token type 只给 `webcodex-runner` 使用。

## Authentication

REST、polling、MCP 和 GPT Actions calls 必须使用：

```text
Authorization: Bearer <token>
```

`?token=` 只用于 `/api/agents/ws` WebSocket handshake 兼容场景。

## Codex CLI

WebCodex 不再暴露 `run_codex` 或 legacy `/api/codex/*` routes。GPT Actions 和 MCP clients 应先使用 structured edit tools 加 `cargo_fmt`、`cargo_check`、`cargo_test`、`validate_patch` 和 `apply_patch_checked`。`run_job` 和 `run_shell` 只是受限 fallback diagnostics/build/test 工具，不是默认 validation source。需要 Codex-specific workflows 时，请在 WebCodex 外部运行 Codex。

## Management setup

优先使用：

```bash
webcodex server init
webcodex server install
webcodex server status
webcodex pairing create --server-url URL --username alice --client-id alice-laptop
webcodex client enroll --server-url URL --pairing-code CODE --client-id alice-laptop
webcodex agent install --profile workstation --bin /opt/webcodex/bin/webcodex-runner
webcodex agent status --profile workstation --server-url URL
webcodex ops status --strict --server-url URL --token-file PATH
```

`pairing create` 是 server/admin-side。`client enroll`、`agent install` 和 `agent status` 是运行 `webcodex-runner` 的 client-side 操作。不要把 server tokens 复制到 client；只复制短期 pairing code。
## 二进制 help 校验

发布前或大规模修改文档后，应对照实际二进制 help 检查命令示例：

```bash
webcodex -h
webcodex server init -h
webcodex server install -h
webcodex server status -h
webcodex pairing create -h
webcodex client enroll -h
webcodex agent install -h
webcodex agent status -h
webcodex doctor -h
webcodex ops status -h
webcodex-runner -h
webcodex -h
```

重点检查管理员创建账户凭据使用 `users create --server-url ...`，而本地 token 创建使用 `token create-local --server ...` 和 `agent-token create-local --server ...`。


旧 `webcodex-cli` 入口已不再安装；validation 统一使用 `webcodex` 命令。

## Documentation scans

Docs polish 或 release validation 期间，应运行标准 documentation scans，检查三类错误：

1. 指向已删除 historical planning documents 的引用；
2. 旧产品名或 legacy environment/key names；
3. 用户文档和示例中明显真实的 token-looking values。

预期结果：没有 stale deleted-doc references，没有旧产品名，没有真实 token-looking values。`<token>` 和 `<wc_pair_...>` 这类 placeholder 是可以的。
