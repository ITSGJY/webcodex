# Node project validation: #962 P2-0 contract and regression freeze

**Status:** P2-0 v1 scope and N01-N24 regression boundary frozen for pre-implementation review (2026-10-09). Independent agent review has not completed; this design freeze is not approval to implement until its go/no-go gates are met.

**Baseline:** `yyjeqhc/webcodex@6733418db7e0462771fbf8d220b32d03e5c6092e` (includes #982 and #988). **Scope:** next independently reviewable `project_validate` Node `check` slice (P2-A).

## Goal and invariant

Add exactly one bounded, truthful project-script check behind existing `project_validate` without adding a model-facing `node_*` tool, accepting model argv/shell/env, weakening test-count evidence, or creating a second planning authority. The Runner owns project/root resolution, script selection, interpreter probe, exact canonical step, provenance, queue-time replan and Job execution. The Server matches only semantic identity/evidence; the public output schema must accept every legitimately emitted form.

**Truth claim:** A selected Project-local script named `check`, `typecheck` or `lint` ran under the admitted Node execution and its observed process status is known. Success does **not** prove any particular linter/typechecker executed, all checks ran, no project files changed, or no network access occurred. The project-authored script body and its transitive commands remain untrusted executable code. This is an `Execute` operation, not a filesystem sandbox. Do not advertise this as a guaranteed read-only static analyzer.

## P2-A selected execution contract

1. **Runner/runtime:** First production version uses Node's built-in `node --run <selected-script>` on a previously installed, Runner-selected Node >=22.3.0; probe the exact selected program, use the same executable and prepared environment for spawn, and fail closed on unavailable/unsupported versions. No installation, Corepack invocation, manager activation or alternative-runtime fallback. Clear `NODE_OPTIONS` for both probe and actual spawn; document other project/profile environment and PATH limitations. Probe is bounded, managed and cancellable.
2. **Why not npm first:** An isolated fixture with Node v26.10.0/npm v11.19.1 demonstrated `npm run --silent check` executing precheck/check/postcheck, `npm --ignore-scripts run --silent check` executing only check, and local `.npmrc` `script-shell` affecting npm execution. The same fixture with `node --run check` ran only check and did not take the npm shell override. Node's documented `--run` semantics explicitly omit pre/post scripts and package-manager-specific environment variables. Those are narrower first-slice semantics, **not** a claim of universal npm/pnpm/Yarn/Bun compatibility.
3. **Selection:** Resolve one exact, project-contained nearest `package.json` using existing Runner workspace resolver; execute from its resolved recipe root. Select the first present **string-valued** script in this fixed order: `check`, `typecheck`, `lint`. Missing, malformed or empty script means `validation_check_unavailable` / `validation_manifest_invalid` as appropriate. Only one script, no user-supplied script name/flags. Avoid a second parser/selector: extract/reuse the current Node script-name selection fact but keep the general package-manager recipe unchanged.
4. **Canonical step:** `name=check, program=node, args=[--run, <selected-script>], env=[]`; constrain <selected-script> to the above allowlist; disallow additional args, stdin and shell text. This step is created only after the Runner's manifest/root decision. `node --run` may still execute arbitrary project-controlled script contents, so privilege/policy semantics must stay honest.
5. **Manifest authority:** Parse and hash the **same bytes** of the resolved Project-local `package.json` (bounded to at most 1 MiB). Reject containment escapes, oversized files and non-regular files; bind root/path plus manifest and exact invocation digests. Do not read or hash unbounded lockfiles for this path: Node native `--run` does not select its manager using `package-lock.json`, `pnpm-lock.yaml`, `yarn.lock`, or `bun.lock`. Lockfiles remain relevant to existing general recipes and later manager-specific P2-B, not executable authority for P2-A. Source-fence freshness is separate and weaker than a source snapshot.
6. **Identity:** Use a separate `node:script:check` adapter identity and evidence profile. The Runner-derived `validation_target_id` must include the selected canonical script name and execution-engine discriminator **as well as** resolved recipe cwd/action; `check` and `lint` are not the same validation target. Exclude manifest content from target identity per existing source-vs-target contract; compare exact provenance/manifest digest at pre-start fence. The Server must use the Runner-provided plan identity, not independently reselect the script.
7. **Capabilities:** Add a standalone `project_validation_node_script_check_v1` capability (omitted/false for old Runner). Validate at planning, validate the actual retained node adapter/step again at Job admission (including `adapter=auto` and Runner replacement), and reject before process start if unavailable. Existing `project_validation_v1` alone is not authority for the Node extension.
8. **Unsupported surfaces:** `test`, `format_check`, package scope, dependency policy, test selectors/count policy and package-manager-specific scripts remain unavailable in P2-A. P2-B may consider formats/managers; P3 must first establish trustworthy machine-readable executed-test counts. Do not use an arbitrary script's exit code as a proven test count.
9. **Effects and evidence:** Expose `selected_script` (if necessary) only as a bounded enum value **derived from the retained canonical step**; never serialize script bodies, npm/yarn configuration, raw paths, stdin or arbitrary log output as trusted structured diagnostics. Emit no Rust/Go/Python diagnostic counts or test counts; an absent parser is not evidence of zero errors. Preserve process exit, timeout/cancel/lost/unknown/stale outcomes, same-Job continuation and no redispatch.
10. **Public contracts:** Enumerate every planning and prestart error reachable via P2-A; validate sparse planning and rich fenced responses against `project_validate` output schema. Reuse existing canonical codes where correct. Do not leak `package_manager_ambiguous` into the production Node native path, since no package manager is selected there; if execution design changes, extend schema and tests in the same PR. Keep specialist Cargo/Go schemas unchanged.

### Explicit limitations

`node --run` searches ancestor `package.json` locations and prepends ancestor `node_modules/.bin` to PATH. The Runner's exact recipe cwd must therefore be verified; do not claim that child executable/module resolution is wholly Project-confined. `NODE_OPTIONS` can preload arbitrary modules, as confirmed by an isolated fixture; remove it for preflight and spawn. Dependencies and scripts may still perform I/O, network operations or nested manager calls. No special network/offline guarantee, mutation-free guarantee, or project script correctness guarantee exists.

This scope deliberately does not change `project_build`, introduce `project_format`, accept an arbitrary CLI, alter generic Node recipes, or try to unify every package manager.

## Regression contract: freeze before implementation

All checks below must be specified before editing production modules. Focused Rust tests can run on the server; full multi-platform validation belongs to GitHub Actions. Use isolated temporary Node projects and never install dependencies to run contract tests.

| ID | Fixture / perturbation | Required outcome |
| --- | --- | --- |
| N01 | Exact registered project, explicit Node adapter, only `check` | One canonical Node step; Runner-selected root and valid identity |
| N02 | All three scripts defined | Deterministically select `check`, once |
| N03 | `check` missing, `typecheck` present; then introduce `check` | Selected script AND target identity change; old queued plan is stale |
| N04 | Only `lint` defined | Choose `lint`; no claim that all other checks ran |
| N05 | Missing script, wrong script type, malformed JSON, >1 MiB JSON | Stable unavailable/manifest failure before Job |
| N06 | Symlink manifest outside Project, nonfile marker, cwd traversal | Fail closed; no external read or process start |
| N07 | Nested Node package, mixed language markers, auto vs explicit hint | Exact nearest root; ambiguity fails without fallback |
| N08 | `format_check`, `test`, test options, package scope or dependency policy | Explicit unsupported result; no execution |
| N09 | Crafted extra `node --run` argv, substituted script name, nonempty env | `ShellJobValidationStep` and metadata reject |
| N10 | Modify `package.json` while Job waits in queue | Fresh Runner replan rejects stale plan before command starts |
| N11 | Change Project identity, root, cwd or symlink between planning/admission | Fail closed, no misbound command |
| N12 | Capability absent, explicitly false, old Server/Runner, auto-detected Node | No command start on unsupported Runner |
| N13 | New Runner plan followed by old/replaced Runner at Job admission | Recheck actual step/adapter and reject |
| N14 | Selected node executable missing, invalid or older than 22.3 | Bounded unavailable; no install/fallback |
| N15 | `NODE_OPTIONS` preloads fixture module; probe and spawn both sanitize | No preload; same resolved interpreter used |
| N16 | Script with `precheck`, `check`, `postcheck` | Native Node runner executes only `check` |
| N17 | Project `.npmrc` with `script-shell`; packageManager/lockfile disagreement | Native Node command identity unaffected; no manager invoked |
| N18 | Exit 0, exit nonzero, deliberately missing binary, verbose/truncated output | Distinguish process success/failure and unknown diagnostics |
| N19 | Fake cargo, Go, pytest and test-count-looking stdout | Never adopt foreign parser, counts or test assertions |
| N20 | Pending, cancellation, timeout, restart recovery and lost Job | Preserve one execution; never re-dispatch or infer success |
| N21 | Source changes after completed validation | Existing source fence reports stale/unknown, not current proof |
| N22 | Every reachable planner, capability and queue-time failure | Actual public output validates against exact `project_validate` schema |
| N23 | Invalid/contradictory lifecycle fields in output | Output schema rejects contradiction |
| N24 | Linux Node 22/24/26 and Windows/macOS supported runners | Canonical argv, environment and lifecycle work equivalently or fail explicitly |

**Go/no-go:** No production module or test implementation begins until the frozen P2-0 contract and N01-N24 matrix receive independent review. Real Node v26.10.0 script-selection/environment fixtures have been executed and cleaned; Node 22/24 and multi-platform behavior still require validation. The eventual production/test diff must stay within this scope and pass a complete matching-HEAD CI. Later manager-specific and test adapters require independent proof and separate slices. The attempted Codex read-only review did not return a verdict during P2-0 and cannot be treated as an approval.

## Source-truth map

- `crates/webcodex-core/src/project_validation.rs`: public adapter/request/provenance.
- `crates/webcodex-validation/src/recipe.rs`, `crates/webcodex-workspace/src/project_recipe.rs`: Runner-owned resolution and manifest inputs.
- `crates/webcodex-validation/src/adapters/mod.rs`, `crates/webcodex-core/src/validation_identity.rs`: validation identity/evidence, not a second command planner.
- `crates/webcodex-core/src/runner_protocol/job.rs`: exact canonical step and durable metadata.
- `crates/webcodex-runner/src/webcodex_runner/validation/project.rs`: planning and queue-time fence.
- `crates/webcodex-runner/src/webcodex_runner/shell/commands.rs`, `job_manager/local_shell.rs`: selected executable, probe, environment and actual Job.
- `crates/webcodex-runner-registry/src/requests.rs`, `job_updates.rs`: additive capability and prestart admission.
- `src/tool_runtime/validation.rs`, `jobs.rs`: same-Job orchestration and bounded evidence projection.
- `crates/webcodex-tool-contracts/src/registry/output_schemas/testing.rs`, `src/tests/output_schemas.rs`: output contract; see maintainer's follow-up in #988.

Official references: https://nodejs.org/download/release/v24.20.0/docs/api/cli.html#--run ; https://docs.npmjs.com/cli/v11/using-npm/config/ ; https://pnpm.io/cli/run ; https://yarnpkg.com/cli/run .

### Follow-up phases

P2-A: bounded built-in Node project-script check only, after test boundary freeze. P2-B: separately prove `format_check` and package-manager-specific behavior, if necessary. P3: prove test-run counts for each Node test runner before enabling `action=test`. Offline/dependency/mutating-format/build-artifact extensions remain independent #962 follow-ups.
