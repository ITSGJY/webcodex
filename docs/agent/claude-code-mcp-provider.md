# Claude Code MCP provider (experimental)

`webcodex-agent` can use `claude mcp serve` for two allowlisted, single-call
capabilities while WebCodex remains the online MCP/API, authorization, session,
project, permission, timeout, and audit boundary.

The provider is disabled by default. Add this to the agent configuration (not
the server configuration):

```toml
[tool_providers]
strategy = "claude_code_then_native"

[tool_providers.claude_code]
enabled = true
command = "claude"
args = ["mcp", "serve"]
timeout_secs = 30

[tool_providers.claude_code.mapping]
edit_file = "Edit"
```

The mapping values are explicit examples, not built-in assumptions. Configure
`search_project_text` only after a probe confirms that the installed Claude Code
version exposes a schema-compatible search tool; otherwise leave it unmapped so
`claude_code_then_native` uses bounded Native `rg`/`grep`. On every project
process start the provider performs MCP `initialize`, sends
`notifications/initialized`, and calls `tools/list`. A capability is available
only when its configured tool name is present and the discovered input schema
contains all fields required by the adapter. Use the names exposed by the
installed Claude Code version.

Strategies:

- `native` — existing WebCodex execution only; this is the default.
- `claude_code` — return a structured provider error when Claude is disabled,
  missing, incompatible, or fails.
- `claude_code_then_native` — use Claude first. Searches may fall back after
  failure. An edit falls back only when the adapter can prove the
  Claude write was not submitted; a timeout, EOF, JSON-RPC error, or
  unverifiable edit result is treated as an uncertain write and is never
  executed again through Native.

Claude Code builds do not necessarily expose a Grep tool. The real smoke with
Claude Code 2.1.220 exposed `Edit` but no schema-compatible Grep. This does not
disable WebCodex search when using `native` or `claude_code_then_native`: the
latter falls back to the existing bounded Native `rg`/`grep` command. Strict
`claude_code` strategy instead returns a capability error when no mapped Grep
is available. The provider does not route search through Claude's `Bash` tool.

The agent runs one lazy child per canonical registered project root, fixes the
child `cwd` to that root, bounds requests/responses/pending calls, discards
child stderr, and terminates a timed-out child so a late request cannot keep
running. The next
call starts a fresh child lazily. It passes only a small environment allowlist
needed for executable lookup, locale, temporary files, and Claude's local
configuration. It does not inherit API-key or WebCodex credential variables.

WebCodex `read_file` always keeps its existing Native implementation. The
experimental provider does not discover, map, or call Claude's `Read` tool.

Claude tools remain an agent-internal implementation detail. WebCodex builds
its public MCP `tools/list`, runtime registry, OAuth policy, and OpenAPI from
the static WebCodex tool definitions only. Claude `tools/list` output is never
inserted into those registries. A Claude upgrade may therefore add `Read`,
`Bash`, `Write`, or other names to provider discovery without making any of
them visible to an external WebCodex client. Public names and input schemas,
including `replace_in_file`, are identical with the provider disabled or
enabled.

Separately from the two mapped production capabilities, the agent also carries
three experimental agent-internal request kinds (`claude_list_tools`,
`claude_describe_tool`, `claude_tool_call`) that observe the raw Claude tool
surface and call only `Read`/`Edit`/`Write`/`Bash`. They share this provider's
process, bounds, and generation Router lifecycle, and are likewise absent from
public MCP/OpenAPI. See `docs/experiments/claude-tool-harness.md`.

The bounded version reported by MCP `initialize.serverInfo` is exposed in
provider status after a successful start. A version is retained only when it
matches a small version-string character allowlist. Status queries are passive:
`runtime_status`, `list_agents`, and local snapshot reads never start Claude.
With the default agent configuration, the expected snapshot is
`strategy=native`, `enabled=false`, and `process_state=not_started`; this confirms
that observability is deployed but does not mean Claude has been configured or
started. The executable path is not exposed, and a missing command never
prevents the agent from starting.

`runtime_status` and `listAgents` expose the current bounded snapshot under
`tool_providers`. Registration and reconnect carry a complete snapshot. Later
changed revisions reuse the existing agent transport: polling agents attach
them to their next poll, while WebSocket/QUIC agents send a changed-only
`runtime_metadata` envelope after a result or on the existing keepalive tick.
There is no extra blocking round trip per tool call. Repeated identical state
is not resent, and only one metadata snapshot may be in flight at a time so an
older snapshot cannot overwrite a newer `last_call`. Metadata send failure
releases the claim for a later keepalive/reconnect retry, does not change a tool
result, and network I/O occurs after the provider state lock has been released.

## Explicit agent config reload

`agent.toml` is loaded at startup. On Unix, an operator can explicitly reload
the same config path without disconnecting the agent:

```bash
sudo systemctl reload webcodex-agent
```

The generated unit maps this to `SIGHUP`. A valid reload atomically replaces
one request-time generation for `policy` (`allow_raw_shell`,
`allow_cwd_anywhere`, `allowed_roots`, `max_timeout_secs`, `max_output_bytes`),
`shell` (`default_profile`, `profiles`, `program`, `args`, `path_prepend`,
`env`, `init_script`), and `tool_providers` (strategy plus the Claude Code
enabled flag, command, args, mapping, and timeout). Requests and jobs that
already captured the old generation keep its
policy, timeout, shell environment, and Provider route. In-flight Claude edits
are allowed to finish; their old Provider process is shut down when the last
old-generation caller releases it. New calls cannot enter a disabled Provider.

Identity, server/auth, registration, project source, concurrency, and transport
fields still require restart: `server_url`, `token`, `client_id`,
`display_name`, `owner`, `hostname`, `projects_dir`, `poll_interval_ms`,
`capabilities`, `max_concurrent_jobs`, `transport`,
`websocket_connect_timeout_secs`, and `quic.*`. A mixed reload applies the hot
sections and reports these field names as `restart_required_fields`; it never
reports their values. Read, parse, validation, or Provider-config failure keeps
the active generation unchanged.

The latest bounded result is exposed as `tool_providers.config_reload`
(`generation`, result/error code, and restart-required summary). Generation
starts at 1 and advances only after a valid reload. `projects.d/*.toml` keeps
its existing independent cache refresh. Reload does not change public MCP
tools, refresh MCP metadata, or add an OpenAPI operation.

The opt-in process-level smoke exercises the real Server-to-Agent dispatch and
Unix signal path:

```bash
WEBCODEX_E2E_AGENT_RELOAD=1 \
./scripts/test-agent-config-reload-e2e.sh
```

It runs a temporary Server, Agent, project, Git fixture, and config without
systemd or Claude Code. It verifies valid, invalid, mixed, and recovery SIGHUP
reloads, then checks Agent/Server process groups, the loopback port, fixture,
and temporary-directory cleanup.

The provider lifecycle uses `not_started`, `starting`, `initializing`,
`discovering`, `mapping`, `running`, and `stopped`. State revisions are produced
when configuration is initialized, the child starts, initialize succeeds,
tools/list succeeds, mappings are validated, a call succeeds or fails, a
timeout/EOF/process exit occurs, shutdown runs, or a later call restarts the
child lazily. `available=true` means an initialized/discovered provider process
is reusable; a timeout or connection loss makes it unavailable until lazy
restart succeeds.

`claude_code.last_call` is one bounded summary, not an unbounded history:

```json
{
  "capability": "edit_file",
  "selected_provider": "claude_code",
  "fallback_used": false,
  "result": "success",
  "write_state": "confirmed",
  "duration_ms": 14,
  "error_code": null
}
```

`write_state` is absent for search and is one of `not_submitted`, `confirmed`,
or `uncertain` for edit. A successful Native search fallback records
`selected_provider=native`, `fallback_used=true`, and no final error code;
`last_error_code` still identifies the Claude-side reason that caused the
fallback. An Edit may fall back only from `not_submitted`. RPC error, EOF,
timeout, Claude tool error, or failed post-write verification is `uncertain`
and cannot execute a second Native write.

All provider strings are allowlisted or bounded, discovered names are sorted,
deduplicated, and capped at 64, and only the two configured WebCodex capability
keys are accepted by the server. Provider status never contains environment
variables, authentication data, Claude configuration, executable/project
paths, request arguments, file contents, user code, stderr, raw RPC responses,
tokens, or cookies.

For a non-mutating active probe, use the opt-in diagnostic test. It creates an
empty temporary directory, starts `claude mcp serve`, performs only initialize
and tools/list, prints the safe status object, and shuts the process group down.
It does not call Edit, read a project file, run Bash, install Claude, log in, or
start a model conversation:

```bash
WEBCODEX_PROBE_CLAUDE_PROVIDER=1 \
cargo test --bin webcodex-agent opt_in_real_claude_mcp_probe -- --nocapture
```

The default test suite uses a standalone fake stdio MCP server. A real local
smoke check is opt-in:

```bash
WEBCODEX_TEST_CLAUDE_MCP=1 cargo test --bin webcodex-agent opt_in_real_claude_mcp_smoke -- --nocapture
```

This smoke test prints a bounded tool/schema inventory, resolves configured or
schema-compatible Grep/Edit mappings, calls available tools only inside a
temporary fixture, verifies Edit, reports Grep as unavailable when the installed
version has none, and confirms provider shutdown reaps its Claude process. It
does not install Claude Code or perform login.

The full server/agent path is also opt-in:

```bash
WEBCODEX_E2E_CLAUDE_PROVIDER=1 \
./scripts/test-claude-provider-e2e.sh
```

It builds a temporary Git fixture, uses independent server/agent configuration,
an automatically selected loopback port, and a temporary HOME/XDG/Claude config
directory. It checks the public MCP tool set before and after Claude discovery,
Native read, Native `rg`/`grep` search fallback, strict Claude Edit evidence,
file restoration, a clean worktree, provider process-group cleanup, and port
release. The default suite never requires Claude Code, login, network, a fixed
fixture path, or user-global configuration. Default `native` strategy behavior
is unchanged.
