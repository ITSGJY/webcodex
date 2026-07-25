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
search_project_text = "Grep"
edit_file = "Edit"
```

The mapping values are explicit examples, not built-in assumptions. On every
project process start the provider performs MCP `initialize`, sends
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
Claude Code 2.1.217 exposed `Edit` but no schema-compatible Grep. This does not
disable WebCodex search when using `native` or `claude_code_then_native`: the
latter falls back to the existing bounded Native `rg`/`grep` command. Strict
`claude_code` strategy instead returns a capability error when no mapped Grep
is available. The provider does not route search through Claude's `Bash` tool.

The agent runs one lazy child per canonical registered project root, fixes the
child `cwd` to that root, bounds requests/responses/pending calls/stderr, and
terminates a timed-out child so a late request cannot keep running. The next
call starts a fresh child lazily. It passes only a small environment allowlist
needed for executable lookup, locale, temporary files, and Claude's local
configuration. It does not inherit API-key or WebCodex credential variables.

WebCodex `read_file` always keeps its existing Native implementation. The
experimental provider does not discover, map, or call Claude's `Read` tool.

The bounded version reported by MCP `initialize.serverInfo` is exposed in
provider status after a successful start; status queries never start Claude.
The executable path is not exposed, and a missing command never prevents the
agent from starting.

`runtime_status` and `listAgents` expose the agent's last registration snapshot
under `tool_providers`. It includes the strategy, enabled/available flags,
bounded MCP version, process state, at most 64 discovered tool names,
`available` / `schema_mismatch` / `unmapped` for each of the two capabilities,
and the last bounded error code. The snapshot is refreshed on agent
registration or transport reconnect. It never contains environment variables,
authentication data, Claude configuration, executable paths, or stderr.

The default test suite uses a standalone fake stdio MCP server. A real local
smoke check is opt-in:

```bash
WEBCODEX_TEST_CLAUDE_MCP=1 cargo test --bin webcodex-agent opt_in_real_claude_mcp_smoke -- --nocapture
```

This smoke test prints a bounded tool/schema inventory, resolves configured or
schema-compatible Grep/Edit mappings, calls both tools only inside a temporary
fixture, verifies the edit, and confirms provider shutdown reaps its Claude
process. It does not install Claude Code or perform login.
