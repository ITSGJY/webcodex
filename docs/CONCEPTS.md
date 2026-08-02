# Concepts

[English](CONCEPTS.md) | [简体中文](CONCEPTS.zh-CN.md)

WebCodex lets an online AI client operate a private repository through a self-hosted, auditable tool runtime. This page defines the terms used across the setup, MCP, GPT Actions, security, and architecture docs.

## Mental Model

```text
Online model / client
        |
        | MCP / GPT Actions / REST tool calls
        v
WebCodex server
        |
        | authenticated agent bridge
        v
WebCodex agent
        |
        v
Agent-registered project
```

Projects live on the agent machine. The agent registers allowed directories with the server. The server does not scan your filesystem.

## Core Terms

### Online Model / Client

The online model is ChatGPT, Claude, Grok, or another hosted model. The client is the host surface that sends tool calls to WebCodex, such as remote MCP, GPT Actions, or a REST integration.

The model does not receive direct filesystem access. It can only call the tools exposed by the configured client and authorized by WebCodex.

### WebCodex Server

`webcodex` is the self-hosted server. It exposes the MCP endpoint, the GPT Actions OpenAPI schema, and runtime REST APIs. It authenticates callers, applies tool policy, records bounded session evidence, and routes project work to connected agents.

The server is the stable online entry point. It should be deployed behind HTTPS before connecting hosted clients.

### WebCodex Agent

`webcodex-runner` runs on the machine that has the code. It connects back to the server, registers allowed projects, and executes file, Git, patch, validation, shell, job, artifact, and checkpoint requests inside those project boundaries.

The agent is the trust boundary closest to your repository. Configure it with narrow allowed roots and shell profiles appropriate for the projects it serves.

### Agent-Registered Project

An agent-registered project is a directory the agent has made available to the server. The server does not invent or discover project paths on its own.

Runtime project ids use this shape:

```text
agent:<client_id>:<project_id>
```

`client_id` identifies the agent connection profile. `project_id` is the
project id registered by that agent. A project-bound Connector resolves this
internally; ordinary users do not put the runtime project id in prompts.

### Tool Runtime

The ToolRuntime is the protocol-independent execution layer. MCP, GPT Actions, and REST wrappers translate client requests into the same runtime tool calls.

Common tool groups:

- Discovery: `runtime_status`, `list_projects`, `list_agents`, `tool_manifest`.
- Inspect: `read_file`, then `run_shell` with `rg` or `git grep` for code
  search, plus `git_status` / `git_diff_hunks` for worktree review.
  `search_project_text` remains available as a compatibility path.
- Edit: `apply_text_edits` (guarded transactional file changes), `apply_patch_checked` (complex checked unified diff), `write_project_file` (intentional full rewrite). Line/pattern tools remain compatibility paths.
- Validate: `validate_patch`, `cargo_fmt`, `cargo_check`, `cargo_test`.
- Review: `show_changes`, `workspace_hygiene_check`.
- Finish: `finish_coding_task`, `session_handoff_summary`.
- Escape hatch: `run_shell`, `run_job`.

### MCP

MCP clients connect to:

```text
https://your-domain.example/mcp
```

Use MCP if your client supports remote MCP. MCP exposes WebCodex runtime tools through MCP framing while keeping the same server, agent, project id, and safety boundaries used by GPT Actions.

### GPT Actions

GPT Actions import the WebCodex OpenAPI schema:

```text
https://your-domain.example/openapi.json
```

Use GPT Actions if you are building a Custom GPT. A project-bound Connector
exposes the same focused twelve-capability surface as MCP and shares its
authorization and execution boundaries.

### Project Work Context

The same chat window continues its current repository work. WebCodex keeps
durable history separately for each exact repository, switches context when
the window changes repository, and restores the previous context when it
returns. Follow-up instructions append to that history.

Before reuse, WebCodex checks the repository path, branch and HEAD, worktree,
applicable repository rules, target directory, and project manifests. It reuses
unchanged context and refreshes changed slices. Task IDs and window bindings
remain implementation details in the ordinary Connector path.

### Session

A Workflow Session is the full operator runtime's bounded evidence ledger.
`start_coding_task` defaults to continuing the same window and exact repository,
appending each follow-up instruction while retaining the original root goal.
Repository switches preserve independent contexts; `new_session=true` is the
explicit advanced isolation path. Current bindings are process-local, so an
explicit returned id remains the restart-recovery path. Project-bound Connector
users do not create, bind, upgrade, or pass Workflow Session IDs; their normal
continuity comes from the project work context above.

Dirty workspace is an expected development state and does not prevent starting a coding task. Existing worktree changes (tracked modified, staged, untracked, renamed, deleted, or conflicted) must be inspected and preserved. They are not automatically reverted, stashed, cleaned, or overwritten. Startup blocking is reserved for conditions that make the project inaccessible or the requested work unsafe or impossible (missing project path, resolution failure, agent offline when required, permission denial, or path safety failures). Review and finish tools may still treat a dirty closeout as non-pass evidence.

Sessions are task-continuity evidence, not a full surveillance log. They record bounded, redacted facts such as tool names, status, project id, validation summaries, permission decisions, and closeout state.

### Handoff / Finish

`finish_coding_task` is the normal closeout tool. It can include review evidence, workspace hygiene, validation summary, job state, warnings, and canonical task/evidence outcomes.

`session_handoff_summary` is the read-only handoff tool. Use it when another operator, client, or later session needs to continue from the current state.

Both, together with `start_coding_task`, also surface a deterministic `continuation_feedback` projection of the previous attempt — a read-only summary of the last attempt's activity, changed paths, validation state, and proven Job/guidance status, plus a `validation_delta` that is only comparable when the two validation runs are proven to cover the same scope. It is never an LLM summary, never a new verdict, and never re-runs validation.

### Validation

Validation is evidence that the change was checked. WebCodex provides structured helpers such as `validate_patch`, `cargo_fmt`, `cargo_check`, and `cargo_test`.

Choose validation that fits the change. A docs-only edit may need `git diff --check` outside WebCodex or a focused review; a Rust behavior change should run Cargo checks or tests.

`cargo_check`, `cargo_test`, and `cargo_fmt(check=true)` run the command exactly
once with `timeout_secs` as the total runtime budget (1..=3600). A short
validation returns immediately; a long one continues as a queryable Job and
returns `job_id` — poll `job_status` / `validation_summary` rather than re-running.
`cargo_fmt(check=false)` mutates source and never auto-promotes.

### Review / Hygiene

Review tools show what changed before the user accepts it. Use `show_changes` for file lists, status, diff stats, and optional bounded hunks. Use `workspace_hygiene_check` to detect untracked smoke files, temporary files, blocking jobs, and other closeout risks.

### `run_shell` As Escape Hatch

`run_shell` can run bounded project commands through the agent. It is the
preferred code-search and inspection path with `rg` or `git grep`, and is also
useful for project-specific checks that do not have a structured helper yet.

It is not the default editing path and not a way to bypass project policy.
Treat shell/job tools as powerful operations that require trusted
configuration and human review.

## Default Coding Loop

1. Start or continue the instruction with `task_start`.
2. Inspect with `files_list`, `files_read`, and `files_search`.
3. Edit with `edits_apply`.
4. Validate with `checks_run`.
5. Finish with `task_finish` and review with `task_review`.

The model passes durable tool IDs between these calls; the user does not manage
them. `task_list` and `task_resume` are recovery operations when automatic
transport-window continuity is unavailable.

## Where To Go Next

- First setup: [QUICK_START.md](QUICK_START.md)
- Demo workflow: [DEMO.md](DEMO.md)
- Architecture: [ARCHITECTURE.md](ARCHITECTURE.md)
- MCP: [MCP.md](MCP.md)
- GPT Actions: [GPT_ACTIONS.md](GPT_ACTIONS.md)
- Security: [../SECURITY.md](../SECURITY.md)
