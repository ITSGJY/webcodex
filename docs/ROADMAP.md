# Roadmap

This roadmap is intentionally short. Detailed boundaries, LOC/test budgets, acceptance gates, and estimates live in [PROJECT_FIRST_REFINEMENT_PLAN.zh-CN.md](PROJECT_FIRST_REFINEMENT_PLAN.zh-CN.md).

## Completed: WebCodex 0.3.0

- Durable project tasks, executions, validation provenance, results, and local human decisions.
- Project-first setup, readiness, credential boundaries, and a canonical nine-capability hosted surface.
- Rust, Node, Python, and Go validation recipes with deterministic planning and provenance.
- Browser review console with timeline, bounded diff/output, approvals, guidance, devices, activity, and Connect panel.
- Cross-session continuity through `task_list` and `task_resume`.
- Read-only Rust, Python, and TypeScript/JavaScript LSP navigation.
- Hard cut from `webcodex-agent` to `webcodex-runner` across binary, service, npm command, and QUIC ALPN.
- WebSocket and polling release acceptance, package smoke, full binary suites, and the 0.3.0 release baseline.

## Current: Iteration 9 — Agent-Aligned Evidence and Task Reporting

Keep the hosted capability surface stable. Do not make code-size reduction or smooth legacy migration a current goal. Prioritize the runtime and reporting problems exposed by real coding work:

- Keep specialized tools responsible for authorization, project boundaries, transactional edits, command execution, structured state, and raw evidence, while avoiding context-free task-level verdicts.
- Record `cargo_*`, `run_shell`, `run_job`, and other execution paths in one evidence ledger based on actual commands, exit status, and declared intent; do not report `validation_not_run` solely because a specialized validation tool was not used.
- Reserve hard blockers for deterministic facts such as permission denial, conflicts, command or test failures, and sensitive-path risks. Dirty worktrees, truncation, and resolved historical failures are advisory context.
- Let the local Agent produce the final task report: work performed, validation passed or skipped, workspace state, remaining risks, and whether commit, merge, or release is recommended.
- Clarify shell/cwd behavior, layered reconnect status, and oversized startup/read/log payloads to reduce meaningless failures, polling, and context consumption.
- Default project chat connections to the project-bound minimal surface while keeping the full operator runtime for administration; do not try to predict every project situation by adding dedicated tools.

### Stage 1 implemented

- `cargo_fmt`, `cargo_check`, `cargo_test`, declared-purpose `run_shell`, and terminal declared-purpose `run_job` now share one bounded execution-evidence projection. Stable assertion/command identities preserve `historical_failures` while separating `resolved_failures` from `unresolved_failures`.
- `validation_summary`, `session_handoff_summary`, and `finish_coding_task` reuse that projection. Closeout exposes `facts`, `hard_blockers`, and `advisories`; an ordinary dirty worktree and absent task-optional validation are advisory, while unresolved conflicts, command/test failures, active blocking jobs, and sensitive-path risks are blockers.
- `run_shell` and `run_job` accept `purpose` and `shell=sh|bash`, resolve omitted/empty/`.` cwd to the project root, and report project-relative cwd plus executor/shell facts. Job logs default to bounded tails with line counts, truncation, detected summary, and continuation cursors.
- `start_coding_task(detail=minimal|standard|full)` replaces startup flag combinations, `read_file` returns one text representation, and readiness is split across runner process, server transport/registration, project registry, connector endpoint, session binding, and last successful call.
- Console chat connection data names the project-bound surface explicitly and does not expose the operator runtime as the model default; the full operator runtime remains available for management and internal execution.

### Stage 2 delivered — Trusted Agent Authority and Reconnect Continuity

- Canonical two-mode authority: `WEBCODEX_AUTHORITY_MODE=trusted_agent|restricted` (default `trusted_agent`). Under `trusted_agent`, consequential runtime tools auto-execute after hard safety checks with no approval interruptions while still recording auditable decisions (`trusted_agent_authority`); external release actions stay user-task-scoped. Under `restricted`, runtime tools are denied and the connector `commands_run` keeps the one-time human approval loop. A set `WEBCODEX_PERMISSION_MODE` is rejected fail-closed with no alias or migration.
- Hard boundaries unchanged by authority mode: OAuth scopes, project roots, read-only sessions, path/sensitive-path policy, concurrent-overwrite guards, credential redaction, job cancel/reclaim, and immutable release targets.
- `runtime_status`/`start_coding_task` project a canonical `authority` object; the old `permissions` profile object is gone. Connector auto-authorization records a durable `authority_auto_authorized` task event instead of approval records.
- `runtime_status.connection_layers` became an observation contract: every layer carries status/observed_at/source/age/staleness/reason_code plus real facts; configuration never implies readiness, stale registrations are never callable, session bindings are honestly reported as process-local and lost after restart (continue with the durable `wc_sess_*` id), and `last_successful_tool_call` counts only meaningful successful calls.
- `runtime_status.version_compatibility` diagnoses mixed server/runner versions (compatible / version_mismatch / capability_mismatch / no_runners) with per-runner build and protocol facts and no fallback shims. Runners report `process_started_at`, build version/commit, and shell profile dialects (`default_dialect`, `available_dialects`, per-profile `dialect`).
- `start_coding_task` hard cut: the legacy startup flags are removed from the wire and internals; `detail=minimal|standard|full` is the only projection control and unknown fields error strictly.
- New validation lanes: `cargo test --bin webcodex reconnect`, `cargo test --bin webcodex trusted_smoke`, and the real-process harness `scripts/e2e_reconnect_ws.sh`.

## Deferred

- Code-size reduction and LOC gates.
- Legacy migration compatibility and automatic upgrade migration.
- SSH and cross-device Operations Profile.
- PTY terminal workflows.
- Workflow DSL and generic checkpoint orchestration.
- Additional hosted capabilities.
- Full browser IDE.
- Write-capable LSP operations such as rename and code actions.

## Non-Goals

- Full IDE replacement.
- Autonomous DevOps by default.
- Arbitrary computer use as a core promise.
- Compatibility aliases or dual response shapes for hypothetical consumers.
- Treating tool count, test count, or LOC growth as product progress.
