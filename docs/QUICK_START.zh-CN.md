# 快速开始

[English](QUICK_START.md) | [简体中文](QUICK_START.zh-CN.md)

这是唯一 canonical project-first 路径。它配置一个本地 Git 项目，不要求用户提供
Agent client ID、runtime project ID、transport、workflow session、executor
reference 或内部 config path。

## 前置条件

- 已安装三个 WebCodex binaries：`webcodex`、`webcodex-server`、
  `webcodex-runner`；
- `PATH` 中有 Git；
- 一个可以安全查看和修改的 Git 项目。
- 仅默认 `webcodex share` 路径需要：已安装 `cloudflared`，并可从 `PATH` 找到。若缺失，请从 [Cloudflare 官方下载页](https://developers.cloudflare.com/tunnel/downloads/)安装；WebCodex 不会静默安装系统软件包。

安装 Linux x64 或 macOS arm64 package：

```bash
npm install -g @yyjeqhc/webcodex
```

或从本仓库构建：

```bash
cargo build --release --workspace --bins
export PATH="$PWD/target/release:$PATH"
```

## 1. Setup 当前项目

进入 Git 项目并执行：

```bash
webcodex setup
```

第一次运行会：

- 解析 Git top-level directory；
- 在 checkout 外创建 private state；
- 创建最小 project registration 和 Agent config；
- 创建一个供本项目 Connector 与 Agent 使用的精确 Project Credential，但不打印
  其内容；
- 保持 server 和 Agent 停止。

它不会修改项目文件或 Git，不会启动 service、修改 shell config、开放网络端口或
上传源码。

再次执行同一命令验证幂等：

```bash
webcodex setup
```

第二次返回 `already configured`。若一个生成组件缺失，只修复该组件。若已有字段
与当前 Git root/profile 冲突，setup 会指出字段并停止，不覆盖现有配置。

长期 Connector credential 与 Agent Token 是两把独立 secret，并映射到同一个稳定、
非秘密的 project grant identity。对应文件都属于 owner-only private state；数据库
不保存明文。runtime 会 hash credential candidate 并使用 constant-time comparison。
这条路径独立于普通 shared-key quick start：project mode 会拒绝任意未知 Bearer
value。

Setup 不会静默轮换仍存在的 credential。credential 丢失时应恢复两份匹配的 private
file；若无法恢复，先停止 runtime，明确退役整个 private project-state profile，再
重新运行 setup。该显式重建也会退役其中的本地 Task/Execution history；Iteration
8.0 没有 in-place rotate subcommand。

## 2. 诊断下一步

```bash
webcodex doctor
```

Doctor 完全只读。Agent 尚未启动时，预期 verdict 是 `Needs action`：

```text
Next:
  webcodex run
```

每条 finding 都有稳定 `name`、`status`、`code`、`summary` 和 `next_action`。
需要结构化 projection 时使用 `webcodex doctor --json`。

## 3. 启动本地 runtime

```bash
webcodex run
```

这是 canonical foreground action，会启动绑定当前项目的 loopback Server 和本地
Agent；不会安装 system service。保持该终端运行，Ctrl-C 会停止两个进程。
Loopback 不构成认证豁免；只有 setup 配置的精确 Project Credential 能访问该项目
Connector。

在同一项目的另一个终端运行：

```bash
webcodex status
```

ready 时只显示 Project、Connection、Agent、coding readiness 和 next action。需要
完整诊断时再次运行 `webcodex doctor`。

## 4. 使用 project-bound Connector

当前项目生成的 Connector profile 会把一个 logical project 确定性绑定到一个
registered executor。使用这份 approved connection 及其精确 credential 的本地
MCP/OpenAPI client 可以直接调用：

```text
task_start
```

它不需要 `list_projects`、`runtime_status`、`tool_manifest`、`start_session` 或
`current_session`，prompt 中也不需要 `agent:<client>:<project>`。同一个聊天窗口
会自动继续当前仓库的工作；切换到另一份已配置仓库时会自动切换项目上下文，
之后切回会恢复该仓库之前的工作。只有 client 无法继续提供 transport 窗口身份
时，才需要用 `task_list` 和 `task_resume` 做显式恢复。

### 为临时分享安装 `cloudflared`

默认 `webcodex share` 使用 Cloudflare Quick Tunnel。如果 `PATH` 中还没有
`cloudflared`，可以使用 Cloudflare 官方 package 安装方式：

```bash
# macOS
brew install cloudflared

# Debian / Ubuntu：把 Cloudflare 官方 APT 安装步骤合并成一条可直接复制的命令
sudo mkdir -p --mode=0755 /usr/share/keyrings && curl -fsSL https://pkg.cloudflare.com/cloudflare-main.gpg | sudo tee /usr/share/keyrings/cloudflare-main.gpg >/dev/null && echo "deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared any main" | sudo tee /etc/apt/sources.list.d/cloudflared.list >/dev/null && sudo apt-get update && sudo apt-get install -y cloudflared
```

其他平台见 Cloudflare [官方下载与安装说明](https://developers.cloudflare.com/tunnel/downloads/)。
WebCodex 不会静默安装系统软件包，也不会自行提升权限。

ChatGPT hosted client 无法访问 loopback address。如果只是开发/测试期间临时让
hosted MCP client 访问当前项目，先停止 `webcodex run`，然后执行：

```bash
webcodex share
```

`share` 复用同一套 project-first setup 和 local runtime，但会启动 Cloudflare Quick
Tunnel，并为本次 session 创建一把独立的临时 Connector credential。命令会输出
临时 `https://*.trycloudflare.com/mcp` URL 和 Bearer token；命令退出后两者都失效。
`webcodex share --tunnel none` 可在不创建公网 tunnel 的情况下启动同一 share
runtime，便于本地 debug。Quick Tunnel 不是 production 部署方式。

如果 Quick Tunnel 启动失败，并且机器上已经存在
`~/.cloudflared/config.yaml`，需要注意 Cloudflare Quick Tunnel 不支持该配置文件。
可以为 Quick Tunnel 使用独立环境（或临时移开该配置），也可以使用 `--tunnel none`
进行纯本地 debug。

需要长期稳定 endpoint 时，应使用 stable HTTPS domain/tunnel、service 管理以及
OAuth 或其他生产认证方案。见 [DEPLOYMENT.zh-CN.md](DEPLOYMENT.zh-CN.md)、
[MCP.zh-CN.md](MCP.zh-CN.md) 或 [GPT_ACTIONS.zh-CN.md](GPT_ACTIONS.zh-CN.md)。

## 5. 运行 golden coding path

让 client 完成一个小型、可逆修改。canonical 调用为：

```text
task_start
→ files_list
→ files_read 或 files_search
→ edits_apply
→ checks_run
→ task_finish
→ task_review
```

edit、command 和 check 使用 `operation_id` 提供 exact retry identity：同一 payload
重试会复用 operation；同一 ID 搭配不同 payload 会 fail closed。

普通可写 task 未运行 structured check 时不能 finish。check 真正运行后 non-zero
才属于 project assertion failure；check 无法 spawn 属于 executor/infrastructure
failure，不产生 assertion evidence 或 trusted workspace provenance。

### Project-aware validation recipes

`checks_run` 保留现有 `format`、`check`、`test` 语义名，并增加可选 enum
`recipe: rust|node|python|go`。省略 `recipe` 即 auto resolution，不存在 `auto`
alias。resolver 从 Task execution workspace 内的相对 `cwd` 开始，只向该 workspace
root 逐级查找并选择最近的 manifest 目录。同一最近目录存在多个 supported marker
时 ambiguous；显式提供实际存在的 recipe 可解除歧义。唯一的 markerless 例外是：
没有选中 `pyproject.toml` 时，显式 `recipe=python` 加 `checks=["test"]` 会从
`cwd` 运行 `python -B -m unittest discover -v`。其他 recipe 的 marker 不匹配、
auto 模式 manifest 缺失、绝对/父目录路径或 symlink escape 都在 reservation 前
拒绝。

| Recipe | Marker | `format` | `check` | `test` |
|---|---|---|---|---|
| Rust | `Cargo.toml` | `cargo fmt -- --check` | `cargo check --all-targets` | `cargo test` 加一个安全 argv filter |
| Node | `package.json` | 依次选择 `format:check`、`format-check`、`check:format` | 依次选择 `check`、`typecheck`、`lint` | 精确 `test` |
| Python | `pyproject.toml`，或显式 markerless test | 已配置 Ruff，否则 Black | 已配置 Ruff，否则 Mypy | 已配置 pytest；markerless 时 `unittest discover` |
| Go | `go.mod` | unavailable | `go vet ./...` | `go test ./...` |

Node 从有效 `packageManager` 声明或唯一、无歧义的 supported lockfile
（`pnpm-lock.yaml`、`yarn.lock`、`package-lock.json`、`npm-shrinkwrap.json`、
`bun.lock`、`bun.lockb`）选择 package manager。证据冲突或缺失时 fail closed；
script 只以 `<manager> run --silent <allowlisted-name>` 调用，script body 不进入
plan 或 error。Python 的 format/check 和 pytest 仅启用 `pyproject.toml` 有配置
证据的工具；format 时 Ruff 优先于 Black，check 时 Ruff 优先于 Mypy。
Manifestless Python 只支持固定的 unittest test plan。

recipe 不安装依赖、不运行 install hook、不生成配置、不创建 environment、不修改
lockfile、不联网。只有 Rust 支持 `test_filter`，且作为单独 argv；其他 recipe
会拒绝 filter，绝不忽略后运行全量测试。executable 或 Python module 缺失属于
executor failure，不生成 failed check 或 assertion evidence；真实进程以 non-zero
返回 validation verdict 才属于 assertion failure。

`task_finish` 会从 result patch 排除 untracked interpreter/test cache、coverage
output 和 `node_modules`，并以 bounded warning 报告；项目已 tracked 的同名路径
绝不排除。

durable plan 记录 recipe ID/version、相对 root、semantic checks、tool identity 和
invocation/manifest evidence digest，并全部进入 request hash。因此同一
`operation_id` 只复用完全相同的 resolved plan；recipe binary 变化会与旧 ID
conflict，使用新 ID 才按新 recipe 解析。manifest、lockfile 或 workspace 改变会使
成功 provenance stale。

## 6. 本机 review 和 accept

coding result 会与 target checkout 保持隔离，直到人类决定。该决定是本机授权，有两条
入口——离线 CLI 与浏览器 console——两者共用同一套 accept/reject 授权：

```bash
webcodex task list
webcodex task show <task-id>
webcodex task accept <task-id>
```

使用 `webcodex task reject <task-id>` 丢弃结果。Accept 前会验证 target Git state
仍匹配 task baseline。

在浏览器中，打开 `/console`，输入 project credential（仅保存在内存中、绝不持久化），
用工作队列选择任务。review 详情展示目标、状态、validation、changed files、bounded
unified diff 与 bounded output tail，**Accept / Reject / Cancel** 均需页面内显式确认。
Accept 与 Reject 调用与 CLI 相同的授权；Cancel 停止正在运行的 execution。Hosted Chat
只能提议工作、永远无法接受；server 在应用前会重新校验 checkout 与 result——浏览器上
的点击无法绕过这些前置条件。

## Browser console

本地 runtime 运行时，`/console` 显示：

- Project header（当前 Project、Connection、Agent readiness、coding capability
  readiness、下一步 action）；
- 可执行工作队列（最近需要关注的任务）；
- 选中任务的 review 详情，含 bounded diff 与 output tail。

它消费 doctor/status 同一组 application readiness facts，并驱动与 CLI 相同的本机
决策授权。它不显示 Agent registry、client ID、transport implementation、queue ID
或 token，也不是 browser editor / terminal——无法编辑代码、运行命令或启动任务。

## Troubleshooting

始终先运行：

```bash
webcodex status
webcodex doctor
```

常见 stable code：

| Code | 含义 | 下一步 |
|---|---|---|
| `project_not_configured` | 当前 Git 项目/profile 没有 setup | `webcodex setup` |
| `project_registration_invalid` | 现有 state 冲突或不完整 | 解决指出的字段后重新 setup |
| `project_credential_invalid` | private credential 缺失、不可读、权限不安全、格式错误或两份不匹配 | 恢复两份匹配的 private file，或显式重建 profile |
| `project_credential_rejected` | server 拒绝本地配置的 credential | 恢复匹配 credential；不得折叠成 Agent offline |
| `server_unreachable` | loopback runtime 不可达 | `webcodex run` 或查看 doctor |
| `agent_offline` | server 可达但本地 Agent 不可用 | `webcodex run` |
| `required_capability_unavailable` | Agent 太旧或不完整 | 升级全部 WebCodex binaries |
| `structured_validation_unavailable` | Agent 缺少 structured validation | 升级全部 WebCodex binaries |
| `workspace_unavailable` | Git 或配置的项目路径不可用 | 恢复 path/Git workspace |
| `validation_recipe_not_found` | auto resolution 从 `cwd` 到 Task root 没有 supported marker | 选择包含 manifest 的 `cwd`，或显式使用 markerless Python unittest test recipe |
| `validation_recipe_ambiguous` | 最近 root 有多个 supported marker | 提供匹配的显式 `recipe` |
| `validation_recipe_mismatch` / `validation_manifest_invalid` | recipe、marker、安全路径或 manifest evidence 无效 | 修复报告的公开 evidence |
| `validation_check_unavailable` / `test_filter_unsupported` | recipe 无法安全映射 check/filter | 修改 checks/filter 或选择匹配 recipe |
| `package_manager_ambiguous` | Node package-manager evidence 缺失或冲突 | 修正 `packageManager` 或 lockfile |
| `validation_tool_unavailable` | Agent host 缺少所选 executable/module | 提供项目已有工具并使用新 operation ID |
| `checks_required` | 普通 result 尚未运行 checks | 运行 `checks_run` 后 finish |
| `checks_stale` | 上次可信 check 后 workspace 改变 | 运行新的 check operation |

高级 server、enrollment、OAuth、transport 和 fleet diagnostics 继续放在
`webcodex` 与 operations 文档中，不是 onboarding 步骤。

## 本机活动记录与命令预览

控制台的活动账本会把每次改动型工具调用**持久化到本机状态库**：工具名、发起面、
涉及路径、错误摘要，以及 shell 类调用的**命令预览**（前 120 个字符）。

命令预览默认开启。它是"知情审批"的依据——看不到命令内容就无从判断该不该批准。
但也因此：

- **不要把 token、密码或密钥直接写在命令行里。** 它们会连同命令一起落库，
  并显示在控制台上。改用文件或环境变量传递。
- 需要完全关闭预览时：

  ```bash
  WEBCODEX_ACTIVITY_COMMAND_PREVIEW=0 webcodex run
  ```

  关闭后仍会记录工具名、路径和结果，只是命令文本存为空。

账本只存在本机状态目录，不会外发。行数有上限，超出后自动修剪最旧的记录。

每条记录还会记下它**写入时**属于哪个项目。这个归属一次确定、此后不再重算——
因为设备名并非长期唯一：如果一台叫 `laptop` 的设备之后以另一个项目的身份重新
连接，它只会有自己的历史，读不到先前项目的命令、路径和错误。旧版本产生、事后
无法确定归属的记录会保留在账本里，但只有宿主管理员可见。
