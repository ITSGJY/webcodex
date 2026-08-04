# WebCodex

[English](README.md) | [简体中文](README.zh-CN.md)

[![CI](https://github.com/yyjeqhc/webcodex/actions/workflows/ci.yml/badge.svg)](https://github.com/yyjeqhc/webcodex/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/%40yyjeqhc%2Fwebcodex)](https://www.npmjs.com/package/@yyjeqhc/webcodex)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

[Latest release](https://github.com/yyjeqhc/webcodex/releases/latest) ·
[0.3.1 release notes](docs/RELEASE_NOTES_v0.3.1.md) ·
[Documentation](docs/INDEX.md)

**Let ChatGPT or Claude work on your private code — and nothing lands until
you review and accept it.** The AI edits and tests in an isolated workspace on
your machine; you see the diff and click Accept. That review gate is the whole
point: coding tools that let a model write straight to your repository cannot
retrofit it.

```
ChatGPT / Claude (web chat)
        │  MCP or GPT Actions over HTTPS
        ▼
WebCodex server ──▶ local Agent: edit → run checks → propose result
        │                      (isolated workspace, never your checkout)
        ▼
You: browser console or CLI ── review the diff ──▶ Accept ✓ / Reject ✗
        │  accept only
        ▼
Your repository
```

- **Human review gate** — results stay isolated until you accept them, from
  the `/console` web UI or `webcodex task accept`.
- **Everything stays on your machine** — source, Git, edits, and checks run
  on the host that owns the repository; the server exposes twelve bounded,
  audited capabilities instead of a raw shell.
- **Built for real work** — LSP navigation, structured edits with sha256
  guards, project-aware check recipes (Rust/Node/Python/Go), idempotent
  retries, and a full per-task event timeline.

| ChatGPT drives a task over MCP | Review and accept locally |
| --- | --- |
| ![MCP session](docs/assets/mcp-1.png) | ![GPT Action review](docs/assets/gpt-action-1.png) |

<details>
<summary>More screenshots</summary>

![MCP](docs/assets/mcp-2.png)
![MCP](docs/assets/mcp-3.png)
![MCP](docs/assets/mcp-4.png)
![GPT Actions](docs/assets/gpt-action-2.png)
![GPT Actions](docs/assets/gpt-action-3.png)
![GPT Actions](docs/assets/gpt-action-4.png)
![GPT Actions](docs/assets/gpt-action-5.png)

</details>

WebCodex lets a coding client work on private code through a project-scoped
server and local Agent. Source files, Git operations, edits, and checks remain
on the machine that owns the repository.

## Install

On supported Linux x64, Linux arm64, and macOS arm64 systems:

```bash
npm install -g @yyjeqhc/webcodex
```

Or build every binary from source:

```bash
cargo build --release --workspace --bins
export PATH="$PWD/target/release:$PATH"
```

See [docs/BUILD_INSTALL.md](docs/BUILD_INSTALL.md) for installation details.

The npm install location and the systemd service scope are separate choices.
An ordinary user can install the package in a user-owned npm prefix, log in
without `sudo`, and run a persistent Runner as a user service:

```bash
webcodex login https://your-domain.example --code <wc_pair_...> \
  --allowed-root "$HOME/git"
webcodex agent install --scope user \
  --config <login-reported-agent-config>
webcodex agent status --scope user \
  --config <login-reported-agent-config>
```

User services use `systemctl --user`, require no root privileges, and store
their unit under `$XDG_CONFIG_HOME/systemd/user` (or
`$HOME/.config/systemd/user`). Non-root users default to this scope. See the
[build/install guide](docs/BUILD_INSTALL.md#runner-service-scopes) for the
administrator-managed system scope and its non-root Runner requirement.

## Hosted Quick Start

The lowest-cost path uses the official hosted Server and one background Runner
on the machine that owns your code. You do not deploy a Server, database,
HTTPS, reverse proxy, OAuth, or systemd service:

```bash
cd /path/to/your/repository
webcodex connect https://sg4.yyjeqhc.cn
```

The current directory is the default project. `connect` generates a strong
`wck_...` shared key, creates or reuses a local profile and client ID,
registers the project outside the checkout, starts one detached Runner, and
waits until the same key can see both the Runner and project. It then prints
the MCP URL, profile, safe config/log paths, and the generated key.

Configure the MCP client with the values printed by the command:

```text
MCP URL: https://sg4.yyjeqhc.cn/mcp
Authentication: Bearer token
Token: the generated MCP key
```

Copy an automatically generated key immediately: it is printed in full only
when first created. The owner-only `agent.toml` path is shown in the output,
but status and log commands deliberately never reveal the key. Keep that file
and key out of Git. Advanced users may supply `--key-file <path>` or `--key`,
and `--project` is needed only when connecting a directory other than the
current one.

Closing the terminal does not stop the detached Runner, but a machine reboot
does. After a reboot, either rerun the same `webcodex connect` command or run
`webcodex agent start --profile <profile>` using the profile printed during
setup. Re-running `connect` reuses the existing profile, identity, project
record, and live Runner instead of creating duplicates. See
[AI Onboarding](docs/AI_ONBOARDING.md) for the hosted/managed/self-hosted
decision tree.

## Choose a Setup Path

| Goal | Recommended path |
| --- | --- |
| Connect one local project to ChatGPT/Claude now | Use `webcodex connect` with the official hosted Server. |
| Need user identity, device authorization, revocation, or audit | Use the managed `webcodex login` flow. |
| Need full infrastructure and identity-system control | Follow the self-hosting path in [DEPLOYMENT.md](docs/DEPLOYMENT.md). |
| Keep everything loopback-only | Use the three local project commands below. |

The packaged 0.3.1 path supports Linux x64, Linux arm64, and macOS arm64.
Full self-hosting on Linux still assumes systemd, `sudo`, and an HTTPS domain or trusted tunnel;
the hosted `connect` path does not.

## One Project, One Entry

Run the following from the Git project you want to expose:

```bash
webcodex setup
webcodex doctor
webcodex agent start
```

`setup` creates minimal private state outside the checkout. It does not modify
Git content, start a background service, open a port, edit shell startup files,
or send project files anywhere. Running it again is safe: valid configuration
is preserved, missing pieces are repaired, and conflicting state fails closed.
The private state contains one exact Project Credential shared by this
project's Connector and Agent. It is never printed and an arbitrary Bearer
token cannot substitute for it.

`doctor` is read-only. Immediately after setup it normally reports that the
local Agent still needs to start and gives the exact next command.

`agent start` is the explicit foreground action that starts the project-bound
loopback runtime and Agent. Leave that terminal open. In another terminal:

```bash
webcodex status
```

The default output uses only product concepts: Project, Connection, Agent,
Capabilities, readiness, and the next action. It does not print credentials,
client IDs, runtime project IDs, executor references, workflow sessions, or
transport details.

The complete walkthrough is in [docs/QUICK_START.md](docs/QUICK_START.md).

## Connect a Hosted Chat

The runtime listens on loopback; hosted ChatGPT/Claude need a public HTTPS
URL. Any tunnel you trust works:

```bash
cloudflared tunnel --url http://127.0.0.1:8080
```

Open `https://<tunnel-host>/console` and use the **Connect a chat client**
panel: it renders the exact MCP URL for Claude (Settings → Connectors → Add
custom connector) and the GPT Actions schema URL for ChatGPT (Create a GPT →
Actions → Import from URL), with copy buttons. Authentication stays the
Project Credential from setup — the console never displays it. Set
`WEBCODEX_PUBLIC_URL` when the advertised schema should pin a fixed public
address.

WebCodex currently integrates with ChatGPT as an OpenAPI-based **Custom GPT
Action** (also called GPT Actions); it is not claiming to be a published
ChatGPT plugin. The current ChatGPT/Codex plugin directory contains installable
bundles that may combine apps, skills, connectors, and MCP servers, while an
app, a Custom GPT, and an Action are different layers. See OpenAI's
[GPT Actions introduction](https://developers.openai.com/api/docs/actions/introduction),
[plugin documentation](https://learn.chatgpt.com/docs/plugins), and the
[WebCodex GPT Actions guide](docs/GPT_ACTIONS.md).

## Long-Running Self-Hosting

For a persistent installation, use this order:

1. Install the three binaries on the Linux server and on every machine that
   owns repositories.
2. Initialize the server environment and install the `webcodex` systemd unit.
3. Put Nginx, Caddy, or another trusted reverse proxy in front of the loopback
   server, enable HTTPS, and set `WEBCODEX_PUBLIC_URL`.
4. Create a short-lived pairing code on the server; enroll each repository
   machine with that code instead of copying long-lived credentials.
5. Install the `webcodex-runner` service on each repository machine and finish
   with `webcodex ops status --strict`.

The exact commands and rollback-safe credential rules are in
[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md). OAuth2 is an optional later step for
delegated client login; PAT authentication is sufficient for the first
single-operator deployment.

## Canonical Coding Path

A configured MCP/OpenAPI Connector exposes exactly twelve project-bound
capabilities:

```text
task_start
→ files_list
→ files_read / files_search
→ edits_apply
→ checks_run
→ task_finish
→ task_review
→ task_cancel (when needed)
```

The same chat window continues its current repository work automatically.
Switching to another configured repository changes the active project without
closing the first task; returning to the first repository restores its prior
context. Each follow-up instruction is appended to the durable task history,
and WebCodex selectively refreshes Git, worktree, manifest, and repository-rule
state before reuse. `task_list` and `task_resume` remain recovery tools for a
lost transport identity, not ordinary setup steps.

The configured Connector context resolves the project deterministically.
Ordinary coding does not need `list_projects`, `runtime_status`,
`tool_manifest`, `start_session`, `current_session`, Agent listing, or project
registration calls.

`webcodex setup` selects this ordinary-user surface with
`WEBCODEX_CONNECTOR_SURFACE=task-v1`. A server started without Connector
configuration serves the focused `local_coding` surface on `/mcp` by default;
the explicit `WEBCODEX_MCP_MODEL_SURFACE=full-operator-v1` selects the full
operator runtime. `GET /mcp`, MCP initialize, and `runtime_status.model_surface`
all identify the active surface. On `local_coding`, `work_on_project` is the
lightweight ordinary entry point: one call returns the applicable repository
rules, Git state, LSP readiness, jobs, and blockers so the model can start
targeted inspection immediately. It deliberately does not run a repository
overview scan; the compatibility `repository` field reports
`reason_code=not_requested_by_work_on_project` without overview lists or a
failure warning. A fresh Workflow Session includes bounded rule bodies such as
`AGENTS.md`; an exact continuation with unchanged fingerprints returns rule
metadata without repeating those bodies, while changed rules are returned
again. Use `start_coding_task(detail=standard|full)` on the full runtime, or
call `project_overview` explicitly, when a complete repository overview is
actually needed. `work_on_project` creates a new Workflow Session or exactly
resumes the given one, never binds a current window, and never falls back to a
guessed Session. The returned context is informational: it modifies or executes
nothing, and the model still uses `read_file`, search, edits, and validation
tools as needed. The project source is either the existing `project` runtime id or
`client_id` plus an existing absolute `path`; the two forms are mutually
exclusive. For the path form, the owning Runner canonicalizes and policy-checks
the directory, reuses one unique enabled canonical-path registration, or
atomically persists a stable hashed id under `projects.d` before Session
handling. The result adds bounded, path-free `project_resolution` metadata
(`reused_existing_registration` or `auto_registered`). Registration is the
path resolver's only filesystem mutation; it requires project-write authority
and does not create or modify the target directory or initialize Git. The full
runtime's `start_coding_task` has the same
window/repository start-or-continue semantics, but its current binding is
process-local and its broader tool set is intended for operator and debugging
workflows.

Normal writable tasks must run structured checks before `task_finish`. The
result remains isolated until a human reviews and accepts it locally. The same
host-local human authority is reachable two ways — the offline CLI and the
in-browser console — and both share one accept/reject decision path:

```bash
webcodex task list
webcodex task show <task-id>
webcodex task accept <task-id>
```

In the Browser, `/console` surfaces the actionable work queue and a review
detail with the bounded diff, and its Accept / Reject / Cancel buttons drive
that same authority. Hosted Chat can propose work but can never accept it; the
server re-verifies the target checkout and the result before applying, so a
Browser click cannot bypass the durable preconditions.

Task, operation, execution, and result IDs still provide exact retry, progress,
review, and acceptance identity between tools and the host, but ordinary users
do not need to choose or manage them. Executor routing and queue IDs stay
internal.

### Project-aware validation

`checks_run` remains one of the twelve capabilities. Omit its optional `recipe`
field to resolve the nearest supported manifest from the Task execution
workspace and relative `cwd`; use `recipe: rust|node|python|go` only to resolve
a same-directory ambiguity. Resolution never scans sibling projects, and
absolute, parent-traversing, or symlink-escaping `cwd` values fail closed.

Rust supports `format`, `check`, and `test`; Node selects only fixed
non-mutating script names; Python selects configured Ruff/Black, Ruff/Mypy,
and pytest tools; Go supports `check` and `test` while `format` is deliberately
unavailable. Recipes never install dependencies, generate configuration,
change lockfiles, or access the network. Missing tools are executor failures;
a non-zero result after a tool starts is an assertion failure. The resolved
recipe version, relative root, manifest/lock evidence, and structured
invocation are part of `operation_id` exact-retry identity. See
[docs/QUICK_START.md](docs/QUICK_START.md#project-aware-validation-recipes).

## Readiness

Use `webcodex status` for a quick “can this project work now?” answer. Use
`webcodex doctor` for structured, actionable checks covering local config,
authentication presence, project registration, Git/workspace access, the Agent
runtime, server reachability, Agent registration, required capabilities, and
structured validation.

The Browser console at `/console` projects the same readiness facts and adds
the host-local human review surface: the actionable work queue, a review detail
with the bounded diff and output tail, and Accept / Reject / Cancel actions. It
is not a second status implementation, not a model-facing capability, and not a
browser IDE — it cannot edit code, run commands, or start tasks. The project
credential entered there is kept in memory only and is never persisted in the
browser.

## Client Access

The canonical setup starts on loopback and does not create public ingress.
Local clients can use the project-bound Connector when they share the approved
local connection configuration and its exact Project Credential. Loopback is a
network boundary, not an authentication exemption: unknown credentials are
rejected before readiness, task state, or Agent dispatch. Hosted ChatGPT
clients require an operator-
managed HTTPS endpoint and authentication; setup deliberately does not create a
tunnel, open a public port, or change production auth. See
[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md), [docs/MCP.md](docs/MCP.md), and
[docs/GPT_ACTIONS.md](docs/GPT_ACTIONS.md).

The legacy ToolRuntime discovery and operations tools remain available for
administration and diagnostics. They are not prerequisites for the project
coding path.

## Safety Boundary

- Setup registers only the resolved Git root; it never guesses from names or
  recent usage.
- Project setup uses its exact credential path, not the ordinary arbitrary-key
  quick-start fallback; Connector and Agent must resolve to the same
  non-secret project grant identity.
- Explicit project binding is principal-scoped and transport-scoped where the
  protocol requires it; ambiguous binding fails closed.
- Read-only tasks deny mutation, shell, and job-like actions.
- Structured edits and validation are preferred over raw shell.
- A validation command that cannot spawn is an executor failure, not a failed
  project assertion.
- Tokens, Authorization headers, hashes, private keys, and secret paths must
  never appear in prompts, logs, examples, or committed configuration.

Read [SECURITY.md](SECURITY.md) and
[docs/CONCEPTS.md](docs/CONCEPTS.md) for the full boundary model.

## Scope

WebCodex supports both the official hosted coordination Server and fully
self-hosted deployments. Source code and execution remain on the
user-controlled Runner unless the user explicitly deploys them elsewhere.
Advanced multi-client enrollment, production OAuth, remote deployment, QUIC,
shell profiles, and operator observability remain available through the
management documentation and `webcodex`; they do not change the ordinary
project entry above.

## Documentation

- Full documentation index: [docs/INDEX.md](docs/INDEX.md)
- Getting started: [docs/QUICK_START.md](docs/QUICK_START.md)
- Build/install: [docs/BUILD_INSTALL.md](docs/BUILD_INSTALL.md)
- Concepts: [docs/CONCEPTS.md](docs/CONCEPTS.md)
- MCP: [docs/MCP.md](docs/MCP.md)
- GPT Actions: [docs/GPT_ACTIONS.md](docs/GPT_ACTIONS.md)
- Deployment: [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md)
- Roadmap: [docs/ROADMAP.zh-CN.md](docs/ROADMAP.zh-CN.md)

## Disclaimer

WebCodex is provided only for research and learning. It can read and modify
files and execute commands within configured project boundaries. Use it only
on repositories you are prepared to restore from version control or backups.
The author is not responsible for filesystem damage, data loss, or other
consequences arising from use of the software.

## Acknowledgements

Thanks to the [LINUX DO](https://linux.do/) community for its welcoming space
for technical discussion and support for open-source sharing.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
