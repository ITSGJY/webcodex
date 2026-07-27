# WebCodex 0.3.0

[English](RELEASE_NOTES_v0.3.0.md) | [简体中文](RELEASE_NOTES_v0.3.0.zh-CN.md)

WebCodex 0.3.0 turns the self-hosted coding runtime into a durable, project-first workflow that can survive client-window changes, expose reviewable execution evidence, and keep human control in the loop.

The release is aimed at operators using ChatGPT, MCP clients, or GPT Actions against private repositories while execution remains on machines they control.

## Highlights

- **Durable project tasks.** Hosted clients can start project-bound tasks, apply edits in isolated task workspaces, run project-aware validation recipes, review results, and accept or reject them explicitly.
- **Cross-session continuity.** `task_list` and `task_resume` recover recent tasks without relying on a client-specific hidden session signal. Rejection reasons can be delivered back to the model as one-time human guidance.
- **A usable review console.** The browser console now includes task timelines, applied paths, diff review, accept/reject/cancel controls, guidance, pending approvals, device and activity views, and a Connect panel for MCP, GPT Actions, and OAuth endpoints.
- **Better inspection and validation.** The runtime adds Git-index-backed file discovery, deterministic project overview, richer project search, compact numbered reads, read-only Rust LSP navigation, and structured validation summaries.
- **Stronger execution boundaries.** Session lifecycle, permission decisions, durable evidence, transport timeouts, disconnect handling, process-group cleanup, provenance checks, sensitive-path policy, and symlink protections have been tightened.
- **Simpler operator commands.** Project setup, doctor/status flows, login/logout/status, enrollment, service installation, and the runner naming contract are more consistent.

## Project-First Execution

0.3.0 introduces a durable execution path behind the hosted connector surface:

- Tasks are owned by the authenticated subject and bound to an agent-registered project.
- Editing and validation run through a reusable execution engine rather than an ad-hoc client loop.
- Task results preserve applied paths, validation state, execution evidence, and the local accept/reject decision.
- Review remains available when a live workspace scan degrades by falling back to persisted applied-path evidence.
- Deterministic workspace-provenance failures terminate promptly instead of being flattened into retryable storage errors.
- `doctor` reports common repository hygiene issues such as untracked build artifacts and a missing `.gitignore` without incorrectly marking an otherwise usable project as unavailable.

## Review, Guidance, And Continuity

The host-local console is no longer only a minimal result page:

- Running and completed tasks expose a bounded event timeline and currently applied paths.
- Review actions bind to a stable task/result identity and support accept, reject, and cancel.
- A reject action may include a bounded reason; the decision remains durable even if guidance delivery later fails.
- Human guidance is delivered once through the normal capability response path and is not consumed by console refreshes.
- `task_list` and `task_resume` let a new chat window find and rebind to tasks owned by the same credential.
- The Connect panel derives non-secret MCP, OpenAPI, and OAuth endpoint information from the configured public origin or the current browser origin and warns when a loopback address is not reachable from a hosted client.

## Tooling And Developer Experience

- `start_coding_task` returns a deterministic startup package with project resolution, Git state, project rules, runtime health, semantic-navigation availability, and a bounded tool manifest.
- `finish_coding_task`, session handoff, validation summaries, and hygiene checks provide bounded closeout evidence without exposing raw command output or credentials.
- `list_project_tracked_files` discovers files from the Git index with roll-up, pagination, and glob filtering.
- `search_project_text` supports bounded context, include/exclude globs, result modes, and explicit backend/truncation metadata.
- `read_file` avoids duplicated line payloads and keeps a stable `numbered_text` representation.
- Rust workspaces can use read-only document symbols, diagnostics, definitions, references, hover, and workspace symbols through the agent-side language server bridge.
- Structured Cargo validation records parser-backed events for later closeout and recovery analysis.
- Canonical edits favor `apply_text_edits`; checked unified diffs use `apply_patch_checked`; bounded shell and jobs remain escape hatches rather than the default editing or validation path.

## Reliability And Security Changes

- Workflow sessions have explicit lifecycle and close semantics; valid closed sessions remain queryable while consequential tools are denied.
- Permission decisions are centralized before mutation or agent enqueue, with audit correlation and fail-closed read-only guards.
- Pending synchronous requests fail promptly when an agent transport disconnects or an agent falls outside the online window.
- MCP, HTTP service, local command, Git, and validation paths have bounded timeout backstops.
- Local commands reap process groups so background descendants cannot keep output pipes open indefinitely.
- Session persistence moves off request-critical paths, and SQLite storage uses hardened open/WAL/cleanup behavior.
- Sensitive-path checks are shared across file operations; `read_file` and connection storage reject symlink escapes and unverified directories.
- The experimental Landlock command-sandbox foundation remains disabled for `read_only` tasks because it does not restrict reads, inherited environment variables, or network access. `read_only` therefore continues to refuse command execution.

## Breaking Changes

### `webcodex-agent` is now `webcodex-runner`

The executable, npm command, systemd unit, configuration examples, and QUIC ALPN now use:

```text
webcodex-runner
webcodex-runner.service
webcodex-runner/1
```

No old-name binary, npm, service, or protocol alias is shipped. Mixed 0.2.x and 0.3.0 runner/server deployments may fail to start or connect.

### GPT Actions editing surface is smaller

The dedicated `writeProjectFile` and `replaceProjectFileText` operations are no longer part of the 25-operation GPT Actions schema. Compatibility tools remain available through `callRuntimeTool`; new workflows should prefer `apply_text_edits` and `apply_patch_checked`. Refresh the imported OpenAPI schema after upgrading.

### `read_file` line output is canonical

When `with_line_numbers: true`, clients should read `numbered_text`. The duplicate `lines` array is not returned.

### `read_only` tasks do not run commands

`commands_run` remains consequential and is denied before approval, reservation, or agent enqueue in a `read_only` task.

### Canonical response shapes

Several obsolete wire aliases and duplicate closeout fields were removed. Clients that pinned old response fields should refresh MCP/OpenAPI schemas and consume the canonical names returned by 0.3.0.

## Upgrade Notes

1. Replace all three binaries with matching 0.3.0 builds: `webcodex`, `webcodex-cli`, and `webcodex-runner`.
2. Stop and disable any old `webcodex-agent.service`, then install and enable `webcodex-runner.service`. Verify that both units are not running simultaneously.
3. Update scripts, service overrides, binary paths, and runner configuration that still reference `webcodex-agent`.
4. When using QUIC, upgrade server and runner together so both use the `webcodex-runner/1` ALPN.
5. Refresh `/openapi.json` in Custom GPT Actions and reconnect MCP clients when they cache tool schemas.
6. Restart the server and runners, run compact `runtime_status`, and verify that every deployed binary reports 0.3.0 and the same clean build revision.
7. Use `task_list` followed by `task_resume` when continuing durable work from a new client window.

The npm package remains a thin installer. For 0.3.0 it is prepared for `linux-x64` only and must not be published until the exact GitHub Release tarball checksum has been written to `npm/webcodex/manifest.json`.

## Security Model

- Repository access is limited to agent-registered projects and configured roots.
- The server does not scan arbitrary agent filesystems.
- Tokens, pairing codes, Authorization headers, env files, complete client configs, and reusable token hashes must not be exposed in prompts, logs, examples, or commits.
- Shell/job tools remain powerful consequential operations and require scoped configuration and operator review.
- Browser console projections expose bounded, non-secret operational facts; credentials remain outside the DOM and response payloads.
- Session, task, validation, audit, and finish evidence improve reviewability but do not replace normal code review, host logging, backups, or infrastructure hardening.

See [../SECURITY.md](../SECURITY.md), [CONCEPTS.md](CONCEPTS.md), and [READ_ONLY_COMMAND_SANDBOX.md](READ_ONLY_COMMAND_SANDBOX.md).

## Known Limitations

- WebCodex is self-hosted infrastructure, not hosted SaaS.
- The 0.3.0 npm wrapper is prepared for Linux x64 only.
- The browser console is a review and operations surface, not a full IDE.
- LSP navigation is read-only, Rust-focused, workspace-only, and does not navigate dependency source.
- `read_only` tasks cannot run commands.
- WebSocket and polling are the standard zero-config release-smoke transports. QUIC remains an advanced deployment option with separate focused coverage.
- Desktop GUI, PTY terminal workflows, and full multi-window optimistic coordination are not part of this release.
- Production security still depends on HTTPS, reverse-proxy policy, scoped tokens, OS-user isolation, agent configuration, and operator discipline.

## Validation

The release candidate passed the full Rust binary suite (1,750 main tests with 4 ignored, 220 CLI tests, and 402 runner tests with 2 ignored), focused process-group cleanup coverage, source/release checks, frontend typecheck/tests/dist verification, and npm self-tests. WebSocket and polling zero-config E2E each passed 108/108 checks; the coding-loop comparison passed 6/6 cases; 83 Markdown files contained 436 valid local links with no missing targets; and the release-mode npm package smoke installed all three 0.3.0 binaries successfully.

The release-preparation commit intentionally carries a checksum placeholder. The published npm package is not ready until the exact uploaded 0.3.0 Linux x64 artifact checksum is committed after the immutable release tag without moving that tag. Post-deployment acceptance remains a release-operator step after the final binaries are installed.

## Next

After 0.3.0, development should prioritize reduced round trips, deterministic retries, better deployment-health diagnostics, and decision-ready review summaries before expanding the public capability surface. See [ROADMAP.md](ROADMAP.md).
