# Agent Project Registry

[English](AGENT_PROJECTS.md) | [简体中文](AGENT_PROJECTS.zh-CN.md)

An agent reports project registry entries to the server. GPT Actions and MCP then use ids like:

```text
agent:<client_id>:<project_id>
```

Projects are registered by agents, not by server-side projects.toml.

## Project registry files

Each agent has a `projects_dir` containing one project file per registered project. The server sees those entries through the connected agent registry.

A project entry contains a human name, an absolute path on the agent host, and policy flags such as `allow_patch`.

## Agent `projects.d/*.toml` format

Agent project files are one-file-per-project TOML files in the agent's configured `projects_dir`.

Correct agent `projects.d/webcodex.toml` format:

```toml
id = "webcodex"
path = "/srv/webcodex/projects/webcodex"
name = "WebCodex"
kind = "repo"
description = "WebCodex repository"
allow_patch = true

[hooks]
status = ["git status --short"]
fmt = ["cargo fmt"]
check = ["cargo check --all-targets"]
test = ["cargo test"]
```

Incorrect for agent `projects.d/*.toml`:

```toml
[projects.webcodex]
path = "/srv/webcodex/projects/webcodex"
```

That nested `[projects.webcodex]` shape belongs to an old server-side projects file format. In an agent `projects.d/*.toml` file it leaves the top-level `id` absent and will fail with `missing field id`. Use top-level `id` and `path` fields instead.


## Agent-side project management tools

Current project management tools:

- `register_project` / `registerProject`: register an existing directory.
- `create_project` / `createProject`: create a new directory, optionally initialize a template and git repo, and register it.

These tools are available through the runtime tool list, MCP tools/list, and dedicated GPT Actions. They are constrained by the selected agent's policy.

## Policy boundaries

`allowed_roots` controls where project paths may be registered or created.

- Missing or empty `allowed_roots` defaults to `$HOME`.
- Explicit `allowed_roots` overrides the `$HOME` default.
- Use explicit roots to narrow an agent to a known workspace tree.

Example narrow policy:

```toml
[policy]
allow_cwd_anywhere = false
allowed_roots = ["/root/git"]
```

This is only an example of a narrowed deployment, not the default.

## Observability

`runtime_status`, `listAgents`, and `listProjects` show project summaries, redacted policy summaries, and a sanitized `shell_profiles` summary. They do not expose tokens, env values, `Authorization` headers, full `agent.toml`, the full env snapshot, or shell `init_script` bodies.

Each project in `listProjects` also carries `agent_status` (`online` / `stale`), `connected`, `last_seen`, `shell_profile` (the project's setting), `resolved_shell_profile` (the actually-used name), and `shell_profile_status` (`configured` / `missing` / `not_configured` / `unknown`).

## Agent-registered runtime surface

Runtime tool execution (`run_shell`, `apply_patch`, git, files, jobs, sessions)
uses **agent-registered** projects only. Use the id returned by `listProjects`,
for example `agent:<client_id>:<project_id>`.

If you see older docs or deployment prompts telling you to configure a
server-side `projects.toml`, that is legacy guidance and is not required for new
deployments.

## Troubleshooting

If `createProject` or `registerProject` returns a policy error, check whether the requested path is under the agent's effective `allowed_roots`.

If a new project does not appear in `listProjects`, verify the agent is online and that its project registry refresh succeeded.

For a canonical project, run `webcodex doctor`. For an advanced enrolled
profile, use `webcodex agent status --profile workstation` and see
[SHELL_PROFILES.md](SHELL_PROFILES.md).

## Admin project lifecycle API (source capability)

The source tree exposes an admin-only HTTP allowlist under `/api/admin/projects/*` for registering, creating, enabling, disabling, and unregistering agent projects. These endpoints require bootstrap authentication or an admin-scoped PAT, same-origin JSON, bounded strict request bodies, an explicit idempotency key, and (for enable/disable/unregister) the current project revision. They are intentionally absent from GPT Actions, MCP, the runtime tool registry, and the project console.

Lifecycle state remains authoritative in the selected agent's `projects.d/*.toml` registry. Disable persists `disabled = true`, leaves the source directory untouched, and prevents new runtime resolution while allowing already-started jobs to finish. Enable only reactivates an existing disabled registration after the agent revalidates its canonical path and policy. Unregister removes only the registry entry: it never deletes the source directory, `.git`, or project files, and it fails closed while active jobs exist.

Completed idempotent responses are stored in the existing server SQLite database with bounded retention using only request/key digests and safe response projections. Project revisions are stable SHA-256 values derived from persisted registry content; stale revisions return a conflict rather than overwriting another mutation. Offline agents are not queued for later replay. Runners that do not advertise the structured lifecycle capability receive an explicit version/capability error.

The read-only admin dashboard projection includes lifecycle status, revision, active job count, and server-computed allowed actions. The corresponding `/admin` write UI is not implemented in this source version.

### Lifecycle consistency and retry semantics

Admin lifecycle idempotency records only deterministic terminal outcomes. Agent
unavailability, active-job conflicts, transport enqueue failures, response
timeouts, receiver loss, and other transient or indeterminate results are
audited but are not stored as completed responses. A retry with the same key and
payload may therefore execute again after the dependency recovers. A timeout or
lost response is reported as `operation_indeterminate`; the Runner converges a
retry against its authoritative registry state: enable/disable return the
current already-achieved state even when the request carries the prior revision,
unregister returns `already_unregistered` when the registry entry is absent, and
register/create return the completed result only when the existing registry
entry and requested project side effects match exactly. Mismatched existing
projects fail closed.

Active-job checks use the complete internal job registry for the exact runtime
project id rather than the paginated job-list API. Unregister installs a
short-lived project fence under the same registry lock used by job enqueue, so a
new project job cannot start between the zero-active-jobs check and the Agent
registry mutation. Disable continues to allow existing jobs and reports their
exact count.

All `projects.d` mutations write unique temporary files in the registry
directory, sync file contents, atomically rename, and then sync the parent
directory. Unregister uses a unique hidden tombstone, syncs the rename, removes
the tombstone, and syncs the removal. Registry loading ignores these non-`.toml`
tombstones; a later unregister retry safely removes a crash-left tombstone and
never touches the source directory or Git metadata.
