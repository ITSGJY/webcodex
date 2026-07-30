# Session Model — Two Non-Interchangeable Concepts

WebCodex uses the word **session** for two independent systems. They share
casual vocabulary only. They must not be merged, cross-wired, or inferred from
each other.

Executable constraints that agents must obey live in
[`AGENTS.md`](../../AGENTS.md) §7, **Sessions**. Standing architecture summary:
[`architecture-decisions.md`](architecture-decisions.md) §1.

---

## Formal names

| Formal name | Casual aliases (avoid in design) | Implementation home |
|---|---|---|
| **Workflow Session** | coding session, tool ledger session, `wc_sess_*` session | `tool_runtime::sessions` |
| **Action Audit Session** | HTTP action session, audit session, operator action trail | Internal module `action_audit_sessions` (SQLite table still named `action_sessions` for compatibility) |

When writing code, docs, or reviews, prefer the formal names above. If a
statement is true for only one kind, name that kind explicitly.

---

## Project Connector continuity is not a third session type

The ordinary project-bound product path uses existing durable Connector Tasks
and task events. A lightweight SQLite map associates the hashed client-window
identity, authenticated subject, exact Connector project, and canonical-root
hash with one current durable task. It does not create another event ledger and
must never be cross-wired to either session system below.

`task_start` resolves get-or-create/continue context without duplication:

- no mapping creates a durable Connector Task;
- an active exact mapping appends a `task_instruction` event to that task;
- changing repository activates a separate mapping without closing the first;
- returning to the repository restores its mapping;
- a read-only-to-write transition rechecks project-write authority and upgrades
  the same task's execution workspace;
- a terminal task advances that repository mapping to a new task while keeping
  old history.

Raw window identifiers are neither tool arguments nor stored data. MCP uses the
server-minted `Mcp-Session-Id` from initialize; hosted Actions use their
conversation-scoped request header; other HTTP clients use a server-minted
HttpOnly cookie and one cookie jar per logical window. Only a domain-separated
SHA-256 key is stored. Every lookup is also scoped by authenticated subject,
Connector project id, and canonical-root hash. The process-local
current-project navigation map is intentionally separate from the durable
per-repository task mapping.

Restart recovery has a strict boundary: task history and the durable exact
mapping survive; current navigation does not. A retained MCP header or HTTP
cookie can recover the exact repository. Missing identity never falls back to a
user, credential, project name, or repository path. MCP rejects anonymous
`task_start`; HTTP clients that discard cookies require explicit task recovery.
When `task_resume` has a new stable window identity, it moves the lightweight
binding to that window without copying history or sharing one active task
between two windows.

---

## 1. Workflow Session

### Purpose

Bounded **coding-task continuity and evidence** for MCP, GPT Actions, and
runtime tools. It records what happened in a task so review, validation,
handoff, and finish can reason about the same unit of work.

### Responsibilities

- Coding task start / finish lifecycle
- Tool-call evidence (bounded, redacted)
- Checkpoint-related task continuity
- Session-local message board
- Validation evidence and closeout summaries
- Handoff / finish tooling (`session_handoff_summary`, `finish_coding_task`, …)

### Identity

| Aspect | Contract |
|---|---|
| ID form | `wc_sess_*` (`SESSION_ID_PREFIX`) |
| Business field | `session_id` on tools that take a workflow session as input |
| Coding resume field | `resume_session_id` on `start_coding_task`; distinct from ordinary project-tool `session_id` |
| Recorder field | `recording_session_id` on generic wrappers (metadata only; stripped before concrete tool dispatch) |

### Storage and ownership

| Aspect | Contract |
|---|---|
| Module | `tool_runtime::sessions` (model, store, events, JSON persistence) |
| Primary store | In-memory session store |
| Durability | JSON-oriented session ledger (bounded events/messages per session) |
| Current-session binding | In-memory exact-key cache plus a bounded durable projection in the same JSON ledger; isolated by client window, principal, transport, resolved project, and canonical repository-root hash |

### Full-runtime coding continuity

`start_coding_task` is the ordinary start-or-continue aggregate on the full
operator surface. With a stable transport window, its default behavior is:

- no valid binding creates and binds one active Workflow Session;
- an exact repository binding reuses that Session and appends the accepted
  instruction as a `task_instruction` event;
- switching repositories preserves independent bindings, and switching back
  restores the earlier Session;
- `inspect`/`read_only` to `normal` rechecks project-write scope and changes
  mode/guards without changing Session identity;
- `new_session=true` explicitly creates an isolated Session without closing or
  rewriting the previous one.

Startup selection is strict and ordered:

1. `resume_session_id` resumes only that existing Workflow Session. A malformed
   or unknown id, non-active lifecycle, project mismatch, access denial, or
   unsafe capability change fails without consulting a current binding or
   creating a replacement.
2. Without an explicit resume, `new_session=true` creates an isolated Session.
   The two fields are mutually exclusive.
3. With neither field, the stable-window exact binding keeps the default
   get-or-create/continue behavior above.

Explicit resume preserves both the `wc_sess_*` id and root title, then appends
exactly one new `task_instruction`. With a stable window and
`bind_current=true`, it atomically replaces that exact window/repository
binding; a previously bound Session remains active and available by explicit
id. `bind_current=false` leaves both binding layers unchanged. Without a stable
window identity, explicit resume still succeeds but creates neither a
process-local nor durable binding. The result reports that state, and later
project tools must continue to pass the resumed `session_id` explicitly.

The first instruction remains the root title. Later instructions record their
timestamp, requested mode/guards, capability-change result, context-refresh
fact, explicit-resume fact, and whether the current binding was established,
without overwriting that root. Target validation, Session create/update,
instruction event, process-local cache replacement, and durable binding
replacement are decided under one store lock and enter one ledger snapshot
generation; a failed validation, permission check, or injected pre-commit
failure leaves them unchanged. Retrying after such a failure appends the
instruction only once. A title change is never an isolation signal.

The durable projection stores only:

```text
domain-separated SHA-256(exact CurrentSessionKey)
→ wc_sess_* session_id
→ updated_at
```

The canonical hash input is the principal kind/id, transport, already-hashed
stable window identity, resolved project, and already-hashed canonical
repository root. It uses fixed field order and length prefixes under
`webcodex.workflow-current-binding.v1`. Raw MCP session ids, hosted
conversation ids, cookies, credentials, authorization headers, and repository
paths never enter this projection, and neither binding hashes nor component
hashes are returned to the model.

After restart, a request with the same complete exact key may restore only an
existing active Workflow Session whose project still matches, repopulate the
process-local cache, and continue it. Missing stable window identity never
falls back to a principal-, credential-, project-, or repository-wide binding.
Changed principal, transport, window, resolved project, or canonical root
derives a different durable key. `new_session=true` atomically repoints only
that exact binding and preserves the previous Session for explicit-id access.
Explicit bind and unbind update both layers; close and LRU eviction remove all
bindings to the affected Session.

When the complete exact key cannot be formed, `resume_session_id` is the
intentional recovery path. It validates the Session against the currently
resolved project and derives any binding key from that project's current
canonical repository root, never a client-supplied path. It never derives a
Workflow Session from a Connector Task or Action Audit Session. Missing window
identity does not manufacture a key, create a credential-wide projection, or
write a durable binding.

The binding field is an additive, serde-defaulted field in ledger version 1, so
older ledgers load it as empty without migration and keep their existing
Session events/messages. Restore accepts only bounded, lowercase SHA-256 keys
that reference known active `wc_sess_*` records. Malformed, duplicate,
conflicting, missing, closed, project-mismatched-on-lookup, and excess entries
are discarded without rejecting valid Session data. Internal status exposes
only bounded counts (`durable_binding_count`, `restored_binding_count`,
`discarded_binding_count`) and never a binding key.

This remains intentionally separate storage and state from the Connector's
SQLite window/project/Task map, while presenting the same ordinary
window/repository continuity semantics. Connector Task continuation and resume
remain their own model and do not infer or mutate Workflow Sessions.

### Current lifecycle contract

- New Workflow Sessions are `active`; older ledgers without a lifecycle field
  are also read as `active`.
- `close_session` is the only explicit `active → closed` transition. It requires
  an explicit `session_id`, never uses current-session fallback, and returns
  `unknown_session_id` for malformed or unknown IDs without creating a session.
- Re-closing a closed session is idempotent. Only a real transition records one
  `session_closed` event.
- Closing removes process-local and durable current bindings that point at the
  Session; it does not delete the Workflow Session ledger.
- Closed sessions still allow queries and pure reads. They reject write-like
  tools, shell/job tools, session-message mutation, and session-scoped
  checkpoint create/restore/delete with `session_closed`.
- `finish_coding_task`, `session_handoff_summary`, and other summary/query tools
  produce closeout information but do not close the session.
- `archived` is a reserved wire state that current code does not produce. LRU
  eviction is capacity management, not a lifecycle transition, and removes
  bindings to the evicted Session.
- Session modes (`normal`, `inspect`, `read_only`) are execution policy, not
  lifecycle state.

This is **not** the same state machine as Action Audit Sessions. Lifecycle
tools and error kinds (`unknown_session_id`, `session_closed`, mode denials,
guard failures) apply only to Workflow Sessions.

### Continuation feedback (`continuation_feedback`)

`continuation_feedback` is a **deterministic, read-only projection** surfaced
by `start_coding_task`, `finish_coding_task`, `session_handoff_summary`, and
(as `validation_delta`) `validation_summary`. It is derived only from existing
persistent state — the Workflow Session ledger, validation evidence, bounded
Job metadata, and the session message board — and it is never a substitute for
a `finish_coding_task` verdict.

- **Read-only contract:** it never executes shell, reads project files,
  enqueues Agent/Runner requests, mutates the ledger, refreshes activity,
  consumes or auto-resolves guidance, or calls an LLM. Surfacing it on
  `start_coding_task` appends only the legitimate new `task_instruction` event
  and no further events; reading it never changes `updated`/activity timestamps.
- **Startup describes the previous attempt:** for reused, explicitly resumed,
  and restored-after-restart sessions, `start_coding_task` snapshots the
  pre-instruction state *before* appending the new instruction, so
  `continuation_feedback.attempt` describes the *previous* attempt's bounded,
  redacted instruction excerpt, activity, changes, current unresolved failure
  identities, and validation — not the empty new attempt. When an unresolved
  identity is available, the first suggested action names that concrete target.
  A fresh session reports
  `status = not_applicable`, `reason_code = fresh_session`.
- **Attempt boundary:** the attempt window is segmented by the most recent
  `task_instruction` retained in the ledger window. When that instruction has
  been evicted by the bounded event limit, the boundary is reported as
  `source = unavailable`, `reason_code = attempt_boundary_evicted`, and
  `event_range.complete = false` — the projection never masquerades a truncated
  retained window as `session_start` with `complete = true`.
- **Exploration workset:** `attempt.exploration` projects only successful,
  structured evidence from focused `read_file`, `search_project_text`, and
  typed LSP navigation calls. The existing ledger retains only a bounded set
  of validated project-relative paths; it never retains search patterns or
  previews, file contents, symbol/hover/diagnostic bodies, arbitrary result
  JSON, shell commands/output, or the absolute repository root for this
  workset. Paths are deduplicated newest successful observation first.
  Enumeration tools such as `project_overview`, `list_project_files`, and
  `list_project_tracked_files`, Git diff lists, failed calls, error text, and
  shell output are not exploration evidence. The workset is segmented by the
  same attempt boundary; when that boundary was evicted,
  `exploration.complete = false` as well.
- **Continuation reuse, not execution:** automatic continuation, explicit
  resume, inspect/read-only to normal mode upgrades, and ledger restoration
  reuse the prior attempt's workset. Startup returns at most 3 paths in
  `minimal` and 12 in `standard` (including the core embedded by `full`); full
  continuation feedback returns at most 100 with the real total and
  truncation state. This is a hint for model judgment only: startup never
  reads, searches, or navigates those paths automatically.
- **Handoff is independent of the display limit:** `session_handoff_summary`
  builds its display list from the caller-supplied `limit`, but
  `continuation_feedback` reads an independent bounded evidence snapshot (the
  maximum retained event window), so a small display limit cannot shrink the
  attempt boundary. `include_validation = false` does not fabricate validation;
  it reports `validation_not_requested` rather than `not_run`.
- **Validation delta comparability:** `validation_delta` is `available` only
  when the latest and prior runs are *proven* comparable — same validation
  kind/tool/cwd and structured scope (package, filter, features, targets,
  purpose), with complete evidence on both sides and a consistent parser
  identity. Otherwise it reports a stable reason code
  (`no_previous_validation`, `validation_scope_changed`,
  `previous_evidence_incomplete`, `current_evidence_incomplete`,
  `parser_changed`, `parser_identity_unavailable`, `test_identity_unavailable`,
  `insufficient_scope_identity`, `validation_not_requested`). Count deltas are
  signed integers (a decrease in passed tests yields a negative `passed_delta`);
  zero-test success never resolves a prior test failure.
- **Opaque scope identity:** `comparison.scope_identity` is a domain-separated,
  opaque stable identity (`validation_scope:v1:<sha256>`) over the normalized
  *structured* scope. It never returns a raw command, absolute path, or test
  filter — command text is not re-exposed through another field.
- **Jobs report only proven status:** the `attempt.jobs` block reports counts
  computed over the full bounded active Job aggregate, never the truncated
  `recent` list, so a hidden recovering job is never misreported as healthy.
  Fields that cannot be reliably proven are not reported.
- **No new persistence model:** continuation feedback introduces no new
  durable table and no second attempt state machine. Exploration adds only a
  serde-defaulted field to the existing version-1 event ledger, so older
  ledgers restore it as empty without a version bump; feedback remains a
  projection over that existing state.

### Invariants (must)

These are also summarized in `AGENTS.md` §7, **Sessions**:

1. **ID format:** Workflow Session IDs use `wc_sess_*`. Do not change the
   prefix, ledger event schema, or lifecycle semantics without an explicit
   design task.
2. **Explicit wins:** An explicit `session_id` always wins over current session.
3. **Unknown rejects:** Unknown explicit `session_id` → `unknown_session_id`.
   Never silently fall back to current session.
4. **Mode guards:** `inspect` denies structured write-like tools and permits
   shell/job-like tools only through the fail-closed Landlock inspect sandbox;
   `read_only` denies both write-like and shell/job-like tools.
5. **Guards first:** Guard denial happens before mutation or agent enqueue;
   record a failed session event when the session id is valid.
6. **Business vs recorder:** `session_summary` (and similar) required
   `session_id` is business input; do not replace it with current session or
   with `recording_session_id`.
7. **No inference from HTTP audit:** Never derive a `wc_sess_*` id from an
   Action Audit Session id (or from `x-action-session-id` / audit SQLite rows).
8. **Window isolation:** Current-session lookup and mutation require a stable
   transport window identity. Missing identity skips generic fallback and
   fails explicit current-binding operations; it never falls back to a
   credential-wide binding.

---

## 2. Action Audit Session

### Purpose

**HTTP Action call auditing** and operator-facing grouping of external API
requests. It answers “what HTTP/API actions happened in this audit window?” —
not “what is the coding task ledger for this repo work?”.

### Responsibilities

- Group HTTP Action / REST audit events under one audit session
- Persist action audit records (endpoints, status, durations, redacted summaries)
- Idle open-session reuse and explicit close for operator audit views
- Aggregate stats for read-only audit APIs

### Identity

| Aspect | Contract |
|---|---|
| ID form | UUID string (or client-supplied id via headers/query), **not** `wc_sess_*` |
| Request affinity | Headers `x-action-session-id` / `x-webcodex-session-id`, or query `action_session_id` |
| Default creation | Server may create a new UUID when no open recent session is reused |

### Storage and ownership

| Aspect | Contract |
|---|---|
| Internal module | `action_audit_sessions` (crate-private; formerly the module path `action_sessions`) |
| HTTP handlers | `audit_http` under `/api/audit/*` |
| Persistence | SQLite tables `action_sessions` and `action_events` |
| Related types | `ActionSessionRecord`, `ActionEventRecord`, DB helpers in `db/audit.rs` |

### Lifecycle (sketch)

1. An audited HTTP request arrives; optional explicit audit session id is read
   from headers/query.
2. `get_or_create_active_session` attaches the event to an existing open session
   (explicit id, or recent idle-open session) or creates a new one.
3. Events are written to SQLite; session aggregate counters update.
4. Operator APIs list sessions, fetch one session with events, or compute stats.
5. Sessions may be closed (`status = closed`); idle open sessions time out for
   reuse purposes (`ACTION_SESSION_IDLE_TIMEOUT_SECS`).

This lifecycle is **orthogonal** to Workflow Session start/finish tools.

### What it is not

- Not a coding / workflow session
- Not a substitute for `start_coding_task` evidence
- Not an input to `session_summary`, message board, or `finish_coding_task`
- Not automatically correlated to any `wc_sess_*`

---

## 3. No unified state machine

The two systems:

- Use different ID namespaces
- Use different storage backends
- Expose different APIs (runtime tools / MCP vs `/api/audit/*`)
- Define different open/close and failure semantics

There is **no** shared session state machine, no shared store, and no
requirement that a request participate in both. A single HTTP call may
incidentally touch both only when a tool invocation both (a) records workflow
ledger evidence via `session_id` / `recording_session_id` and (b) is wrapped by
HTTP action audit middleware — those are still two separate writes.

---

## 4. Do not merge implementations

Do **not**:

- Fold Action Audit Sessions into `tool_runtime::sessions`
- Store workflow ledger events in SQLite `action_*` tables
- Reuse `wc_sess_*` as SQLite `action_sessions.session_id` by convention
- Drive workflow guards from audit session status, or audit close from
  `finish_coding_task`
- “Simplify” by making one ID type serve both products

Merge would couple coding-task continuity to HTTP transport audit, break
identity rules, and blur security/guard boundaries. Keep two implementations.

---

## 5. Future association (explicit only)

The standing optional-correlation contract is:

- **Direction:** Action Audit side holds optional `workflow_session_id`
  (`wc_sess_*`); prefer **event/record** level first.
- **Optional & explicit:** absence is normal; no inference from current Action
  Audit Session, time, thread, connection, or current-session bindings.
- **Independent lifecycle:** audit must not create/close/transition Workflow
  Sessions; correlation is not ownership.
- **Validation sketch:** missing → unlinked; malformed → parameter error;
  well-formed but unknown → store without create or fallback.
- **Named migration** required before SQLite / OpenAPI / external JSON change.

Until that design is implemented, code must treat the systems as unlinked.

---

## 6. Forbidden inference

| Forbidden | Why |
|---|---|
| Infer `wc_sess_*` from current HTTP Action Audit Session | Wrong namespace; audit ids are not workflow ids |
| Fall back to Action Audit Session when Workflow Session is missing | Breaks `unknown_session_id` and explicit-wins |
| Treat `/api/audit/session` payload as coding-task summary | Different evidence model and redaction rules |
| Pass audit UUID as tool `session_id` expecting ledger semantics | Unknown or wrong session; not a supported bridge |

---

## 7. Compatibility surface (do not rename casually)

The following names are part of **storage, HTTP, or external API contracts**.
Internal Rust module renames for clarity are allowed; these surfaces are not
renamed without an explicit compatibility migration:

### SQLite

- Table: `action_sessions`
- Table: `action_events`
- Index: `idx_action_sessions_status_last_event`
- Column names and migration history in `db/schema.rs` / `db/audit.rs`

### HTTP routes

- `POST /api/audit/sessions`
- `POST /api/audit/session`
- `POST /api/audit/stats`
- Request affinity: `x-action-session-id`, `x-webcodex-session-id`,
  query `action_session_id`

### JSON / type shapes (illustrative)

- Audit session records (`session_id`, `status`, counters, timestamps, …)
- Audit event views and stats aggregates
- Workflow tool fields: `session_id`, `recording_session_id`, session mode
  values such as `normal` / `inspect` / `read_only`
- Error kinds such as `unknown_session_id`

### OpenAPI / MCP / runtime tool surface

- GPT Action OpenAPI operation ids and schemas that mention workflow
  `session_id` / `recording_session_id`
- MCP tool input schemas for session tools
- Runtime tool names (`start_session`, `start_coding_task`,
  `session_summary`, …)

### Internal vs external naming

| Layer | Current clarity practice |
|---|---|
| Docs / design | Prefer **Workflow Session** and **Action Audit Session** |
| Rust module path | `tool_runtime::sessions` vs `action_audit_sessions` |
| SQLite / HTTP / JSON | Keep existing `action_sessions` / `session_id` names for compatibility |

Renaming a **crate-private** module path does not change wire contracts.
Renaming tables, routes, or serialized field names does.

---

## 8. Quick decision guide

| Question | Answer with… |
|---|---|
| Coding task, guards, validation ledger, handoff? | **Workflow Session** (`wc_sess_*`) |
| HTTP Action audit trail, `/api/audit/*`, SQLite action events? | **Action Audit Session** |
| Tool argument `session_id` on runtime/MCP tools? | Workflow Session (business input) |
| Wrapper field `recording_session_id`? | Workflow Session (recorder metadata only) |
| Header `x-action-session-id`? | Action Audit Session |
| Should these share one store or state machine? | **No** |
| Need a link later? | Optional explicit `workflow_session_id`; never infer it from transport or current bindings |

---

## Related docs

- [`AGENTS.md`](../../AGENTS.md) — executable Session invariants
- [`architecture-decisions.md`](architecture-decisions.md) — dual-model summary
- [`openapi-guidelines.md`](openapi-guidelines.md) — `session_id` vs
  `recording_session_id` on GPT Actions
- [`../CONCEPTS.md`](../CONCEPTS.md) — product vocabulary (Workflow Session in
  client-facing language)
- [`../ARCHITECTURE.md`](../ARCHITECTURE.md) — module map
