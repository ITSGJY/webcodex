# Build and Install Quick Reference

[English](BUILD_INSTALL.md) | [简体中文](BUILD_INSTALL.zh-CN.md)

这是构建和安装的快速参考。生产部署细节见 [DEPLOYMENT.md](DEPLOYMENT.md) / [DEPLOYMENT.zh-CN.md](DEPLOYMENT.zh-CN.md)。

## 最快 hosted 安装

官方 hosted shared-key 路径只需在持有项目的机器上安装 CLI/Runner package，并
执行一条命令：

```bash
npm install -g @yyjeqhc/webcodex
cd /path/to/your/repository
webcodex connect https://sg4.yyjeqhc.cn
```

当前目录就是默认项目。`connect` 会自动生成强随机 key，并且只完整显示一次；随后
写入 owner-only profile、启动 detached Runner，并等待 Server 确实能看到 Runner
与项目。MCP client 使用命令输出的 `https://sg4.yyjeqhc.cn/mcp` URL 和 key。这条
路径不需要本地 Server、数据库、反向代理、systemd unit 或 sudo。输出会给出
profile、配置路径和日志路径。机器重启后，重新执行同一条 `connect`，或运行
`webcodex agent start --profile <profile>`。

## 构建 binaries

为当前 host 构建三个 binaries：

```text
webcodex
webcodex-server
webcodex-runner
```

`webcodex-runner` 执行服务端下发的 shell 命令，并不是一个 agent 循环。
binary、npm 命令、systemd unit 与 QUIC ALPN（`webcodex-runner/1`）统一使用
这个名称，不保留旧名称 alias。

不要运行 unauthenticated production deployments。

## 已按二进制 help 校验的命令形态

本文档中的示例已对照当前 `webcodex -h`、`webcodex server -h` 和 `webcodex agent -h` 的输出检查。需要特别注意这些 flag 差异：

| 任务 | 推荐命令形态 |
| --- | --- |
| Hosted 项目连接 | `webcodex connect <server-url> [--key ...] --project ...` |
| 普通项目 onboarding | `webcodex setup` |
| 项目诊断/readiness | `webcodex doctor` / `webcodex status` |
| 初始化服务器 env | `webcodex server init --listen ... --data-dir ... --env-file ...` |
| 安装服务器 systemd unit | `webcodex server install --env-file ... --bin ...` |
| 检查服务器状态 | `webcodex server status --env-file ...` |
| 管理员创建账户凭据 | `webcodex users create --server-url ... --token ... --username ... --issue-credential` |
| 用户创建 PAT | `webcodex token create-local --server ... --user ... --credential ... --scopes ...` |
| 用户创建 agent token | `webcodex agent-token create-local --server ... --user ... --credential ... --client-id ...` |
| 创建 pairing code | `webcodex pairing create --server-url ... --username ... [--client-id ...]` |
| 客户端 enrollment（主入口） | `webcodex login <server-url> --code <pairing-code>` |
| 客户端 enrollment（高级） | `webcodex client enroll --server-url ... --pairing-code ... --client-id ...` |
| 前台运行 agent | `webcodex-runner --profile ...` |
| 安装 agent service | `webcodex agent install --profile ... --bin ...` |

账户管理命令使用 `users create` 和 `--server-url`；本地 token 创建命令使用 `--server`。这是当前 CLI surface 的实际差异，示例中会按这个差异书写。

## 安装 packages

推荐分发路径是 npm thin installer/wrapper：

```bash
npm install -g @yyjeqhc/webcodex
```

v0.3.1 npm wrapper 支持 `linux-x64`、`linux-arm64` 和 `darwin-arm64`。本次发布不包含 `darwin-x64`、Windows 和其他 targets。三个 v0.3.1 GitHub Release artifacts 都存在，并且 `npm/webcodex/manifest.json` 写入每个实际上传 tarball 的 SHA-256 checksum 后，才能发布 npm package。

npm package 是 native release artifacts 的 thin wrapper。安装时会下载匹配的 GitHub Release artifact，并使用 manifest 中的 SHA-256 checksum 验证。发布前先运行本地 package smoke；它不会发布：

```bash
bash scripts/npm_package_smoke.sh
```

## 示例文件

`deploy/` 目录包含可改造的短示例：

- `deploy/webcodex.env.example`
- `deploy/webcodex.service.example`
- `deploy/webcodex-runner.toml.example`
- `deploy/webcodex-runner.service.example`
- `deploy/nginx.webcodex.example.conf`

nginx 文件只是示例。WebCodex CLI 不会自动配置 reverse proxy。

## Binary deployment flow

本节剩余流程用于完整自托管；使用官方 hosted `connect` 路径时不需要执行。

Server：

1. 安装公开 `webcodex` CLI 和 `webcodex-server` binary。
2. 初始化 server env file：

```bash
sudo webcodex server init \
  --listen 127.0.0.1:8080 \
  --data-dir /var/lib/webcodex \
  --env-file /etc/webcodex/webcodex.env
```

这只会在 `/etc/webcodex/webcodex.env` 中创建 server bootstrap/admin `WEBCODEX_TOKEN`。该文件只属于 server-side，不会创建 user API tokens 或 agent tokens。

3. 安装 server service。只有替换旧 unit 时才使用 `--overwrite`。

```bash
sudo webcodex server install \
  --env-file /etc/webcodex/webcodex.env \
  --bin /usr/local/bin/webcodex-server
```

4. Reload systemd，启动 service 并检查状态：

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now webcodex
webcodex server status --env-file /etc/webcodex/webcodex.env
```

Server/admin：

5. 创建短期一次性 pairing code：

```bash
webcodex pairing create \
  --server-url https://your-domain.example \
  --env-file /etc/webcodex/webcodex.env \
  --username friendname \
  --display-name "Friend Name" \
  --ttl-secs 600
```

`pairing create` 是 server/admin-side 命令。这个普通流程创建未绑定 code，由执行 `login` 的设备使用自动生成的 id 认领。只复制短期 `wc_pair_*` code 给 client；不要复制 `WEBCODEX_TOKEN`、`wc_pat_*`、`wc_agent_*`、完整 env files 或完整 `agent.toml` files。

Client：

6. 安装公开 `webcodex` CLI 和 `webcodex-runner` binary。
7. 通过 HTTPS 交换 pairing code，并写入 client-side credentials/config：

```bash
sudo webcodex login https://your-domain.example --code <wc_pair_...> \
  --allowed-root /home/friend/git
```

`login` 会自动生成唯一设备名（hostname + 本地后缀），兑换 pairing code，把 `wc_pat_*` user token 写入 `webcodex-user-token`，并把 `wc_agent_*` agent token 存入生成的 `agent.toml`；两个文件在 Unix 上均使用 `0600` 权限。`/etc/webcodex/webcodex.env` 只属于 server 侧。如需显式 client id 或自定义输出目录，高级的 `webcodex client enroll` 流程仍支持原有参数。

8. 安装并启动 agent service，然后验证：

```bash
sudo webcodex agent install --config /path/to/login/wrote/agent.toml --overwrite
sudo systemctl daemon-reload
sudo systemctl enable --now webcodex-runner
webcodex agent status --server-url https://your-domain.example
webcodex ops status \
  --server-url https://your-domain.example \
  --token-file /path/to/login/wrote/webcodex-user-token
```

GPT Actions 应使用生成的 client-side user-token file。GPT Actions 需要 public HTTPS URL；WebCodex CLI 不会自动配置 reverse proxies 或 tunnels。

## Agent config

`login` / `client enroll` 会写入 `agent.toml`。systemd service 使用 `webcodex agent install --config <path>`；前台测试可运行：

```bash
webcodex-runner --config /path/to/login/wrote/agent.toml
```

高级手工生成只保留低层入口 `webcodex agent init`；
`webcodex-runner init` alias 已删除。

## 项目 readiness

普通 Git 项目使用 canonical 只读诊断：

```bash
webcodex setup
webcodex doctor
webcodex agent start
webcodex status
```

`doctor` 检查当前项目 config、registration、Git workspace、Agent runtime、
connection、Agent registration、必要 coding capability 和 structured validation，
且不修改 state。

高级 multi-client deployment 将项目 readiness 与 operator fleet diagnostics 分开：

```bash
webcodex agent status \
  --profile workstation \
  --server-url https://your-domain.example
webcodex ops status \
  --server-url https://your-domain.example \
  --token-file /etc/webcodex/clients/workstation/webcodex-user-token
```

这些命令不会让 transport/fleet discovery 重新成为普通 Connector coding path 的
前置步骤。高级 profile 配置和排障见
[SHELL_PROFILES.zh-CN.md](SHELL_PROFILES.zh-CN.md)。

## Auth reminders

REST、polling、MCP 和 GPT Actions 使用：

```text
Authorization: Bearer <token>
```

`?token=` 只允许用于 `/api/agents/ws` WebSocket handshake 兼容场景。

## systemd PATH reminder

systemd services 不读取交互式 shell 启动文件，例如 `~/.bashrc`。如果命令需要 Rust/Cargo、Node 或 Codex CLI，请通过 agent shell profiles 或 service manager environment 暴露它们。

WebCodex 不再暴露 `run_codex` 或 legacy `/api/codex/*` routes。需要 Codex-specific workflows 时，请在 WebCodex 外部运行 Codex。
