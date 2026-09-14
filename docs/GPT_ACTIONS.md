# GPT Actions

[English](GPT_ACTIONS.md) | [简体中文](GPT_ACTIONS.zh-CN.md)

Use GPT Actions when a Custom GPT needs the Server's OpenAPI compatibility integration. Use [MCP](MCP.md) when the client supports MCP directly; MCP remains the primary ChatGPT integration.

The schema at `/openapi.json` depends on the Server mode:

- a generic runtime Server projects the canonical Adaptive Runtime model surface;
- a project-bound Connector Server projects its separate fourteen-capability Connector surface.

## Import the schema

Import:

```text
https://your-domain.example/openapi.json
```

ChatGPT requires public HTTPS. Configure API-key authentication as HTTP Bearer and use a generated user token (`wc_pat_*`). Runner tokens (`wc_agent_*`) are Runner-transport credentials and must not be placed in a GPT.

After upgrading a Server that used the older generic GPT Actions schema, **re-import `/openapi.json`**. The generic operation names are now the canonical WebCodex runtime tool names rather than the retired camelCase Action vocabulary.

## Generic runtime Server

Generic GPT Actions do not have an independent tool registry. They are a constrained OpenAPI projection of the same `ToolDefinition` authority used by Adaptive Runtime MCP:

```text
ToolDefinition
  -> Adaptive Runtime direct tools
      -> direct GPT Action operations
  -> Adaptive Runtime long tail
      -> call_runtime_tool
```

A tool marked Adaptive Direct is automatically a direct GPT Action unless its canonical definition explicitly declares that GPT Actions cannot represent its protocol semantics. Adding, removing, or re-ranking Adaptive Direct tools therefore updates GPT Actions automatically; there is no separate GPT Action rank or operation list.

Direct operations use canonical snake_case names and canonical input contracts. Examples include `work_on_project`, `runtime_status`, `tool_manifest`, `search_project_texts`, `read_files`, `apply_text_edits`, `run_process`, `run_detached_process`, `run_shell`, `observe_jobs`, `list_jobs`, `cargo_check`, `cargo_test`, `git_review_summary`, `git_diff_hunks`, `show_changes`, `workspace_hygiene_check`, and `finish_coding_task` when those tools are currently Adaptive Direct.

Long-tail model-visible tools use the single gateway:

```json
{
  "tool": "apply_patch",
  "arguments": {
    "project": "agent:runner:project",
    "patch": "..."
  }
}
```

The operation name is `call_runtime_tool`. It accepts only `tool` and `arguments`; there is no `params` envelope and no flattened union of every runtime tool's fields. A currently direct tool should use its own direct Action operation. Model-hidden tools, unknown tools, and tools explicitly unsupported by GPT Actions fail closed.

Protocol-only MCP presentation is not emulated through Actions. For example, Goal Plan / Agent continuation / Work Result App presentation, MCP ResourceLink artifact export, and continuation Endpoint rotation that depends on a separately authorized MCP Host binding are excluded from GPT Actions.

All authorization still runs through the normal ToolRuntime kernel. The Action adapter does not own OAuth/PAT scope policy, Project authority, permission/approval decisions, Runner capability checks, path policy, Session fences, retry rules, or destructive semantics. `x-openai-isConsequential` is only a host UX hint derived from canonical tool approval metadata.

### Descriptions and the 300-character Action limit

Custom GPT Actions reject operation/tool descriptions above 300 characters. WebCodex keeps the canonical MCP description budget independent and larger. An Action reuses the canonical description when it fits; only over-limit tools carry a short presentation override on their canonical `ToolDefinition`. Generated schema/property descriptions are bounded by a presentation-only projector that changes description text, not JSON-schema shape.

### OpenAPI import size

The Custom GPT importer also rejects OpenAPI schemas at 1 MB. WebCodex therefore keeps the generated generic Action document below an internal 800,000-byte JSON budget with CI coverage for compact and pretty-printed serialization. Direct Action request schemas remain the canonical `ToolSpec.input_schema`; response schemas use the real compact `ToolResult` envelope with a generic `output` field instead of inlining each potentially large canonical output schema. This changes only the OpenAPI presentation contract: actual runtime JSON results and canonical/MCP output schemas are unchanged.

### Conversation file import

`import_conversation_files_to_project` remains a direct generic Action when it is Adaptive Direct. ChatGPT supplies `openaiFileIdRefs`; the HTTP adapter converts the host's Action file-reference shape to the canonical internal shape and attaches private GPT Action host provenance. The model cannot set that provenance itself.

MCP host-file import remains a separate trusted provenance path and still requires the configured trusted OAuth MCP client. The two provenance modes share canonical authorization but are not interchangeable.

## Project-bound Connector Server

When the Server runs with project-bound Connector configuration, OpenAPI continues to be generated from the same fourteen capabilities as the canonical MCP Connector:

```text
task_start
task_list
task_resume
files_list
files_read
files_search
code_navigate
edits_apply
checks_run
commands_run
task_review
task_cancel
task_finish
code_impact
```

The Connector already owns the project binding. Start with the Connector actions directly; do not perform broader runtime/project discovery first, and do not put Runner client IDs or runtime project IDs in the prompt.

`task_start` accepts only `normal` (default) and `read_only`. `normal` performs writable work in a managed isolated Git worktree and fails closed if that workspace cannot be prepared; the model never writes the target checkout or accepts its own result. `read_only` permits analysis but rejects edits, commands, and checks.

## Suggested Connector GPT instructions

```text
Use the configured WebCodex project.
Start or continue each user instruction with task_start.
Let task_start reuse the current project context; do not ask the user for IDs.
Use task_list and task_resume only when WebCodex explicitly asks you to recover
or continue an existing task.
Use files_list to see what the project contains before guessing paths.
Use files_read/files_search before edits_apply.
Use code_navigate for read-only semantic status, symbols, definitions,
references, diagnostics, and hover; provide only project-relative paths.
Use code_impact for bounded incoming/outgoing call hierarchy and change-impact
inspection; provide only a project-relative path and source position.
Run checks_run before task_finish.
Use task_review for execution progress and result review.
Use commands_run only when structured capabilities are insufficient and
approval is available.
Never ask the user for internal WebCodex identifiers; use values returned by
the tools when a later call needs one.
```

`checks_run` is the Connector's structured validation Action. It accepts an optional `recipe` enum (`rust`, `node`, `python`, `go`); omit it for deterministic nearest-manifest resolution. See [MCP](MCP.md#validation-recipes) for the recipe table.

`task_finish` creates a stable result; it does not silently apply changes to the target checkout. The host user reviews and decides locally with `webcodex task show`, `webcodex task accept`, or `webcodex task reject`.

## Management and safety

The OpenAPI model surface intentionally excludes users, API tokens, Runner tokens, pairing/enrollment, setup, doctor, npm, server management, and audit endpoints. Use the `webcodex` CLI for those tasks.

Both MCP and GPT Actions use the same ToolRuntime authority model. GPT Actions changes model presentation and HTTP transport only; it never grants authority that the same canonical tool would not have through MCP/runtime execution.

## Related

- [Full Setup](PERSONAL_SETUP.md)
- [Quick Trial](QUICK_START.md)
- [MCP](MCP.md)
- [Authentication](AUTH_MODEL.md)
- [Deployment](DEPLOYMENT.md)
- [SECURITY.md](../SECURITY.md)