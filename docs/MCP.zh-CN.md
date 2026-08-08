# MCP

[English](MCP.md) | [简体中文](MCP.zh-CN.md)

client 能连接 project-bound WebCodex endpoint 时使用 MCP。先完成
[QUICK_START.zh-CN.md](QUICK_START.zh-CN.md)。

## Endpoint 与认证

本地 client 可以使用：

```text
http://127.0.0.1:<configured-port>/mcp
```

Hosted client 需要 HTTPS，目前有三条用户路径：

- **Hosted：** `webcodex connect <server>` 使用现有 hosted Server，本地只运行 Runner。
- **Local Share：** `webcodex share` 启动本地 Server + Agent 与 Cloudflare Quick Tunnel，并输出临时 HTTPS `/mcp` URL 和一把独立的临时 Bearer credential。Ctrl-C 会停止 runtime/tunnel 并删除临时 share state，因此访问随之失效；URL 每次运行都可能变化。`--tunnel none` 仅用于本地测试/debug。
- **Self-hosted：** 长期运行时使用 stable HTTPS domain/tunnel、durable service 管理，以及 OAuth 或 scoped credential。

Cloudflare Quick Tunnel 面向开发/测试，不是 production 部署方案。稳定自托管 endpoint
通常形如：

```text
https://your-domain.example/mcp
```

不要把 bootstrap/admin、account、Agent credential，或 project-first setup 的长期
Connector credential 当成公网分享 secret。`share` 会创建并只打印本次 session 的
临时 Connector credential。该 credential 在 session 存活期间允许对当前项目进行
修改并执行 share runtime 允许的命令，因此必须保持私密，也不要提交到 Git。

临时使用 `webcodex share` 接入 ChatGPT Developer Mode 时，用命令输出的公网 HTTPS
`/mcp` URL 创建自定义 app。如果认证菜单提供 **访问令牌/API 密钥**，选择它并粘贴
本次临时 Bearer credential，然后执行 **Scan Tools / 扫描工具**。ChatGPT 的 UI
文案和可用范围可能随 workspace 与 rollout 变化。

对于已经启用 OAuth 的 managed / 自托管 HTTPS Server，则使用用户自定义 OAuth
client。WebCodex 当前支持 PKCE S256 与 `client_secret_post`；需要把 ChatGPT 显示的
callback URL 原样注册为 redirect URI。OAuth discovery 会发布 `offline_access` 以
支持 refresh token 连续性；它是协议级 scope，不会增加任何 WebCodex API 权限，
因此不要把它写入 OAuth client 的 `allowed_scopes`。WebCodex 当前不要求、也未实现
Dynamic Client Registration。

Canonical `webcodex setup` 仍然不打印 credential value 或 secret path、不创建
tunnel，也不开放公网端口。`webcodex share` 是显式的临时例外：它只打印本次
session-scoped credential，绝不打印长期 project Connector credential。production
enrollment、scoped user token 和 OAuth 见
[AUTH_MODEL.zh-CN.md](AUTH_MODEL.zh-CN.md)、
[DEPLOYMENT.zh-CN.md](DEPLOYMENT.zh-CN.md) 与
[OAUTH2_SMOKE_TEST.md](OAUTH2_SMOKE_TEST.md)。

## 协议兼容性

WebCodex 通过两代 MCP 协议提供同一套已选择的 tools surface：

- **MCP 2026-07-28 stateless tools core：** client 可以先调用 `server/discover`，
  随后直接调用 `tools/list` 和 `tools/call`。每个请求都自描述；WebCodex
  会校验 2026 protocol metadata，以及标准的 `MCP-Protocol-Version`、
  `Mcp-Method`，并对带名称的请求校验 `Mcp-Name`。这条路径不会创建、消费或
  回传 `Mcp-Session-Id`。
- **MCP 2025-06-18 compatibility：** 旧 client 继续使用 `initialize` /
  `notifications/initialized`，并可以保留 server 签发的 `Mcp-Session-Id`。
  这条路径为现有 client 保留，不会静默切换成 2026 lifecycle。

两条路径暴露相同的 WebCodex tool surface 与授权规则。MCP transport state 不等于
WebCodex application state；持久工作应由显式 WebCodex handle 标识，例如
`task_id`、Workflow Session ID 和 Job ID。

这里刻意定义为 **2026 tools-core support**，不代表已经实现 MCP 2026 的全部
extension。WebCodex 当前不发布 MCP resources、prompts、Tasks、Apps、MRTR、
subscriptions 或其它可选 2026 extension。现代 client 调用未实现的方法时，
WebCodex 会先完成 transport metadata 校验，再返回标准 method-not-found。

## Model surface 选择

MCP 在 server 启动时选择一个 model surface：

- `WEBCODEX_CONNECTOR_SURFACE=task-v1` 加上 `webcodex setup` 写入的完整
  Connector 项目配置，会选择 `canonical_connector` 和下述十二项 project-bound
  capability。
- 未配置 Connector 且未设置 `WEBCODEX_MCP_MODEL_SURFACE` 时，`/mcp` 默认暴露
  聚焦的 `local_coding` surface（普通用户默认；工具集见下）。
- `WEBCODEX_MCP_MODEL_SURFACE=local-coding-v1` 显式选择 `local_coding`；
  `WEBCODEX_MCP_MODEL_SURFACE=full-operator-v1` 显式选择 `full_operator_runtime`
  （operator/debug surface）。
- 同时设置 Connector 配置和 `WEBCODEX_MCP_MODEL_SURFACE`，或使用不受支持的
  `WEBCODEX_MCP_MODEL_SURFACE` 值时，server 启动配置失败，不会静默切换到另一个
  surface。

`GET /mcp`、MCP `initialize.serverInfo` 和 `runtime_status.model_surface`
通过 `modelSurface` 报告同一个选择结果。标准普通用户 setup 选择
`canonical_connector`；没有它时默认是 `local_coding`。只有 operator 显式设置
`WEBCODEX_MCP_MODEL_SURFACE=full-operator-v1` 时才服务 full operator runtime。

## Project-bound surface

当 `modelSurface=canonical_connector` 时，MCP `tools/list` 恰好包含：

```text
task_start
task_list
task_resume
files_list
files_read
files_search
edits_apply
checks_run
commands_run
task_review
task_cancel
task_finish
```

Connector context 已经绑定当前配置的仓库。对于 legacy 2025-06-18 client，
保留的 MCP protocol session 还可以把聊天窗口绑定到当前持久任务，因此同一窗口
重复调用 `task_start` 时可以复用任务。对于 2026-07-28 client，transport request
是 stateless 的：WebCodex 会明确忽略 `Mcp-Session-Id`，不会从 credential 推断
隐藏 window；`task_start` 会返回显式的持久 `task_id`。client/model 应保留这个
handle；需要重新恢复 task context 时使用 `task_resume(task_id)`。切换仓库连接时
各仓库的 task history 仍然彼此隔离。

Connector context 已绑定项目。直接从 `task_start` 开始；不要调用
`list_projects`、`runtime_status`、`tool_manifest`、`start_session` 或
`current_session`，也不要向用户索取 Agent client ID、runtime project ID、
executor reference 或 workflow session。

复用前，WebCodex 会比较仓库实际路径、分支与 HEAD、工作区、适用的仓库规则及
项目 manifest。未变化的上下文直接复用，只把变化部分标为已刷新。如果有界扫描
无法证明某个 slice 完整，response 会把它标记为 partial/unknown 并返回紧凑
warning，不会声称已经复用。`task_list` 和 `task_resume` 用于 client 已经没有
application-level task context 时的显式恢复，不是每次普通 tool call 的前置步骤。

在 `local_coding` 上，MCP `tools/list` 恰好包含如下聚焦 coding 工具集，且顺序
严格一致：

```text
work_on_project
list_projects
project_overview
list_project_tracked_files
list_project_files
search_project_text
search_project_texts
read_file
read_files
lsp_status
document_symbols
document_diagnostics
hover
workspace_symbols
goto_definition
find_references
apply_text_edits
apply_patch_checked
run_shell
run_job
job_status
job_log
list_jobs
stop_job
cargo_fmt
cargo_check
cargo_test
validation_summary
git_status
git_log
git_diff
git_diff_hunks
show_changes
workspace_hygiene_check
finish_coding_task
```

同一份清单也是 `tool_manifest(intent="coding")` 的单一事实源。Session 管理
（`start_session`、`current_session`、persistent shell）、项目注册/生命周期
（`register_project`、`create_project`）、artifact/checkpoint 工具、cleanup 工具
以及 runtime/operator 管理（`runtime_status`、`tool_manifest`）都不属于该 surface：
`tools/call` 会在 MCP 边界、进入 ToolRuntime dispatch 之前明确拒绝它们，
`tools/list` 也绝不公布它们。

在 `local_coding` 上，`work_on_project(project, instruction, session_id?)` 是
轻量普通入口：一次调用即可返回规则、Git 状态、LSP 就绪度、jobs 与 blockers，
让模型可以立即开始或继续聚焦工作。没有 `session_id` 时创建新的 Workflow
Session；有 `session_id` 时只精确继续该 Session（绝不猜测最近 Session，也绝不
credential-wide fallback），且从不建立 current-window binding。成功调用返回
`session_id`、解析后的项目 id、`readiness` verdict、`workspace` Git 投影、有界的
`instructions`（`status` 为 loaded/reused/changed/not_found/unavailable，含每个
source 的 fingerprint、headings、有界正文与 `read_more` 提示）、
`semantic_navigation` 就绪度、紧凑 `jobs` 计数，以及 `blockers`/`warnings`/
`suggested_next_actions`。新 Session 首次调用仍会包含 `AGENTS.md` 等适用规则的
有界正文；同一 Session 精确继续且 fingerprint 未变化时返回 `status=reused`，
不会重复正文；规则变化时返回 `status=changed` 并包含更新后的正文。

`work_on_project` 不会请求 Runner repository overview，也不会在本地扫描概览。
为保持输出形状，`repository` 返回紧凑标记
`{"status":"unavailable","reason_code":"not_requested_by_work_on_project"}`，
不包含 project types、manifests、key files、roots、top-level、suggested reads
或 scan metadata，也不会生成 overview failure warning。确实需要完整概览时，
可在 `full_operator_runtime` 使用 `start_coding_task(detail=standard|full)`，
或显式调用 `project_overview`。这些启动信息仅用于参考：不会自动修改或执行任何
内容，模型仍需按需使用聚焦读取、搜索、编辑与验证工具。

在 `full_operator_runtime` 上，普通 coding 使用 `start_coding_task` 开始或继续。
稳定窗口默认继续同一仓库；切换仓库会切换上下文，切回会恢复此前 Workflow
Session。`new_session=true` 是显式的高级隔离请求。current binding 同时保留在
server 的内存缓存和有界哈希持久化投影中；保留相同窗口身份与仓库时，server
重启后可以自动恢复。仍应保留返回的 session id，以便 transport 身份丢失时显式
恢复。返回的 `continuation_feedback` 是对*上一轮* attempt 的活动、改动与
validation 状态的确定性只读投影，并包含有界的上一轮指令摘录和当前未解决失败标识
（另附带仅在两次运行被证明 scope 一致时才可比的 `validation_delta`）；
它既不是 LLM summary，也不是新的 verdict，更不会执行 validation。
其中 `attempt.exploration` 只包含由成功的定向读取、结构化项目搜索结果或 typed
LSP 导航证明的、有界且已验证的项目相对路径。workset 按 attempt 分段并按最近
成功观察优先；attempt boundary 被淘汰时返回 `complete=false`。搜索文本和
preview、文件或 LSP 正文、任意结果、命令/输出及仓库绝对根路径都不会进入
workset。自动 continuation、显式 resume、inspect/read_only 升级 normal 和重启
恢复都可复用它，但 startup 不会自动执行工具，也不能替代模型判断。compact
startup core 中 `minimal` 最多 3 条路径，`standard` 及 `full` 内嵌 core 最多
12 条；完整 feedback 最多 100 条并保留真实 total/truncation。

该 surface 还支持已注册项目 Workflow Session 的有界持久化执行上下文：
`execution_context = {default_cwd?, default_shell?}`。创建时会保存它；
continuation/resume 时省略会保留原值；显式对象与 instruction update 在内存
store lock 下共同提交，`{}` 会清空。`update_session_context(project, session_id,
execution_context)` 要求调用者有权访问解析后的项目，并拒绝任何 Session-project
不匹配，不提供跨项目逃逸。成功响应仅表示内存 context 与 event 已共同提交；JSON
ledger 随后交给现有后台 writer 异步写入，失败仍通过 runtime status 与日志报告，
不表示已经同步落盘。`run_shell`/`run_job` 先采用单次调用参数，再采用项目精确匹配的 Session 默认值，
最后才使用现有项目根目录和 configured shell；不跨项目继承。上下文不保存 env、
凭据、任意 options 或 shell 状态。`run_shell`/`run_job` 每次仍启动 fresh shell，
`cd`/`export` 不会隐式写回 Session。

Full operator surface 只通过四个显式工具提供持久进程状态：

```text
open_session_shell(project, session_id, cwd?, shell?)
session_shell_exec(project, session_id, shell_id, command, timeout_secs?, purpose?)
session_shell_status(project, session_id, shell_id)
close_session_shell(project, session_id, shell_id)
```

Open 返回不可预测的新 `shell_id`；每个 Workflow Session 最多一个活动 Shell。
`session_shell_exec` 返回 `command_started`、`command_completed`、`exit_code`、有界
`stdout`/`stderr` 及截断标志、`duration_ms`、`execution_state`、`shell_state`
和可观察 cwd。Status/close 还会在可用时报告绑定的 dialect/profile、initial cwd、
时间戳、busy/terminal 状态和 close reason。Close 幂等，但已关闭 id 不能操作之后
重新打开的 Shell。

Agent 项目在所属 Runner 上执行该进程；只有项目类型可用时，Server-local 项目才在
Server 主机使用同一进程引擎。每次操作都要求精确匹配的 active Session/project 和
正常调用者授权；`read_only`、`inspect`、缺少 `persistent_shell` capability 的旧
Runner，以及选择 SSH resource 的 Session 都会安全失败。`run_shell` 和 `run_job`
仍是独立进程，绝不复用 persistent shell。该功能是命令执行，不是 PTY 或 terminal
stream，也不能跨 Server/Runner 重启恢复。

在该 surface 上进行显式跨窗口或人工交接时，使用旧 `wc_sess_*` id 调用
`session_handoff_summary`。它与 `finish_coding_task` 返回同一份 strict
`handoff_brief`：这是对有界任务摘录、workspace 状态、changed/recent exploration
路径、validation、Job/guidance attention 计数和固定 next actions 的确定性只读
投影。brief 按实际 JSON 序列化大小硬限制为 8 KiB，不新增 handoff 持久化，
builder 也不执行额外工具；但公开 MCP dispatch 仍会向指定 Workflow Session
统一追加正常的 `tool_call_started` / `tool_call_finished` telemetry。这些 recorder
事件不代表投影发生业务修改。它不是 Session replay，也不会恢复模型隐藏上下文；
需要完整 attempt 证据时读取同时返回的 `continuation_feedback`。新窗口可以正常
创建自己的 Session，再显式读取旧 Session 的 handoff。若选择
`resume_session_id`，仍会执行既有严格的 active-session resume 检查。

这些 durable ID 用于模型工具与 host review 之间的稳定关联，普通用户无需管理：

- `task_id`：继续/review 一个 bounded task；
- `operation_id`：mutation/execution 的 exact retry identity；
- `execution_id`：查看、等待或取消一个 durable execution；
- `result_id`：review 并决定一个 stable result。

Agent transport、executor routing 和 pending request ID 保持内部实现。

## Golden coding loop

```text
task_start
→ files_list
→ files_read / files_search
→ edits_apply
→ checks_run
→ task_finish
→ task_review
```

`commands_run` 只作为需要 approval 的 escape hatch。queued/running execution
需要停止时使用 `task_cancel`。

普通可写 task 必须在 finish 前运行 structured checks。成功 check 带 trusted
workspace provenance；之后任何 mutation 都使其 stale，并要求新的 operation ID。
command 无法 spawn 属于 executor failure，不是 assertion evidence。

### `checks_run` recipes

`checks_run` schema 仍只暴露 `format`、`check`、`test`，以及可选 enum
`recipe`（`rust`、`node`、`python`、`go`）。省略时，从 Task workspace 的相对
`cwd` 选择最近的 `Cargo.toml`、`package.json`、`pyproject.toml` 或 `go.mod`。
显式匹配 recipe 可解除同 root 歧义。唯一的 markerless 例外是：没有选中
`pyproject.toml` 时，显式 `recipe=python` 加 `checks=["test"]` 会从 `cwd`
运行固定 unittest discovery plan。resolver 不扫描 sibling project，也不允许
path/symlink escape。

Rust 支持三项 check，并且是唯一支持单 argv `test_filter` 的 recipe。Node 使用有
证据的 package manager 和固定 script allowlist；Python 选择已配置的
Ruff/Black、Ruff/Mypy 或 pytest，manifestless Python 则只支持
`python -B -m unittest discover -v`；Go 支持 `check/test`，`format`
unavailable。所有 recipe 都不安装依赖、不修改 lockfile、不联网。tool 缺失属于
executor failure；validator 启动后 non-zero 才属于 assertion failure。recipe
version、相对 root 和 invocation/manifest evidence 绑定 operation exact-retry
identity。recipe 永远不会新增 MCP tool；MCP、OpenAPI 与 capability registry 共享
同一份 capability 清单。finish 时会排除 untracked interpreter/test cache、coverage
output 和 `node_modules` 并返回 bounded warning；tracked 路径始终保留。

### 长时间结构化验证自动继续为 Job

`cargo_check`、`cargo_test` 与 `cargo_fmt(check=true)` 只运行命令一次。
`timeout_secs` 是命令的总运行预算（1..=3600；默认值：check 600、test 1800、
fmt check 120），与工具调用自身阻塞多久无关。短验证在进程内直接完成并返回
既有的终端结果；长验证（预算超过内部同步等待窗口）把同一次执行提升为可查询
的 Job 并返回 `job_id`、`promoted_to_job=true`、`execution_state=queued/running`
与 `effective_timeout_secs`，且 handoff 时绝不报告 `failure_kind=timeout`。
用 `job_status` / `job_log` 轮询，或读取 `validation_summary`——Job 的终端状态会
汇入 summary；不要为了找结果而重跑命令。`cargo_fmt` 的 `check=false` 会修改源码，
绝不自动提升。handoff/cancel 竞态是安全的：被 cancel 的 handoff 不会遗留孤儿进程，
`stop_job(confirm=true)` 可以停止已提升的 Job。

旧 Runner 兼容路径有明确边界。如果 Runner 具备基础 shell 执行，但不支持 async
validation Job 和 structured validation argv：省略 `timeout_secs` 时只同步执行一次，
有效预算为 120 秒，并报告 `async_handoff_available=false`；显式预算不超过 120 秒时
同样同步执行一次；显式预算超过 120 秒时，会在命令启动前以
`failure_kind=capability_unavailable` 拒绝，不会静默截短，也不会再次启动命令。
升级 Runner 后才能恢复长验证 Job handoff。

```bash
webcodex task show <task-id>
webcodex task accept <task-id>
# 或：webcodex task reject <task-id>
```

## 第一个安全 Prompt

```text
Use the configured WebCodex project. Start a read-only task, read README.md,
summarize the project, review the result, and finish. Do not edit files.
```

prompt 中不需要 project discovery 或 runtime identifier。

## 常见错误

| Code | 含义 | Action |
|---|---|---|
| `project_not_configured` | canonical setup 不存在 | `webcodex setup` |
| `project_registration_invalid` | 本地 project state malformed、incomplete 或冲突 | 解决报告的 private-state conflict |
| `project_credential_invalid` | private Project Credential 缺失、权限不安全、格式错误或两份不匹配 | 恢复两份匹配 private file，或显式重建 profile |
| `project_credential_rejected` | 可达 server 拒绝已配置 Project Credential | 恢复与 server 匹配的 credential；不得折叠为 Agent offline |
| `workspace_unavailable` | 配置的 Git workspace 不可用 | 恢复 workspace 后运行 doctor |
| `server_unreachable` | project runtime 不可用 | 本地 project-first 模式运行 `webcodex run` |
| `agent_offline` | 本地 Agent 未 ready | `webcodex doctor` |
| `required_capability_unavailable` | Agent 缺少 coding capability | 升级全部 binaries |
| `structured_validation_unavailable` | Agent 不能运行 structured checks | 升级全部 binaries |
| `task_not_active` | task 已不能 mutation/execute | 新建 task |
| `execution_not_terminal` | active/unknown work 阻止 finish | review/wait/cancel |
| `validation_recipe_not_found` / `validation_recipe_ambiguous` | auto resolution 没有 recipe 或最近 root 多 recipe | 修改 `cwd` 或提供匹配 `recipe` |
| `validation_recipe_mismatch` / `validation_manifest_invalid` | 显式 recipe、路径、marker 或 manifest evidence 无效 | 修复报告的公开 evidence |
| `validation_check_unavailable` / `test_filter_unsupported` | 请求的 semantic input 没有安全映射 | 修改 check/filter |
| `package_manager_ambiguous` | Node package-manager evidence 缺失或冲突 | 修正 `packageManager` 或 lockfile |
| `validation_tool_unavailable` | 所选 executable/module 缺失 | 提供项目已有工具并使用新 operation ID |
| `checks_required` | 普通 task 尚未运行 checks | 调用 `checks_run` |
| `checks_stale` | 上次 check 后 workspace 改变 | 运行新的 check |

短答案使用 `webcodex status`，完整只读 findings 使用 `webcodex doctor`。

## 有界源码阅读（`read_file`）

`read_file` 是针对 agent 注册项目的有界、流式 UTF-8 范围读取。本地项目与
agent 项目复用同一套范围算法，因此除解析出的 project id 和传输方式外，
模型侧输出完全一致。

输入保持不变：`project`、`path`、可选 `session_id`、可选 `start_line`
（默认 1，最小 1）、可选 `limit`（默认 2000，clamp 到 `1..=2000`）、可选
`with_line_numbers`。未新增任何输入字段、批量模式或配置项。

成功读取会顺序扫描文件一次——流式计算完整文件 SHA-256 与总行数，仅保留
所请求范围——返回：

```text
text              # 选中范围的正文，plain 或 numbered，行以 \n 连接
format            # "plain" | "numbered"
path              # 项目相对输入路径
sha256            # 完整文件的 64 位小写十六进制摘要
start_line        # 有效起始行（>= 1）
limit             # 有效行数上限（1..=2000）
total_lines       # 完整文件行数（>= 0）
returned_lines    # 实际返回的原始文件行数（>= 0，<= limit）
end_line          # start_line + returned_lines - 1；无返回时为 null
has_more          # 仅当返回范围之后仍有文件行时为 true
next_start_line   # 续读起始行 end_line + 1；到文件末尾为 null
```

`with_line_numbers=true` 只改变 `text` 和 `format`，绝不改变
`returned_lines`、`end_line`、`has_more`、`next_start_line`。

使用 `next_start_line` 续读：

```jsonc
// 第一次
{ "project": "demo", "path": "src/main.rs", "limit": 40 }
// -> next_start_line: 41, has_more: true
// 从上一次停止处继续
{ "project": "demo", "path": "src/main.rs", "start_line": 41, "limit": 40 }
```

边界由代码直接保证，不依赖 transport 尾部截断。选中原始正文有独立的
192 KiB 预算；Runner 在发送前序列化完整的
`webcodex.file_read_range.v1` envelope，并按有效 transport cap 与 256 KiB
两者中的较小值复检。ToolRuntime 随后还会在添加行号和 JSON 转义后复检
最终模型输出。任一层超出预算都稳定返回 `reason_code: range_too_large`——
请缩小 `limit` 或缩窄范围后重试；绝不返回半行，也不返回与 SHA/行数元数据
不一致的正文。

失败返回小而稳定、有 schema 的对象——绝不包含绝对路径、原始 OS 错误、
命令或 Runner stdout/stderr：

```text
error_kind:   "read_file_failed"
reason_code:  invalid_path | sensitive_path | not_found | not_file |
              permission_denied | invalid_utf8 | range_too_large |
              agent_unavailable | timeout | malformed_agent_response | io_error
path:         项目相对输入路径（仅用于定位）
state_changed: false
```

Agent 的 `file_read_range.v1` 响应被视为不可信输入：每个正式字段都会被
严格验证，模型输出仅由这些字段重建。未知字段、padding、与请求不一致的范围
元数据、错误 SHA、或 content/行数不一致都会被剥离或拒绝
（`malformed_agent_response`），绝不透传到模型。

### 有界批量读取（`read_files`）

`read_files` 是独立工具，不改变 `read_file`。它接收必填 `project`、包含
1 到 8 个 `{path, start_line?, limit?}` 的 `items`，以及批次共享的可选
`with_line_numbers`。路径复用 `read_file` 的项目相对路径和敏感路径校验。

每个批次只解析一次项目；随后各项复用单文件读取的范围规范化、UTF-8 校验、
SHA-256、行号格式、Runner 响应解析、稳定错误码和序列化安全检查。各项独立
执行，最终按输入顺序恢复结果。最多四个 future 同时覆盖校验、Runner enqueue
和等待响应，所以并发槽位释放前，第五个读取不会进入 Runner 队列。

整个批次共享 30 秒 deadline。已完成结果保持不变；未完成项变为 `timeout`，
已 enqueue 的未完成请求会逐项取消。一个普通文件失败不会取消其他项。

最终序列化结果固定为 256 KiB 预算。工具按输入顺序加入完整 item，绝不截断
单个 item；下一个 item 放不下时返回 `output_truncated=true`，并用
`next_index` 指向第一个未返回的原始输入项。调用方可从原 `items` 的该位置
重试。Session 与 permission 元数据只附加到批次外层；顶层 `project` 是解析后
的 runtime project id。

### 有界批量文本搜索（`search_project_texts`）

`search_project_texts` 是独立的只读工具，不改变 `search_project_text` 契约。
它接收一个必填 `project`、1 到 8 个独立 `queries`，以及可选的外层 Workflow
`session_id`。每个 query 复用现有的 `pattern`、`path`、`result_mode`、`limit`、
上下文、glob 和超时字段。它不会合并 pattern、读取命中文件、调用 LSP，也不会
进行语义或模型分析。

项目只解析和授权一次。各 query 随后复用单查询的校验、受保护路径排除、
rg-first/grep fallback、超时规范化、结果解析、路径过滤、截断和错误映射。
校验、Runner enqueue 与等待响应全部占用同一个二并发槽；槽位释放前第三个搜索
不能进入 Runner。结果最终恢复输入顺序；某个普通失败、无匹配或超时 query
不会取消其他槽位的工作。无匹配继续保持单查询的成功语义。

整个批次由一个精确的 30 秒 deadline 约束。query 的命令超时取规范化超时与
剩余批次预算的较小值（Runner 协议以整秒表示），精确的外层 deadline 始终
有效。已完成 item 保留，未完成的 Runner 请求会取消，只有未完成 item 变为
`timeout`。

最终 `ToolResult` 按真实 JSON 序列化大小计入 256 KiB 预算，并为外层 Session
元数据预留空间。工具按输入顺序只加入完整 item；下一个 item 放不下时，返回
`output_truncated=true`，`next_index` 指向调用方可从原列表重新提交的第一个
query。解析后的 runtime project id 与 Session/permission 元数据只位于外层；
item 使用 `index` 对应输入，因此不会重复原始 pattern。一个批次只记录一个
read-like search event，exploration `search_count` 只增加一次。只有成功且已返回
item 中去重后的项目相对路径进入探索证据；`queries[*].pattern` 会从所有
Workflow Session ledger 投影与持久化数据中移除。

## 有界项目文本搜索（`search_project_text`）

`search_project_text` 是默认的 inspect/search 工具。它优先使用 ripgrep，
在基本 matches 请求上保留 grep fallback，并且在“工作量”和“字节”两个维度
都有界：

- **尽早停止。** 搜索结果按遍历顺序输出（不做全局路径排序），当请求的
  记录预算满足时，命令管道立即关闭，因此小 `limit` 搜索会快速返回，而不会
  等待整仓库扫描完成。因此匹配顺序不确定，但结果有界且及时。
- **字节预算。** 管道中第二级 cap 只比正式搜索预算多输出一个有界 probe
  byte；服务端仅用该字节证明 cap 已触发，包括边界恰好落在换行之后的情况，
  并返回 `truncation_reason = "output_bytes"`。单个超长匹配行或上下文行不会
  突破 Runner transport cap，且只返回完整记录。
- **超时部分成功。** 若有效超时在已收集到完整记录后才触发，工具仍返回这些
  记录，并标注 `truncated = true` 与 `truncation_reason = "timeout"`，而不是
  丢弃它们。`count` 模式绝不把部分计数冒充完整总数：`count_complete` 保持
  false，`total_matches` 保持 null。若超时前没有完整记录，则返回结构化
  `search_timeout` 失败。
- **可信路径。** 返回的路径均为项目相对路径且经过校验；绝对路径、父目录
  遍历、临时文件路径、Shell 命令和 Runner stderr 一律不返回。

截断元数据稳定：`truncated` 配 `truncation_reason`（`limit | output_bytes |
timeout | transport` 之一），完整时为 `null`。

## Advanced runtime surface

WebCodex 也可以作为 multi-project management ToolRuntime 运行。其 discovery、
session、LSP、raw job、artifact 和 operator tools 继续记录在
[OPERATIONS.md](OPERATIONS.md)。那是高级 surface，不是 canonical project
Connector，也不是普通 coding 的前置步骤。
