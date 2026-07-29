# Build and Install Quick Reference

[English](BUILD_INSTALL.md) | [简体中文](BUILD_INSTALL.zh-CN.md)

This is the short install path. See [DEPLOYMENT.md](DEPLOYMENT.md) for production details.

## Build binaries

Build the three current binaries for your host:

```text
webcodex
webcodex-server
webcodex-runner
```

`webcodex-runner` runs shell commands the server sends rather than an agent
loop. The binary, npm command, systemd unit, and QUIC ALPN
(`webcodex-runner/1`) use that name without old-name aliases.

Do not run unauthenticated production deployments.

## Help-verified command shape

The examples in this guide were checked against the current help output from `webcodex -h`, `webcodex server -h`, and `webcodex agent -h`. Keep these flag differences in mind:

| Task | Preferred command shape |
| --- | --- |
| Ordinary project onboarding | `webcodex setup` |
| Project diagnostics/readiness | `webcodex doctor` / `webcodex status` |
| Server env bootstrap | `webcodex server init --listen ... --data-dir ... --env-file ...` |
| Server systemd unit | `webcodex server install --env-file ... --bin ...` |
| Server status | `webcodex server status --env-file ...` |
| Admin-created account credential | `webcodex users create --server-url ... --token ... --username ... --issue-credential` |
| User-created PAT | `webcodex token create-local --server ... --user ... --credential ... --scopes ...` |
| User-created agent token | `webcodex agent-token create-local --server ... --user ... --credential ... --client-id ...` |
| Pairing code | `webcodex pairing create --server-url ... --username ... --client-id ...` |
| Client enrollment | `webcodex client enroll --server-url ... --pairing-code ... --client-id ...` |
| Agent foreground run | `webcodex-runner --profile ...` |
| Agent service | `webcodex agent install --profile ... --bin ...` |

The account-management command uses `users create` and `--server-url`; local token creation commands use `--server`. That difference comes from the current CLI surface and is intentionally reflected in the examples.

## Install packages

The documented distribution path uses the npm thin installer/wrapper:

```bash
npm install -g @yyjeqhc/webcodex
```
The v0.3.0 npm wrapper is prepared for `linux-x64` only. `linux-arm64`, `darwin-arm64`, `darwin-x64`, Windows, and other targets are not included in v0.3.0 unless matching artifacts are added before release. Do not publish the npm package until the v0.3.0 GitHub Release artifact exists and `npm/webcodex/manifest.json` contains the SHA-256 checksum of that exact uploaded tarball.

The npm package is a thin wrapper around native release artifacts. During install it downloads the matching GitHub Release artifact and verifies the SHA-256 checksum from the manifest. Before publishing, run the local package smoke without publishing:

```bash
bash scripts/npm_package_smoke.sh
```

## Example files

The `deploy/` directory contains short examples you can adapt:

- `deploy/webcodex.env.example`
- `deploy/webcodex.service.example`
- `deploy/webcodex-runner.toml.example`
- `deploy/webcodex-runner.service.example`
- `deploy/nginx.webcodex.example.conf`

The nginx file is only an example. WebCodex CLI does not automate reverse proxy setup.

## Binary deployment flow

Server:

1. Install the public `webcodex` CLI and the `webcodex-server` binary.
2. Initialize the server env file:

```bash
sudo webcodex server init \
  --listen 127.0.0.1:8080 \
  --data-dir /var/lib/webcodex \
  --env-file /etc/webcodex/webcodex.env
```

This creates only the server bootstrap/admin `WEBCODEX_TOKEN` in `/etc/webcodex/webcodex.env`. That file is server-side only; it does not create user API tokens or agent tokens.

3. Install the server service. Use `--overwrite` only when replacing an old unit.

```bash
sudo webcodex server install \
  --env-file /etc/webcodex/webcodex.env \
  --bin /usr/local/bin/webcodex-server
```

4. Reload systemd, start the service, and check status:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now webcodex
webcodex server status --env-file /etc/webcodex/webcodex.env
```

Server/admin:

5. Create a temporary one-time pairing code:

```bash
webcodex pairing create \
  --server-url https://your-domain.example \
  --env-file /etc/webcodex/webcodex.env \
  --username friendname \
  --client-id friend-laptop \
  --display-name "Friend Name" \
  --ttl-secs 600
```

`pairing create` is a server/admin-side command. It needs server bootstrap/admin auth. Copy only the short-lived `wc_pair_*` code to the client; do not copy `WEBCODEX_TOKEN`, `wc_pat_*`, `wc_agent_*`, complete env files, or complete `agent.toml` files. Each friend should use a unique `username` and `client_id`.

Client:

6. Install the public `webcodex` CLI and the `webcodex-runner` binary.
7. Exchange the pairing code over HTTPS and write client-side credentials/config:

```bash
sudo webcodex client enroll \
  --server-url https://your-domain.example \
  --pairing-code <wc_pair_...> \
  --client-id friend-laptop \
  --profile workstation \
  --allowed-root /home/friend/git
```

Client enroll creates the `wc_pat_*` user token, `wc_agent_*` agent token, and `/etc/webcodex/clients/workstation/agent.toml` locally with `0600` permissions on Unix. `/etc/webcodex/webcodex.env` is server-side only; isolate client-side token/config files under `/etc/webcodex/clients/<profile>/` when multiple users or clients share one machine.

8. Install and start the agent service, then validate:

```bash
sudo webcodex agent install \
  --profile workstation \
  --bin /opt/webcodex/bin/webcodex-runner \
  --overwrite
sudo systemctl daemon-reload
sudo systemctl enable --now webcodex-runner-workstation
webcodex agent status \
  --profile workstation \
  --server-url https://your-domain.example
webcodex ops status \
  --server-url https://your-domain.example \
  --token-file /etc/webcodex/clients/workstation/webcodex-user-token
```

GPT Actions should use the generated client-side user-token file. GPT Actions require a public HTTPS URL; WebCodex CLI does not automate reverse proxies or tunnels.

Compatibility commands still work, but should not be the first choice in new docs:

```bash
webcodex users ...
webcodex tokens ...
webcodex agent-tokens ...
webcodex setup single-user
```

## Agent config

Client enroll writes `agent.toml`. For a systemd service, use `webcodex agent install`; for a foreground test, run:

```bash
webcodex-runner --profile workstation
```

For advanced manual generation, use the single low-level entry
`webcodex agent init`. The `webcodex-runner init` alias was removed.

## Project readiness

For an ordinary Git project, use the canonical read-only diagnostics:

```bash
webcodex setup
webcodex doctor
webcodex agent start
webcodex status
```

`doctor` checks the current project configuration, registration, Git
workspace, Agent runtime, connection, Agent registration, required coding
capabilities, and structured validation without modifying state.

For an advanced multi-client deployment, keep project readiness separate from
operator fleet diagnostics:

```bash
webcodex agent status \
  --profile workstation \
  --server-url https://your-domain.example
webcodex ops status \
  --server-url https://your-domain.example \
  --token-file /etc/webcodex/clients/workstation/webcodex-user-token
```

These commands never make transport/fleet discovery a prerequisite for the
ordinary Connector coding path. See [SHELL_PROFILES.md](SHELL_PROFILES.md) for
advanced profile config and troubleshooting.

Agent policy defaults:

- Missing or empty `allowed_roots` defaults to `$HOME`.
- Explicit `allowed_roots` replaces the `$HOME` default.
- To narrow an agent, set an explicit workspace root such as:

```toml
[policy]
allowed_roots = ["/root/git"]
```

The example above is a narrowing example, not the default.

## Auth reminders

Use:

```text
Authorization: Bearer <token>
```

for REST, polling, MCP, and GPT Actions.

`?token=` is allowed only for `/api/agents/ws` WebSocket handshake compatibility.

## systemd PATH reminder

systemd services do not read interactive shell startup files such as `~/.bashrc`. If commands need Rust/Cargo, Node, or Codex CLI, expose them through configured agent shell profiles or through the service manager's environment.

WebCodex no longer exposes `run_codex` or legacy `/api/codex/*` routes. Run Codex outside WebCodex for Codex-specific workflows.
