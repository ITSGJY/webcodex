# AGENTS.md — WebCodex Trusted Agent Contract

Executable rules for autonomous coding agents working in this repository.
Long-form design context lives under [`docs/agent/`](docs/agent/).

---

## 1. Project Identity

- **Project:** WebCodex
- **Canonical repository:** `https://github.com/yyjeqhc/webcodex.git`
- **Default managed project id:** `agent:special:webcodex`
- **Canonical checkout path:** `/root/git/webcodex`
- Other registered checkouts or deployment paths may exist. Treat them as
  independent worktrees and verify their branch, HEAD, remote, and dirty state
  before using them.
- Confirm the resolved project, active repository, branch, worktree state, and
  recent commits before changing files.
- Do not modify unrelated repositories, worktrees, hosts, or deployment targets.

---

## 2. Operating Model

This repository uses a **trusted-agent** workflow.

- Platform, system, and user instructions define the authorization boundary.
  Do not create a second approval system inside the coding workflow.
- Once the user assigns a task, autonomously perform the inspection, edits,
  shell commands, tests, builds, local service operations, Git operations, and
  recovery steps reasonably required to complete it.
- Use project tools as a reliable execution and evidence layer, not as a
  substitute for contextual engineering judgment.
- Tool verdicts are facts or signals. The Agent owns the final task conclusion.
- Do not interrupt merely because the worktree is dirty, a generic shell/job
  tool was used, output was truncated, a retry was needed, or a previous attempt
  failed and was later corrected.
- Ask the user only when a required credential or external target is missing,
  the requested outcome is materially ambiguous, instructions conflict, or
  unknown overlapping work may be destroyed.
- For a large task, form and update a plan while continuing execution. Do not
  stop solely because the task spans many files or subsystems.

The target interaction is: the user gives an outcome, the Agent completes the
work, validates it, and returns one evidence-based report.

---

## 3. Hard Boundaries

These are execution-correctness and privacy boundaries, not a second layer of
product judgment.

- Never print, commit, or expose reusable credentials, authorization headers,
  private keys, token values, or secret file contents.
- Stay inside the resolved project and explicitly authorized deployment roots.
- Inspect and preserve existing user work. Stop only when ownership is unclear,
  changes overlap, or proceeding could lose work.
- Do not silently overwrite concurrent file changes; use guarded or
  transactional edits when available.
- Do not weaken security checks, required schema fields, permission enforcement,
  or meaningful tests merely to obtain a green result.
- Do not bypass platform policy, valid session guards, or an explicit user
  restriction.
- Do not force-push, overwrite tags/releases, rewrite published history, or
  destructively reset other work unless the user explicitly requests that exact
  destructive operation and its target.
- Redact sensitive command output. Prefer bounded summaries and retrieve full
  logs only when needed for diagnosis.

---

## 4. Editing and Execution

- Inspect relevant code, documentation, and existing diffs before editing.
- Keep changes aligned with the requested outcome, but make all necessary
  cross-cutting fixes rather than leaving knowingly inconsistent surfaces.
- Choose the most effective execution path:
  - use `apply_text_edits` for guarded transactional text changes;
  - use `apply_patch_checked` for complex unified diffs;
  - use `write_project_file` for creates or intentional full rewrites;
  - use shell, scripts, or repository-native tools whenever they are the clearer
    or more complete way to perform project work.
- Shell is a first-class execution path. It may be used for inspection, editing,
  tests, builds, package management, Git, service control, release work, and
  diagnostics within the authorized task scope.
- Specialized tools are preferred only when they improve transactional safety,
  structured evidence, or efficiency. Their absence does not make an operation
  invalid.
- Do not preserve compatibility aliases, dual response shapes, or obsolete
  paths for hypothetical consumers. Add compatibility only for a named current
  contract or an explicitly requested migration.
- Code-size reduction and legacy migration are not current iteration goals.
  Perform them only when directly required by the task or when deletion is the
  clearest fix for the behavior being changed.

For current product direction, read
[`docs/PROJECT_FIRST_REFINEMENT_PLAN.zh-CN.md`](docs/PROJECT_FIRST_REFINEMENT_PLAN.zh-CN.md)
and [`docs/ROADMAP.md`](docs/ROADMAP.md).

---

## 5. Git, Commit, Release, and Deployment

- Check `git status` and recent `git log` before editing and before commit.
- A dirty worktree is context, not automatic failure. Understand and preserve
  it; ordinary development does not require a globally clean tree.
- Create commits when the user requests a commit or when the assigned task
  explicitly includes completing the repository change as a committed unit.
- Commit prefixes: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`.
- Prefer coherent commits. Do not split changes mechanically when one behavior
  requires code, tests, and documentation together.
- Push, tag, publish, create a GitHub Release, or deploy when the user's task
  explicitly includes that action and identifies the target sufficiently to
  execute safely.
- Before an external release action, verify the actual repository, branch,
  version/tag/package, remote target, relevant validation, and immutable-target
  state. Never print credentials or move an existing published tag.
- Report post-tag checksum/manifest commits without moving the tag.

Expanded release mechanics:
[`docs/agent/release-process.md`](docs/agent/release-process.md) and
[`docs/RELEASE_CHECKLIST.md`](docs/RELEASE_CHECKLIST.md).

---

## 6. Validation and Evidence

The Agent chooses validation proportional to the change and owns the final
interpretation.

- Code changes require relevant validation before claiming completion.
- Any successfully recorded execution path may provide validation evidence:
  `cargo_*`, `run_shell`, `run_job`, repository scripts, or other native tools.
- Prefer structured tools when they reduce parsing or preserve better evidence,
  but never report `validation_not_run` solely because validation used shell or
  a generic job path.
- Record what ran, the resolved cwd/shell when relevant, exit status, detected
  test/build summary, truncation, and skipped checks.
- Distinguish current failures, historical failures, resolved retries, and
  pre-existing failures. A later successful retry should mark the earlier
  failure as resolved without deleting its evidence.
- Dirty worktree, optional LSP unavailability, output truncation, or absence of
  a full suite are advisory facts unless the current task makes them blocking.
- Deterministic hard blockers are limited to facts such as permission denial,
  unresolved conflict, command/test failure, sensitive-path risk, lost
  execution state, or an explicitly required check not completed.

Typical Rust baseline, adjusted to the touched domain:

```text
cargo fmt --all -- --check
cargo check --workspace --all-targets
focused cargo test lanes
git diff --check
git status --short
```

Use an explicit package for binary-focused tests:

```text
cargo test -p webcodex --bin webcodex
cargo test -p webcodex-cli --bin webcodex-cli
cargo test -p webcodex-runner --bin webcodex-runner
```

Run `cargo test -p webcodex --bin webcodex` for broad server changes,
release/merge readiness, or when the Agent judges focused lanes insufficient.
Do not run it mechanically for pure documentation changes.

Broader lanes: [`docs/TESTING.md`](docs/TESTING.md).

---

## 7. Architecture Invariants

### Runtime and hosted surfaces

- Runtime tool metadata, registry, OAuth scope policy, MCP `tools/list`, and
  OpenAPI names must stay synchronized when a tool is added or renamed.
- The project-bound model surface remains the canonical minimal coding surface.
  The full operator runtime is for administration and internal execution.
- Do not add dedicated tools merely to predict every project situation. Prefer
  general execution plus reliable evidence and Agent reasoning.
- GPT Actions must stay below the 30-operation limit and must not expose legacy
  `/api/codex`, token-management, or pairing endpoints.

### Sessions

- Workflow session IDs retain the `wc_sess_*` format.
- Explicit `session_id` wins over current-session binding.
- Unknown explicit IDs fail as `unknown_session_id`; never silently fall back.
- Explicitly read-only sessions deny writes and shell/jobs.
- Session denial occurs before mutation or agent enqueue and is recorded when
  the session is valid.
- Current-session bindings remain isolated by principal, transport, and project.

### Compatibility

- One canonical field per concept.
- No alias/dual shape without a named migration.
- No compatibility code for hypothetical consumers.

Detailed architecture:
[`docs/agent/architecture-decisions.md`](docs/agent/architecture-decisions.md),
[`docs/agent/session-model.md`](docs/agent/session-model.md), and
[`docs/agent/openapi-guidelines.md`](docs/agent/openapi-guidelines.md).

---

## 8. Final Agent Report

For code, documentation, operations, release, or deployment tasks, return one
complete contextual report containing:

- outcome and behavior changed;
- files or external resources changed;
- commands and validation performed;
- validations passed, failed, skipped, or resolved by retry;
- current worktree and commit state;
- remaining risks or limitations;
- whether commit, merge, push, release, or deployment is recommended or already
  completed.

Do not copy a tool's aggregate verdict as the task conclusion. Explain what the
recorded facts mean for the user's requested outcome.

For review-only tasks, report findings, evidence, and recommendations without
inventing changes or validation.
