# Roadmap

WebCodex is a remote, auditable, bounded execution layer for coding assistants. It is not an embedded model, autonomous agent loop, or full browser IDE.

## Current baseline

- Project-bound MCP and OpenAPI surfaces expose a small canonical capability set.
- Durable tasks, executions, events, results, approvals, resumable review, and bounded output are persisted.
- Server, CLI, and runner share code through workspace library crates with enforced package boundaries.
- Authentication, project grants, allowed roots, path policy, authority mode, and audit evidence remain explicit boundaries.
- Structured validation supports Rust, Node, Python, and Go recipes without installing dependencies or running networked setup hooks.
- The review console, reconnect continuity, read-only LSP navigation, shell profiles, and transport fallbacks are available.

## Next priorities

1. Improve task continuation and operator visibility without expanding the public capability surface unnecessarily.
2. Tighten installation, upgrade, rollback, and mixed-version diagnostics for self-hosted deployments.
3. Continue reducing duplicated projections and oversized responses while preserving protocol compatibility.
4. Extend end-to-end coverage for authentication, transport recovery, validation provenance, and process cleanup.
5. Evaluate additional provider integrations only when they preserve project, permission, timeout, and audit boundaries.

## Completion criteria

A roadmap item is complete only when its public contract is documented, focused and regression validation pass, failure behavior is explicit, and deployment or rollback guidance exists when operations are affected.

## Non-goals

- Built-in model selection, prompt loops, context compaction, or token budgeting.
- Full IDE replacement or arbitrary computer use.
- Autonomous deployment or production mutation by default.
- Compatibility aliases for hypothetical consumers.
- Treating tool count, test count, or lines of code as product progress.
