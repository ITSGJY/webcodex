# Operations Guide

[English](OPERATIONS.md)

This guide covers day-to-day WebCodex operations: server initialization, client enrollment, pairing, project registration, token management, and smoke testing. For first deployment, see [QUICK_START.md](QUICK_START.md). For production hardening, Nginx, QUIC, and OAuth2 details, see [DEPLOYMENT.md](DEPLOYMENT.md).

Operator-friendly read-only checks are available through:

```bash
webcodex-cli ops status --server-url "$SERVER_URL" --token-file "$USER_TOKEN_FILE"
webcodex-cli ops agents --server-url "$SERVER_URL" --token-file "$USER_TOKEN_FILE"
webcodex-cli ops projects --server-url "$SERVER_URL" --token-file "$USER_TOKEN_FILE"
webcodex-cli ops smoke-preflight \
  --server-url "$SERVER_URL" \
  --token-file "$USER_TOKEN_FILE" \
  --project agent:workstation:my-repo
webcodex-cli ops smoke-preflight \
  --server-url "$SERVER_URL" \
  --token-file "$USER_TOKEN_FILE" \
  --project agent:workstation:my-repo \
  --strict
```

These commands accept `--server-url`/`--url`, `--env-file`, `--token-file`,
`--token`, `--json`, and `--strict`. They require a user token/PAT or another
bearer token with suitable runtime/project/job read scopes. Prefer
`--token-file` for operator use; `--token` is supported for constrained
one-off calls but is easier to expose through shell history or process lists.
They do not print token or env values.

`WARN` means the check found something worth reviewing, but it is not
necessarily a deploy blocker. By default, ops commands exit `0` when they can
generate a report, even when `Overall: FAIL`. Add `--strict` for deployment
gates: `PASS` and `WARN` exit `0`, while `FAIL` exits `2`.

`ops smoke-preflight` short-circuits when the target project is missing,
offline, disconnected, or not git-backed. In that case it reports the blocking
reason without sending `show_changes` or `workspace_hygiene_check` to a stale or
offline agent.

## Server initialization

### Environment file

`webcodex-cli server init` creates the server environment file containing the bootstrap admin token and runtime settings.

```bash
SERVER_URL="https://webcodex.example.com"
ENV_FILE="/etc/webcodex/webcodex.env"
DATA_DIR="/var/lib/webcodex"
BIN="/opt/webcodex/bin/webcodex"
CLI="/opt/webcodex/bin/webcodex-cli"

sudo "$CLI" server init \
  --listen 127.0.0.1:8080 \
  --data-dir "$DATA_DIR" \
  --env-file "$ENV_FILE" \
  --public-url "$SERVER_URL"
```

This writes:

- `WEBCODEX_TOKEN` — the bootstrap/admin token. Used only for initial setup, user creation, pairing, and emergency admin. Do not put it in GPT Actions, MCP, or agent config.
- `WEBCODEX_ADDR` — the server listen address.
- `WEBCODEX_DATA` — the data directory path.
- `WEBCODEX_PUBLIC_URL` — the public HTTPS URL.

The env file is server-side only. Do not copy it to client machines.

### Loading the env file

For one-off admin CLI commands, load the env file:

```bash
set -a
. "$ENV_FILE"
set +a
```

Or pass `--env-file "$ENV_FILE"` when the command supports it.

## Server startup

### systemd (recommended)

```bash
sudo "$CLI" server install-service \
  --env-file "$ENV_FILE" \
  --bin "$BIN"

sudo systemctl daemon-reload
sudo systemctl enable --now webcodex

"$CLI" server status --env-file "$ENV_FILE"
```

Use `--overwrite` only when replacing an existing unit.

### Manual foreground / background

For testing or environments without systemd:

```bash
# Foreground
WEBCODEX_ENV_FILE="$ENV_FILE" "$BIN"

# Background
nohup env WEBCODEX_ENV_FILE="$ENV_FILE" "$BIN" > /var/log/webcodex.log 2>&1 &
```

Manual mode does not provide automatic restart, log rotation, or boot persistence. Use systemd for production.

## Authority Mode

`WEBCODEX_AUTHORITY_MODE` is the canonical authorization switch for
consequential runtime tools:

| Value | Behavior |
| --- | --- |
| unset / empty | `trusted_agent` (product default for self-hosted single-operator deployments); reported source is `default`. |
| `trusted_agent` | Project read/write, shell, async jobs, git operations, script/build execution, dependency install, and local service control auto-execute after hard safety checks, with no human approval interruptions. Push/tag/publish/release/deploy execute only when the user task explicitly includes that action (`release: "user_task_scoped"`). Every permission-bearing call still records an auditable decision (`policy=trusted_agent`, `status=auto_approved`, `reason=trusted_agent_authority`) on the session ledger. |
| `restricted` | Consequential runtime tools are denied (`restricted_requires_human_authorization`). The project-bound connector `commands_run` keeps the one-time human approval loop (`wc_approvals`, `task_cli approve/deny`). |
| anything else | Invalid configuration; consequential tools fail closed with `invalid_authority_mode:...`. |

`WEBCODEX_PERMISSION_MODE` is removed. If it is set to any value, the
configuration is invalid: consequential tools fail closed with reason
`invalid_authority_mode:...` and source
`rejected_legacy_env:WEBCODEX_PERMISSION_MODE`. Delete the variable; there is
no alias or migration.

Hard boundaries are never relaxed by `trusted_agent`: OAuth scopes, project
boundary/allowed roots, explicitly read-only sessions (writes and shell
denied), path and sensitive-path policy, concurrent-overwrite guards,
credential redaction, job cancel/reclaim semantics, and immutable release
targets all remain enforced.

`runtime_status` and `start_coding_task` report the resolved state as an
`authority` object: `{mode, source, project_write, shell, git, network,
package_install, service_control, release, human_approval_required}`. Under
`trusted_agent`, connector `commands_run` records a durable
`authority_auto_authorized` task event (mode, source, resolved rule, action
hash/summary, risk, principal, project) instead of approval records or
`approval_required` interruptions.

Full contract: [agent/permission-model.md](agent/permission-model.md).

## Connection Layers and Version Compatibility

`runtime_status.connection_layers` is an observation contract: every layer
reports `{status, observed_at, source, age_secs, stale_after_secs,
reason_code}` plus layer-specific facts. Statuses are backed by real
observations only; configuration presence never implies readiness, and a stale
layer is never presented as callable.

| Layer | Statuses | Notes |
| --- | --- | --- |
| `runner_process` | `ready` \| `stale` \| `not_observed` | `ready` comes from `runner_process_report` (runner-reported `process_started_at`) or `transport_liveness`; `stale` means `heartbeat_expired`; `not_observed` means `no_runner_registered`. Never fakes "running". |
| `server_transport` | `connected` \| `disconnected` \| `not_observed` | Facts: transport kind, `connection_instance` (agent instance UUID), `connected_at`, `last_heartbeat_at`, `disconnected_at`. |
| `server_registration` | `registered` \| `stale` \| `not_observed` | `stale` means `registration_instance_disconnected`. Facts: `runner_instance`, `registered_at`, `last_refreshed_at`. |
| `project_registry` | `registered` \| `stale` \| `not_configured` | `stale` means `providing_runner_disconnected`; `not_configured` means `no_projects_registered`. Counts registered/online projects. |
| `connector_endpoint` | `not_configured` \| `not_observed` \| `ready` \| `unknown` | `not_configured` when the connector runtime is disabled; `not_observed` (`no_connector_requests_observed`) until a real readiness probe (`/connector/readiness`) or successful connector request is seen. |
| `session_binding` | `not_observed` (runtime_status) / `bound` \| `not_bound` (start_coding_task) | Bindings are process-local and principal+transport scoped. `runtime_status` reports `not_observed` with reason `binding_is_process_local_and_principal_scoped`, `process_local=true`, `lost_after_restart=true`. |
| `last_successful_tool_call` | `observed` \| `not_observed` | Scoped by principal/project/surface/session/tool. Only successful meaningful calls are recorded — `runtime_status`, `list_tools`, `list_agents`, `list_projects`, and `tool_manifest` never refresh it. Bounded in-memory store; no arguments, outputs, or secrets. |

After a server restart, session bindings are lost by design. The correct
recovery is to continue with the explicit durable `wc_sess_*` session id — do
not restart the runner to "fix" a `not_bound` binding.

`runtime_status` also reports `version_compatibility`:

```json
{
  "status": "compatible | version_mismatch | capability_mismatch | no_runners",
  "server": {"version": "...", "build": "..."},
  "runners": [{
    "client_id": "...",
    "agent_protocol_version": "...",
    "protocol_supported": true,
    "build_version": "...",
    "build_git_commit": "...",
    "build_matches_server": true,
    "status": "...",
    "reason_code": "...",
    "action": "..."
  }]
}
```

Connected does not mean compatible. The per-runner facts say which side to
upgrade; there are no compatibility fallback shims. Runners also report
`process_started_at`, `build {version, git_commit}`, and shell dialect facts
(see [SHELL_PROFILES.md](SHELL_PROFILES.md)) at registration.

## Client enrollment

### Profile-based config (recommended)

Each client or user profile gets its own directory under `/etc/webcodex/clients/`:

```text
/etc/webcodex/clients/<profile>/agent.toml
/etc/webcodex/clients/<profile>/projects.d/
/etc/webcodex/clients/<profile>/webcodex-user-token
/etc/webcodex/clients/<profile>/webcodex-runner-token
```

Enroll a client with a profile:

```bash
"$CLI" client enroll \
  --server-url "$SERVER_URL" \
  --pairing-code <wc_pair_...> \
  --client-id workstation \
  --display-name "Workstation" \
  --profile workstation \
  --allowed-root /root/git
```

Install a profile-specific agent service:

```bash
"$CLI" agent install-service \
  --profile workstation \
  --bin /opt/webcodex/bin/webcodex-runner \
  --overwrite

sudo systemctl daemon-reload
sudo systemctl enable --now webcodex-runner-workstation
```

### Legacy flat paths (not recommended)

Older setups may use flat paths directly under `/etc/webcodex/`:

```text
/etc/webcodex/agent.toml
/etc/webcodex/projects.d/
/etc/webcodex/webcodex-user-token
/etc/webcodex/webcodex-runner-token
```

This layout does not support multiple clients on the same machine. Migrate to profile-based config when possible.

## Pairing flow

Pairing creates a short-lived code on the server side that the client exchanges to enroll. This avoids copying long-lived credentials between machines.

### Server/admin side

```bash
"$CLI" pairing create \
  --server-url "$SERVER_URL" \
  --env-file "$ENV_FILE" \
  --username alice \
  --client-id workstation \
  --display-name "Alice Workstation" \
  --ttl-secs 600
```

This returns a `wc_pair_*` code. Send only this code to the client user.

### Client side

```bash
"$CLI" client enroll \
  --server-url "$SERVER_URL" \
  --pairing-code <wc_pair_...> \
  --client-id workstation \
  --display-name "Alice Workstation" \
  --profile alice \
  --allowed-root /home/alice/git
```

### What not to copy

- Do not copy `WEBCODEX_TOKEN` to client machines.
- Do not copy `wc_agent_*` tokens between machines.
- Do not copy `wc_pat_*` tokens between machines.
- Do not put the bootstrap token in agent config or GPT Action config.
- Each client should generate its own tokens through `client enroll` or `token create-local`.

## Project registration

### register_project

`register_project` is an agent-level runtime tool. It registers an existing directory as a project on a connected agent.

```json
{
  "tool": "register_project",
  "params": {
    "client_id": "workstation",
    "id": "my-repo",
    "name": "My Repo",
    "path": "/root/git/my-repo",
    "allow_patch": true,
    "overwrite": true
  }
}
```

Key behaviors:

- Does not require the project to already exist in the agent's `projects.d/`.
- Finds the online agent by `client_id`.
- The agent validates that `path` exists and is within its `allowed_roots`.
- The agent writes `projects.d/<id>.toml` on its own machine.
- The resulting runtime project id is `agent:<client_id>:<project_id>` (e.g., `agent:workstation:my-repo`).

### create_project

`create_project` creates a new directory and registers it. It is subject to the agent's `allowed_roots` policy.

```json
{
  "tool": "create_project",
  "params": {
    "client_id": "workstation",
    "id": "tmp-smoke",
    "name": "Temporary Smoke Project",
    "path": "/root/git/tmp-smoke",
    "git_init": true,
    "allow_patch": true
  }
}
```

If `allowed_roots` is `/root/git`, then paths outside that root (e.g., `/tmp/...`) are rejected by default. For temporary or test projects, place them under the allowed root:

```text
/root/git/tmp-smoke-project
```

## Token model

### WEBCODEX_TOKEN

- Server bootstrap/admin token.
- Created by `server init`.
- Lives only in the server env file (`/etc/webcodex/webcodex.env`).
- Used for: initial setup, creating users, issuing account credentials, pairing, emergency admin.
- Do not use for: GPT Actions, MCP, agent connections, daily runtime calls.

### wc_pat_* (Personal API Token)

- Belongs to a user (owner).
- Generated locally by `webcodex-cli token create-local`; the server stores only the hash.
- Not bound to a single device — the same PAT works from any client.
- Used for: GPT Actions, MCP, REST API, `callRuntimeTool`, `tools/list`, `tools/call`.
- A single PAT can access multiple agents and projects under the same owner on the same server, provided the scopes are sufficient.
- Do not use for: agent WebSocket/QUIC connections.

### wc_agent_* (Agent Token)

- Belongs to an agent instance.
- Generated locally by `webcodex-cli agent-token create-local`; the server stores only the hash.
- Bound to a specific `client_id`.
- Used for: `webcodex-runner` WebSocket/QUIC connections only.
- Do not use for: GPT Actions, MCP, REST API calls.

### wc_acct_* (Account Credential)

- One-time credential issued by `webcodex-cli users create --issue-credential`.
- Used to locally create `wc_pat_*` and `wc_agent_*` tokens.
- Do not use for: GPT Actions, MCP, agent connections, or any ongoing auth.

### wc_oat_* (OAuth2 Access Token)

- Delegated token issued via the OAuth2 authorization code flow.
- Used for: GPT Actions and MCP when OAuth2 is enabled.
- Requires `WEBCODEX_OAUTH2_ENABLED=true` on the server.

## Owner / client / project model

### Ownership

- Each agent has an `owner` (the user who created or enrolled it).
- Each PAT has an `owner` (the user who generated it).
- A PAT can only access agents and projects owned by the same user.
- Owner mismatch results in access denial.

### Client ID

The `client_id` is a stable identifier for an agent instance, typically named after the machine or role:

```text
workstation
laptop
server-a
container-dev
```

### Project ID format

Runtime project ids follow the pattern:

```text
agent:<client_id>:<project_id>
```

Examples:

```text
agent:workstation:my-repo
agent:laptop:my-repo
agent:server-a:service-api
agent:container-dev:tmp-smoke
```

The `client_id` portion identifies which agent hosts the project. The `project_id` portion is the local registry id from the agent's `projects.d/*.toml` file.

## GPT Action / MCP configuration

### Per-server, not per-device

Create GPT Actions and MCP connectors per server, not per device. If a server hosts multiple agents owned by the same user, a single PAT can access all of them.

Example GPT/MCP app names:

```text
WebCodex Production
WebCodex Staging
WebCodex Lab
```

### Token for GPT Actions

Use a `wc_pat_*` personal API token. Generate one with:

```bash
"$CLI" token create-local \
  --server "$SERVER_URL" \
  --user alice \
  --credential "$WEBCODEX_ACCOUNT_CREDENTIAL" \
  --name gpt-action \
  --scopes runtime:read,project:read,project:write,job:run
```

Do not use:

- `WEBCODEX_TOKEN` — admin-only.
- `wc_agent_*` — agent-only.
- `wc_acct_*` — one-time enrollment only.

### Recommended scopes

| Scope | Purpose |
| --- | --- |
| `runtime:read` | Read runtime status, list tools, list agents. |
| `project:read` | Read files, search, git status/diff, show_changes. |
| `project:write` | Write files, apply patches, structured edits. |
| `job:run` | Run shell commands, Cargo helpers, Codex tasks. |
| `account:manage` | Optional: manage OAuth clients and tokens. |

### MCP with OAuth2

When OAuth2 is enabled (`WEBCODEX_OAUTH2_ENABLED=true`), MCP clients can use the authorization code flow instead of a static PAT:

- No PAT needed in the client config.
- The client redirects to `https://your-domain.example/oauth/authorize`.
- After consent, a `wc_oat_*` access token is issued.
- Scopes are delegated from the authorizing user.

### MCP with Bearer token

For static-token MCP clients:

- Use a `wc_pat_*` in the `Authorization: Bearer` header.
- Do not use `wc_agent_*` or `WEBCODEX_TOKEN`.

## Coding And Session Workflow

For coding tasks, prefer the deterministic coding-task aggregate tools. They
create and close out a session while keeping all continuity explicit.

### 1. Start a coding task

```json
{
  "tool": "start_coding_task",
  "params": {
    "project": "agent:workstation:my-repo",
    "title": "fix authentication bug",
    "detail": "standard",
    "bind_current": false
  }
}
```

Returns a `wc_sess_*` session id in `output.session.session_id`. Keep that id
and pass it explicitly to subsequent project tools. `detail` is the canonical
startup projection:

- `minimal`: session id, resolved project, branch/head/workspace state, compact
  runtime/readiness layers, semantic-navigation summary, hard blockers, and
  advisories; the `authority` block is omitted;
- `standard` (default): minimal plus the `authority` block;
- `full`: explicitly adds full runtime status, recent commits, project rules,
  recommended flow, and the compact tool manifest.

`detail` is the only projection control. The legacy startup flags
(`compact_startup`, `include_runtime_status`, `include_git`,
`include_recent_commits`, `include_rules`, `include_tool_manifest`,
`tool_manifest_intent`, `tool_manifest_categories`, `tool_manifest_limit`) are
removed; sending any of them returns a strict unknown-field error.

Minimal and standard do not return repeated manifest/rules/recent-commit
payloads. Use `tool_manifest` directly when focused discovery is needed.
Read `output.startup_verdict.status` first. If it is `warn` or `fail`, inspect
`startup_verdict.checks` and `startup_verdict.suggested_next_actions`; detailed
startup fields remain the audit source.

Standalone `runtime_status` also accepts `summary_only=true` or `compact=true`
for the same compact health shape. Use that for first-contact deployed sanity;
reserve full no-arg `runtime_status` for deeper troubleshooting.

Startup sanity verdict rules:

- PASS: compact runtime status is present, `tools.count` is nonzero,
  `jobs.active_count=0`, an agent is online when the task depends on an agent
  project, and requested git/workspace status is clean.
- WARN: runtime status or git/workspace was not requested, validation has not
  run yet, or the requested workspace contains ordinary tracked, staged, or
  untracked changes. Existing changes must be inspected and preserved; they
  are not automatically reverted, stashed, cleaned, or overwritten.
- FAIL: runtime status failed, blocking jobs are active, agent required for the
  task is offline, unresolved merge/rebase conflicts exist, or another
  infrastructure/safety condition makes the project inaccessible or the
  requested work unsafe or impossible. Ordinary dirty state is not a blocker.

`output.connection_state` reports runner process, server transport, server
registration, project registry, connector endpoint, session binding, and last
successful tool call separately (see
[Connection Layers and Version Compatibility](#connection-layers-and-version-compatibility)).
`not_observed` means that layer has no evidence; it must not be collapsed into
an overall offline verdict. `session_binding` here reports `bound` or
`not_bound`; after a server restart, continue with the explicit durable
`wc_sess_*` session id instead of restarting the runner.

Console **Connect a chat client** targets the project-bound canonical capability
surface by default. Its connection projection reports
`surface.mode=project_bound` and `operator_runtime_exposed=false`. The complete
operator runtime remains available for management, development, and internal
execution, but it is not the model-default project chat endpoint.

The response also includes `output.authority` (omitted at `detail=minimal`).
On a default self-hosted deployment this reports `mode=trusted_agent`,
`source=default`, `human_approval_required=false`, and
`release=user_task_scoped`. See [Authority Mode](#authority-mode).

### 2. Discover and inspect

```json
{"tool": "runtime_status", "params": {"summary_only": true}}
{"tool": "list_projects", "params": {}}
{"tool": "read_file", "params": {"project": "agent:workstation:my-repo", "path": "src/auth.rs"}}
{"tool": "search_project_text", "params": {"project": "agent:workstation:my-repo", "pattern": "authenticate", "path": "src"}}
{"tool": "show_changes", "params": {"project": "agent:workstation:my-repo", "session_id": "wc_sess_example", "include_diff": false}}
```

`read_file` returns exactly one primary representation in `output.text`.
`output.format` is `plain` by default or `numbered` when
`with_line_numbers=true`; it never duplicates the full body into a second
`content`/`numbered_text` field.

When choosing a smoke target from `list_projects`, prefer
entries in `projects` whose `capabilities.recommended_for_smoke=true`. The
output shape is `{count, projects, recommended_for_smoke}`. For git smoke, also
require `capabilities.git_available=true`; a project such as
`agent:special:test-mcp` may be safe but not git-backed.

### 3. Edit with structured tools

#### Preferred workflow

1. Read and inspect the **current worktree** (`read_file`, `show_changes`). Dirty
   worktrees are a valid baseline; protect existing user edits. Do not rebuild
   file content from HEAD and overwrite the current file.
2. **Transactional file changes:** `apply_text_edits` (canonical). A bounded
   batch can edit, create, delete, and rename files. Existing source files must
   carry their current SHA-256 so the whole batch can be rejected before the
   first mutation when any input is stale.
3. **Multi-file / complex unified diffs:** `apply_patch_checked` (canonical).
   Preflight first; apply only when validation passes. Prefer over raw
   `apply_patch`.
4. **New files or intentional whole-file rewrite:** `write_project_file`. Not the
   default for ordinary local edits.
5. **Compatibility tools** (`replace_line_range`, `insert_at_line`,
   `delete_line_range`, `replace_in_file`, `replace_exact_block`,
   `insert_before_pattern`, `insert_after_pattern`) remain supported for special
   cases; prefer `apply_text_edits` for new workflows.

```json
{"tool": "apply_text_edits", "params": {"project": "agent:workstation:my-repo", "changes": [{"kind": "edit", "path": "src/auth.rs", "expected_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", "edits": [{"kind": "replace_exact", "old_text": "old", "new_text": "new"}]}]}}
{"tool": "apply_patch_checked", "params": {"project": "agent:workstation:my-repo", "patch": "diff --git ..."}}
{"tool": "write_project_file", "params": {"project": "agent:workstation:my-repo", "path": "src/new.rs", "content": "fn main() {}\n"}}
```

### 4. Validate

```json
{"tool": "cargo_fmt", "params": {"project": "agent:workstation:my-repo"}}
{"tool": "cargo_check", "params": {"project": "agent:workstation:my-repo"}}
{"tool": "cargo_test", "params": {"project": "agent:workstation:my-repo"}}
{"tool": "validate_patch", "params": {"project": "agent:workstation:my-repo", "patch": "diff --git ..."}}
```

Specialized Cargo tools automatically declare purpose and parser metadata.
General execution is equally valid evidence when its intent is explicit:

```json
{"tool": "run_shell", "params": {"project": "agent:workstation:my-repo", "session_id": "wc_sess_example", "purpose": "test", "shell": "bash", "cwd": ".", "command": "cargo test -p webcodex --bin webcodex focused"}}
{"tool": "run_job", "params": {"project": "agent:workstation:my-repo", "session_id": "wc_sess_example", "purpose": "validation", "shell": "sh", "command": "make check"}}
```

Purposes are `validation`, `test`, `build`, `format`, `release`, `diagnostic`,
`operation`, and `other`. Purpose records evidence; it does not change
authorization. `run_shell` defaults to local `sh`; `run_job` preserves local
`bash`; an explicit `shell=sh|bash` selects the command language. Agent-backed
omission uses the Agent's configured shell and records it as configured.

For both tools, omitted cwd, `cwd=""`, and `cwd="."` mean the project root.
Other cwd values are project-relative and cannot escape the project root.
Responses expose only `.` or a project-relative cwd.

Use `run_job` for async diagnostics/build/test work, then supervise it with
`job_status`, `job_log`/`job_tail`, or `list_jobs`. Job log responses default to
200 bounded lines per stream (maximum 500) and return status, exit code, total
line counts, bounded tails, truncation flags, detected summary, and `cursor`;
continue with `offset=cursor.stdout`. They never return an unbounded full log.
To stop a WebCodex-started job, call `stop_job` with the same project, job id,
explicit session id when available, and `confirm=true`.

### 5. Review and summarize

```json
{
  "tool": "show_changes",
  "recording_session_id": "wc_sess_example",
  "params": {
    "project": "agent:workstation:my-repo",
    "session_id": "wc_sess_example",
    "include_diff": false,
    "session_event_limit": 30
  }
}
```

```json
{
  "tool": "workspace_hygiene_check",
  "params": {
    "project": "agent:workstation:my-repo",
    "session_id": "wc_sess_example"
  }
}
```

Review order for coding closeout is deterministic: call `show_changes`, inspect
`clean`, `warnings`, `hunks_truncated`, and `suggested_next_actions`; then call
`workspace_hygiene_check`, inspect `clean`, `findings`, `warnings`, and
`suggested_next_actions`; then use `session_handoff_summary` or
`finish_coding_task` with `summary_only=true` for compact canonical outcomes.
`show_changes` and `workspace_hygiene_check` expose top-level `verdict`
summaries; read them first, but keep the detailed fields as the auditable basis.
For final Agent reporting, use `finish_coding_task.facts`,
`finish_coding_task.hard_blockers`, and `finish_coding_task.advisories`, not
nested component verdicts. These are a compact fact package; the Agent still
decides test sufficiency and the task-specific engineering conclusion.

Discovery taxonomy is intentional: `start_coding_task` and
`finish_coding_task` are `workflow` category tools for the coding lifecycle.
`start_session`, `bind_current_session`, `session_summary`, and
`session_handoff_summary` are `session` category tools for raw ledger and
session-control workflows. Use `category=workflow` for lifecycle discovery and
`category=session` for session ledger/control discovery.

### 6. Finish or hand off

```json
{
  "tool": "finish_coding_task",
  "params": {
    "project": "agent:workstation:my-repo",
    "session_id": "wc_sess_example",
    "include_handoff": true,
    "include_workspace": true,
    "include_hygiene": true,
    "include_validation_summary": true,
    "include_diff": false,
    "summary_only": true
  }
}
```

`finish_coding_task` and `session_handoff_summary` should be used with
`summary_only=true` for compact handoff and closeout checks. For handoff, also
pass `include_workspace=true` and `include_validation=true`. For finish, pass
`include_workspace=true`, `include_hygiene=true`,
`include_validation_summary=true`, `include_diff=false`, and keep
`include_handoff=true` when a handoff aggregate is useful.
`finish_coding_task.include_workspace` controls the nested handoff workspace
projection when handoff is included; the top-level finish workspace inspection
still runs. For `summary_only=true`, `facts` contains `work_performed`,
`changed_paths`, `executions`, passed/failed/skipped validation counts,
resolved/unresolved failures, workspace state, active jobs, and evidence
integrity. `hard_blockers` and `advisories` classify only deterministic facts.
They do not change authorization, guards, session binding, expected-failure
classification, or job lifecycle behavior.

For `summary_only=true` final outputs, sanity checks should reject stdout/stderr
bodies, command text, tails, and excerpts. Raw lower-level diagnostic/status
payloads may contain empty string fields such as `stderr: ""`; treat non-empty
stdout/stderr bodies as sensitive/high-noise unless explicitly requested, and
never allow env values, tokens, or secrets to appear.

`finish_coding_task` and `session_handoff_summary` include a bounded `jobs`
section. `active_count` is the broad active count; `blocking_active_count` and
`nonblocking_active_count`, with
`running_count`, `stop_requested_count`, and `terminal_pending_count` for model
closeout decisions. `queued`, `running`, `started`, and `agent_queued` are
blocking active states and produce `active_jobs_present`. `stop_requested` is
nonblocking terminal-pending state and produces `jobs_terminal_pending` with
`blocking=false`; it should not prompt "stop active jobs before proceeding" by
itself. The jobs summary includes only metadata such as `job_id`, `kind`,
`status`, `project`, and timestamps; it does not include raw stdout/stderr,
tails, excerpts, or command text.

Compact handoff/finish classification:

- hard blockers: unresolved workspace conflicts, blocking active jobs,
  unexpected command/tool failures, expectation mismatches, unresolved
  validation failures, sensitive-path risk, and evidence-integrity errors;
- advisories: ordinary dirty worktree, task-optional validation not observed
  (including docs/review-only work), resolved historical failures, bounded
  truncation, non-git context, and terminal-pending nonblocking jobs.

Unresolved validation failures and non-validation tool failures remain
blocking. Inspect `validation.historical_failures`, `resolved_failures`, and
`unresolved_failures` to distinguish resolved feedback from a clean first pass.

For a read-only handoff without finish aggregation:

```json
{
  "tool": "session_handoff_summary",
  "params": {
    "session_id": "wc_sess_example",
    "project": "agent:workstation:my-repo",
    "include_validation": true,
    "summary_only": true
  }
}
```

Smoke and acceptance tests can mark intentional negative paths with runtime
testing metadata:

```json
{
  "tool": "stop_job",
  "params": {
    "project": "agent:workstation:my-repo",
    "session_id": "wc_sess_example",
    "job_id": "missing-job",
    "confirm": false,
    "expected_failure": true,
    "expected_failure_kind": "confirmation_required",
    "assertion_name": "stop_job requires confirm=true"
  }
}
```

`expected_failure`, `expected_failure_kind`, and `assertion_name` are ledger
metadata only. They do not change authorization,
permission decisions, hard guards, execution, `command_started`, or the
immediate success/error result. `finish_coding_task` and
`session_handoff_summary` classify matching expected failures separately from
unexpected failures. They surface `expectation_mismatch_count` when the actual
`failure_kind` / `error_kind` differs, and `unexpected_success_count` when a
call marked `expected_failure=true` succeeds. Only unexpected failures,
mismatches, or unexpected successes should trigger "review failed tool calls"
style next actions; matched expected failures may produce an informational
`expected failure assertions matched` action.

For expected Cargo validation failures, use `expected_failure=true` with
`expected_failure_kind=validation_failed`. `cargo_fmt`, `cargo_check`, and
`cargo_test` set `failure_kind="validation_failed"` only when the underlying
Cargo command started and returned a nonzero exit code. Permission denials,
session/project mismatches, guard denials, timeouts, malformed arguments,
disconnected agents, commands that did not start, and runtime errors keep their
existing failure or error kind.

`cargo_test` reports zero-tests metadata when it can parse Rust test harness
output: `tests_detected`, `tests_run_count`, and `zero_tests_run`. The parser
sums all `running N test` / `running N tests` sections, so a mixed lib
`running 0 tests` plus integration `running 1 test` run is not considered
zero-tests. A successful `cargo_test` with `zero_tests_run=true` should not be
treated as strong validation; closeout summaries warn with
`cargo_test_zero_tests` and suggest checking the filter or command. If
`expected_failure=true` but `cargo_test` exits successfully after running zero
tests, it is still an unexpected success / invalid negative assertion.
`cargo_fmt` and `cargo_check` do not report zero-tests metadata.

In GPT Actions, that same expected negative path may still show an outer
`tool_error` because `/api/tools/call` returns HTTP 400 for a concrete runtime
`ToolResult.success=false`. Do not treat the outer GPT Action label alone as a
transport failure. Judge intentional negative-path smoke from the immediate
`failure_kind` / `error_kind` and from
`session_handoff_summary(summary_only=true).tool_failures` or
`finish_coding_task(summary_only=true).tool_failures`. The classifier separates
`expected_count`, `unexpected_count`, `expectation_mismatch_count`, and
`unexpected_success_count`; expected failures must not bypass auth, permission,
guards, `session_project_mismatch`, confirmation requirements, schema checks,
invalid JSON handling, or unknown-tool failure semantics.

`finish_coding_task.validation` and `session_handoff_summary.validation` are
ledger-derived unified execution summaries. Evidence sources include dedicated
Cargo validation tools and `run_shell`/terminal `run_job` executions declaring
`validation`, `test`, `build`, `format`, or `release` purpose. Summaries never
depend on tool name alone and never expose unbounded stdout/stderr.

Each evidence item carries a stable identity. An explicit assertion name wins;
otherwise purpose plus normalized bounded command identity is hashed. A later
success resolves only failures with the same identity. The original failure
remains in `historical_failures`, moves into `resolved_failures`, and is removed
only from `unresolved_failures`; a different assertion can never resolve it.

`events_total=0` yields `status=not_run` and
`reason=no_validation_tool_invoked`. This is an observed fact, not an automatic
task failure. Docs-only, review-only, or otherwise validation-optional work is
classified with task context as an advisory. Non-validation command failures
and unresolved validation failures remain blockers.

For `cargo_test`, validation events preserve parsed zero-tests metadata when
available. A successful zero-test run remains visible through closeout warnings
rather than counting as strong test coverage.

`finish_coding_task.review_evidence` and
`session_handoff_summary.review_evidence` are separate ledger-derived,
non-cargo review summaries. They count successful read/search/diff/workspace/
hygiene inspection tools such as `read_file`, `search_project_text`,
`show_changes`, `git_diff_hunks`, and `workspace_hygiene_check`.
`finish_coding_task.review_evidence` may include the closeout review calls that
`finish_coding_task` performs itself. Compact review evidence also includes a
bounded `tools` list for explainability. It never includes file contents,
stdout/stderr, diff hunks, command text, tokens, secrets, or raw input payloads.
For docs-only or read-only audit tasks, `validation.status=not_run` can coexist
with `review_evidence.total>0`; closeout uses the
`validation_not_run_with_review_evidence` advisory and does not manufacture a
failed task. Review evidence is not a replacement for validation when the task
explicitly requires validation.

`finish_coding_task.permissions` and `session_handoff_summary.permissions`
summarize high-risk permission decisions from the session ledger. A high-risk
tool is one that is not read-only, is destructive, or is shell/job-like according
to runtime metadata. Under `trusted_agent` authority, those tools record
`status=auto_approved` with `reason=trusted_agent_authority` after hard safety
checks pass. Auto authorization does not
bypass auth, OAuth scopes, read-only sessions, explicit deny guards,
cross-project session mismatch denial, path safety, sensitive path denial, or
agent policy. The permission summaries are bounded metadata only and must not
contain stdout/stderr, command bodies, patches, file contents, env, tokens,
secrets, or excerpts. `approved_count` remains a compatibility manual approval
count; use `manual_approved_count`, `auto_approved_count`, and
`total_approved_count` for clear totals.

### Session id semantics

**REST / GPT Action:**

- Top-level `recording_session_id` = recorder metadata for the current generic wrapper call; it is stripped before concrete tool dispatch.
- Top-level `session_id` = ordinary flattened tool input when `params`/`arguments` are absent.
- `params.session_id` = business parameter used by `show_changes` or `session_summary` to select which session to summarize.
- The two may be the same or different.
- `tool_manifest` is the recommended way to discover accepted flattened args.
  It returns `accepted_flattened_args` and `deprecated_or_unsupported_args` per
  tool without full schemas.
- `start_session` creates a session record but does not automatically bind
  future calls.
- `session_handoff_summary` requires explicit `session_id`; it never implicitly
  uses current-session binding.

**MCP:**

- `_session_id` in arguments = reserved recorder metadata. Stripped before tool dispatch.
- `session_id` in arguments = business parameter for `show_changes` or `session_summary`.
- Current-session bindings are process-local in-memory convenience state, not
  the durable session ledger.

## Smoke Test (read-only)

Use this sequence to verify a deployment without modifying any project.

Assumes a registered project `agent:workstation:my-repo`.

```json
{"tool": "runtime_status", "params": {"summary_only": true}}
```

Confirm service/build, `tools.count`, `jobs.active_count`, agent summary, and
project effective status. Use full no-arg `runtime_status` only when you need
deeper details such as `output.authority.mode` (normally `trusted_agent` with
`source=default` on self-hosted deployments), `connection_layers`, or
`version_compatibility`.

```json
{"tool": "list_agents", "params": {}}
```

```json
{"tool": "list_projects", "params": {}}
```

```json
{"tool": "start_session", "params": {"project": "agent:workstation:my-repo", "title": "smoke test"}}
```

```json
{"tool": "read_file", "params": {"project": "agent:workstation:my-repo", "path": "README.md", "start_line": 1, "limit": 10}}
```

```json
{"tool": "show_changes", "params": {"project": "agent:workstation:my-repo", "session_id": "wc_sess_example", "include_diff": false}}
```

```json
{"tool": "session_summary", "params": {"session_id": "wc_sess_example"}}
```

## Post-Deployment Acceptance Smoke

### Post-deploy smoke facts

Fastest full check: run the real-process reconnect harness, which boots a
server plus runner, asserts layered connection observations, crashes and
restarts both sides, and prints post-deploy smoke facts:

```bash
bash scripts/e2e_reconnect_ws.sh
```

Alternatively, call `POST /api/runtime/status` (or the `runtime_status` tool)
and verify:

- server `version` and `build.git_commit` match the deployed build;
- `authority.mode` is the intended mode (`trusted_agent` by default) with the
  expected `source`;
- `version_compatibility.status` is `compatible`;
- `connection_layers`: `runner_process` is `ready`, `server_transport` is
  `connected`, `server_registration` and `project_registry` are `registered`;
- `agents.clients[].shell_profiles.default_dialect` reports the expected
  runner shell dialect (`sh`, `bash`, or `custom`).

### Full acceptance sequence

After deploying a new server, agent, or runtime build:

1. Refresh the GPT Action or MCP schema if runtime tool schemas changed.
2. Run `tool_manifest` or focused `list_tools` with `summary_only=true` plus
   `category`, `features`, or `limit`; avoid full `listRuntimeTools` in GPT
   Actions unless debugging schemas. If `truncated=true` is caused by the
   caller-supplied limit, `truncation_reason="limit"` confirms it is a bounded
   response rather than `ResponseTooLarge`.
3. Run `runtime_status(summary_only=true)` or `runtime_status(compact=true)`;
   confirm `projects.effective.status`, `projects.effective.count`, and
   `projects.agent_registered.online_count`. Projects are registered by agents,
   not by server-side `projects.toml`. For workflow sanity, also use
   `start_coding_task(detail=minimal)` and
   inspect `startup_verdict.status`; reserve full runtime status for deeper
   troubleshooting.
4. Confirm `start_coding_task` and `finish_coding_task` are available through
   the generic runtime tool path.
5. Confirm `session_handoff_summary` exposes `validation` when
   `include_validation` defaults to true.
6. On a `list_projects.projects[]` entry with
   `capabilities.recommended_for_smoke=true`, run `start_coding_task`,
   `read_file` or `search_project_text`,
   `show_changes`, and `finish_coding_task`.
7. Run local or staging E2E and eval checks:

Preferred deployed generic sanity sequence:

1. `runtime_status(summary_only=true)` or `runtime_status(compact=true)`.
2. `tool_manifest`.
3. `tool_manifest(category=runtime)`, `tool_manifest(category=session)`, and
   `tool_manifest(category=git)` for focused discovery.
4. `show_changes(include_diff=false)` on the selected smoke project.
5. `workspace_hygiene_check` on the same smoke project.
6. `finish_coding_task(summary_only=true, include_workspace=true,
   include_hygiene=true, include_handoff=true,
   include_validation_summary=true, include_diff=false)` with the explicit
   `session_id`.

```bash
bash scripts/e2e_zero_config_ws.sh
E2E_TRANSPORT=polling bash scripts/e2e_zero_config_ws.sh
EVAL_MODE=compare bash scripts/eval_coding_loop.sh
```

## Binary Deployment and Rollback Checklist

Use this short runbook for a conservative binary deployment. Adjust service
names and install paths to match the host, and keep token values in the
operator's shell or secret manager rather than in commands, logs, or docs.

1. Build the release binaries:

```bash
cargo build --release --workspace --bins
```

2. Back up the current install directory:

```bash
backup_dir="/opt/webcodex/bin.backups/$(date -u +%Y%m%dT%H%M%SZ)"
sudo install -d -m 0755 "$backup_dir"
sudo cp -a /opt/webcodex/bin/. "$backup_dir/"
```

3. Install the new binaries:

```bash
sudo install -m 0755 target/release/webcodex /opt/webcodex/bin/webcodex
sudo install -m 0755 target/release/webcodex-runner /opt/webcodex/bin/webcodex-runner
sudo install -m 0755 target/release/webcodex-cli /opt/webcodex/bin/webcodex-cli
```

4. Restart services on the appropriate hosts:

```bash
sudo systemctl restart webcodex
sudo systemctl restart webcodex-runner
```

5. Verify the public schema and operation budget:

```bash
curl -fsS https://webcodex.example.com/openapi.json > /tmp/webcodex-openapi.json
python3 - /tmp/webcodex-openapi.json <<'PY'
import json
import sys

schema = json.load(open(sys.argv[1], encoding="utf-8"))
ops = [
    op.get("operationId")
    for methods in schema.get("paths", {}).values()
    for op in methods.values()
    if isinstance(op, dict)
]
print(f"operation_count={len(ops)}")
if len(ops) > 30:
    raise SystemExit("operation_count exceeds GPT Actions limit")
PY
```

The current recommended GPT Action operation count is 25, and it must remain at
or below 30. Runtime/MCP tools such as `stop_job` remain available through the
generic `callRuntimeTool` surface and do not add dedicated GPT Action operations.

6. Run deployment smoke checks:

```bash
WEBCODEX_PUBLIC_URL="https://webcodex.example.com" \
WEBCODEX_TOKEN="<wc_pat_or_allowed_shared_key>" \
bash scripts/smoke_deployment.sh

WEBCODEX_SMOKE_RUN=1 \
WEBCODEX_PUBLIC_URL="https://webcodex.example.com" \
WEBCODEX_TOKEN="<wc_pat_or_allowed_shared_key>" \
bash scripts/smoke_artifact_transfer.sh
```

For GPT Actions, re-import the schema from `/openapi.json` when needed, then run
a read-only discovery/status smoke before mutation. For MCP, reconnect the
client and run `initialize`, `tools/list`, and a read-only `tools/call` such as
`runtime_status` or `list_projects`.

GPT Actions and MCP should use a managed `wc_pat_*` token or a
deployment-allowed shared key. `wc_agent_*` is only for `webcodex-runner`; do not
copy it into GPT Actions or MCP configuration.

7. Check service logs:

```bash
journalctl -u webcodex --since "15 minutes ago"
journalctl -u webcodex-runner --since "15 minutes ago"
```

8. Roll back from the backup if smoke or logs show a deployment regression:

```bash
sudo cp -a "$backup_dir"/. /opt/webcodex/bin/
sudo systemctl restart webcodex
sudo systemctl restart webcodex-runner
```

Do not use production mutation as smoke coverage. Any write-path smoke must stay
inside a disposable test project or temporary project under an allowed root.
Use artifact paths such as `artifacts/smoke/<name>.artifact` or
`artifacts/smoke/<name>.txt`. For abort cleanup verification, prefer
`artifact_upload_abort.final_file_exists` or
`read_project_artifact_metadata` with `allow_missing=true`; do not use an
expected read failure to prove absence. In session summaries,
`policy_rejected` means policy blocked the request before a write.

### register_project example

```json
{
  "tool": "register_project",
  "params": {
    "client_id": "workstation",
    "id": "my-repo",
    "name": "My Repo",
    "path": "/root/git/my-repo",
    "allow_patch": true,
    "overwrite": true
  }
}
```

### create_project example

```json
{
  "tool": "create_project",
  "params": {
    "client_id": "workstation",
    "id": "tmp-smoke",
    "name": "Temporary Smoke Project",
    "path": "/root/git/tmp-smoke",
    "git_init": true,
    "allow_patch": true
  }
}
```

## Related docs

- [DEPLOYMENT.md](DEPLOYMENT.md) — production hardening, Nginx, QUIC, OAuth2.
- [QUICK_START.md](QUICK_START.md) — first deployment walkthrough.
- [AUTH_MODEL.md](AUTH_MODEL.md) — credential model summary.
- [GPT_ACTIONS.md](GPT_ACTIONS.md) — GPT Action setup and tool surface.
- [MCP.md](MCP.md) — MCP endpoint, client config, and troubleshooting.
- [AGENT_PROJECTS.md](AGENT_PROJECTS.md) — agent project registry format.
