# MCP

[English](MCP.md) | [简体中文](MCP.zh-CN.md)

client 能连接 project-bound WebCodex endpoint 时使用 MCP。先完成
[QUICK_START.zh-CN.md](QUICK_START.zh-CN.md)。

## Endpoint 与认证

本地 client 可以使用：

```text
http://127.0.0.1:<configured-port>/mcp
```

Hosted client 需要 operator 管理的 HTTPS endpoint：

```text
https://your-domain.example/mcp
```

Bearer credential 必须用于 runtime/project access。不要使用或暴露
bootstrap/admin、account 或 Agent credential。优先使用 client secret store，
不要提交 token。

Canonical setup 不打印 credential value 或 secret path，不创建 tunnel，也不开放
公网端口。production enrollment、scoped user token 和 OAuth 见
[AUTH_MODEL.zh-CN.md](AUTH_MODEL.zh-CN.md) 与
[DEPLOYMENT.zh-CN.md](DEPLOYMENT.zh-CN.md)。

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

同一个聊天窗口会自动继续已配置仓库的工作。`task_start` 会为该窗口和项目解析
唯一上下文，不创建无意义的重复任务；每条后续指令都会追加到当前持久任务。
切换仓库连接时各仓库历史保持隔离，切回此前连接时恢复原任务。兼容的 MCP
client 会自动保留协议 session，用户不需要在 prompt 中传入它。

Connector context 已绑定项目。直接从 `task_start` 开始；不要调用
`list_projects`、`runtime_status`、`tool_manifest`、`start_session` 或
`current_session`，也不要向用户索取 Agent client ID、runtime project ID、
executor reference 或 workflow session。

复用前，WebCodex 会比较仓库实际路径、分支与 HEAD、工作区、适用的仓库规则及
项目 manifest。未变化的上下文直接复用，只把变化部分标为已刷新。如果有界扫描
无法证明某个 slice 完整，response 会把它标记为 partial/unknown 并返回紧凑
warning，不会声称已经复用。`task_list` 和 `task_resume` 只用于 client 丢失 MCP
transport session 后的显式恢复，不是普通工作流的前置步骤。

在 `local_coding` 上，MCP `tools/list` 恰好包含如下聚焦 coding 工具集，且顺序
严格一致：

```text
work_on_project
list_projects
project_overview
list_project_tracked_files
list_project_files
search_project_text
read_file
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
普通入口：一次调用即可返回规则、仓库结构、Git 状态、LSP 就绪度、jobs 与
blockers，让模型可以立即开始或继续聚焦工作。没有 `session_id` 时创建新的
Workflow Session；有 `session_id` 时只精确继续该 Session（绝不猜测最近 Session，
也绝不 credential-wide fallback），且从不建立 current-window binding。成功调用
返回 `session_id`、解析后的项目 id、`readiness` verdict、`workspace` Git 投影、
`repository` 概览、有界的 `rules`（`status` 为 loaded/reused/changed/not_found/
unavailable，含每个 source 的 fingerprint、headings、有界正文与 `read_more`
提示）、`semantic_navigation` 就绪度、紧凑 `jobs` 计数，以及 `blockers`/
`warnings`/`suggested_next_actions`。返回的 `repository` block 是确定性的
元数据扫描：只读取目录项、文件类型和 Git tracked index（project types、
manifests、key files、roots、top-level 项以及带原因的项目相对 suggested
reads）；绝不读取普通文件正文、执行项目代码、跟随符号链接或扫描
protected/sensitive/build/cache 路径。每个列表都有界，并各自记录
total/returned/truncated 元数据。概览不可用时，Session 仍正常启动，
`repository.status=unavailable` 并带有 `repository_overview_unavailable` warning；
原始错误、绝对路径或 Runner 输出永不返回。这些新增信息仅用于参考：不会自动
修改或自动执行任何内容，模型仍需按需使用 `read_file`、搜索、编辑与验证工具。

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

review 后由人类在 host 上接受或拒绝：

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
| `server_unreachable` | project runtime 不可用 | `webcodex agent start` |
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

## Advanced runtime surface

WebCodex 也可以作为 multi-project management ToolRuntime 运行。其 discovery、
session、LSP、raw job、artifact 和 operator tools 继续记录在
[OPERATIONS.md](OPERATIONS.md)。那是高级 surface，不是 canonical project
Connector，也不是普通 coding 的前置步骤。
