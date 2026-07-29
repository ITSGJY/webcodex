# AGENTS.md — WebCodex Repository Guide

These are the always-on rules for ordinary repository work. This file is a map,
not a complete design specification. Read linked documents only when the task
touches their domain. A deeper `AGENTS.md`, if added later, governs its directory
tree and may refine these rules.

## 1. Verify the actual state

- Work only in the repository, worktree, and external target authorized by the
  user.
- Before editing, confirm the repository root, branch, HEAD, worktree status,
  relevant existing changes, and recent history.
- Treat hashes, paths, branches, and runtime state in a prompt as expectations to
  verify, not reasons to overwrite the actual repository.
- Preserve unrelated or concurrent work. Do not reset, rebase, restore, clean, or
  rewrite history merely to match an expected baseline.
- Inspect the relevant implementation, tests, documentation, and current diff
  before changing them.

## 2. Follow intent with engineering judgment

- The requested outcome, scope, prohibited actions, safety requirements, and
  acceptance conditions are hard constraints.
- Suggested file names, symbols, commands, code sketches, and step order are
  guidance unless explicitly made mandatory.
- If guidance conflicts with the current code, repository conventions, an
  upstream interface, or a safer implementation, make the smallest necessary
  adjustment that still satisfies the task.
- Do not mechanically implement instructions that create a known bug,
  inconsistent state, compatibility hazard, resource leak, or false validation.
- Ask only when required information cannot be discovered, a credential or target
  is missing, instructions materially conflict, or proceeding could destroy work.
  Otherwise continue with reasonable judgment.
- Explain material deviations and their evidence in the final report.

## 3. Protect data, security, and ownership

- Never print, commit, or expose credentials, authorization headers, private
  keys, tokens, secret files, or sensitive command output.
- Do not silently overwrite concurrent changes; prefer guarded or
  conflict-detecting edits.
- Do not weaken authentication, authorization, validation, schemas, sandboxing,
  or meaningful tests merely to obtain a green result.
- Do not force-push, move published tags, overwrite releases, destructively reset
  other work, or rewrite published history without an explicit request naming
  that operation and target.
- Push, publish, tag, release, deploy, restart production services, or alter
  external systems only when the task explicitly includes it and identifies the
  destination sufficiently.

## 4. Make coherent changes

- Prefer the smallest coherent change that fully resolves the requested behavior.
- Follow existing architecture and naming unless the task requires changing them.
  Avoid speculative compatibility, duplicate representations, unrelated cleanup,
  and broad refactors without a named current need.
- Keep code, tests, schemas, generated surfaces, packaging, and documentation
  consistent across every affected interface.
- Add or update tests for behavior changes and regressions when practical.
- Update user or operator documentation when commands, configuration, public
  behavior, packaging, or operational procedures change.
- Prefer repository-native scripts and workflows over ad hoc substitutes.

Product direction and structure:
[`docs/ROADMAP.md`](docs/ROADMAP.md) and
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## 5. Validate proportionally

- Run checks relevant to the changed files and behavior; do not run the full
  workspace mechanically for documentation-only edits.
- For code, normally include formatting, compilation or static checks, focused
  tests, and broader tests when the affected surface or release risk warrants it.
- Record commands, working directory when relevant, exit status, result summary,
  skipped checks, and output truncation.
- Distinguish current failures from pre-existing failures, expected negative
  tests, and failures resolved by a successful retry.
- Interpret underlying evidence rather than treating a dirty worktree, optional
  tool absence, or generic tool label as proof of failure.
- Before finishing, review the diff, check whitespace, and confirm worktree,
  conflict, and active-job state.

Testing guidance: [`docs/TESTING.md`](docs/TESTING.md).

## 6. Keep Git and delivery explicit

- Review status, diff, and recent history before committing.
- Commit when requested or when a committed repository unit is clearly part of
  the assignment. Use `feat:`, `fix:`, `refactor:`, `docs:`, or `test:`.
- Do not mix unrelated work into the task commit or amend an unrelated commit.
- Before release or deployment, verify the actual repository, branch, version,
  artifact, remote target, validation state, and immutable targets.

Release guidance:
[`docs/agent/release-process.md`](docs/agent/release-process.md) and
[`docs/RELEASE_CHECKLIST.md`](docs/RELEASE_CHECKLIST.md).

## 7. Load domain rules when relevant

- Runtime tools, API projections, and hosted surfaces:
  [`docs/agent/openapi-guidelines.md`](docs/agent/openapi-guidelines.md).
- Workflow and audit sessions:
  [`docs/agent/session-model.md`](docs/agent/session-model.md).
- Authority and execution boundaries:
  [`docs/agent/permission-model.md`](docs/agent/permission-model.md).
- Architecture decisions and compatibility policy:
  [`docs/agent/architecture-decisions.md`](docs/agent/architecture-decisions.md).
- Experimental Claude Code MCP provider:
  [`docs/agent/claude-code-mcp-provider.md`](docs/agent/claude-code-mcp-provider.md).

Use these documents as sources of truth instead of copying subsystem invariants
into every task prompt.

## 8. Report the completed state

Report the outcome, changed files or external resources, validation performed,
final Git and job state, material deviations, and remaining risks. State whether
push, release, publication, deployment, or service operations were performed.
For review-only work, report findings and evidence without inventing changes.
Tool output is evidence, not a substitute for engineering judgment.
