# WebCodex Server-only Docker Compose Deployment

[English](DOCKER_DEPLOYMENT.md) | [简体中文](DOCKER_DEPLOYMENT.zh-CN.md)

This deployment runs the WebCodex coordination Server in Docker. It does not
run the Runner, mount project repositories, or include language toolchains.

```text
Internet
   │ HTTPS / WebSocket
   ▼
Reverse proxy
   │ http://127.0.0.1:8080
   ▼
webcodex-server container ── persistent SQLite volume

Repository machine
   └── webcodex-runner ── HTTPS / WebSocket ──▶ public WebCodex URL
```

## What the container includes

- `webcodex-server`
- the administrative `webcodex` CLI
- a persistent data volume under `/var/lib/webcodex`

It intentionally excludes `webcodex-runner`, repositories, Git credentials,
and project toolchains. Run the Runner on the workstation or server that
actually owns the code.

## 1. Start the Server

From the repository root:

```bash
./deploy/docker/bootstrap.sh https://webcodex.example.com
```

The script creates a private `.env`, generates a random bootstrap token, and
runs:

```bash
docker compose up -d --build
```

The default host binding is:

```text
127.0.0.1:8080
```

The port is not exposed directly to the Internet.

Manual setup is also available:

```bash
cp .env.compose.example .env
chmod 600 .env
# Set WEBCODEX_PUBLIC_URL and WEBCODEX_TOKEN in .env
docker compose up -d --build
```

The current Compose file builds the image from the checked-out source. A GHCR
or Docker Hub image can be published later and substituted with a fixed tag or
digest; that does not change the server-only architecture.

## 2. Configure HTTPS

Point an existing reverse proxy at:

```text
http://127.0.0.1:8080
```

The proxy must:

- serve the public endpoint over HTTPS;
- preserve `Host` and `X-Forwarded-Proto`;
- support WebSocket upgrades;
- allow long-lived connections on `/api/agents/ws`.

Minimal Nginx example:

```nginx
server {
    listen 443 ssl http2;
    server_name webcodex.example.com;

    # Add your certificate configuration here.

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

When the reverse proxy also runs in Docker, do not use the proxy container's
own `127.0.0.1`. Put both containers on a shared network or route to the host
port.

## 3. Verify the Server

```bash
docker compose ps
docker compose logs -f webcodex
curl -fsS https://webcodex.example.com/openapi.json >/dev/null
```

Useful endpoints:

```text
https://webcodex.example.com/console
https://webcodex.example.com/openapi.json
https://webcodex.example.com/mcp
```

## 4. Create a pairing code

```bash
docker compose exec webcodex sh -lc \
  'webcodex pairing create \
    --server-url "$WEBCODEX_PUBLIC_URL" \
    --username admin \
    --ttl-secs 600'
```

Send only the short-lived `wc_pair_...` code to the repository machine. Do not
copy the Server bootstrap `WEBCODEX_TOKEN`.

## 5. Connect a repository machine

Install WebCodex on the machine that owns the repositories:

```bash
npm install -g @yyjeqhc/webcodex
```

Enroll it and restrict the allowed code root:

```bash
webcodex login https://webcodex.example.com \
  --code '<wc_pair_...>' \
  --allowed-root "$HOME/git"
```

Use the Agent config path printed by `login`:

```bash
webcodex agent install --scope user \
  --config /path/reported/by/login/agent.toml
webcodex agent status --scope user \
  --config /path/reported/by/login/agent.toml
```

An ordinary user can install the npm package and user systemd service without
`sudo`. For an administrator-managed system service, use an explicit non-root
Runner account and working directory. See [Build and Install](BUILD_INSTALL.md)
for the complete service-scope rules.

## Common operations

```bash
# Status
docker compose ps

# Logs
docker compose logs -f webcodex

# Restart
docker compose restart webcodex

# Rebuild after updating the checked-out source
docker compose up -d --build

# Stop and keep data
docker compose down

# Delete the persistent data volume; destructive
docker compose down -v
```

For OAuth, systemd deployment, backups, and production operations, see
[Deployment](DEPLOYMENT.md).
