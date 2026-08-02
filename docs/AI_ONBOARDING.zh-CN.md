# AI Coding Agent 接入指南

[English](AI_ONBOARDING.md) | [简体中文](AI_ONBOARDING.zh-CN.md)

本文面向帮助用户把本地仓库接入 WebCodex 的 AI coding agent。执行命令前，先选择
一条路径。

## 决策树

1. 用户是否希望用最低成本接入一个或多个本地仓库，并且不运维 Server？
   - 是：选择 **Hosted shared key**，运行 `webcodex connect`。
2. 用户是否需要独立用户身份、设备级授权、独立 token 撤销、身份审计或组织管理？
   - 是：选择 **Managed flow**，运行 `webcodex login`。
3. 用户是否需要完全控制基础设施、企业内网、自有 HTTPS、自有身份系统，或完全
   不依赖官方 Server？
   - 是：选择 **完整自托管**，阅读 [DEPLOYMENT.zh-CN.md](DEPLOYMENT.zh-CN.md)。

Hosted 路径不要部署 WebCodex Server。

## 最快接入：Hosted shared key

在持有仓库的机器上运行：

```bash
npm install -g @yyjeqhc/webcodex

webcodex connect https://sg4.yyjeqhc.cn \
  --key '<user-chosen-key>' \
  --project .
```

然后配置 MCP client：

```text
MCP URL: https://sg4.yyjeqhc.cn/mcp
Authentication: Bearer token
Bearer token: 完全相同的 key
```

去掉两端 key 首尾空白后，MCP 和 Runner 必须完全相同。请使用不以 `wc_` 开头的
随机值，不要提交进 Git。

`connect` 会完成整条本地链路：

- 规范化 Server origin；
- 按 origin 与 key 创建或复用 profile；
- 生成并持久化唯一 client ID；
- canonicalize 本地项目，并在 checkout 外注册；
- 写入 `0600` Runner 配置和项目边界 policy；
- 无需 sudo 或 systemd，启动唯一的 detached Runner；
- 等待同一个 key 确实能看到 Runner 与目标项目；
- 输出 MCP URL、runtime project ID、安全配置路径和日志路径。

重复执行相同命令会复用 profile、client ID、项目记录和运行中的 Runner。向同一个
profile 增加第二个项目时，会保留已有项目并扩展 allowed roots。

自动化场景优先使用 `--key-file <path>`，避免 key 进入 shell history。不能同时传
`--key` 和 `--key-file`。

## 自动 key

用户可以省略 key：

```bash
webcodex connect https://sg4.yyjeqhc.cn --project .
```

命令会生成 URL-safe 的 `wck_...` key，随机性超过 256 bits。完整 key 只在首次创建
时显示，并安全写入本地 profile。提醒用户立即复制到 MCP client。重复连接会找到
匹配的本地 profile，不再完整打印 key。

`wck_` 刻意不同于 managed credential 保留前缀 `wc_`。

## 正式 Managed flow

用户需要以下能力时使用 managed flow：

- 独立用户身份；
- token 撤销；
- 设备级授权；
- 身份审计；
- 组织管理。

入口：

```text
webcodex login
```

Managed Server 使用 pairing/account credential，MCP/API 使用 PAT，Runner 使用
独立绑定的 Agent token。不要用 shared key 破坏这层分离。

## 完整自托管

以下需求应阅读 [DEPLOYMENT.zh-CN.md](DEPLOYMENT.zh-CN.md)：

- 完全控制 Server 与数据；
- 企业内网部署；
- 自有 HTTPS endpoint；
- 自有身份系统；
- 不依赖官方 Server。

该路径包括 Server、数据库/state、反向代理、TLS、service 和 credential 运维。
Hosted `webcodex connect` 不需要这些前置条件。

## AI agent 必须遵守的 credential 规则

- 不要运行 `webcodex token generate` 后假设远程 Server 会接受。它只生成离线
  材料，不会完成远程注册。
- 不要使用 `wc_*` 作为 hosted shared key。未知或已撤销的 managed credential
  不会 fallback 成 shared key。
- 不要把 `wc_agent_*` 当作 MCP token。
- 不要把 bootstrap `WEBCODEX_TOKEN` 填进 MCP 或本地 hosted profile。
- 不要打印、记录、提交或复制完整 `agent.toml`。
- 先运行 `connect`，确认 Runner/项目完整链路，再配置 MCP。

## 本地 state 与故障排查

普通非 root 用户的 profile 配置默认位于
`~/.config/webcodex/clients/<profile>/`（或
`$XDG_CONFIG_HOME/webcodex/clients/<profile>/`）。Runner state 与日志默认位于
`~/.local/state/webcodex/clients/<profile>/`（或
`$XDG_STATE_HOME/webcodex/clients/<profile>/`）。

常用命令：

```bash
webcodex agent status --profile <profile>
webcodex agent logs --profile <profile> --lines 100
webcodex agent stop --profile <profile>
```

连接失败时使用 `connect` 输出的 profile 和日志路径。检查 Server reachable、
shared-key 是否启用、两端 key 是否完全相同、client ID 是否碰撞，以及 project
path 是否有效。Status 与日志不会打印 key。稳定排障步骤见
[TROUBLESHOOTING.zh-CN.md](TROUBLESHOOTING.zh-CN.md)。
