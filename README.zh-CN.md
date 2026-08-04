# WebCodex

[English](README.md) | [简体中文](README.zh-CN.md)

[![CI](https://github.com/yyjeqhc/webcodex/actions/workflows/ci.yml/badge.svg)](https://github.com/yyjeqhc/webcodex/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/%40yyjeqhc%2Fwebcodex)](https://www.npmjs.com/package/@yyjeqhc/webcodex)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

[下载最新版本](https://github.com/yyjeqhc/webcodex/releases/latest) ·
[0.3.1 发布说明](docs/RELEASE_NOTES_v0.3.1.zh-CN.md) ·
[完整文档](docs/INDEX.zh-CN.md)

**让网页版 ChatGPT / Claude 直接改你的私有代码——但没有你的审核，一行都落不了地。**
AI 在你机器上的隔离工作区里编辑、跑测试、提交结果；你看 diff、点 Accept 才真正生效。
这道人工审核闸门是整个产品的核心：那些让模型直接写盘的工具，架构上补不回这一层。

```
ChatGPT / Claude（网页聊天窗口）
        │  MCP 或 GPT Actions（HTTPS）
        ▼
WebCodex server ──▶ 本地 Agent：编辑 → 跑检查 → 提交结果
        │                    （隔离工作区，不碰你的 checkout）
        ▼
你：浏览器 console 或 CLI ── 审核 diff ──▶ Accept ✓ / Reject ✗
        │  仅 accept 后
        ▼
你的仓库
```

- **人工审核闸门** —— 结果先隔离，`/console` 网页或 `webcodex task accept` 审核通过才落地。
- **一切留在你的机器** —— 源码、Git、修改、检查都在拥有仓库的主机上；对外只暴露十二个有界、可审计的能力，而不是裸 shell。
- **为真实开发准备** —— LSP 语义导航、带 sha256 守卫的结构化编辑、四语言校验配方（Rust/Node/Python/Go）、服务端强制的幂等重试、完整的任务事件时间线。

| ChatGPT 通过 MCP 驱动任务 | 本机审核与落地 |
| --- | --- |
| ![MCP 会话](docs/assets/mcp-1.png) | ![GPT Action 审核](docs/assets/gpt-action-1.png) |

<details>
<summary>更多截图</summary>

![MCP](docs/assets/mcp-2.png)
![MCP](docs/assets/mcp-3.png)
![MCP](docs/assets/mcp-4.png)
![GPT Actions](docs/assets/gpt-action-2.png)
![GPT Actions](docs/assets/gpt-action-3.png)
![GPT Actions](docs/assets/gpt-action-4.png)
![GPT Actions](docs/assets/gpt-action-5.png)

</details>

WebCodex 让 coding client 通过项目级 server 和本地 Agent 操作私有代码。源码、
Git、修改和验证仍留在拥有仓库的机器上。

## 安装

支持的 Linux x64、Linux arm64 和 macOS arm64 环境可以直接安装：

```bash
npm install -g @yyjeqhc/webcodex
```

也可以从源码构建全部 binaries：

```bash
cargo build --release --workspace --bins
export PATH="$PWD/target/release:$PATH"
```

安装细节见 [docs/BUILD_INSTALL.zh-CN.md](docs/BUILD_INSTALL.zh-CN.md)。

npm 安装位置与 systemd service scope 是两件事。普通用户可以把 package 安装到
自己拥有的 npm prefix，不使用 `sudo` 登录，并把常驻 Runner 安装为用户 service：

```bash
webcodex login https://your-domain.example --code <wc_pair_...> \
  --allowed-root "$HOME/git"
webcodex agent install --scope user \
  --config <login 输出的 agent config 路径>
webcodex agent status --scope user \
  --config <login 输出的 agent config 路径>
```

user service 使用 `systemctl --user`，不需要 root 权限，unit 位于
`$XDG_CONFIG_HOME/systemd/user`（未设置时为 `$HOME/.config/systemd/user`）。
非 root 用户省略 scope 时默认使用 user scope。管理员管理的 system scope 及其
非 root Runner 要求见[构建与安装指南](docs/BUILD_INSTALL.zh-CN.md#runner-service-scope)。

## Hosted 最快接入

最低成本路径使用官方托管 Server，只在持有代码的机器上运行一个后台 Runner。
无需部署 Server、数据库、HTTPS、反向代理、OAuth 或 systemd service：

```bash
cd /path/to/your/repository
webcodex connect https://sg4.yyjeqhc.cn
```

当前目录就是默认项目。`connect` 会自动生成强随机 `wck_...` shared key，创建或
复用本地 profile 与 client ID，在 checkout 外注册项目，启动唯一的 detached
Runner，并等待同一个 key 确实能看到 Runner 与项目。成功后会直接输出 MCP URL、
profile、安全的配置/日志路径和生成的 key。

按命令输出配置 MCP client：

```text
MCP URL: https://sg4.yyjeqhc.cn/mcp
Authentication: Bearer token
Token: 命令生成的 MCP key
```

自动生成的 key 只在首次创建时完整显示，请立即复制。输出会给出 owner-only
`agent.toml` 路径，但 status 与日志命令刻意不会再次显示 key。不要把该文件或 key
提交进 Git。高级用户可以使用 `--key-file <path>` 或 `--key`；只有接入非当前目录
时才需要 `--project`。

关闭终端不会停止 detached Runner，但机器重启会终止它。重启后可以重新执行同一条
`webcodex connect`，也可以使用首次输出的 profile 运行
`webcodex agent start --profile <profile>`。重复执行 `connect` 会复用已有 profile、
身份、项目记录和运行中的 Runner，不会重复创建。Hosted、managed、自托管的选择见
[AI 接入指南](docs/AI_ONBOARDING.zh-CN.md)。

## 先选部署方式

| 目标 | 推荐路径 |
| --- | --- |
| 立即把一个本地项目接入 ChatGPT/Claude | 使用官方 hosted Server 和 `webcodex connect`。 |
| 需要独立用户身份、设备授权、撤销或审计 | 使用 managed `webcodex login` 流程。 |
| 需要完全控制基础设施与身份系统 | 阅读 [DEPLOYMENT.zh-CN.md](docs/DEPLOYMENT.zh-CN.md) 走完整自托管。 |
| 只在本机 loopback 使用 | 使用下面三条本地项目命令。 |

0.3.1 的 package 路径支持 Linux x64、Linux arm64 和 macOS arm64。Linux 上的完整
自托管仍要求 systemd、`sudo` 以及 HTTPS 域名或可信隧道；hosted `connect` 路径不需要这些运维
步骤。

## 一个项目，一个入口

在希望开放的 Git 项目中运行：

```bash
webcodex setup
webcodex doctor
webcodex agent start
```

`setup` 在 checkout 外创建最小私有状态。它不会修改 Git 内容、启动后台服务、
开放端口、修改 shell 启动文件或发送项目文件。再次运行是安全的：已有有效配置
保持不变，只修复缺失部分；冲突状态会 fail closed。私有状态包含一个由本项目
Connector 与 Agent 共用的精确 Project Credential；默认输出不会打印它，任意
Bearer token 不能替代它。

`doctor` 只读。刚完成 setup 时，它通常会报告本地 Agent 尚未启动，并给出唯一
下一条命令。

`agent start` 是显式 foreground action，会启动绑定当前项目的 loopback runtime
和 Agent。保持该终端运行，在另一个终端执行：

```bash
webcodex status
```

默认输出只使用 Project、Connection、Agent、Capabilities、readiness 和 next
action 等产品概念，不打印 credentials、client ID、runtime project ID、executor
reference、workflow session 或 transport 细节。

完整步骤见 [docs/QUICK_START.zh-CN.md](docs/QUICK_START.zh-CN.md)。

## 接入托管窗口

runtime 监听 loopback；托管 ChatGPT/Claude 需要公网 HTTPS 地址。任何你信任的
隧道都可以：

```bash
cloudflared tunnel --url http://127.0.0.1:8080
```

打开 `https://<tunnel-host>/console`，使用 **Connect a chat client** 面板：它会
渲染 Claude 自定义连接器（Settings → Connectors → Add custom connector）要粘贴
的 MCP URL，以及 ChatGPT GPT Action（Create a GPT → Actions → Import from URL）
要导入的 schema URL，均带复制按钮。认证始终使用 setup 生成的 Project
Credential——console 永远不会显示它。需要在 schema 里固定公网地址时设置
`WEBCODEX_PUBLIC_URL`。

WebCodex 当前通过 OpenAPI **Custom GPT Action**（也称 GPT Actions）接入
ChatGPT；本文不声称 WebCodex 已发布为 ChatGPT 官方插件。当前 ChatGPT/Codex 的
插件目录提供可安装的 bundle，可组合 app、skill、connector 和 MCP server；app、
Custom GPT、Action 又是不同层次。参见 OpenAI 的
[GPT Actions 介绍](https://developers.openai.com/api/docs/actions/introduction)、
[插件文档](https://learn.chatgpt.com/docs/plugins)以及
[WebCodex GPT Actions 指南](docs/GPT_ACTIONS.zh-CN.md)。

## 长期自托管

长期运行时按下面顺序操作：

1. 在 Linux server 和每台持有代码仓库的机器上安装三个 binaries。
2. 初始化 server env，并安装 `webcodex` systemd unit。
3. 用 Nginx、Caddy 或其他可信 reverse proxy 代理 loopback server，启用 HTTPS，
   并设置 `WEBCODEX_PUBLIC_URL`。
4. 在 server 上创建短期 pairing code；每台代码机器用该 code enroll，不复制长期
   credential。
5. 在每台代码机器上安装 `webcodex-runner` service，最后执行
   `webcodex ops status --strict`。

完整命令和可回滚的 credential 规则见
[docs/DEPLOYMENT.zh-CN.md](docs/DEPLOYMENT.zh-CN.md)。OAuth2 是之后可选的委托登录
能力；首次单 operator 部署使用 PAT 认证即可。

## Canonical coding path

配置完成的 MCP/OpenAPI Connector 只暴露十二项项目级能力：

```text
task_start
→ files_list
→ files_read / files_search
→ edits_apply
→ checks_run
→ task_finish
→ task_review
→ task_cancel（需要时）
```

聊天会话会过期，任务不会。新会话先用 `task_list` 找到可继续的工作，再用
`task_resume` 拿到单个任务的紧凑 bootstrap——目标、状态、已应用路径，以及评审者
留下的 guidance（包括拒绝理由）。

Connector context 确定性解析项目。普通 coding 不需要先调用 `list_projects`、
`runtime_status`、`tool_manifest`、`start_session`、`current_session`、Agent listing
或 project registration tools。

普通可写任务必须在 `task_finish` 前运行 structured checks。结果保持隔离，直到
人类在本机 review 并接受。同一套本机人类授权有两条入口——离线 CLI 与浏览器
console——两者共用同一条 accept/reject 决策路径：

```bash
webcodex task list
webcodex task show <task-id>
webcodex task accept <task-id>
```

在浏览器中，`/console` 展示可执行的工作队列与带 bounded diff 的 review 详情，
其 Accept / Reject / Cancel 按钮驱动同一套授权。Hosted Chat 只能提议工作、永远
无法接受；server 在应用前会重新校验 target checkout 与 result，因此浏览器上的一次
点击无法绕过持久化的前置条件。

task、operation、execution 和 result ID 会保留，因为它们用于精确 retry、进度、
review 和 accept；executor routing 和 queue ID 保持内部实现。

### Project-aware validation

`checks_run` 仍是十二项 capability 之一。省略可选 `recipe` 时，从 Task execution
workspace 的相对 `cwd` 开始解析最近的 supported manifest；仅在同目录歧义时显式
提供 `recipe: rust|node|python|go`。解析不会扫描 sibling project，绝对路径、
父目录穿越和 symlink escape 都 fail closed。

Rust 支持 `format/check/test`；Node 只选择固定的非修改型 script name；Python
选择已有配置证明的 Ruff/Black、Ruff/Mypy 和 pytest；Go 支持 `check/test`，
`format` 有意返回 unavailable。recipe 不安装依赖、不生成配置、不修改 lockfile、
不联网。工具缺失属于 executor failure；进程成功启动后的 non-zero 才属于
assertion failure。resolved recipe version、相对 root、manifest/lock evidence 和
structured invocation 都进入 `operation_id` 的 exact-retry identity。详见
[docs/QUICK_START.zh-CN.md](docs/QUICK_START.zh-CN.md#project-aware-validation-recipes)。

## Readiness

`webcodex status` 快速回答“当前项目现在能不能工作”；`webcodex doctor` 提供
结构化、可执行的诊断，覆盖本地配置、认证材料是否存在、项目注册、Git/workspace、
Agent runtime、server reachability、Agent registration、必要 capability 和
structured validation。

Browser `/console` 投影同一组 readiness facts，并新增本机人类 review 界面：可执行
工作队列、带 bounded diff 与 output tail 的 review 详情，以及 Accept / Reject /
Cancel 操作。它不是第二套 status 逻辑，不是 model-facing capability，也不是 Browser
IDE——无法编辑代码、运行命令或启动任务。在其中输入的 project credential 仅保存在
内存中，绝不写入浏览器存储。

## Client 接入

canonical setup 默认只启动 loopback，不创建公网 ingress。本地 client 可以使用
经过批准的本地连接配置及其精确 Project Credential 访问 project-bound
Connector。Loopback 是网络边界，不是认证豁免；未知 credential 会在 readiness、
Task state 或 Agent dispatch 之前被拒绝。ChatGPT 托管 client 需要 operator
管理的 HTTPS endpoint 和认证；setup 不创建 tunnel、不开放公网端口，也不重设
production auth。详见 [docs/DEPLOYMENT.zh-CN.md](docs/DEPLOYMENT.zh-CN.md)、
[docs/MCP.zh-CN.md](docs/MCP.zh-CN.md) 和
[docs/GPT_ACTIONS.zh-CN.md](docs/GPT_ACTIONS.zh-CN.md)。

legacy ToolRuntime discovery/operations tools 继续供管理和诊断使用，但不再是普通
项目 coding path 的前置步骤。

## 安全边界

- setup 只注册 Git 明确解析出的 root，不根据目录同名或最近使用记录猜测。
- project setup 使用精确 credential verifier，不进入普通 arbitrary-key
  quick-start fallback；Connector 与 Agent 必须映射到同一个非秘密 project grant。
- 显式 project binding 按 principal 隔离，并在协议需要时按 transport 隔离；
  ambiguity 会 fail closed。
- read-only task 拒绝 mutation、shell 和 job-like action。
- 优先使用结构化 edit 和 validation，而不是 raw shell。
- validation command 无法 spawn 时属于 executor failure，不是项目 assertion
  failure。
- token、Authorization header、hash、private key 和 secret path 不得出现在
  prompt、日志、示例或提交的配置中。

完整边界见 [SECURITY.md](SECURITY.md) 和
[docs/CONCEPTS.zh-CN.md](docs/CONCEPTS.zh-CN.md)。

## 范围

WebCodex 同时支持官方托管协调 Server 和完整自托管部署。用户代码与实际执行默认
仍留在用户控制的 Runner 机器上，除非用户明确将它们部署到其他位置。高级
multi-client enrollment、production OAuth、remote deployment、QUIC、shell profile
和 operator observability 继续通过管理文档和 `webcodex` 提供，但不会改变上面的
普通项目入口。

## 文档

- 完整文档索引：[docs/INDEX.zh-CN.md](docs/INDEX.zh-CN.md)
- 快速开始：[docs/QUICK_START.zh-CN.md](docs/QUICK_START.zh-CN.md)
- 构建安装：[docs/BUILD_INSTALL.zh-CN.md](docs/BUILD_INSTALL.zh-CN.md)
- 概念：[docs/CONCEPTS.zh-CN.md](docs/CONCEPTS.zh-CN.md)
- MCP：[docs/MCP.zh-CN.md](docs/MCP.zh-CN.md)
- GPT Actions：[docs/GPT_ACTIONS.zh-CN.md](docs/GPT_ACTIONS.zh-CN.md)
- 部署：[docs/DEPLOYMENT.zh-CN.md](docs/DEPLOYMENT.zh-CN.md)
- Roadmap：[docs/ROADMAP.zh-CN.md](docs/ROADMAP.zh-CN.md)

## 免责声明

WebCodex 仅用于研究与学习。它能够在配置的项目边界内读取、修改文件并执行命令；
请只在可通过版本控制或备份恢复的仓库中使用。若因使用本软件造成文件系统损坏、
数据丢失或其他后果，作者概不负责。

## 鸣谢

感谢 [LINUX DO](https://linux.do/) 社区提供的交流氛围与开源推广支持。

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
