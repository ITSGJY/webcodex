# Agent Protocol

[English](AGENT_PROTOCOL.md) | [简体中文](AGENT_PROTOCOL.zh-CN.md)

WebCodex agents 连接 server，并执行已注册项目上的 tools。新部署建议配置 QUIC 并使用 `transport = "auto"`；WebSocket 和 polling 继续作为 fallback transports。

## Authentication

Agents 应使用 client enrollment 期间创建的 agent tokens：

```bash
webcodex client enroll --server-url URL --pairing-code CODE --client-id CLIENT_ID
```

Server/admin 侧用 `webcodex pairing create` 创建临时代码。Agent token 在 client enroll 期间返回给 client，并写入生成的 `agent.toml`；不要从 server 复制 agent token files。二进制部署时，使用 `webcodex agent install` 安装 client-side service，并用 `webcodex agent status` 检查。

Transport auth rules：

- QUIC：agent token 保留在顶层 agent config 中，并通过 QUIC stream 内的 agent registration envelope 发送。
- WebSocket：优先在 handshake headers 中使用 `Authorization: Bearer <agent-token>`。
- WebSocket compatibility：`/api/agents/ws?token=...` 只用于 handshake 兼容。
- Polling：每个 request 都必须使用 `Authorization: Bearer <agent-token>`。
- REST、MCP 和 GPT Actions ordinary APIs 必须使用 `Authorization: Bearer ...`。

不要在 `/api/agents/ws` 之外使用 query-string tokens。

## Registration and identity

Agents 注册时提交：

- `client_id`
- `owner`
- `transport`
- `agent_instance_id`
- capabilities
- registered projects
- redacted policy summary

`agent_instance_id` 标识一个正在运行的 agent instance，区别于稳定的 `client_id`。

## 同一进程内的 Job 状态协调

当前 Runner 会声明 `job_state_reconciliation` capability。此后每次注册及同实例
重新注册都必须提交一个完整 active、并对近期 terminal history 设限的
`job_inventory`。Polling 通过 registration body 携带；WebSocket 和 QUIC 在
`Register` envelope 中携带完全相同的模型。声明 capability 却缺少完整 inventory
属于协议错误。未声明该能力的旧 Runner 保持保守语义：断线后立即将活动 Job 标为
`lost`。

滚动升级应先升级 Server，再升级 Runner。新 Server 对旧 Runner 缺失的字段使用
optional/default 兼容；Runner 一旦声明 `job_state_reconciliation`，inventory
与单调 update 协议就是强制要求，不能静默降级。

每个 snapshot 保留原 `job_id` 和 `request_id`、生命周期与结果字段、Runner
生成的单调 `update_seq`、validation progress，以及带绝对 retained-line cursor
的有界 stdout/stderr tail。Server 仅在完成正常 project resolution 和权限检查后，
通过 start request 提供恢复所需的 project、Workflow Session 与执行元数据。
其中只有沿用现有限制与脱敏规则的有界 command preview，不保存第二份 raw command。
Inventory 不包含 stdin、环境变量值、token、认证 header 或完整 Agent 配置。

Runner 总是先更新进程内 record，再尝试网络发送。Server 只接受更高序号；相同序号
重放幂等，旧序号被忽略。Register reconciliation 会用 Runner 的权威有界 tail
替换 Server tail，而不是再次 append。当前 Runner 的每个单调 update 也携带该有界
权威 tail，因此乱序到达的更高序号已经包含低序号的 retained output。新 transport
sink 安装后，Runner 还会使用同一序号规则重放最新有界 snapshot，以覆盖
register/ack 窗口内的状态变化。
已接受的 terminal 状态不会回退为 active，也不会被另一种 terminal 状态覆盖。

内部协议边界为：

- active record 最多 64 个，并始终排在 terminal history 前；
- terminal record 最多 64 个，在 Runner 中保留 15 分钟；
- 每个 stdout/stderr tail 最多 64 KiB；
- 序列化 inventory 总计最多 1 MiB。

可恢复断线后，已由 Runner 接管的 Job 最多 120 秒处于 `recovering`。同一实例的
完整 inventory 会恢复实际状态、日志、归属和原 `job_id`；缺失项以
`runner_inventory_missing` 变为 `lost`。不同 `agent_instance_id` 替换旧实例时，
旧实例活动 Job 以 `runner_instance_replaced` 变为 `lost`，迟到的注册或 update
继续被拒绝。尚未分发的 Server queue entry 不会重放。

本阶段要求 Runner 进程仍然存活且 `agent_instance_id` 不变。Runner 自身重启会
丢失 child/process-group handle，无法恢复这些 Job。本机制不承诺通用的
exactly-once command execution；`run_job` 调用级幂等属于后续独立阶段。

## Policy summary

`runtime_status` 和 `listAgents` 为 operators 暴露 redacted summary：

- `allow_raw_shell`
- `allow_cwd_anywhere`
- `allowed_roots`
- `max_timeout_secs`
- `max_output_bytes`

它们不会暴露 tokens、full env、`Authorization` headers、完整 `agent.toml` 或 shell `init_script` values。

Policy 默认值：

- 如果 `allowed_roots` 缺失或为空，默认使用 `$HOME`。
- 显式 `allowed_roots` 会替换 `$HOME` 默认值。

## Project ids

Agent-backed project ids 报告为：

```text
agent:<client_id>:<project_id>
```

Server 会把 project tool calls 路由到拥有该项目的 connected agent。

## LSP 只读导航

支持只读 LSP intelligence 的 agent 会注册
`lsp_read_only_navigation` capability。Server 只发送 typed
`AgentLspRequest` operations：status、document symbols、go to definition 和
find references、document diagnostics、hover，以及 workspace symbols。Agent 返回带版本的
`AgentLspResultEnvelope`，其中包含成功结果或 structured error。Document
diagnostics 使用每个 server instance 独立的 bounded `publishDiagnostics` cache，
并明确报告结果是否 fresh，或共享的两秒等待是否 timed out。

带 document 的 operations 只接受 project-relative `.rs` path。Agent 从 canonical
project root 读取已验证的普通文件，在启动 server 前执行 LSP document byte cap，
并发送 disk-backed full-text `didOpen` / `didChange` notifications。模型不能提供
document text 或 incremental edit payload。Workspace-symbol query 会 trim，trim 后
必须非空且不超过 200 字符；result limit 会 clamp 到 1..200。

Diagnostics cache 对每个 server instance 最多保留 256 个 URI，每个 URI 最多 500 条
raw diagnostics，并且只保留 latest publication。`fresh=true` 表示 publication version
匹配当前 document，或观察到 prepare generation 之后的新 publication；
`timed_out=true` 是成功返回 stale/empty 结果，不是 transport error。Server unavailable
或 crash 仍返回 structured LSP error。Hover 和 symbol results 会在 transport 前完成
normalize 和 bound。

不提供 arbitrary LSP-method passthrough。Agent 只在已注册 project boundary 内
解析请求，并在本地运行 language server。
Project 外部、dependency、registry 和 sysroot locations 会从 public results 中省略；
不会返回 absolute path 或 file URI。未声明
`lsp_read_only_navigation` 的旧 agent 会被视为这些 tools 不可用，并安全失败；
其他已支持的操作仍可继续使用。

## Codex-specific workflows

WebCodex 不再暴露 `run_codex` 或 legacy `/api/codex/*` routes。Agent lifecycle 和 project dispatch 使用 structured runtime tools、agent-registered projects、bounded shell/job validation、MCP 和 GPT Actions。需要时请在 WebCodex 外部运行 Codex。
