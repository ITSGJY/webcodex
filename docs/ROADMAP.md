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
