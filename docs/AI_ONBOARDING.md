# AI Coding Agent Onboarding

[English](AI_ONBOARDING.md) | [简体中文](AI_ONBOARDING.zh-CN.md)

This guide is for an AI coding agent helping a user connect a local repository
to WebCodex. Choose one path before running commands.

## Decision tree

1. Does the user want the fastest connection to one or more repositories,
   without operating a Server?
   - Yes: use **Hosted shared key** and `webcodex connect`.
2. Does the user need an individual account, device-level authorization,
   independent token revocation, identity audit, or organization management?
   - Yes: use the **Managed flow** and `webcodex login`.
3. Does the user need full infrastructure control, an internal network, their
   own HTTPS or identity system, or no dependency on the official Server?
   - Yes: use **Full self-hosting** and read [DEPLOYMENT.md](DEPLOYMENT.md).

Do not deploy a WebCodex Server for the hosted path.

## Fastest connection: hosted shared key

Run on the machine that owns the repository:

```bash
npm install -g @yyjeqhc/webcodex

webcodex connect https://sg4.yyjeqhc.cn \
  --key '<user-chosen-key>' \
  --project .
```

Then configure the MCP client:

```text
MCP URL: https://sg4.yyjeqhc.cn/mcp
Authentication: Bearer token
Bearer token: exactly the same key
```

The key must be identical on the MCP and Runner sides after surrounding
whitespace is trimmed. Use a random value that does not begin with `wc_`. Do
not commit it to Git.

`connect` performs the complete local setup:

- normalizes the Server origin;
- creates or reuses a profile scoped to that origin and key;
- generates and persists a unique client ID;
- canonicalizes and registers the local project outside the checkout;
- writes a `0600` Runner config with a project-bounded policy;
- starts one detached Runner without sudo or systemd;
- waits until the same key can see the Runner and target project;
- prints the MCP URL, runtime project ID, safe config path, and log path.

Running the same command again reuses the profile, client ID, project record,
and live Runner. Adding another project to the same profile preserves existing
projects and expands the allowed roots.

For automation, prefer `--key-file <path>` over putting a key in shell history.
Do not pass `--key` and `--key-file` together.

## Automatic key

The user may omit the key:

```bash
webcodex connect https://sg4.yyjeqhc.cn --project .
```

The command generates a `wck_...` URL-safe key with more than 256 bits of
randomness. It stores the key in the protected profile and prints the complete
value only when first created. Tell the user to copy it immediately into the
MCP client. A repeat connection recovers the matching local profile and does
not print the key again.

`wck_` is deliberately different from the reserved managed prefix `wc_`.

## Managed flow

Use the managed flow when the user needs:

- a separate user identity;
- token revocation;
- device-level authorization;
- identity audit;
- organization administration.

Start with:

```text
webcodex login
```

The managed Server flow uses pairing/account credentials, a PAT for MCP/API,
and a separately bound Agent token. Do not replace that split with a shared
key.

## Full self-hosting

Use [DEPLOYMENT.md](DEPLOYMENT.md) when the user needs:

- complete Server and data control;
- enterprise/internal-network deployment;
- their own HTTPS endpoint;
- their own identity system;
- no dependency on the official Server.

That path includes Server, database/state, reverse proxy, TLS, service, and
credential operations. None of those are prerequisites for hosted
`webcodex connect`.

## Credential rules for AI agents

- Never run `webcodex token generate` and assume a remote Server will accept
  its output. It creates offline material only; it does not register it.
- Never use a `wc_*` value as a hosted shared key. Unknown or revoked managed
  credentials do not fall back to shared-key auth.
- Never substitute a `wc_agent_*` for an MCP token.
- Never paste a bootstrap `WEBCODEX_TOKEN` into MCP or a local hosted profile.
- Never print, log, commit, or copy a full `agent.toml`.
- Run `connect` before configuring MCP, so the full Runner/project path is
  verified first.

## Local state and troubleshooting

For a normal non-root user, profile configuration defaults below
`~/.config/webcodex/clients/<profile>/` (or
`$XDG_CONFIG_HOME/webcodex/clients/<profile>/`). Runner state and logs default
below `~/.local/state/webcodex/clients/<profile>/` (or
`$XDG_STATE_HOME/webcodex/clients/<profile>/`).

Useful commands:

```bash
webcodex agent status --profile <profile>
webcodex agent logs --profile <profile> --lines 100
webcodex agent stop --profile <profile>
```

On connection failure, use the profile and log path printed by `connect`.
Check Server reachability, shared-key enablement, exact key equality, client ID
collision, and project-path validity. Status and logs do not print the key.
See [TROUBLESHOOTING.md](TROUBLESHOOTING.md) for stable failure guidance.
