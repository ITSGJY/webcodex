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
- 未设置该变量时，`/mcp` 暴露 `full_operator_runtime`。server 会输出明确的启动
  warning；这是 operator/debug surface，不是带有 Connector 连续性承诺的隐式
  Connector。
- surface value 不受支持或 Connector 配置不完整时，server 启动配置失败，不会
  静默切换到另一个 surface。

`GET /mcp` 和 MCP `initialize.serverInfo` 通过 `modelSurface` 报告选择结果。
Full operator surface 上的 `runtime_status.model_surface` 也报告同一事实。标准普通
用户 setup 选择 `canonical_connector`；只有 operator 明确不配置 Connector 时
才使用 full runtime。

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

在 `full_operator_runtime` 上，普通 coding 使用 `start_coding_task` 开始或继续。
稳定窗口默认继续同一仓库；切换仓库会切换上下文，切回会恢复此前 Workflow
Session。`new_session=true` 是显式的高级隔离请求。current binding 只存在于当前
server process，因此应保留返回的 session id，以便 server 重启后显式恢复。

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
