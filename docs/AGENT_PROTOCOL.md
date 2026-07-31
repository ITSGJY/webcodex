# Agent Protocol

[English](AGENT_PROTOCOL.md) | [简体中文](AGENT_PROTOCOL.zh-CN.md)

WebCodex agents connect to the server and execute registered project tools. New deployments should prefer `transport = "auto"` with QUIC configured; WebSocket and polling remain fallback transports.

## Authentication

Agents should use agent tokens created during client enrollment:

```bash
webcodex client enroll --server-url URL --pairing-code CODE --client-id CLIENT_ID
```

The server/admin side creates the temporary code with `webcodex pairing create`. The agent token is returned to the client during enroll and written into the generated `agent.toml`; do not copy agent token files from the server. For binary deployments, install the client-side service with `webcodex agent install` and inspect it with `webcodex agent status`.

Transport auth rules:

- QUIC: the agent token stays in the top-level agent config and is sent inside the agent registration envelope over the QUIC stream.
- WebSocket: `Authorization: Bearer <agent-token>` in the handshake headers is preferred.
- WebSocket compatibility: `/api/agents/ws?token=...` is accepted for handshake compatibility only.
- Polling: every request must use `Authorization: Bearer <agent-token>`.
- REST, MCP, and GPT Actions ordinary APIs must use `Authorization: Bearer ...`.

Do not use query-string tokens outside `/api/agents/ws`.

## Registration and identity

Agents register with:

- `client_id`
- `owner`
- `transport`
- `agent_instance_id`
- capabilities
- registered projects
- redacted policy summary

`agent_instance_id` identifies a running agent instance separately from the stable `client_id`.

## Same-process job state reconciliation

Current runners advertise `job_state_reconciliation`. Every registration and
same-instance re-registration then includes one complete active and bounded
recent-terminal `job_inventory`. Polling uses the registration body; WebSocket
and QUIC carry the same model in the `Register` envelope. A declaration without
the required complete inventory is a protocol error. Older runners do not
advertise the capability and retain the conservative immediate-`lost`
disconnect behavior.

Roll this protocol out server-first, then upgrade runners. New server fields
are optional/defaulted for older runners; once a runner advertises
`job_state_reconciliation`, the inventory and sequenced-update contract is
mandatory rather than silently downgraded.

Each snapshot keeps the original `job_id` and `request_id`, lifecycle/result
fields, a runner-owned monotonic `update_seq`, validation progress, bounded
stdout/stderr tails with absolute retained-line cursors, and server-derived
project/Workflow Session/execution metadata. The start request supplies that
safe job context only after normal project resolution and permission checks.
It contains the existing redacted, bounded command preview, not another raw
command copy. Inventory never carries stdin, environment values, tokens, authorization
headers, or complete agent configuration.

The runner updates its in-memory record before attempting network delivery.
Server updates with a higher sequence are accepted; equal-sequence replay is
idempotent and older updates are ignored. Register reconciliation replaces the
server's bounded tail with the authoritative runner tail instead of appending
it. Each current-runner sequenced update carries that bounded authoritative
tail, so an out-of-order higher update subsumes retained output from lower
updates. The runner also replays its latest bounded snapshot after a new
transport sink is installed, closing the register/ack race with the same sequence rule.
Accepted terminal states never revert to active or change terminal class.

The bounds are part of the internal protocol:

- at most 64 active records, always ordered before terminal history;
- at most 64 terminal records, retained for 15 minutes by the runner;
- at most 64 KiB per stdout/stderr tail;
- at most 1 MiB for the serialized inventory.

On a recoverable disconnect, an already accepted job becomes `recovering` for
a bounded grace window (default 120 seconds, overridable with
`WEBCODEX_JOB_RECOVERY_GRACE_SECS`, clamped to 5–3600 seconds). `recovering` is
a bounded, not permanent, state: a complete same-instance inventory restores
its actual state, logs, ownership, and original `job_id`; omission marks it
`lost` with `runner_inventory_missing`. A replacement `agent_instance_id`
marks the old instance's active jobs `lost` with `runner_instance_replaced`
and fences late register/update traffic; the new instance does not migrate or
inherit those jobs. A delayed disconnect from the already-replaced instance
is a no-op with respect to the current instance — it neither clears the
current notifier nor marks the current instance's jobs lost/recovering — but
the old instance's jobs were already terminated to `lost` at replacement
time. An undispatched server queue entry is not replayed.

### Recovery deadline sweep

The deadline is enforced even without request traffic: an in-process sweep
runs on a fixed interval and transitions any `recovering` job whose grace
window has elapsed to terminal `lost` with `runner_recovery_deadline_exceeded`.
The sweep is bounded (a per-pass cap of expired jobs), holds the registry
mutex only for in-memory work (no disk/network/await under the lock), is
idempotent, and sets `ended_at` only once. A job reconciled before its
deadline is never lost, and stale-connection Ping/Pong/metadata or a repeated
disconnect does not extend the deadline (the deadline is anchored to
`recovering_since`, set once when the job enters `recovering`, not to client
liveness).

The deadline is a per-Server-process property, not a durable record. The Job
Registry is Server in-memory state; a Server restart clears it, and the
deadline is not persisted across Server processes. Post-restart recovery
depends on the runner reconnecting and submitting its inventory: when the
inventory re-registers a job, a fresh bounded recovery window begins (this
phase does not preserve recovery time already consumed before the restart).
If the runner never reconnects after a restart, the Server has no durable
record of the job and cannot run a recovery timeout on an unknown job. A
durable Server-side Job ledger is a separate future phase and is out of scope
here.

A malformed structured validation progress update is an executor protocol
violation, not a transient recoverable state: an out-of-order, regressing, or
skipped `completed` cursor, a plan/step-name mismatch, a duplicated or
inconsistent completion, or progress on a job without a validation plan moves
the job to terminal `failed` with a bounded, stable, non-payload-leaking
`validation_progress_invalid`-class error, the last accepted valid progress is
retained, `ended_at` is set once, the pending request and request-to-job
mapping are released, and no re-execution occurs. Equal- and older-sequence
replays remain idempotent, and an already terminal job is never revived by a
late update or by register inventory.

This phase requires the same runner process and the same
`agent_instance_id`. Restarting the runner loses child/process-group handles
and cannot recover those jobs. Older runners that do not advertise
`job_state_reconciliation` keep the conservative immediate-`lost` disconnect
behavior (`legacy_runner_disconnected`) and never enter `recovering`; a
same-client new instance cannot take over their terminated jobs. There is no
generalized exactly-once command execution promise, no cross-runner or
cross-machine job migration, and `run_job` call-level idempotency remains a
separate future phase.

## Policy summary

`runtime_status` and `listAgents` expose a redacted summary for operators:

- `allow_raw_shell`
- `allow_cwd_anywhere`
- `allowed_roots`
- `max_timeout_secs`
- `max_output_bytes`

They do not expose tokens, full env, `Authorization` headers, complete `agent.toml`, or shell `init_script` values.

Policy default:

- If `allowed_roots` is missing or empty, it defaults to `$HOME`.
- Explicit `allowed_roots` replaces that `$HOME` default.

## Project ids

Agent-backed project ids are reported as:

```text
agent:<client_id>:<project_id>
```

The server routes project tool calls to the owning connected agent.

## Session SSH resources

Runners that can invoke local OpenSSH advertise the `ssh_shell` capability.
An old Runner that lacks it continues to serve ordinary local projects, while
a Session that selects an SSH resource receives a clear capability-unavailable
error rather than a local fallback.

The Server sends only the Session id and a named Runner-local resource in safe
execution metadata; it never sends an SSH host, SSH configuration, key,
password, agent socket, or connection object. The Runner resolves the name
from `[ssh.resources.<name>]` and invokes its own OpenSSH client, so Host
aliases and authentication remain local to that Runner.

The Runner may multiplex one authenticated transport per Session/resource/config
generation, but every `run_shell` and `run_job` uses a fresh remote exec
channel. A transport failure after dispatch is marked uncertain and is never
automatically retried. `run_shell`, `run_job`, `stop_job`, `job_status`, and
`job_log` retain their normal interfaces; file, Git, LSP, and checkpoint
requests are not redirected to SSH in this phase.

## LSP read-only navigation

Agents that support read-only LSP intelligence register the
`lsp_read_only_navigation` capability. The server sends only typed
`AgentLspRequest` operations: status, document symbols, go to definition, find
references, document diagnostics, hover, and workspace symbols. The agent
returns a versioned `AgentLspResultEnvelope` with a success result or a
structured error. Document
diagnostics use an instance-local bounded `publishDiagnostics` cache and report
whether the result is fresh or the shared two-second wait timed out.

Document-bearing operations accept project-relative `.rs` paths only. The agent
reads the validated regular file from the canonical project root, enforces the
LSP document byte cap before server startup, and sends disk-backed full-text
`didOpen` / `didChange` notifications. Models cannot supply document text or an
incremental edit payload. Workspace-symbol queries are trimmed, non-empty, and
bounded to 200 characters; result limits are clamped to 1..200.

For diagnostics, each server instance retains the latest publication for at
most 256 URIs and at most 500 raw diagnostics per URI. `fresh=true` means a
matching current document version or a publication newer than the prepare
generation was observed. `timed_out=true` is a successful stale/empty result,
not a transport error. Server unavailability and crashes remain structured LSP
errors. Hover and symbol results are normalized and bounded before transport.

There is no arbitrary LSP-method passthrough. The agent resolves requests only
inside the registered project boundary and runs the language server locally.
External, dependency, registry, and sysroot locations are omitted from public
results; absolute paths and file URIs are never returned.
An older agent that does not advertise `lsp_read_only_navigation` is treated as
unavailable for these tools and fails safely; its other supported operations
continue to work.

## Codex-specific workflows

WebCodex no longer exposes `run_codex` or legacy `/api/codex/*` routes. Agent lifecycle and project dispatch use structured runtime tools, agent-registered projects, bounded shell/job validation, MCP, and GPT Actions. Run Codex outside WebCodex when needed.
