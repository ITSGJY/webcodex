# GPT Actions

[English](GPT_ACTIONS.md) | [简体中文](GPT_ACTIONS.zh-CN.md)

Use GPT Actions when a Custom GPT should call the project-bound WebCodex
Connector. Use MCP when the client supports MCP directly.

## Schema

Import:

```text
https://your-domain.example/openapi.json
```

ChatGPT requires public HTTPS. `webcodex setup` intentionally creates only a
loopback project runtime; ingress and production authentication are operator
responsibilities described in [DEPLOYMENT.md](DEPLOYMENT.md).

Configure Bearer/API-key authentication with a scoped runtime credential. Do
not paste bootstrap/admin, account, or Agent credentials into a GPT.

## Canonical Hosted Operations

For a project-bound Connector, OpenAPI is generated from the same twelve
capabilities as MCP:

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

The operation count is generated and tested; setup, pairing, token management,
Agent management, audit endpoints, and legacy `/api/codex` routes are not in
the Action schema.

The Connector already owns a deterministic project binding. A Custom GPT must
not call `listProjects`, `runtime_status`, `tool_manifest`, `start_session`, or
Agent listing before normal coding, and the prompt must not contain an Agent
client ID or runtime project ID.

On deployments that also expose the advanced generic `callRuntimeTool`
compatibility path, a successful `start_coding_task` Action only wraps the
shared MCP/REST startup core; it does not rebuild continuation data. That core's
attempt-scoped exploration workset contains only bounded, validated
project-relative paths from successful focused reads, structured searches, and
typed LSP navigation. It excludes search/file/LSP content, commands/output, and
absolute roots, marks an evicted attempt boundary `complete=false`, and is
reused across continuation, explicit resume, mode upgrade, and restart without
automatically executing tools. The standard/full core returns at most 12 paths
(`minimal`: 3), and the complete Action response remains below 32 KiB.

The same generic path exposes a strict flattened `execution_context` object
with only `default_cwd` (project-relative) and `default_shell` (`sh` or
`bash`). `start_coding_task` can set or replace it and
`update_session_context` can replace or clear it for an explicit active
Workflow Session only when its required `project` resolves to the exact Session
project and the caller is authorized for it. Cross-project escape is rejected.
Success reports the in-memory context/event commit; JSON ledger persistence is
queued to the background writer and may still be pending. It affects only
`run_shell`/`run_job`; per-call arguments
remain authoritative, and no environment, credential, or persistent shell
state is accepted.

That generic runtime schema also publishes the strict `HandoffBrief` component.
`session_handoff_summary` and `finish_coding_task` reuse it for the same
deterministic, read-only, at-most-8-KiB `handoff_brief`. This compact projection
is for a new window, new Agent, or human receiver; detailed evidence remains in
`continuation_feedback`. It is not Session replay, does not recover hidden
model context, and its builder stores no new Session data or executes additional
tools. A public generic-runtime call still records the standard
`tool_call_started` / `tool_call_finished` Session telemetry; this is uniform
dispatch recording, not a business side effect of the handoff projection. A new
window may create a new Session and explicitly read the old Session's handoff;
explicit resume keeps its existing safety checks.

Within one retained chat-window identity, `task_start` automatically continues
the repository's active durable context and appends the new instruction.
Changing to another configured repository keeps the two histories isolated;
returning restores the first repository. WebCodex refreshes only Git, worktree,
repository-rule, target-directory, and manifest state that changed.

## Suggested GPT Instructions

```text
Use the configured WebCodex project.
Start or continue each user instruction with task_start.
Let task_start reuse the current project context; do not ask the user for IDs.
Use task_list and task_resume only after WebCodex reports that automatic
transport-window recovery is unavailable.
Use files_list to see what the project contains before guessing paths.
Use files_read/files_search before edits_apply.
Use a stable operation_id for exact retry.
Run checks_run before task_finish.
Use task_review for execution progress and result review.
Use commands_run only when structured capabilities are insufficient and
approval is available.
Never ask the user for task, session, current-binding, Agent, transport, queue,
or workflow identifiers.
```

## Validation recipe contract

`checks_run` remains the only structured validation Action. It accepts an
optional `recipe` enum (`rust`, `node`, `python`, `go`); omit it for
deterministic nearest-manifest resolution from the Task workspace and relative
`cwd`. Supply a matching recipe when `validation_recipe_ambiguous` identifies
multiple markers at the same nearest root. Explicit `recipe=python` with
`checks=["test"]` is the only markerless exception and selects a fixed unittest
discovery plan from `cwd`. The model cannot provide a program, argv, script
body, or shell command through this Action.

Rust supports `format/check/test`; Node uses an evidenced package manager and
fixed non-mutating script-name order; Python uses configured Ruff/Black,
Ruff/Mypy, and pytest, or the fixed manifestless
`python -B -m unittest discover -v` test plan; Go supports `check/test` and
reports `format` unavailable. Recipes do not install dependencies, mutate
lockfiles, or use the network. A missing tool is an executor failure, while a
started validator's non-zero verdict is an assertion failure. Resolved recipe
version, relative root, invocation, and manifest/lock evidence bind
`operation_id`, so use a new ID after a recipe or workspace change.

## Human Decision

`task_finish` creates a stable result; it does not silently apply changes to the
target checkout. The host user reviews and decides:

```bash
webcodex task show <task-id>
webcodex task accept <task-id>
# or: webcodex task reject <task-id>
```

This keeps the acceptance authority local even when the model is hosted.

## Common Errors

- `project_not_configured`: run `webcodex setup`.
- `project_registration_invalid` / `project_credential_invalid`: resolve the
  reported private-state problem; setup will not overwrite or silently rotate
  it.
- `project_credential_rejected`: restore the credential matching the reachable
  server; this is not `agent_offline`.
- `server_unreachable` / `agent_offline`: run `webcodex doctor`, then the
  reported next action.
- `required_capability_unavailable` /
  `structured_validation_unavailable`: upgrade all WebCodex binaries.
- `task_not_active`: start a new task.
- `execution_not_terminal`: review, wait, or cancel the execution.
- `validation_recipe_not_found` / `validation_recipe_ambiguous`: change `cwd`
  or provide the matching explicit recipe.
- `validation_recipe_mismatch` / `validation_manifest_invalid` /
  `package_manager_ambiguous`: correct the reported public project evidence.
- `validation_check_unavailable` / `test_filter_unsupported`: request only a
  semantic input the resolved recipe supports.
- `validation_tool_unavailable`: provide the project's existing tool on the
  Agent host, then use a new operation ID.
- `checks_required`: call `checks_run`.
- `checks_stale`: run a fresh check with a new operation ID.

Every error carries a stable code, human message, retryability,
`user_action_required`, and a suggested next action. Control flow must use the
code, never arbitrary English message matching.

## Related Documentation

- [QUICK_START.md](QUICK_START.md)
- [MCP.md](MCP.md)
- [AUTH_MODEL.md](AUTH_MODEL.md)
- [DEPLOYMENT.md](DEPLOYMENT.md)
- [../SECURITY.md](../SECURITY.md)
