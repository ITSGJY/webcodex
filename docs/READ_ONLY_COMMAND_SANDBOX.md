# Inspect command sandbox

[English](READ_ONLY_COMMAND_SANDBOX.md) | [简体中文](READ_ONLY_COMMAND_SANDBOX.zh-CN.md)

## Session modes

WebCodex has three Workflow Session modes:

| Mode | Structured write tools | Shell/job/validation commands |
|---|---|---|
| `normal` | Allowed, subject to the configured guards and policy | Allowed normally |
| `inspect` | Denied | Allowed only through the fail-closed Landlock inspect sandbox |
| `read_only` | Denied | Denied before enqueue or execution |

`read_only` has not gained a shell. Use `inspect` only when trusted project
inspection needs commands such as `rg`, `git status`, `node --check`, or
`cargo check`.

## Security promise

`inspect` is a trusted inspection mode with one narrow guarantee:

> An inspect command and its descendants cannot perform ordinary local
> filesystem writes outside their one private scratch directory.

This is not a no-side-effect or confidentiality sandbox. Reads remain
unrestricted, the runner environment is substantially inherited, network
access is not isolated, and commands can contact external services. A command
may therefore read sensitive data, transmit data, or cause remote side effects.
Do not describe `inspect` as fully isolated, harmless, or generally
side-effect-free.

## Linux Landlock boundary

Inspect command execution is Linux-only and requires Landlock ABI v3. ABI v3 is
the minimum because its write access set includes `TRUNCATE`; the ruleset also
covers create, remove, rename/refer, and the other write-related filesystem
rights represented by that ABI.

The runner:

- handles all ABI v3 write rights with hard compatibility;
- accepts only `FullyEnforced`, never partial or best-effort enforcement;
- applies the ruleset in `pre_exec`, so it is active before the requested
  program starts and inherited by every descendant;
- leaves project, dependency-cache, and system paths readable but not writable;
- permits persisted writes beneath exactly one per-command/job scratch
  directory; the exact non-persistent `/dev/null` character sink accepts
  `WriteFile` for common Git/Cargo behavior, while the `/dev` hierarchy remains
  immutable;
- creates scratch atomically with mode `0700`, verifies it is a real directory
  and not a symlink, and removes it after the command/job reaches terminal
  state;
- rejects inspect execution on non-Linux, unsupported kernels, failed probes,
  unknown sandbox modes, missing capability, or any policy-application error.

There is no silent fallback to ordinary shell.

## Temporary write environment

Each inspect command receives:

- `TMPDIR=<scratch>`
- `CARGO_TARGET_DIR=<scratch>/target`

Existing Cargo registries, Git data, dependency caches, toolchains, and project
files stay readable. Cargo build artifacts go to scratch, so ordinary
`cargo check` and `cargo test` can run without creating `target/` in the
checkout. Commands that need to modify the checkout, such as `cargo fmt`
without `--check` or package installation, should fail normally.

Shell-profile preparation can execute an init script before the requested
command. Inspect requests therefore skip prepared profile initialization and
use the base configured shell; any global shell init script runs inside the
Landlock boundary.

## Recommended inspection flow

Prefer `run_shell` with `rg` or `git grep` for code search and targeted
inspection. `search_project_text` remains available for compatibility.
Useful inspect commands include:

```text
rg 'pattern' src
git grep 'pattern'
git status --short
git diff
git show
node --check path/to/file.js
cargo check --all-targets
cargo test
```

Project writes through redirection, `truncate`, `rm`, `mv`, or a child shell
are denied. Writes under `$TMPDIR` are allowed.

## Known limitations

- File reads are not restricted, including reads outside the project.
- Environment-variable isolation is not a security guarantee.
- Network and IPC access are not restricted by this filesystem ruleset.
- Remote APIs and other external services can still be changed.
- Landlock governs the filesystem rights exposed by the required ABI; it is
  not a container, VM, syscall filter, or complete host sandbox.
- Abrupt process or host termination can leave scratch cleanup for normal
  operating-system temporary-directory maintenance.
