# Project formatting lifecycle — #962 contract freeze

Status: **pre-implementation v1 contract and regression boundary**. This document belongs to the eventual full feature PR; it is **not** a standalone planning PR. Base: upstream/main `8c7a1c3fd1cf8baeac40652ca8b252cc89e2c685` (includes #1004).

## Product goal and authority

Expose **one** ordinary `project_format` lifecycle that actually applies supported formatters and reports observed mutation truth, rather than requiring callers to choose `cargo_fmt` or an unconstrained process. Keep `project_validate(action=format_check)` separate and unchanged.

Initial backend target: Rust/Cargo and Python/Ruff, **only if both satisfy this contract**. If a backend cannot prove bounded selection and truthful effects, fail closed or defer that backend rather than weakening the contract for parity. No Node/Go, arbitrary script, user-provided executable/argv, formatter installation, network fallback, workspace-wide exclusions, or new model-visible aliases.

An intentional writer MUST be independently authorized as a source mutation (`project:write`) and as Runner execution (`job:run`) under canonical permission/Runner capability checks. Public effect = **Mutate**; idempotency must not imply safe replay. This is **not** read-only validation, and no status code or truncated stdout alone proves what files changed. Do not rewrite legacy `cargo_fmt` behavior without a separate demonstrated compatibility need.

## Single execution contract

1. Model inputs are the exact registered Project, optional project-relative cwd, closed adapter choice (`auto|rust|python`) and bounded timing only. No options implying arbitrary command execution. Auto must fail closed on ambiguous or mismatched recipes.
2. Runner resolves the owning recipe root, selected formatter/configuration and fixed argv, validates containment and bounded manifest inputs, computes a deterministic plan identity/provenance. Unsupported versions or missing installed tooling are **not started**; never install or silently switch executables.
3. The admission path and the worker immediately before execution independently re-resolve the plan against the authoritative Project root and manifest/lock/config/target facts. Invalidated plans never write.
4. A single Runner-owned execution identity governs the precheck, optional mutation, and post-execution evidence. Precheck passes -> known no-op only if a bounded, complete file witness establishes no mutation; stable formatter diff -> mutate once; other precheck results -> no mutation. Never run a second independent operation after a lost first result. Preserve same Job for asynchronous continuation, cancellation, timeouts and disconnects.
5. Before *potential* mutation, collect a **bounded** manifest/source witness of the exact permitted candidate files (canonical path, regular-file type, size/content digest). Reject links, escaped paths, unsupported source topologies or oversized scans instead of following them. After the operation compare against a complete bounded witness. Only complete trustworthy before/after comparisons may establish `state_changed=true|false`. Missing/partial/changed file set, concurrent interference, truncated observation, uncertain dispatch and incomplete postflight -> unknown; never infer `state_changed=true` from exit 0. Explicitly do not claim OS-level confinement of a formatter binary or all external filesystem effects.
6. Formatter execution is project-authored toolchain code: a successful exit attests to the selected native formatter run, not independent assurance of source safety. Ruff retains pinned local pyproject.toml, explicit target-version, rejected extend, isolated interpreter/module and cache/bytecode/output-file safeguards; `cargo fmt` retains its native mode/edition and project-root constraints. No public test-run counts or validation diagnostics are invented.
7. The existing `edit_project_files` canonical mutation receipt remains a separate, authoritative producer. The new operation must not synthesize that receipt or weaken the Code Mode dependent-validation gate. Canonical mutation truth is captured internally **before** model-result projection, and result fields stay minimal.
8. An unknown outcome requires observing the original Job and inspecting workspace state before any new write. No automatic retry, re-dispatch, detached duplicate, ambient SSH resource, or shell fallback. Maintain version/capability and replacement-runner fail-closed behavior.

## Regression boundary (must be in the same feature PR)

| ID | Requirement |
| --- | --- |
| PF01 | ToolSpec, ToolCall, schema and metadata recognize one `project_format` Mutate operation, with both write+execute permissions. |
| PF02 | Missing/legacy Runner capability and unauthorized scopes reject without dispatch, including auto backend. |
| PF03 | Project/cwd exact authority, explicit vs auto adapter, nested/ambiguous recipes and mismatched markers reject correctly. |
| PF04 | Rust/Ruff supported version, fixed executable/argv and no user-provided script, environment or dependency installation. |
| PF05 | Ruff config nonregular/extend/missing target-version and missing interpreter/module fail closed. |
| PF06 | Bounded files, path traversal, symlink, external workspace/project members and oversized content reject before mutation. |
| PF07 | Clean precheck performs no mutation and reports a proven no-op, not a fabricated diff count. |
| PF08 | Stable native diff leads to exactly one formatting pass and a demonstrably changed content witness. |
| PF09 | Precheck tool failure, timeout or ambiguous exit never starts mutation. |
| PF10 | Manifest, selected source file, cwd, root, registered Project or configuration changes after planning, while queued, or before worker start reject stale plan. |
| PF11 | Missing/partial after-witness, a changed candidate set, concurrent source changes or formatter partial failure do not invent known change truth. |
| PF12 | Complete known *zero* change and complete known *positive* change remain distinguishable from unknown outcomes. |
| PF13 | Worker timeout/cancel/Runner interruption uses original Job identity and no effect redispatch or blanket retry. |
| PF14 | Model-facing projection, full/sparse Job result, Session/Window correlation and canonical internal `state_changed` do not disagree. |
| PF15 | Code Mode mutation accounting respects authoritative internal facts and does not masquerade as `edit_project_files`. |
| PF16 | Other project validation/build paths and existing `cargo_fmt` remain unchanged and cross-platform builds/tests pass. |

## Implementation and review sequence

Within the **same** multi-commit feature branch and one future PR: first review the frozen contract against code; then typed contract/schemas/auth/capability; Runner planning/admission/source witnesses; durable execution/result truth and Host integration; focused tests + actual formatter probes + docs; self-review + independent Codex/agy review; fork full CI for exact final SHA. **Do not open an upstream PR without user approval** and complete review/CI evidence.

Open preimplementation gates: identify the exact existing durable Job and result-truth extension points, prove the source-witness candidate set can cover native Rust/Cargo and Ruff without changing unrelated files, and verify how Runner-side mutation facts survive Job observation. Failure of any gate requires a narrower honest capability, not invented guarantees.
