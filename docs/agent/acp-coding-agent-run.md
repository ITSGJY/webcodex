# ACP Coding Agent Run Contract

This note defines the P0 architecture baseline for running an Agent Client
Protocol (ACP) coding agent from WebCodex. It is deliberately narrower than a
public implementation: the goal is to fix the execution, identity, lifecycle,
configuration, permission, observation, and recovery semantics that P1 must
preserve.

The product model is a **protocol-aware detached Job**, but the product object is
not a WebCodex Job. `CodingAgentRun` is a separate execution primitive whose
payload is ACP protocol state and structured coding-agent activity rather than a
shell command plus stdout/stderr.

This contract is based on current WebCodex V1 architecture plus real dogfood of
the official `agentclientprotocol/codex-acp` adapter on 2026-08-23. The dogfood
used `npx -y @agentclientprotocol/codex-acp`, package version **1.6.2**, without a
global install or credential changes. Protocol details below describe that
observed implementation and the current ACP surface; P1 must continue to
negotiate and validate rather than hard-code incidental adapter behavior.

## 1. Product model

The intended user flow is:

```text
ChatGPT / API client
  -> coding_agent_start
  -> exact WebCodex Project + Runner + configured ACP provider
  -> CodingAgentRun R
  -> Runner-owned ACP child
  -> initialize
  -> session/new
  -> optional validated session/set_config_option calls
  -> session/prompt
  -> structured session/update observations
  -> correlated terminal prompt response
```

WebCodex owns:

- exact Runner and registered Project routing;
- provider identity and stale-provider fencing;
- run admission and idempotent initiation;
- bounded lifecycle, timeout, cancellation, and process-tree cleanup;
- sanitized structured observation;
- provenance, authorization, audit, and telemetry;
- recovery classification when transport or process state is uncertain.

The ACP agent owns:

- coding reasoning and planning;
- its own shell/edit/tool decisions;
- its own sandbox and approval behavior;
- account, organization, model, and provider policy;
- provider-specific coding behavior.

WebCodex must not turn ACP into a second implementation of WebCodex file, shell,
or patch tools. The ACP child is a delegated local coding agent, not an MCP tool
provider and not a raw JSON-RPC endpoint exposed to remote callers.

## 2. Four identities that must remain separate

```text
Workflow Session W
      |
      | optional provenance / evidence relationship only
      v
CodingAgentRun R          model-visible: wc_agent_run_...
      |
      | Runner-private protocol execution
      v
ACP session S             never model authority
      |
      v
Codex / future ACP agent

WebCodex Job J             independent process-execution primitive
```

### Workflow Session

A Workflow Session remains the bounded coding-task evidence and collaboration
ledger described by `session-model.md`. A Run may record the `wc_sess_*` that
initiated it, but that reference is provenance only.

Knowing a Workflow Session id does not authorize a Run. Recorder metadata must
never be accepted as Run authority, and a Run must continue to enforce its own
caller/project/provider ownership on observe and cancel.

### CodingAgentRun

`CodingAgentRun` is the WebCodex business identity for one admitted autonomous
coding turn. The future public id should be opaque and WebCodex-owned, for
example `wc_agent_run_*`. It is the only agent-execution identity a model needs
to retain after successful admission.

A Run owns the bounded normalized event history, current state, exact Project,
exact Runner instance, exact ACP provider instance/revision, initiation intent,
optional Workflow Session provenance, and the Runner-private ACP session id.

### ACP session

The raw ACP `sessionId` is a provider/protocol identity. It must stay Runner
private and must never be accepted as Project, Workflow Session, or Run
authority.

The current Codex adapter advertises `loadSession`; P0 dogfood verified that a
session created in one `codex-acp` process could be loaded in a fresh adapter
process and then accept another prompt. That is a useful future recovery
building block, but it is not an idempotency guarantee for an in-flight prompt.
P1 must not expose the ACP session id or rely on it as the product Run id.

### Job

A WebCodex Job is stdout/stderr/process/exit oriented. A CodingAgentRun is
agent-message/reasoning/tool/file/terminal/usage/permission oriented. Reusing the
Job record would either discard ACP structure or overload Job semantics with a
second event model.

P1 should reuse proven Job machinery where the semantics really match:

- `ManagedChild` process-tree ownership and cleanup;
- bounded timeout and cancellation patterns;
- detached initiation/idempotency concepts;
- opaque observation-token patterns;
- Runner inventory and same-instance reconciliation concepts;
- existing `execution_state`, `failure_kind`, and `recovery_kind` vocabulary.

It should not serialize ACP updates into stdout/stderr or make `job_id` an alias
for `run_id`.

## 3. P0 real Codex ACP dogfood

### Environment

The probe ran from the registered ACP worktree using the existing local Codex
authentication. It did not read, print, replace, or migrate credential files.
No global npm package was installed.

Observed adapter identity:

```text
@agentclientprotocol/codex-acp 1.6.2
ACP initialize protocolVersion = 1
agentInfo.name = @agentclientprotocol/codex-acp
agentInfo.title = Codex
agentInfo.version = 1.6.2
```

The adapter advertised auth, load-session, MCP, prompt, provider, and session
capabilities. `session/new` returned a private `sessionId` plus modes, models,
and config options.

The current official adapter source maps its advertised `agent` mode to Codex
`approvalPolicy=on-request` with a workspace-write sandbox and network access
disabled. This is adapter/provider behavior: WebCodex did not set those values in
the P0 no-override probes. It is the concrete reason that "no WebCodex override"
must not be documented as "identical to bare Codex CLI defaults".

### Successful no-tool turn

A prompt explicitly forbidding tool use completed with `stopReason=end_turn`.
While the `session/prompt` request was outstanding, the client observed
`session/update` notifications including:

- `available_commands_update`;
- `session_info_update`;
- `agent_message_chunk`;
- `usage_update`.

No permission request occurred in that turn.

### Read-oriented coding turn and permission behavior

A safe prompt asked Codex to inspect a small amount of repository source and
answer an architecture question without modifying files. The adapter emitted
structured updates including:

- agent thought chunks;
- agent message chunks;
- tool calls and tool-call updates;
- usage updates.

In that run the adapter also issued **two** `session/request_permission` client
requests. Each advertised options including `allow_once`, `allow_always`, and
`reject_once`. The disposable headless probe deliberately returned a cancelled
permission outcome rather than auto-allowing; the prompt subsequently terminated
with `stopReason=cancelled`.

Other safe probes did not always produce a permission request. Permission
callbacks are therefore real but contextual. They are not a sound basis for
assuming that every read/tool action asks, nor for assuming that the path never
asks.

### Cancel

P0 sent `session/cancel` during an active no-tool prompt. The correlated
`session/prompt` response completed with `stopReason=cancelled`. The same ACP
session subsequently accepted another prompt that completed with `end_turn`.

For WebCodex, cancellation therefore means cancellation of the active Run/turn.
It must not be modeled as authority to destroy or expose the underlying ACP
session object.

### Config options

The current adapter advertised these config ids in the dogfood environment:

```text
mode
collaboration_mode
model
reasoning_effort
fast-mode
```

The observed current/default mode was `agent`, with advertised mode values:

```text
read-only
agent
agent-full-access
```

An invalid `session/set_config_option` id returned JSON-RPC error `-32602`
(`Invalid params`). Setting the advertised `mode` option to the advertised
`read-only` value succeeded and returned refreshed `configOptions`.

These names and values are observations, not WebCodex enums. P1 must validate
against the exact options advertised by the selected live provider session.

### Adapter process environment

With the ordinary current process environment, real prompts completed. A
separate probe launched the adapter with only `HOME` and `PATH`; `initialize`
and `session/new` succeeded, but `session/prompt` did not complete within the
probe deadline.

P0 did not isolate which environment dependency was missing, so the contract
must not claim one. The architectural conclusion is narrower: an ACP provider
cannot assume that a universal two-variable environment is sufficient. The
Runner must let the operator explicitly map required host environment through a
bounded `env_from_env` configuration while continuing to prohibit remote
caller-supplied environment and secret values.

## 4. Protocol facts P1 depends on

### Transport and correlation

The official Codex adapter accepted newline-delimited JSON-RPC 2.0 over stdio.
Each request/response pair carries a JSON-RPC id. Notifications such as
`session/update` and `session/cancel` are not terminal acknowledgements.
Agent-to-client requests such as `session/request_permission` have their own
request ids and require a correlated client response.

Identity lifetime is intentionally asymmetric:

| Identifier | P0 lifetime conclusion |
|---|---|
| JSON-RPC request id | Connection/process-local correlation only; never recovery or authority identity. |
| ACP `sessionId` | Provider-private. Current Codex ACP can persist/load it across adapter processes, but that is a provider capability, not a universal ACP/WebCodex guarantee. |
| ACP provider instance id | WebCodex Runner process/provider-instance fence; replacement makes old requests stale. |
| `wc_agent_run_*` | Future WebCodex business identity, retained independently of one HTTP/MCP request and reconciled only from authoritative Runner Run state. |
| Workflow `wc_sess_*` | Independent evidence/collaboration identity; optional Run provenance only. |

The Runner must own the JSON-RPC id space/correlation machinery. Remote callers
must never provide a JSON-RPC method or id.

### Initialize and version negotiation

The client sends `initialize` with its supported ACP protocol version and client
capabilities, and validates the returned negotiated version and advertised
capabilities before creating a session. Unsupported/malformed negotiation is a
pre-prompt failure and must fail closed.

### Session creation

For P1 the Runner supplies the exact registered Project root as the `session/new`
`cwd`. The caller cannot provide an arbitrary cwd. MCP server inputs, if any are
supported later, are Runner-owned; P1 should send only the closed configuration
it explicitly supports.

The new-session response supplies the private session id and current advertised
modes/models/config options. These are provider observations, not authority.

### Prompt lifecycle

`session/prompt` is one correlated long-running request. `session/update`
notifications may arrive before its response. The correlated prompt response
with a `stopReason` is the terminal protocol result for the turn.

There is no separate P0-observed durable acknowledgement proving exactly when a
prompt became safe to retry. Once the Runner has successfully written/flushed
the prompt request, loss of the ACP process/stdio or its correlated response can
leave the effect uncertain.

### Cancel

`session/cancel` is a notification naming the private ACP session. P1 should
send it only from Runner-owned state, then continue bounded observation for the
prompt's correlated terminal response. A successful write of the cancel
notification alone is not proof that cancellation took effect.

### Permission requests

`session/request_permission` is an ACP agent-to-client request, not a WebCodex
runtime-tool permission evaluation. The client must implement it because the
current Codex adapter can actually send it. It must never default to allow.

Current `codex-acp` bridges Codex approval activity through this callback and
fails closed when the ACP client's approval interaction fails or is cancelled.
This reinforces the product boundary: WebCodex performs one admission decision
for the Run, then the delegated agent owns its normal internal coding policy;
WebCodex does not rerun `PermissionEvaluator` for every ACP tool action.

### Error and process-exit semantics

Request-level protocol failures are JSON-RPC errors; terminal prompt outcomes
are represented by the correlated prompt result/stop reason. The ACP child
process has a separate OS lifetime. Process exit is diagnostic/lifecycle input,
not a substitute for a terminal prompt result.

If the child exits after prompt dispatch without a correlated prompt response,
WebCodex cannot infer success or failure from its exit code alone. The Run must
be treated as uncertain unless exact protocol recovery proves otherwise.

## 5. Configuration semantics

### `config` omitted or `{}` means no WebCodex override

P0 corrects an important earlier assumption. For WebCodex:

```text
config omitted
or
config = {}
```

means **send no `session/set_config_option` calls**.

It does not mean "the ACP adapter behaves exactly like a bare local Codex CLI".
The adapter itself may have defaults. In real dogfood, `codex-acp 1.6.2`
advertised current `mode=agent`; that mode is adapter/provider policy, not a
WebCodex override.

Therefore the precise inheritance contract is:

> WebCodex inherits the selected Runner-owned ACP provider's effective defaults
> by abstaining from run-level ACP config overrides.

The Run should record a bounded sanitized snapshot of the effective advertised
config ids/current values needed for diagnosis. It must not claim that those
values came directly from `~/.codex`, an account, or organization policy.

### Explicit run-level overrides

For a non-empty caller `config` object P1 must perform this order:

1. start/initialize the exact configured provider;
2. create the private ACP session;
3. obtain the session's advertised config options;
4. reject any caller key not currently advertised;
5. reject any value not legal for that advertised option;
6. apply Runner/operator allow/deny policy for remotely overridable options;
7. call `session/set_config_option` only for approved explicit overrides;
8. validate the returned refreshed config options;
9. only then dispatch the prompt.

An invalid override is `not_started` with `recovery_kind=fix_input`; it must not
partially begin the coding prompt.

The initial P1 operator policy should be an explicit allowlist of option ids (and,
where needed, values) rather than a generic policy language. Unknown newly
advertised options remain non-overridable until the operator permits them.

### Fields remote callers never control

`coding_agent_start` must not accept:

- executable or argv;
- arbitrary environment or secret/API-key material;
- arbitrary cwd;
- transport selection;
- raw ACP JSON-RPC method/params/id;
- raw ACP session id.

Those belong to Runner configuration or closed Runner protocol.

## 6. Runner-owned ACP provider configuration

ACP should get a new narrow Runner section rather than being placed under
`[mcp]` or the existing Claude MCP tool-provider router. ACP is a bidirectional,
long-lived coding-agent protocol with callbacks and structured turn state; the
MCP gateway is a `tools/list`/`tools/call` provider surface. Reusing the latter
would collapse distinct semantics.

A minimal P1 configuration is:

```toml
[acp]
max_concurrent_runs = 1

[[acp.agents]]
id = "codex"
name = "Codex"
executable = "/runner/owned/path/to/codex-acp"
args = []

[acp.agents.env_from_env]
# Explicit operator mappings only. Values never go to the Server.
HTTPS_PROXY = "HTTPS_PROXY"
```

The exact executable example is operator-specific; WebCodex must not prescribe
`npx -y` as a production default or download packages at request time.

P1 should make `[acp]` startup/restart-owned, following the simpler precedent of
the static MCP gateway rather than adding hot replacement immediately. A config
change therefore requires Runner restart. This avoids creating a new dynamic
provider-reload framework for one consumer.

Each advertised provider needs bounded sanitized identity such as:

```text
provider_id
provider_instance_id   # opaque, fresh for the Runner/provider process lifetime
provider_revision      # bounded revision of the sanitized advertisement
name
available/configured capability facts only
```

A new Runner process or provider replacement gets a new instance identity. Any
Server request naming an old provider instance/revision fails before starting a
Run. The Server must never receive executable path, argv, PID, environment
values, credential material, stderr, local config contents, or raw ACP auth
data.

`env_from_env` is resolved only on the Runner immediately before spawn. The
caller supplies neither source names nor values. P1 must preserve existing
secret-redaction rules; it must not log resolved environment values.

Admission capacity is an ACP-run plane, not Job concurrency. P1 should use a
small bounded `max_concurrent_runs` and **reject before start when full** rather
than add queueing/scheduling states. Do not infer ACP capacity from
`max_concurrent_jobs` and do not build a worker pool.

## 7. Project binding and confinement truth

Project binding gives WebCodex three real guarantees:

1. the selected Run is routed to the exact Runner owning the registered Project;
2. the Run records that exact Project identity;
3. `session/new.cwd` starts at the Runner-authoritative Project root.

That is not a filesystem sandbox.

`cwd == Project root` does **not** prove that the delegated agent can read or
write only that tree. ACP itself is not a filesystem confinement mechanism.
The selected coding agent may apply its own sandbox, OS policy, account/org
policy, and approval mode; those controls can be stronger or weaker than
WebCodex file-tool path rules and may evolve independently.

For the current Codex adapter, the effective mode influences Codex sandbox and
approval behavior. WebCodex may report the provider's sanitized advertised
configuration, but it must not translate that into a claim of WebCodex Project
isolation unless WebCodex separately enforces such isolation.

The P1 product description should therefore say **operator-configured delegated
local coding agent**. It must not promise parity with WebCodex `read_file` /
`apply_text_edits` filesystem isolation.

## 8. Permission-request exceptional path

The normal WebCodex authority decision happens once at `coding_agent_start`:

```text
caller auth + exact Project + provider fence + config override policy
  -> WebCodex start admission decision
  -> ACP Run starts
  -> delegated agent applies its own coding policy
```

An ACP `session/request_permission` callback is not fed back through the normal
WebCodex `PermissionEvaluator`, because that would create a second per-action
policy layer over the agent's own approval system.

P1 nevertheless must implement the callback. The minimum safe behavior is:

- emit a bounded sanitized `permission_request` Run event;
- enter `waiting_permission` while a bounded response deadline is active;
- never choose an allow option automatically;
- because P1 has no public permission-response tool, resolve the deadline
  fail-closed by returning the protocol's cancelled/rejected outcome available
  for that request;
- then continue observing the same prompt until it reaches a terminal result or
  becomes lost.

This makes P1 safe but intentionally incomplete for providers/configurations that
frequently require interactive approval. A later operator UI or model-visible
permission-response capability requires separate evidence and authority design;
it is not part of P0/P1 by implication.

## 9. Minimal CodingAgentRun lifecycle

Use exactly these product states initially:

```text
starting
running
waiting_permission
completed
failed
cancelled
lost
```

Keep prompt dispatch certainty as structured execution metadata rather than
multiplying states. Reuse the existing concepts `not_started`, `started`,
`completed`, and `outcome_unknown` where applicable.

| State | Coding execution / prompt fact | Retry rule | Recovery |
|---|---|---|---|
| `starting` | Run admitted; provider/session/config setup may be in progress. Prompt may still be `not_started`. | Never create a second Run for the same initiation key; observe/reconcile the admitted Run. | `wait` or `reobserve`. |
| `running` | Prompt request was dispatched; agent may have accepted it. | No blind prompt retry. | `reobserve`; after transport failure use `reconcile`. |
| `waiting_permission` | Prompt is active and a real ACP permission callback is pending. | No prompt retry and no automatic allow. | `wait`; P1 fail-closes the permission deadline. |
| `completed` | Correlated terminal prompt result proves normal terminal completion. | No retry of the same Run. | `none`. |
| `failed` | Deterministic terminal failure is known, or setup failed before prompt dispatch. | A new initiation is allowed only when failure metadata proves `not_started`; otherwise caller must not infer retry safety. | Usually `fix_input`, `retry_same`, or `none` based on the concrete failure. |
| `cancelled` | Cancellation reached a correlated terminal cancelled result, or the Run was cancelled while prompt was provably not started. | Do not resend the cancelled prompt as a retry. | `none`. |
| `lost` | No terminal prompt result is available and exact continuation cannot currently be proved. Prompt may have run. | Never blind retry. | `reconcile` / `reobserve`; create a new Run only after authoritative evidence establishes safety or the user intentionally requests new work. |

`lost` is an uncertainty state, not proof that the coding process had no effect.

A timeout is not a separate initial state. On a Run deadline, request cancellation
and wait for a bounded terminal result. A correlated cancelled result becomes
`cancelled`; losing the process/transport before correlation becomes `lost`.
Setup deadline failures before prompt dispatch become `failed` with
`execution_state=not_started`.

## 10. Initiation and retry safety

`coding_agent_start` is consequential autonomous execution. It needs a required
bounded caller-chosen `idempotency_key`, using the same semantic pattern as
`run_detached_process` but a separate CodingAgentRun namespace.

The key should be scoped to authenticated principal and initiation intent and
resolve to one stable `run_id` while that Run is active or retained. Replaying
the same key with the same normalized intent returns/observes that Run and
cannot dispatch the prompt twice. Reusing it with a different Project, provider,
prompt, or config intent fails with an idempotency conflict before dispatch.

The initiation key is not the Run id, not authority, and should not be copied to
the Runner as a credential. The Server can derive/mint the stable Run identity
before dispatch and send the closed Run intent to the exact Runner.

The most important uncertainty rule is:

```text
session/prompt successfully dispatched
+ ACP transport/process lost before correlated terminal response
= outcome_unknown / Run lost
!= retry session/prompt
```

`session/load` support does not change this rule. Loading the conversation can
recover a durable ACP session, but it does not by itself prove whether an
in-flight prompt completed, partially acted, or never ran.

## 11. Observation delta contract

The planned model surface is:

```text
coding_agent_start
coding_agent_observe
coding_agent_cancel
```

P0 does not implement these tools.

`coding_agent_observe` should accept:

```text
run_id
 after_observation_token?  # opaque, exact Run-bound
 wait_secs?                # one bounded wait
```

The Runner is authoritative for the Run's bounded event ring and monotonically
ordered observation revision/sequence. The Server returns normalized only-new
events when continuity is provable. It never returns the full transcript on
every call and never exposes raw ACP JSON-RPC.

Initial normalized event kinds:

```text
agent_message
reasoning
plan
tool_activity
file_change
terminal_activity
usage
permission_request
terminal
```

Not every provider must emit every kind. P1 maps only protocol/provider updates
whose semantics are understood; unknown raw update variants are ignored or
recorded as a bounded diagnostic count, not forwarded verbatim.

The response must make retention explicit:

- token is opaque and bound to exactly one `run_id`;
- no token returns a bounded current baseline, not unbounded history;
- token calls return only retained changes after that cursor;
- `history_lost=true` when the requested cursor predates reconstructable retained
  events;
- `has_more`/continuation advances only through the last returned event;
- `wait_secs` is one bounded wait, not a stream or subscription;
- serialized model output has a fixed budget;
- terminal state is returned even when there are no new textual events.

Raw reasoning and tool payloads can contain sensitive or very large data. P1
must define per-event bounds/redaction and retain only sanitized normalized
evidence needed for model observation and audit. Environment values, auth data,
absolute provider executable paths, raw credentials, and arbitrary stderr are
never event content.

## 12. Cancel, timeout, restart, and replacement

### Cancel

`coding_agent_cancel` requires exact Run authorization and should be idempotent.
For an active prompt the Runner sends `session/cancel` once, then observes the
same prompt toward terminal state. It must not launch a replacement session or
prompt.

### Control Server restart

A surviving Runner process can retain a Run and its event ring independently of
the Server request that started/observed it. P1 should extend the established
Runner reconciliation pattern with a bounded active/recent-terminal
CodingAgentRun inventory. The same Runner `agent_instance_id`, exact `run_id`,
Project, provider instance/revision, state, and observation revision are the
recovery authority.

A Server restart invalidates process-local waits/tokens as needed, but it must
not imply that the Run should be restarted. Re-observation should return a
bounded reset/baseline and fresh token when the Run is still authoritative.

### Runner restart / provider replacement

A new Runner process has a new `agent_instance_id` and new ACP provider instance
identity. P1 does not claim transparent active-Run recovery across that boundary.
An active Run whose owning Runner/provider instance disappears becomes `lost`
unless exact future recovery proves continuity.

Although current Codex ACP can load a durable session in a new adapter process,
P1 should defer automatic cross-process Run recovery. `session/load` alone does
not provide exact in-flight prompt reconciliation, and guessing would risk
re-executing coding effects.

A stale Server request carrying an old provider instance/revision must fail
closed before spawning or retargeting any agent.

## 13. Authorization and scope recommendation

ACP delegated autonomous coding is a new externally reachable authority. It must
have a distinct coarse scope, recommended:

```text
coding_agent:run
```

Do **not** infer it from any of:

```text
project:write
job:run
job:detach
mcp:local
```

Those scopes authorize different primitives. Giving an existing credential the
ability to start an autonomous coding agent merely because it can edit a file,
run a Job, or call a local MCP tool would silently broaden authority.

For the P1 public vertical slice:

- start requires `coding_agent:run` plus normal authorization for the exact
  writable Project;
- observe/cancel require the same Run visibility/ownership and exact Project
  boundary; knowing `run_id` is never sufficient;
- Workflow Session provenance grants no additional authority;
- provider selection is constrained to the sanitized providers advertised by
  the exact Runner instance.

One new scope is enough initially. Do not add separate run/observe/cancel scopes
without a demonstrated consumer that needs that split.

P0 does not modify OAuth or token issuance.

## 14. P1 exact vertical slice

P1 should implement one Codex-capable vertical slice, not a generic plugin
framework or a second provider.

### Core / Runner protocol

Add a narrow transport-neutral coding-agent protocol module (for example
`crates/webcodex-core/src/coding_agent.rs`) with bounded typed values for:

- sanitized ACP provider advertisement and instance/revision identity;
- Run start intent/result;
- Run observe request/result and normalized events;
- Run cancel request/result;
- bounded active/recent-terminal Run inventory for same-Runner reconciliation.

Extend `ShellClientCapabilities` / Runner registration with a coding-agent-run
capability and sanitized provider inventory. Extend the existing agent transport
with a closed typed coding-agent operation/update path. Do not tunnel arbitrary
ACP JSON-RPC.

### Runner

Add a focused ACP module under `crates/webcodex-runner` that:

- parses `[acp]` / `[[acp.agents]]` startup configuration;
- validates ids, executable/argv bounds, explicit `env_from_env`, and concurrency;
- creates process-ephemeral provider identity/revision;
- spawns each admitted Run with `ManagedChild` and owns the whole process tree;
- uses project root as `session/new.cwd`;
- performs `initialize`, `session/new`, validated explicit config overrides, then
  one `session/prompt`;
- handles `session/update`, `session/request_permission`, prompt response,
  `session/cancel`, protocol faults, and process exit;
- retains a bounded normalized event ring and active/recent-terminal Run inventory;
- rejects stale provider instance/revision and over-capacity starts before prompt
  dispatch;
- drains/discards bounded diagnostic stderr without projecting secrets.

Do not reuse `McpGatewayManager` itself: its callbacks are intentionally
unsupported and its request model is synchronous `tools/list`/`tools/call`.
Reuse its good patterns for provider fencing, `ManagedChild`, stdio bounding,
secret-free advertisement, and dispatch certainty.

### Server runtime

Add a `CodingAgentRun` registry/runtime path separate from `jobs.rs`, with:

- stable `wc_agent_run_*` identity derived/admitted before Runner dispatch;
- detached-style idempotency conflict checking;
- exact Project/Runner/provider binding;
- lifecycle and recovery metadata using existing vocabulary;
- same-Runner inventory reconciliation after Server restart;
- bounded observation-token encoding and serialized-output enforcement;
- optional Workflow Session recording/provenance only.

### Public tool layer

Only after the internal vertical slice is typed and tested, add exactly:

```text
coding_agent_start
coding_agent_observe
coding_agent_cancel
```

`coding_agent_start` inputs should be limited to Project, provider id/instance
observation, required idempotency key, prompt/instruction, optional explicit
validated `config`, timeout, and optional Workflow Session provenance as
appropriate. It must not accept executable/argv/env/cwd/transport/raw RPC.

Add the new `coding_agent:run` scope to the normal scope registry and OAuth
issuance surfaces in the same P1 vertical slice, with explicit tests proving
legacy `project:write`, Job, and MCP credentials do not inherit it.

### Focused validation

P1 tests should use a fake bounded ACP stdio process for deterministic protocol
coverage and one opt-in real Codex ACP smoke for compatibility. Cover at least:

- initialize/new/prompt/update/terminal normalization;
- invalid and valid config override sequencing;
- permission request never auto-allows;
- cancel -> correlated terminal cancellation;
- provider crash before vs after prompt dispatch;
- stale provider instance/revision;
- idempotent start replay and intent conflict;
- event retention/history loss and token Run binding;
- Server restart with same Runner inventory;
- Runner/provider replacement -> lost/fail closed;
- project/scope/Workflow-Session authority boundaries;
- environment redaction and bounded serialized output.

## 15. Explicitly deferred

P0/P1 do not imply:

- Claude as a second ACP provider;
- browser-hosted ACP sessions;
- raw ACP JSON-RPC tools;
- model-visible permission-response tooling;
- automatic permission allow;
- hot ACP provider reload/generic provider framework;
- ACP v2 or experimental extensions;
- scheduler, worker pool, automatic worker spawning, or orchestration;
- Room/Discussion/Participant/presence/typing;
- durable Operation DAG;
- unification of Workflow Session, Job, ACP Session, and CodingAgentRun;
- a claim that Project cwd is a filesystem sandbox;
- automatic `session/load` recovery of uncertain in-flight prompts;
- tool slimming or unrelated model-ergonomics work.

## 16. P0 decisions

The P0 architecture baseline is therefore:

1. `CodingAgentRun` is a separate protocol execution primitive, not a Job alias.
2. Workflow Session is optional evidence provenance, never Run authority.
3. Raw ACP session ids stay Runner-private.
4. Default WebCodex config inheritance means **no ACP config override calls**;
   effective behavior is whatever the configured provider advertises.
5. Explicit config is validated against live advertised options and Runner policy
   before prompt dispatch.
6. ACP permission callbacks are real and exceptional; never auto-allow them and
   do not rerun WebCodex `PermissionEvaluator` per agent action.
7. Project root selects initial cwd but is not a filesystem security boundary.
8. Observation is a bounded normalized event delta with opaque Run-bound tokens,
   not a transcript replay or raw ACP stream.
9. After prompt dispatch, transport/process loss is outcome-unknown; no blind
   retry, including when `session/load` exists.
10. Runner/provider instance fencing and same-Runner reconciliation follow the
    established Job/MCP safety patterns without reusing those semantic objects.
11. ACP execution receives a new `coding_agent:run` scope rather than inheriting
    `project:write`, Job, or MCP authority.
12. P1 is one exact Codex vertical slice with typed closed Server<->Runner
    protocol and three eventual model tools, not a generic agent framework.
