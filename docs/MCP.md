# MCP

[English](MCP.md) | [简体中文](MCP.zh-CN.md)

Use MCP when the client can connect to the project-bound WebCodex endpoint.
Complete [QUICK_START.md](QUICK_START.md) first.

## Endpoint and Authentication

Local clients can use:

```text
http://127.0.0.1:<configured-port>/mcp
```

Hosted clients require an operator-managed HTTPS endpoint:

```text
https://your-domain.example/mcp
```

Configure Bearer authentication with a credential issued for runtime/project
access. Do not use or expose bootstrap/admin, account, or Agent credentials.
Prefer the client secret store; never commit a token.

Canonical setup does not print credential values or secret paths, create a
tunnel, or expose a public port. Production enrollment, scoped user tokens, and
OAuth are described in [AUTH_MODEL.md](AUTH_MODEL.md) and
[DEPLOYMENT.md](DEPLOYMENT.md).

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
read_file
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

On `local_coding`, `work_on_project(project, instruction, session_id?)` is the
ordinary entry point: one call returns the rules, repository structure, Git
state, LSP readiness, jobs, and blockers a coding model needs to start or
continue focused work. It creates a new Workflow Session when `session_id` is
absent, and exactly resumes the given Session when present (never a guess or a
credential-wide fallback); it never binds a current window. A successful call
returns `session_id`, the resolved project id, a `readiness` verdict, the
`workspace` Git projection, the `repository` overview, the bounded `rules`
(`status` loaded/reused/changed/not_found/unavailable, per-source fingerprint,
headings, bounded content, and `read_more` hints), `semantic_navigation`
readiness, the compact `jobs` counts, and `blockers`/`warnings`/
`suggested_next_actions`. The returned `repository` block is a deterministic
metadata scan of directory entries, file types, and the Git tracked index
(project types, manifests, key files, roots, top level entries, and
project-relative suggested reads with reasons); it never reads ordinary file
bodies, executes project code, follows symlinks, or scans
protected/sensitive/build/cache paths. Every list is bounded and records its
own total/returned/truncated metadata. If the overview is unavailable, the
session still starts and `repository.status=unavailable` with a
`repository_overview_unavailable` warning; raw errors, absolute paths, or
Runner output are never returned. The extra context is informational only: it
does not modify anything or execute anything, and the model still uses
`read_file`, search, edits, and validation tools as needed.

On `full_operator_runtime`, ordinary coding starts or continues with
`start_coding_task`. A stable window continues the same repository by default;
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

After review, the human accepts or rejects on the host:

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
| `server_unreachable` | The project runtime is unavailable | Run `webcodex agent start` |
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

## Advanced Runtime Surface

WebCodex can also run as a multi-project management ToolRuntime. Its discovery,
session, LSP, raw job, artifact, and operator tools remain documented in
[OPERATIONS.md](OPERATIONS.md). That is an advanced surface, not the canonical
project Connector and not a prerequisite for ordinary coding.
