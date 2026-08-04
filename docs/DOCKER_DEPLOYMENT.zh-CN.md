# WebCodex Server-only Docker Compose 部署

[English](DOCKER_DEPLOYMENT.md) | [简体中文](DOCKER_DEPLOYMENT.zh-CN.md)

这套部署只在 Docker 中运行 WebCodex 协调 Server，不运行 Runner、不挂载项目仓库，
也不包含语言工具链。

```text
Internet
   │ HTTPS / WebSocket
   ▼
反向代理
   │ http://127.0.0.1:8080
   ▼
webcodex-server 容器 ── 持久化 SQLite volume

代码所在机器
   └── webcodex-runner ── HTTPS / WebSocket ──▶ 公网 WebCodex 地址
```

## 容器包含什么

- `webcodex-server`
- 管理用 `webcodex` CLI
- `/var/lib/webcodex` 下的持久化数据卷

容器有意不包含 `webcodex-runner`、项目仓库、Git credential 和项目工具链。Runner
应运行在真正持有代码的工作站或服务器上。

## 1. 启动 Server

在仓库根目录执行：

```bash
./deploy/docker/bootstrap.sh https://webcodex.example.com
```

脚本会创建私有 `.env`、生成随机 Bootstrap Token，并执行：

```bash
docker compose up -d --build
```

默认只绑定：

```text
127.0.0.1:8080
```

该端口不会直接暴露到公网。

也可以手动初始化：

```bash
cp .env.compose.example .env
chmod 600 .env
# 在 .env 中设置 WEBCODEX_PUBLIC_URL 和 WEBCODEX_TOKEN
docker compose up -d --build
```

当前 Compose 会从 checkout 源码构建镜像。以后可以单独发布 GHCR 或 Docker Hub
镜像，再使用固定 tag 或 digest 替换构建来源；这不会改变 server-only 架构。

## 2. 配置 HTTPS

让现有反向代理转发到：

```text
http://127.0.0.1:8080
```

反向代理需要：

- 对外提供 HTTPS；
- 保留 `Host` 和 `X-Forwarded-Proto`；
- 支持 WebSocket Upgrade；
- 为 `/api/agents/ws` 保留适合长连接的 timeout。

最小 Nginx 示例：

```nginx
server {
    listen 443 ssl http2;
    server_name webcodex.example.com;

    # 在这里加入证书配置。

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
        proxy_buffering off;
    }
}
```

如果反向代理也运行在 Docker 中，不要使用代理容器自己的 `127.0.0.1`。应让两个
容器加入共享网络，或者让代理访问宿主机映射端口。

## 3. 检查 Server

```bash
docker compose ps
docker compose logs -f webcodex
curl -fsS https://webcodex.example.com/openapi.json >/dev/null
```

常用入口：

```text
https://webcodex.example.com/console
https://webcodex.example.com/openapi.json
https://webcodex.example.com/mcp
```

## 4. 创建 pairing code

```bash
docker compose exec webcodex sh -lc \
  'webcodex pairing create \
    --server-url "$WEBCODEX_PUBLIC_URL" \
    --username admin \
    --ttl-secs 600'
```

只把短期有效的 `wc_pair_...` code 发送到代码机器。不要复制 Server bootstrap
`WEBCODEX_TOKEN`。

## 5. 接入代码机器

在真正持有仓库的机器安装 WebCodex：

```bash
npm install -g @yyjeqhc/webcodex
```

使用 pairing code 登录，并限制允许访问的代码根目录：

```bash
webcodex login https://webcodex.example.com \
  --code '<wc_pair_...>' \
  --allowed-root "$HOME/git"
```

使用 `login` 输出的 Agent config 路径：

```bash
webcodex agent install --scope user \
  --config /path/reported/by/login/agent.toml
webcodex agent status --scope user \
  --config /path/reported/by/login/agent.toml
```

普通用户安装 npm package 和 user systemd service 都不需要 `sudo`。管理员管理的
system service 必须显式指定非 root Runner 用户与 working directory。完整规则见
[构建与安装](BUILD_INSTALL.zh-CN.md)。

## 常用管理命令

```bash
# 状态
docker compose ps

# 日志
docker compose logs -f webcodex

# 重启
docker compose restart webcodex

# 更新 checkout 后重新构建
docker compose up -d --build

# 停止并保留数据
docker compose down

# 删除持久化数据卷；破坏性操作
docker compose down -v
```

OAuth、systemd 部署、备份和生产运维见
[部署指南](DEPLOYMENT.zh-CN.md)。
