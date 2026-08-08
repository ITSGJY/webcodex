# MCP

[English](MCP.md) | [简体中文](MCP.zh-CN.md)

Use MCP when the client can connect to the project-bound WebCodex endpoint.
Complete [QUICK_START.md](QUICK_START.md) first.

## Endpoint and Authentication

Local clients can use:

```text
http://127.0.0.1:<configured-port>/mcp
```

Hosted clients need HTTPS. There are three paths:

- **Hosted:** `webcodex connect <server>` uses an existing hosted Server; only the Runner runs locally.
- **Local Share:** `webcodex share` starts the local Server + Agent and a Cloudflare Quick Tunnel, then prints a temporary HTTPS `/mcp` URL and a separate temporary Bearer credential. Ctrl-C revokes that access by stopping the runtime/tunnel and removing the temporary share state. The URL can change every run. Use `--tunnel none` only for local testing/debugging.
- **Self-hosted:** use a stable HTTPS domain/tunnel, durable service management, and OAuth or scoped credentials for long-lived operation.

Cloudflare Quick Tunnel is intended for development/testing, not production.
For stable self-hosted access, an endpoint looks like:

```text
https://your-domain.example/mcp
```

Do not use or expose bootstrap/admin, account, Agent, or the persistent
project-first Connector credential as a public sharing secret. `share` creates
and prints its own temporary Connector credential. Prefer the client secret
store; never commit a token.

For ChatGPT Developer Mode, point the custom app at the public HTTPS `/mcp`
endpoint and use a user-defined/custom OAuth client. WebCodex currently supports
PKCE S256 plus `client_secret_post`; register the exact ChatGPT callback URL as a
redirect URI. The OAuth discovery metadata advertises `offline_access` for
refresh-token continuity. `offline_access` is protocol-level and does not add a
WebCodex API permission, so do not put it in the OAuth client's
`allowed_scopes` list. Dynamic client registration is not currently required or
implemented by WebCodex.

Canonical `webcodex setup` still does not print credential values or secret
paths, create a tunnel, or expose a public port. `webcodex share` is the
explicit temporary exception: it prints only its session-scoped credential,
never the persistent project Connector credential. Production enrollment,
scoped user tokens, and OAuth are described in [AUTH_MODEL.md](AUTH_MODEL.md),
[DEPLOYMENT.md](DEPLOYMENT.md), and [OAUTH2_SMOKE_TEST.md](OAUTH2_SMOKE_TEST.md).

## Model Surface Selection

MCP exposes one model surface selected at server startup:

- `WEBCODEX_CONNECTOR_SURFACE=task-v1`, together with the complete Connector
  project configuration written by `webcodex setup`, selects
  `canonical_connector` and the twelve project-bound capabilities below.
- Without Connector configuration, an unset `WEBCODEX_MCP_MODEL_SURFACE`
  selects the focused `local_coding` surface (the default ordinary-user
  surface; its tool set is listed below).
- `WEBCODEX_MCP_MODEL_SURFACE=local-coding-v1` selects `local_coding`
  explicitly; `WEBCODEX_MCP_MODEL_SURFACE=full-operator-v1` selects the
  explicit `full_operator_runtime` operator/debug surface.
- Setting Connector configuration and `WEBCODEX_MCP_MODEL_SURFACE` together,
  or using an unsupported `WEBCODEX_MCP_MODEL_SURFACE` value, is a startup
  configuration error; it does not fall through to another surface.

`GET /mcp`, MCP `initialize.serverInfo`, and `runtime_status.model_surface`
all report the same selected `modelSurface`. The standard ordinary-user setup
selects `canonical_connector`; without it, the default is `local_coding`. The
full operator runtime is served only when an operator explicitly selects it
with `WEBCODEX_MCP_MODEL_SURFACE=full-operator-v1`.

## Project-Bound Surface

When `modelSurface=canonical_connector`, MCP `tools/list` contains exactly:

```text
task_start
task_list
task_resume
files_list
files_read
files_search
edits_apply
checks_run
commands_run
task_review
task_cancel
task_finish
```

The same chat window continues the configured repository automatically.
`task_start` resolves one context for that window and project without
duplicating it; each follow-up instruction is appended to the active durable
task.
Switching repository connections keeps their histories isolated, and returning
to a previous connection restores its prior task. Compatible MCP clients retain
the protocol session automatically; users do not pass it in prompts.

The Connector context already binds the project. Start with `task_start`; do
not call `list_projects`, `runtime_status`, `tool_manifest`, `start_session`,
or `current_session`, and do not ask the user for an Agent client ID, runtime
project ID, executor reference, or workflow session.

Before reuse, WebCodex compares the repository path, branch and HEAD, worktree,
applicable repository rules, and project manifests. Unchanged context is
reused; only changed slices are reported as refreshed. If a bounded scan cannot
prove a slice complete, the response marks it partial/unknown and includes a
compact warning instead of claiming reuse. `task_list` and `task_resume` are
explicit recovery tools for a client that lost its MCP transport session, not
steps in the ordinary loop.

On `local_coding`, MCP `tools/list` contains exactly the focused coding tool
set, in this order:

```text
work_on_project
list_projects
project_overview
list_project_tracked_files
list_project_files
search_project_text
search_project_texts
read_file
read_files
lsp_status
document_symbols
document_diagnostics
hover
workspace_symbols
goto_definition
find_references
apply_text_edits
apply_patch_checked
run_shell
run_job
job_status
job_log
list_jobs
stop_job
cargo_fmt
cargo_check
cargo_test
validation_summary
git_status
git_log
git_diff
git_diff_hunks
show_changes
workspace_hygiene_check
finish_coding_task
```

The same list is the single source of truth for
`tool_manifest(intent="coding")`. Session management (`start_session`,
`current_session`, persistent shells), project registration/lifecycle
(`register_project`, `create_project`), artifact/checkpoint tools, cleanup
tools, and runtime/operator management (`runtime_status`, `tool_manifest`)
are not part of this surface: `tools/call` rejects them at the MCP boundary
before ToolRuntime dispatch, and `tools/list` never advertises them.

On `local_coding`, `work_on_project` is the ordinary entry point. It accepts
exactly one project source:

```json
{"project":"agent:special:webcodex","instruction":"implement it"}
```

or:

```json
{"client_id":"special","path":"/root/git/webcodex","instruction":"implement it"}
```

The path must be an existing absolute directory on that Runner. Under the
Runner's project-registry write lock it canonicalizes the requested path and
configured project paths, enforces `allowed_roots` and dangerous-root policy,
then reuses one unique enabled registration or atomically persists a stable
`<sanitized-basename>-<path-hash-prefix>` id in `projects.d`. Disabled matches
return `project_disabled`; multiple ids for the canonical path return sorted
`ambiguous_project_path` metadata. It never creates or changes the target
directory and never runs `git init`. This conditional registration requires
`project:write` and the same authority decision as `register_project`, even
though the existing-project form retains its prior read-only startup behavior.

After project resolution, one lightweight call returns the rules, Git state,
LSP readiness, jobs, and blockers a coding model needs to start or continue
focused work. It creates a new Workflow Session when `session_id` is absent,
and exactly resumes the given Session when present (never a guess or a
credential-wide fallback); it never binds a current window. Path resolution
always happens before explicit Session validation, so mismatch or unknown
Session failures never fall back. A successful call returns `session_id`, the
resolved project id, bounded path-free `project_resolution` metadata, a
`readiness` verdict, the `workspace` Git projection, bounded `instructions`
(`status` loaded/reused/changed/not_found/unavailable, per-source fingerprint,
headings, bounded content, and `read_more` hints), `semantic_navigation`
readiness, compact `jobs` counts, and `blockers`/`warnings`/
`suggested_next_actions`. A fresh Session includes bounded applicable rule
bodies such as `AGENTS.md`. An exact continuation with unchanged rule
fingerprints reports `status=reused` and does not repeat those bodies; changed
rules report `status=changed` and include the new content.

`work_on_project` deliberately does not request or locally scan a repository
overview. Its compatibility `repository` field is the compact marker
`{"status":"unavailable","reason_code":"not_requested_by_work_on_project"}`;
it contains no project types, manifests, key files, roots, top-level entries,
suggested reads, scan metadata, or overview-failure warning. Use
`start_coding_task(detail=standard|full)` on `full_operator_runtime`, or call
`project_overview` explicitly, when that metadata is useful. The returned
startup context is informational only: it does not modify or execute project
contents, and the model still uses focused reads, search, edits, and validation
tools as needed.

```json
{
  "project_resolution": {
    "source": "path",
    "outcome": "auto_registered",
    "resolved_project": "agent:special:webcodex-4f2a91c8",
    "registered": true
  }
}
```

On `full_operator_runtime`, ordinary coding starts or continues with
`start_coding_task`. It accepts the same `project` or `client_id + path`
sources, in addition to its existing `client_id` managed temporary-project
source (optionally named with `temporary_project_name`). A stable window
continues the same repository by default;
switching repositories changes context and switching back restores the prior
Workflow Session. `new_session=true` is the explicit advanced isolation
request. The exact binding is cached in-process and persisted as a bounded,
hashed projection, so the same stable window and repository can restore it
after a server restart. Retain the returned session id for explicit recovery
when that transport identity is unavailable. The returned
`continuation_feedback` is a deterministic, read-only projection of the
*previous* attempt's bounded instruction excerpt, activity, changes, current
unresolved failure identities, and validation state (plus a `validation_delta`
only comparable across proven-equal scope); it is never an
LLM summary or a new verdict, and it does not run validation.
Its `attempt.exploration` workset contains only bounded, validated
project-relative paths proven by successful focused reads, structured project
search results, or typed LSP navigation. It is attempt-scoped, newest-first,
and reports `complete=false` if the boundary was evicted. Search text/previews,
file or LSP contents, arbitrary results, commands/output, and absolute roots
are never part of the workset. Automatic continuation, explicit resume,
inspect/read-only mode upgrades, and restart recovery can reuse it, but startup
does not execute tools or replace model judgment. The compact startup core
returns at most 3 paths for `minimal` and 12 for `standard`/embedded `full`;
complete feedback returns at most 100 and preserves the real total/truncation.

That surface also supports a bounded persistent execution context for
registered-project Workflow Sessions:
`execution_context = {default_cwd?, default_shell?}`. Creation stores it;
continuation/resume omission preserves it; an explicit object replaces it
with the instruction update in one in-memory store-lock commit, and `{}` clears
it. `update_session_context(project, session_id, execution_context)` requires
access to the resolved project and rejects any Session-project mismatch; there
is no cross-project escape. A successful response means the in-memory context
and event committed together. The JSON ledger is queued to the existing
background writer, and persistence failures remain visible through runtime
status and logs; success does not claim a synchronous disk flush.
`run_shell`/`run_job` resolve explicit per-call
arguments first, then exact project-matched Session defaults, then their
existing root/configured-shell behavior. No env, credential, arbitrary option,
or persistent shell state is stored in the execution context.

The full operator surface exposes persistent process state only through four
explicit tools:

```text
open_session_shell(project, session_id, cwd?, shell?)
session_shell_exec(project, session_id, shell_id, command, timeout_secs?, purpose?)
session_shell_status(project, session_id, shell_id)
close_session_shell(project, session_id, shell_id)
```

Open returns a new unpredictable `shell_id`; at most one shell may be active
per Workflow Session. `session_shell_exec` returns
`command_started`, `command_completed`, `exit_code`, bounded `stdout`/`stderr`
with truncation flags, `duration_ms`, `execution_state`, `shell_state`, and the
observable cwd. Status/close also report the bound dialect/profile, initial
cwd, timestamps, busy/terminal state, and close reason when available. Close
is idempotent, but a closed id cannot operate a later replacement shell.

Agent projects execute the process on their owning Runner. The process engine
uses the Server host only for a Server-local project when that project type is
available. Every operation requires the exact active Session/project and
normal caller authorization; `read_only`, `inspect`, old Runners without the
`persistent_shell` capability, and Sessions selecting an SSH resource fail
closed. `run_shell` and `run_job` remain independent processes and never reuse
the persistent shell. This is command execution, not a PTY or terminal stream,
and it does not recover across Server or Runner restart.

For an explicit cross-window or human handoff on this surface, call
`session_handoff_summary` with the old `wc_sess_*` id. It and
`finish_coding_task` return the same strict `handoff_brief`: a deterministic
read-only projection of bounded task excerpts, workspace state, changed and
recently explored paths, validation, Job/guidance attention counts, and fixed
next actions. The brief is capped by actual serialized size at 8 KiB, adds no
new handoff persistence, and its builder executes no extra tools. The public
MCP dispatch still appends the uniform `tool_call_started` /
`tool_call_finished` telemetry to the named Workflow Session; those recorder
events are not a business mutation by the projection. It is not Session replay
and does not restore hidden model context; use the co-returned
`continuation_feedback` when detailed attempt evidence is needed. A new window
may create its own Session and then read the old Session's handoff explicitly.
Choosing `resume_session_id` instead still invokes the existing strict
active-session resume checks.

The stable IDs have product purposes between model tools and host review, but
ordinary users do not manage them:

- `task_id`: continue/review one bounded task;
- `operation_id`: exact retry identity for a mutation or execution;
- `execution_id`: inspect/wait/cancel one durable execution;
- `result_id`: review and decide one stable result.

Agent transport, executor routing, and pending request IDs remain internal.

## Golden Coding Loop

```text
task_start
→ files_list
→ files_read / files_search
→ edits_apply
→ checks_run
→ task_finish
→ task_review
```

`files_list` answers "what is in this project" from the Git index, so ignored
directories never appear. Call it before guessing paths for `files_read` —
especially in a `read_only` task, which has no shell to list files with.
For trusted command-based inspection, use Workflow Session `inspect` with
`run_shell`, or connector `task_start(mode=inspect)` with `commands_run`, and
prefer `rg` or `git grep`. Inspect shell/jobs are Landlock-restricted against
ordinary local filesystem writes but retain reads, environment, network, and
possible external side effects.

Use `commands_run` only as an approved escape hatch. Use `task_cancel` for a
queued/running execution that should stop.

Normal writable tasks require structured checks before finish. A successful
check carries trusted workspace provenance; any subsequent mutation makes it
stale and requires a new operation ID. A command that cannot spawn is an
executor failure, not assertion evidence.

### `checks_run` recipes

The `checks_run` schema still exposes only `format`, `check`, and `test`, with
an optional `recipe` enum (`rust`, `node`, `python`, `go`). Omit it to select
the nearest `Cargo.toml`, `package.json`, `pyproject.toml`, or `go.mod` from
the Task workspace and relative `cwd`. An explicit matching recipe resolves a
same-root ambiguity. The only markerless exception is explicit
`recipe=python` with `checks=["test"]`, which runs the fixed unittest discovery
plan from `cwd` when no `pyproject.toml` is selected. Resolution never scans
sibling projects or permits path/symlink escape.

Rust supports all three checks and is the only recipe with a one-argv
`test_filter`. Node resolves an evidenced package manager and fixed script
allowlist; Python selects configured Ruff/Black, Ruff/Mypy, or pytest, while
manifestless Python supports only `python -B -m unittest discover -v`; Go
supports `check` and `test`, with `format` unavailable. No recipe installs
dependencies, changes lockfiles, or uses the network. Tool absence is an
executor failure; a started validator returning non-zero is an assertion
failure. Recipe version, relative root, and invocation/manifest evidence bind
the operation exact-retry identity. A recipe never adds an MCP tool; MCP,
OpenAPI, and the capability registry all share one capability list.
At finish, untracked interpreter/test caches, coverage output, and
`node_modules` are omitted with bounded warnings; tracked paths are retained.

### Long structured validation continues as a Job

`cargo_check`, `cargo_test`, and `cargo_fmt(check=true)` run the command exactly
once. `timeout_secs` is the total runtime budget of the command (1..=3600;
defaults: check 600, test 1800, fmt check 120), independent of how long the tool
call blocks. A short validation finishes in-process and returns the existing
terminal result. A long one (budget above the internal sync wait) promotes the
same single execution to a queryable Job and returns `job_id`,
`promoted_to_job=true`, `execution_state=queued/running`, and
`effective_timeout_secs` without ever reporting `failure_kind=timeout` at
handoff. Poll `job_status` / `job_log`, or read `validation_summary` — the Job's
terminal state feeds the summary; do not re-run the command to find the answer.
`cargo_fmt` with `check=false` mutates source and never auto-promotes.
Handoff/cancel races are safe: a cancelled handoff never orphans a process, and
`stop_job(confirm=true)` stops a promoted job.

Compatibility with an older Runner is intentionally bounded. If the Runner has
basic shell execution but lacks both async validation Jobs and structured
validation argv, an omitted `timeout_secs` uses one legacy synchronous execution
with an effective 120-second budget and reports `async_handoff_available=false`.
An explicit budget up to 120 seconds also runs synchronously. An explicit budget
above 120 seconds is rejected before command start with
`failure_kind=capability_unavailable`; it is never silently shortened and never
starts a second execution. Upgrade the Runner to regain long Job handoff.

```bash
webcodex task show <task-id>
webcodex task accept <task-id>
# or: webcodex task reject <task-id>
```

## First Safe Prompt

```text
Use the configured WebCodex project. Start a read-only task, read README.md,
summarize the project, review the result, and finish. Do not edit files.
```

No project discovery or runtime identifier belongs in this prompt.

## Common Errors

| Code | Meaning | Action |
|---|---|---|
| `project_not_configured` | No canonical setup exists | Run `webcodex setup` |
| `project_registration_invalid` | Local project state is malformed, incomplete, or conflicting | Resolve the reported private-state conflict |
| `project_credential_invalid` | The private Project Credential is missing, unsafe, malformed, or mismatched | Restore both matching private files or explicitly recreate the profile |
| `project_credential_rejected` | The reachable server rejected the configured Project Credential | Restore the server-matching credential; do not treat this as Agent offline |
| `workspace_unavailable` | The configured Git workspace is unavailable | Restore the workspace, then run doctor |
| `server_unreachable` | The project runtime is unavailable | Run `webcodex run` for local project-first mode |
| `agent_offline` | The local Agent is not ready | Run `webcodex doctor` |
| `required_capability_unavailable` | The Agent lacks a coding capability | Upgrade all binaries |
| `structured_validation_unavailable` | The Agent cannot run structured checks | Upgrade all binaries |
| `task_not_active` | The task can no longer mutate or execute | Start a new task |
| `execution_not_terminal` | Finish is blocked by active/unknown work | Review/wait/cancel |
| `validation_recipe_not_found` / `validation_recipe_ambiguous` | Auto resolution found no recipe or multiple nearest recipes | Change `cwd` or provide a matching `recipe` |
| `validation_recipe_mismatch` / `validation_manifest_invalid` | Explicit recipe, path, marker, or manifest evidence is invalid | Correct the reported public evidence |
| `validation_check_unavailable` / `test_filter_unsupported` | No safe mapping exists for the requested semantic input | Change the check/filter |
| `package_manager_ambiguous` | Node package-manager evidence is absent or conflicting | Correct `packageManager` or lockfiles |
| `validation_tool_unavailable` | The selected executable/module is absent | Provide the existing project tool and use a new operation ID |
| `checks_required` | A normal task has not run checks | Call `checks_run` |
| `checks_stale` | The workspace changed after the last check | Run a new check |

Run `webcodex status` for the short answer and `webcodex doctor` for full
read-only findings.

## Bounded source reads (`read_file`)

`read_file` is a bounded, streaming UTF-8 range reader over an
agent-registered project. The same range algorithm backs both local and agent
projects, so the model-facing output is identical apart from the resolved
project id and transport.

Inputs stay unchanged: `project`, `path`, optional `session_id`, optional
`start_line` (default 1, min 1), optional `limit` (default 2000, clamped to
`1..=2000`), and optional `with_line_numbers`. No new input fields, batch mode,
or config were added.

A successful read streams the file exactly once — computing the complete-file
SHA-256 and total line count while retaining only the requested range — and
returns:

```text
text              # plain or numbered selected range, lines joined with \n
format            # "plain" | "numbered"
path              # project-relative input path
sha256            # 64 lowercase hex digits of the COMPLETE file
start_line        # effective 1-based start (>= 1)
limit             # effective line cap (1..=2000)
total_lines       # complete file line count (>= 0)
returned_lines    # original file lines actually returned (>= 0, <= limit)
end_line          # start_line + returned_lines - 1, or null when nothing returned
has_more          # true only when file lines remain after the returned range
next_start_line   # end_line + 1 to continue, or null at end of file
```

`with_line_numbers=true` only changes `text` and `format`; it never changes
`returned_lines`, `end_line`, `has_more`, or `next_start_line`.

Continue reading with `next_start_line`:

```jsonc
// first call
{ "project": "demo", "path": "src/main.rs", "limit": 40 }
// -> next_start_line: 41, has_more: true
// continue from where it stopped
{ "project": "demo", "path": "src/main.rs", "start_line": 41, "limit": 40 }
```

Bounds are enforced directly, not by transport tail truncation. Raw selected
content has an independent 192 KiB budget. Before sending, the Runner serializes
the complete `webcodex.file_read_range.v1` envelope and rejects it when it
exceeds the smaller of the effective transport cap and 256 KiB. ToolRuntime
then re-checks the final model output after line numbering and JSON escaping.
Any layer that exceeds its budget fails with `reason_code: range_too_large` —
shrink `limit` or narrow the range and retry; no partial line or content
inconsistent with the SHA/line metadata is ever returned.

Failures return a small, stable, schema-backed object — never an absolute
path, raw OS error, command, or Runner stdout/stderr:

```text
error_kind:   "read_file_failed"
reason_code:  one of invalid_path | sensitive_path | not_found | not_file |
              permission_denied | invalid_utf8 | range_too_large |
              agent_unavailable | timeout | malformed_agent_response | io_error
path:         project-relative input path (for navigation only)
state_changed: false
```

Agent `file_read_range.v1` envelopes are treated as untrusted: every formal
field is strictly validated and the model output is reconstructed from those
fields alone, so unknown fields, padding, mismatched range metadata, wrong
SHA, or inconsistent content/line counts are stripped or rejected
(`malformed_agent_response`) and never leak to the model.

### Bounded batch reads (`read_files`)

`read_files` is a separate tool; it does not change `read_file`. It accepts a
required `project`, `items` with 1 to 8 `{path, start_line?, limit?}` entries,
and one optional batch-wide `with_line_numbers` value. Paths use the same
project-relative and sensitive-path checks as `read_file`.

The project is resolved once for the batch. Each item then uses the same
single-file normalization, UTF-8 validation, SHA-256, numbering, Runner
response parsing, stable errors, and serialization checks as `read_file`.
Items execute independently and results are restored to input order. Up to
four item futures cover validation, Runner enqueue, and response waiting, so
no fifth read is enqueued until a slot opens.

One 30-second deadline covers the whole batch. Completed results remain intact;
unfinished reads become `timeout`, and already-enqueued unfinished requests are
cancelled individually. One failed item never cancels another item.

The final serialized result has a 256 KiB budget. Complete result items are
added in input order; an item is never partially serialized. If the next item
does not fit, `output_truncated=true` and `next_index` identifies the first
omitted input item. The caller can retry from that position in its original
`items` list. Session and permission metadata belong only to the outer batch
result, and the top-level `project` is the resolved runtime project id.

### Bounded batch text search (`search_project_texts`)

`search_project_texts` is a separate read-only tool and does not change the
`search_project_text` contract. It accepts one required `project`, 1 to 8
independent `queries`, and an optional outer Workflow `session_id`. Each query
uses the existing `pattern`, `path`, `result_mode`, `limit`, context, glob, and
timeout fields. It never combines patterns, reads matching files, calls LSP, or
performs semantic/model analysis.

The project is resolved and authorized once. Each query then reuses the
single-search validation, protected-path exclusions, rg-first/grep fallback,
timeout normalization, parsing, path filtering, truncation, and error mapping.
Validation, Runner enqueue, and response waiting all occupy one of two
concurrency slots; a third search cannot reach the Runner until a slot opens.
Results are restored to input order and one failed/no-match/timed-out query
does not cancel ordinary work in other slots. No-match keeps the single-query
success semantics.

One exact 30-second batch deadline bounds all queries. A query's command
timeout is the smaller of its normalized timeout and the remaining batch
budget (with the Runner protocol's whole-second granularity); the exact outer
deadline remains authoritative. Completed items are retained, unfinished
Runner requests are cancelled, and only unfinished items become `timeout`.

The final `ToolResult` is measured by actual JSON serialization against a
256 KiB budget with space reserved for outer Session metadata. Only complete
items are appended in input order. If the next item does not fit,
`output_truncated=true` and `next_index` points to the first query the caller
can resubmit from its original list. The resolved runtime project id and any
Session/permission metadata occur only on the outer result; item `index`
identifies the input query, so raw patterns are not repeated in item output.
One batch records one read-like search event and increments exploration
`search_count` once. Only deduplicated project-relative paths from successful
returned items enter exploration evidence; `queries[*].pattern` is removed
from all Workflow Session ledger projections and persistence.

## Bounded project text search (`search_project_text`)

`search_project_text` is the default inspect/search tool. It uses ripgrep first
with a basic grep fallback, and it is bounded in both work and bytes:

- **Early stop.** The search emits records in traversal order (no global
  path sort) and the command pipeline closes as soon as the requested record
  budget is met, so a small `limit` search returns promptly instead of waiting
  for a full-repository scan. Match order is therefore not deterministic; the
  result set is bounded and timely.
- **Byte budget.** A second pipeline stage emits the formal search budget plus
  one bounded probe byte. The server consumes that probe only to prove the cap
  fired, including when it landed exactly after a newline, and reports
  `truncation_reason = "output_bytes"`. A single over-long match or context line
  cannot blow past the Runner transport cap; only complete records are returned.
- **Timeout partial success.** If the effective timeout fires after complete
  records were collected, the tool still returns them with
  `truncated = true` and `truncation_reason = "timeout"` instead of discarding
  them. `count` mode never presents a partial count as a complete total:
  `count_complete` stays false and `total_matches` stays null. If nothing
  complete was collected, the structured `search_timeout` failure is returned.
- **Trusted paths.** Returned paths are project-relative and validated;
  absolute paths, parent traversal, temp-file paths, Shell commands, and
  Runner stderr are never surfaced.

Truncation metadata is stable: `truncated` with `truncation_reason` one of
`limit | output_bytes | timeout | transport`, or `null` when complete.

## Advanced Runtime Surface

WebCodex can also run as a multi-project management ToolRuntime. Its discovery,
session, LSP, raw job, artifact, and operator tools remain documented in
[OPERATIONS.md](OPERATIONS.md). That is an advanced surface, not the canonical
project Connector and not a prerequisite for ordinary coding.
