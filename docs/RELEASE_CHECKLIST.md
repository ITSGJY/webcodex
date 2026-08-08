# Release Readiness Checklist

This checklist is for final release readiness before tagging, publishing artifacts, updating client schemas, or deploying a new WebCodex server/agent/runtime build.

Do not create tags, push commits, publish npm packages, create GitHub Releases, rewrite history, deploy, or touch secrets while running this checklist unless the operator explicitly requests that action.

## 1. Source Validation

Run:

```bash
cargo fmt --check
cargo check --workspace --all-targets
cargo test --workspace -- --nocapture
git diff --check
git status --short --branch
```

For documentation-only release readiness work, the full test suite may be deferred, but the deferral must be reported.

## 2. Focused Runtime Tests

Run focused lanes when touching runtime metadata, schemas, OpenAPI, MCP, session, handoff, validation, or coding-task behavior:

```bash
cargo test -p webcodex --lib metadata -- --nocapture
cargo test -p webcodex --lib schema -- --nocapture
cargo test -p webcodex --lib openapi -- --nocapture
cargo test -p webcodex --lib mcp -- --nocapture
cargo test -p webcodex --lib validation -- --nocapture
cargo test -p webcodex --lib handoff -- --nocapture
cargo test -p webcodex --lib coding_task -- --nocapture
```

## 3. Product Documentation Check

Confirm the user-facing docs tell one story:

- README states the product position in the first screen.
- Quick Start has one recommended local-first path.
- Concepts explains server, agent, agent-registered projects, runtime project ids, ToolRuntime, MCP, GPT Actions, session, handoff, validation, review/hygiene, and `run_shell` as an escape hatch.
- Architecture starts with client/server/agent/codebase, security-boundary, and runtime-module diagrams before Rust module notes.
- MCP and GPT Actions both say they call the same WebCodex ToolRuntime.
- Security explains what the model can and cannot do, project access, agent trust boundary, shell/job risk, token handling, session/audit evidence, and revocation.
- Release Notes read like external release notes and include highlights, breaking changes, known limitations, upgrade notes, validation, and next steps.
- Roadmap stays short and does not promise a full IDE replacement, autonomous ops, arbitrary computer use, or universal client compatibility.

Run a markdown local link check and report markdown file count, local link count, and missing local link count.

## 4. Legacy Surface Guard

Scan docs and scripts for stale onboarding guidance:

```bash
rg "run_codex|Codex delegation|retained runner|future explicit opt-in|WEBCODEX_ENABLE_LEGACY_CODEX_RUN|PROJECTS_CONFIG|server_static|/api/codex|api/codex|projects.toml" README.md README.zh-CN.md docs deploy scripts SECURITY.md
```

Allowed matches are negative statements, release-note breaking changes, guard tests, and deployment comments that explicitly say the legacy path is removed or not required.

Do not allow docs that ask users to configure server-side project onboarding, imply legacy routes exist, imply `run_codex` exists, or describe retained runner / future opt-in behavior as the current plan.

## 5. E2E Smoke

Run both supported zero-config transports against a safe local test project:

```bash
bash scripts/e2e_zero_config_ws.sh
E2E_TRANSPORT=polling bash scripts/e2e_zero_config_ws.sh
```

These smokes must not target a production repository. Any write checks must stay within disposable probe files or a temporary project.

## 6. Eval Harness

Run the coding-loop comparison:

```bash
EVAL_MODE=compare bash scripts/eval_coding_loop.sh
```

The eval harness measures scripted WebCodex tool-call mechanics. It is not a full model-behavior evaluation.

## 7. Security And Leakage Checks

Confirm:

- No secrets, `.env`, credentials, token files, generated deployment env files, or Authorization headers were touched or printed.
- `finish_coding_task` and `session_handoff_summary` compact outputs do not expose raw stdout/stderr bodies, command text, tails, excerpts, env values, tokens, or secrets.
- `run_shell` is documented as a bounded escape hatch, not the default validation source.
- Model-facing runtime docs keep admin, account, pairing, token-management, and agent-token management outside MCP and GPT Actions.

## 8. Packaging And Artifact Checks

For the planned v0.3.2 binary and npm release:

- `Cargo.toml` and every local WebCodex workspace entry in `Cargo.lock` must be `0.3.2`.
- `npm/webcodex/package.json`, `manifest.json`, `manifest.example.json`, and the npm self-test must agree on `0.3.2`.
- The release-preparation/tag commit may keep `REPLACE_WITH_RELEASE_ARTIFACT_SHA256` for each planned platform in `manifest.json`. Never copy an earlier checksum or invent one to make prepublish checks pass.
- The planned artifacts are `webcodex-v0.3.2-linux-x64.tar.gz`, `webcodex-v0.3.2-linux-arm64.tar.gz`, `webcodex-v0.3.2-darwin-arm64.tar.gz`, and `webcodex-v0.3.2-win32-x64.tar.gz`. Each contains `webcodex`, `webcodex-server`, and `webcodex-runner` built from the exact tagged source revision.
- Build Linux x64 on the release x64 Linux host, Linux arm64 on the release arm64 Linux host, and macOS arm64 on the release Mac. Do not rebuild any artifact on an intermediate packaging machine.
- The Windows x64 artifact must be built on a **Windows release host** from the exact tag/revision using `scripts/package_release_artifact.ps1` (PowerShell + the built-in System32 `tar.exe`; no Git Bash, no WSL). A Linux cross-compiled binary cannot substitute for native Windows validation. The release build must pin `WEBCODEX_BUILT_AT` once so all three binaries report one shared `built_at` in their revision identity. The Windows artifact contains `webcodex.exe`, `webcodex-server.exe`, and `webcodex-runner.exe`; `webcodex-server.exe` is packaged only to keep the three-binary npm contract — a long-running Windows Server runtime is **not** supported.
- Windows officially enters the published-supported platform list only in the release commit that carries a real Windows-built artifact and its checksum; before that, `manifest.json` must not contain a `win32-x64` entry and docs must not claim Windows is published.
- After producing the exact tarballs, calculate each SHA-256, update the release manifest and platform-scope documentation in a clearly reported post-tag commit, and do not move the tag.
- Artifact smoke must run `webcodex --version`, `webcodex-server --version`, and `webcodex-runner --version`; all three must report `0.3.2`, the same clean build revision, and `dirty=false`. The npm package exposes only the `webcodex` wrapper. On Windows this is `scripts/npm_install_windows_smoke.ps1` (artifact -> local manifest -> npm installer -> temp prefix -> wrapper), and `npm --prefix npm/webcodex test` must pass on native Windows.
- Run `node npm/webcodex/test/release-manifest-check.js` only after all real checksums are present; it must reject placeholders, non-hex values, and all-zero values.
- Run `bash scripts/npm_package_smoke.sh` before npm publication and verify the packed tarball identifies `@yyjeqhc/webcodex@0.3.2` and includes its README.
- If publishing a container image, manual local builds and CI builds are both acceptable. Build from the exact immutable tag, verify the image runs as the non-root WebCodex user, confirm the health check, ensure the image contains the Server and administrative CLI but not the Runner, and record the registry, tags, and immutable digest in the GitHub Release.

## 9. v0.3.2 Release Sequence

1. Prepare and review one version/docs commit with placeholder checksums.
2. Run all source, focused, E2E, documentation, security, and local npm package gates from the candidate commit.
3. Only after explicit operator authorization, create the immutable annotated `v0.3.2` tag.
4. Build and smoke the Linux x64, Linux arm64, and macOS arm64 artifacts from that exact tag, then upload the immutable tarballs. If Windows is included in this release, build and smoke the win32-x64 artifact on a Windows release host from the same exact tag (see section 8) and upload it too.
5. Calculate the checksums of the exact uploaded bytes and create the reported post-tag manifest commit without moving `v0.3.2`. Only add the `win32-x64` manifest entry when its real Windows-built artifact and checksum exist.
6. Re-run the manifest check and npm package smoke, then publish npm only after explicit authorization.
7. Create or finalize the GitHub Release from `docs/RELEASE_NOTES_v0.3.2.md`, record artifact/checksum and optional container-digest results, and perform post-deployment acceptance.

## 10. Post-Deployment Acceptance Smoke

After deploying a new server, agent, or runtime build:

1. Refresh the GPT Action or MCP schema if runtime tool schemas changed.
2. Run compact `runtime_status`.
3. Run focused tool discovery.
4. Run `list_projects` and pick an agent-registered project marked appropriate for smoke when available.
5. Run a read-only coding task: `start_coding_task`, `read_file` or `search_project_text`, `show_changes(include_diff=false)`, `workspace_hygiene_check`, and `finish_coding_task(summary_only=true)`.
6. Run one small reversible edit task on a safe project and review the diff before accepting it.

Do not run production mutations as acceptance smoke.
