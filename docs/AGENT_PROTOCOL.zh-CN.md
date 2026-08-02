# Agent Protocol

[English](AGENT_PROTOCOL.md) | [简体中文](AGENT_PROTOCOL.zh-CN.md)

WebCodex agents 连接 server，并执行已注册项目上的 tools。新部署建议配置 QUIC 并使用 `transport = "auto"`；WebSocket 和 polling 继续作为 fallback transports。

## Authentication

Managed Agent 应使用 client enrollment 期间创建的 agent tokens（`webcodex login` 是主入口；`webcodex client enroll` 是高级替代）：

```bash
webcodex login URL --code CODE
```

Server/admin 侧用 `webcodex pairing create` 创建临时代码。Agent token 在 login 期间返回给 client，并写入生成的 `agent.toml`；不要从 server 复制 agent token files。二进制部署时，使用 `webcodex agent install --config <path>` 安装 client-side service，并用 `webcodex agent status` 检查。

当 Server 开启 `WEBCODEX_SHARED_KEY_ENABLED=true` 时，hosted quick-start 是另一种受支持
模式：Runner 可以提交与 MCP 完全相同的 direct、非 `wc_` shared key。Server 与
Runner 两侧统一派生 `SHA-256(trimmed key)`，registry 只保存这个非秘密 auth group。
不同 key、managed identity 和 open-anonymous caller 不能注册进或操作该 group。
OAuth shared-key bridge token 仍不能使用 Agent transport。

Transport auth rules：

- QUIC：Agent token 或 direct shared key 保留在顶层 agent config 中，并通过 QUIC stream 内的 agent registration envelope 发送。
- WebSocket：优先在 handshake headers 中使用 `Authorization: Bearer <agent-token-or-shared-key>`。
- WebSocket compatibility：`/api/agents/ws?token=...` 只用于 handshake 兼容。
- Polling：每个 request 都必须使用 `Authorization: Bearer <agent-token-or-shared-key>`。
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
Managed Agent token 会绑定 `client_id` 与 owner。Direct shared-key registration 会
忽略上报的 owner，并把记录绑定到派生 hash group。跨 group 的 `client_id` 碰撞会以
不泄露既有记录的方式失败，也不会替换原记录。同 group 重连继续沿用 instance 与
connection lease 规则；旧连接不能刷新或代替新 lease 提交结果。

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

可恢复断线后，已由 Runner 接管的 Job 在有界 grace window 内处于 `recovering`
（默认 120 秒，可通过 `WEBCODEX_JOB_RECOVERY_GRACE_SECS` 覆盖，clamp ��� 5–3600 秒）。
`recovering` 是有界而非永久状态：同一实例的完整 inventory 会恢复实际状态、日志、
归属和原 `job_id`；缺失项以 `runner_inventory_missing` 变为 `lost`。不同
`agent_instance_id` 替换旧实例时，旧实例活动 Job 以 `runner_instance_replaced`
变为 `lost`，迟到的注册或 update 继续被拒绝；新实例不会迁移或继承旧实例的 Job。
旧���例迟到 disconnect 相对当前实例是 no-op —— 既不清除当前 notifier，也不把当前
实例的 Job 标记为 lost/recovering —— 但旧实例的 Job 在替换时已终结为 `lost`。
尚未分发的 Server queue entry 不会重放。

### 恢复超时扫描

即便没有请求流量，进程内 sweep 也会按固定间隔运行，将 grace window 已到期的
`recovering` Job 转为 terminal `lost`，reason 为
`runner_recovery_deadline_exceeded`。sweep 有界（每轮转换数量有上限），仅在
registry mutex 内做内存工作（锁内不做磁盘/网络/await），幂等且只设置一次
`ended_at`。deadline 前完成 reconcile 的 Job 不会被置为 lost；stale connection 的
Ping/Pong/metadata、重复 disconnect 或迟到 inventory 都不会延长 deadline
（deadline 锚定在 Job 进入 `recovering` 时一次性写入的 `recovering_since`，而非
client liveness）。

deadline 是单个 Server 进程的属性，不是 durable record。Job Registry 是 Server
内存状态，Server 重启会清空，deadline 不跨 Server 进程持久化。重启后的恢复依赖
Runner 重新连接并提交 inventory：inventory 重新注册某 Job 时会开始一个全新的有界
recovery window（本轮不保留重启前已消耗的恢复时间）；��� Runner 重启后永久不再连接，
Server 没有该 Job 的 durable record，无法对未知 Job 执行 recovery timeout。durable
的 Server 端 Job ledger 属于后续独立阶段，不在本轮实现。

非法的 structured validation progress update 属于 executor protocol violation，
而非可恢复的 transient 状态：乱序、回退或跳跃的 `completed` cursor、计划或 step
名称不一致、重复或不一致的完成、或无 validation 计划的 Job 携带 progress，都会令
Job 进入 terminal `failed`，错误为有界、稳定且不泄露 payload 的
`validation_progress_invalid` 类；最后一次已接受的合法 progress 保留，`ended_at`
只设置一次，pending request 与 request-to-job 映射释放，不会重新执行。相同或更
旧序号重放保持幂等；已 terminal 的 Job 不会被迟到 update 或 register inventory 复活。

本阶段要求 Runner 进程仍然存活且 `agent_instance_id` 不变。Runner 自身重启会
丢失 child/process-group handle，无法恢复这些 Job。不声明
`job_state_reconciliation` 的旧 Runner 保持保守的立即 `lost` 断线语义
（`legacy_runner_disconnected`），永不进入 `recovering`；同 client 的新实例无法接管
其已终结的 Job。本机制不承诺通用的 exactly-once command execution，也不提供跨
Runner/跨机器的 Job 迁移；`run_job` 调用级幂等属于后续独立阶段。

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

## Session SSH 资源

能够调用本机 OpenSSH 的 Runner 会注册 `ssh_shell` capability。旧 Runner 未声明该
能力时仍可执行普通本机项目；选择了 SSH 资源的 Session 会收到明确的
capability-unavailable 错误，绝不会静默回退到 Runner 本机执行。

Server 只在安全执行元数据中发送 Workflow Session id 和 Runner 本地资源名；不会发送
SSH host、完整 SSH 配置、私钥、密码、agent socket 或连接对象。Runner 从
`[ssh.resources.<name>]` 解析资源，并调用自身 OpenSSH client，因此 Host alias 和认证
始终留在该 Runner 机器上。

Runner 可以按 Session/resource/config generation 复用已认证 transport，但每个
`run_shell` 和 `run_job` 都会创建新的远程 exec channel。派发后的 transport failure
会标记为不确定，绝不自动重试。`run_shell`、`run_job`、`stop_job`、`job_status` 和
`job_log` 保持原有接口；本轮不会把 file、Git、LSP 或 checkpoint 请求重定向到 SSH。

## Workflow Session 持久 Shell

当前 Runner 会声明带 serde 默认值的 `persistent_shell` capability。旧 Runner
缺少该字段时仍可兼容运行，但 Server 会返回明确的
`agent_capability_unavailable`，绝不会转换为 `run_shell`。它是独立生命周期协议，
不是 Job：

```text
open -> exec/status -> close
```

Server 在现有 agent request envelope 中发送 typed `PersistentShellRequest`。
Polling 通过 `/api/shell/agent/persistent_shell_result` 返回 typed result；
WebSocket 与 QUIC 使用 payload 相同的 `PersistentShellResult` envelope。请求和
结果都携带精确的 `shell_id`、Workflow Session id 与 runtime project id；完成时
还会校验当前 Runner instance、client、request、Session、project 和 shell 身份，
再释放 waiter。

Runner 在已注册项目所属主机上拥有长生命周期 `sh`/`bash` 进程。打开时校验项目
可执行性、raw-shell policy、cwd/allowed roots、dialect 和 profile；后续 exec/status
操作前再次校验当前边界。Close 作为清理操作，在执行 policy 改变后仍然可用。Profile
环境和初始化脚本只在 open 时运行一次。命令串行执行，输出有界；完成状态走独立
control descriptor，不与普通 stdout/stderr marker 混在一起。

该协议不使用 Session SSH resource，也不会从 SSH resource 静默回退；本轮不支持
SSH persistent shell。它也不提供 PTY、原始字节流、terminal resize 或 terminal
UI。Runner 断连或正常退出会关闭其 Shell；Runner result 不可用或状态不确定时，
Server 将记录视为 lost，不会根据 Session/Server 记录假装进程仍存活。Server 或
Runner 进程重启后必须重新 open，本轮不提供重新附着或 restart recovery。

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
