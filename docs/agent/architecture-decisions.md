# Agent Architecture Decisions

Standing design context for agents working on WebCodex. **Executable constraints
live in [`AGENTS.md`](../../AGENTS.md).** This file explains durable product
structure so agents do not re-litigate settled shape during ordinary tasks.

Related product docs: [`ARCHITECTURE.md`](../ARCHITECTURE.md),
[`CONCEPTS.md`](../CONCEPTS.md), [`TESTING.md`](../TESTING.md).

---

## 1. Session dual model (architecture, not an operation checklist)

WebCodex has **two different "session" concepts**. They share a name in casual
speech but are **not interchangeable** and must not be merged by accident.
Full naming, lifecycle, compatibility, and non-goals:
[`session-model.md`](session-model.md).

### Workflow session (coding / tool ledger)

| Aspect | Contract |
|---|---|
| ID form | `wc_sess_*` |
| Purpose | Coding-task workflow: start/finish coding task, tool events, validation evidence, handoff |
| Storage | In-memory ledger with durable JSON-oriented session records (product surface for MCP / runtime tools) |
| Identity rules | Explicit `session_id` always wins; unknown id → `unknown_session_id`; no silent current-session fallback |
| Mutation policy | `inspect` denies structured writes and Landlocks shell/jobs; `read_only` denies write-like and shell/job-like tools; guard denial before mutation |

Do **not** change `wc_sess_*` ID format, ledger event shape, or lifecycle
semantics casually. Session / guard / current-session work must preserve the
invariants listed in `AGENTS.md` §6 (Architecture) and the session section.

### Action audit session (HTTP / operator audit)

| Aspect | Contract |
|---|---|
| ID form | UUID (HTTP action audit session) |
| Purpose | Operator/API action audit trail, idle timeout, transport-level audit |
| Storage | SQLite-backed audit records (not the coding ledger) |
| Isolation | Separate from workflow sessions; no automatic cross-reference today |

When docs or code say "session", identify which kind is meant. Cross-wiring
workflow ledger APIs to audit UUIDs (or the reverse) is a design change, not a
drive-by fix.

### Correlation decision (standing)

If the two systems are linked later, use **optional, explicit, one-way**
correlation only:

| Decision | Choice |
|---|---|
| Direction | Action Audit → `workflow_session_id: Option<String>` (`wc_sess_*`) |
| Authority | Store on the audit side (prefer event/record); Workflow Session does not own an Action Audit id list |
| Lifecycle | Independent; audit never drives Workflow create/close/guards |
| Inference | Forbidden from current Action Session, time, thread, connection, or current-session fallback |
| Missing field | Keep unlinked behavior |
| Bad format | Parameter error; never silent remap |

The optional correlation contract is summarized above; dual-model lifecycle and
identity rules live in [`session-model.md`](session-model.md).

### Authority decision layer (standing)

Authority is a **decision layer** for whether a consequential tool invocation
may proceed under the active mode. It is **not** a Workflow Session manager,
not Action Audit, and not lifecycle tracing.

| Layer | Owns |
|---|---|
| Authority | auto-authorize / deny outcomes (`trusted_agent` \| `restricted`) |
| Workflow Session | task context and bounded evidence (`wc_sess_*`) |
| Action Audit | HTTP/operator action facts (SQLite) |
| Lifecycle trace | optional request-path observation |

**Default mode is `trusted_agent`** (self-hosted single-operator product
default): no human wait, no approval interruptions, every permission-bearing
call still records an auditable decision. Hard safety (path, secrets, session
guards, scopes, agent policy) remains independent and **not overridable** by
authority mode.

**Implementation:** module under `src/tool_runtime/permissions/`;
authoritative single evaluation at ToolRuntime **dispatch** before mutation;
kernel reuses the attached decision and does not re-evaluate. Modes:
`trusted_agent` auto-authorizes after hard safety; `restricted` denies runtime
tools; unknown values and any set legacy `WEBCODEX_PERMISSION_MODE` fail
closed (see §6). Full contract: [`permission-model.md`](permission-model.md).

---

## 2. Internal API evolution (background)

WebCodex is an **internal / self-use** project. There are no supported external
API consumers, public SDKs, or third-party stable clients of the runtime tool
surface today.

Standing executable rules (also summarized in `AGENTS.md`):

1. Do not retain compatibility fields for hypothetical consumers.
2. Do not emit both a canonical field and an alias field for the same concept.
3. Do not add deprecated aliases, legacy fallbacks, dual-output shapes, or
   version-translation layers without a concrete migration requirement.
4. When duplicate representations are found, choose one canonical structured
   representation and delete the others from outputs, schemas, tests, and docs
   in the same change.

Before keeping any compatibility layer, name a **specific consumer** or a
**specific public contract**. A `version` (or parser version) field may identify
protocol shape; it is not a reason to keep duplicate or alias fields.

When external stable consumers genuinely exist later, revise this decision
explicitly and define a bounded migration window for that concrete contract.

---

## 3. Test organization guidance

Executable editing rules for tests live in `AGENTS.md`. Additional layout
guidance:

- Prefer a `tests/` submodule over large ordinary test blocks in production
  `mod.rs` files.
- `src/tool_runtime/mod.rs` must remain a runtime module, not a test warehouse.
  Domain groups under `src/tool_runtime/tests/` include `schema`, `tool_call`,
  `dispatch`, `sessions`, `checkpoint`, `files`, `git`, `jobs`, and `metadata`.
- Shared setup belongs in `tests/support.rs` or a narrow domain helper.
- Prefer table-driven tests for repeated matrices; keep exact assertions for
  security, destructive actions, required schema fields, session guards, and
  transport envelopes.
- Suggested soft limits: split files beyond ~2,000 lines or mixed domains;
  extract fixtures when a single test exceeds ~80 lines.
- After mechanical test moves, keep names and assertions stable first; semantic
  cleanup in a separate change.
- Use `#[ignore]` only for real external dependencies, long network behavior, or
  intentionally heavy integration; document why.

See also [`TESTING.md`](../TESTING.md).

---

## 4. Validation evidence semantics (product)

- Dedicated validation tools and `run_shell`/terminal `run_job` calls declaring
  `validation`, `test`, `build`, `format`, or `release` purpose project into the
  same bounded execution-evidence contract. Tool name is not the source of
  truth for whether validation occurred.
- Evidence carries execution source, stable assertion/command identity,
  purpose, bounded command summary, project-relative cwd, shell/executor,
  execution state, exit code, detected summary, bounded output metadata,
  timestamps, and failure classification. Full unbounded logs are not ledger
  evidence.
- Retry resolution is exact by stable identity. A later success resolves only
  failures for that identity; it never deletes or rewrites the historical
  failure and cannot resolve a different assertion.
- Closeout and review expose `historical_failures`, `resolved_failures`, and
  `unresolved_failures`. Resolved history is advisory; unresolved command/test
  failure is a hard blocker.
- `validation_summary` is a read of existing ledger evidence; it does not
  re-run Cargo/shell or replace `finish_coding_task`. Handoff and finish reuse
  this projection instead of building independent validation truth.

Closeout is an Agent-ready fact package, not a context-free engineering judge.
Its primary layers are `facts`, `hard_blockers`, and `advisories`. Ordinary
dirty worktrees, bounded truncation, optional validation not observed, and
resolved history are advisory. Permission/session-guard denial, unresolved
workspace conflicts, command/test failures, blocking active executions,
sensitive-path risks, and consistency errors are deterministic blockers.

---

## 5. Refactor preference (design stance)

- Prefer small, reviewable refactors over unbounded accretion when a module
  becomes a dumping ground.
- Do not mix behavior changes with mechanical moves unless unavoidable; report
  any semantic change explicitly.
- Do not preserve obsolete compatibility layers by default (see §2).
- Structural refactors that reduce coupling or clarify ownership are allowed
  when scoped to the task; unrelated broad rewrites are not.

---

## 6. Canonical two-mode authority with fail-closed legacy env rejection

The permission-mode system (`WEBCODEX_PERMISSION_MODE` with
`dev_auto_approve` / `audit_only` / `require_approval`) is replaced by one
canonical authority mode.

| Decision | Choice |
|---|---|
| Env var | `WEBCODEX_AUTHORITY_MODE` = `trusted_agent` \| `restricted` |
| Default (unset/empty) | `trusted_agent`; source reported as `default` |
| `trusted_agent` | Consequential runtime tools auto-execute after hard safety with no approval interruptions; external release actions remain user-task-scoped; every permission-bearing call records an auditable ledger decision (`policy=trusted_agent`, `status=auto_approved`, `reason=trusted_agent_authority`) |
| `restricted` | Runtime tools deny (`restricted_requires_human_authorization`); connector `commands_run` keeps the one-time human approval loop |
| Legacy env set | Invalid configuration; consequential tools fail closed with `invalid_authority_mode:...` and source `rejected_legacy_env:WEBCODEX_PERMISSION_MODE`. No alias, no migration |
| Shared surfaces | Both modes share the same tool implementations, schemas, session model, evidence, and audit records |
| Projection | `runtime_status` / `start_coding_task` report one canonical `authority` object; the old `permissions` profile object is deleted |
| Connector | Under `trusted_agent`, `commands_run` records a durable `authority_auto_authorized` task event instead of approval records or `approval_required` interruptions |

Hard boundaries are never relaxed by authority mode: OAuth scopes, project
boundary/allowed roots, explicitly read-only sessions, path and sensitive-path
policy, concurrent-overwrite guards, credential redaction, job cancel/reclaim,
and immutable release targets. Full contract:
[`permission-model.md`](permission-model.md).

---

## 7. Connection layers are an observation contract

`runtime_status.connection_layers` reports facts that were actually observed;
it never infers readiness from configuration.

| Decision | Choice |
|---|---|
| Layer envelope | Every layer carries `{status, observed_at, source, age_secs, stale_after_secs, reason_code}` plus layer facts |
| No config-inferred readiness | `connector_endpoint` readiness comes only from readiness probes or successful connector requests; configuration presence never implies `ready`. `runner_process` never fakes "running"; a stale registration is never presented as callable |
| Process-local binding honesty | Session bindings are process-local, principal+transport scoped. `runtime_status` reports `not_observed` with `binding_is_process_local_and_principal_scoped`, `process_local=true`, `lost_after_restart=true`; `start_coding_task` reports `bound`/`not_bound`. After a server restart, the correct action is to continue with the explicit durable `wc_sess_*` session id, not to restart the runner |
| Meaningful-activity rule | `last_successful_tool_call` records only successful meaningful calls, scoped by principal/project/surface/session/tool. `runtime_status`, `list_tools`, `list_agents`, `list_projects`, and `tool_manifest` never refresh it. Bounded in-memory store; no arguments, outputs, or secrets |
| Independence | Layers degrade independently; `not_observed` on one layer must not be collapsed into a global offline verdict |

---

## 8. Startup projection is detail-only (hard cut)

`start_coding_task` accepts exactly one projection control:
`detail=minimal|standard|full`.

| Decision | Choice |
|---|---|
| Removed | `compact_startup`, `include_runtime_status`, `include_git`, `include_recent_commits`, `include_rules`, `include_tool_manifest`, `tool_manifest_intent`, `tool_manifest_categories`, `tool_manifest_limit` — removed from the wire and internals |
| `full` | Full runtime status + recent commits + rules + tool manifest + recommended flow |
| `minimal` | Additionally omits the `authority` block |
| Unknown/legacy fields | Strict unknown-field error; no silent acceptance |

No alias or dual shape is kept for the removed flags (consistent with §2).

---

## 9. Mixed-version diagnostics without compatibility fallback

Runner registration reports `process_started_at` and
`build {version, git_commit}`; `runtime_status` projects
`version_compatibility`.

| Decision | Choice |
|---|---|
| Shape | `{status: compatible \| version_mismatch \| capability_mismatch \| no_runners, server: {version, build}, runners: [{client_id, agent_protocol_version, protocol_supported, build_version, build_git_commit, build_matches_server, status, reason_code, action}]}` |
| Connected ≠ compatible | Transport liveness never implies protocol/build compatibility |
| Direction | Per-runner facts say which side to upgrade (`action`); no fallback shims or version-translation layers |
| Shell dialects | `ShellProfilesSummary` reports `default_dialect` (`sh` \| `bash` \| `custom`) and `available_dialects`; each profile entry reports `dialect`. The server never guesses the remote shell; custom profiles that do not map to sh/bash report `custom`, and agents needing deterministic syntax must pass an explicit `shell=sh\|bash`. No PATH/env/init-script contents are ever sent |
