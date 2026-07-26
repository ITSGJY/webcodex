# Experiment: Claude coding tool harness

**Branch:** `experiment/claude-tool-harness`  
**Status:** experimental only — not a production API commitment  
**Known Claude Code version on special:** `2.1.207` (also exercised against fake MCP)

## 1. Purpose

Study the **raw** Claude Code MCP coding harness tools (`Read`, `Edit`, `Write`, `Bash`) by exposing live `tools/list` schemas and `tools/call` results through a fixed WebCodex agent surface.

This is **not** a Claude agent, Task/Workflow/Cron bridge, or production capability adapter.

## 2. Branch nature

- Independent experiment branch; do not merge as a stable product feature without a separate design review.
- Schemas and behavior may change with Claude Code upgrades.
- No promise of long-term API stability for error codes or response shapes.

## 3. Three fixed experimental agent request kinds

| Kind | Role |
|---|---|
| `claude_list_tools` | Start/reuse per-project Claude MCP, `initialize` + `tools/list`, bounded name/`schema_hash` summary |
| `claude_describe_tool` | Return live tool description + `inputSchema` + `schema_hash` |
| `claude_tool_call` | Validate arguments against live schema, call `tools/call`, return bounded raw result |

These are **agent request kinds** handled by `webcodex-agent` external tools routing. They are **not** dynamically expanded when Claude adds tools, and they are **not** published as individual MCP tools for every Claude name.

Public WebCodex model-facing tool count is unchanged: Claude tools are never auto-inserted into MCP `tools/list` / OpenAPI registries.

## 4. Viewing live schemas

With Claude MCP enabled in agent config:

```toml
[tool_providers.claude_code]
enabled = true
command = "claude"
args = ["mcp", "serve"]
timeout_secs = 45
```

Agent request payload for describe:

```json
{
  "kind": "claude_describe_tool",
  "cwd": "<resolved project root>",
  "content": "{\"tool_name\":\"Bash\"}"
}
```

Response includes `claude_version`, `schema_hash` (SHA-256 of key-sorted canonical JSON), `description`, and `input_schema`.

## 5. Calling Read / Edit / Write / Bash

```json
{
  "kind": "claude_tool_call",
  "cwd": "<resolved project root>",
  "content": "{\"tool_name\":\"Read\",\"arguments\":{\"file_path\":\"src/lib.rs\"}}"
}
```

Examples:

- **Read:** `{"tool_name":"Read","arguments":{"file_path":"..."}}`
- **Edit:** `{"tool_name":"Edit","arguments":{"file_path":"...","old_string":"...","new_string":"..."}}`  
  Real Claude Code requires a prior `Read` of the same path in the same MCP process before `Edit` will accept a write.
- **Write:** `{"tool_name":"Write","arguments":{"file_path":"...","content":"..."}}`
- **Bash:** `{"tool_name":"Bash","arguments":{"command":"printf hi"}}`

Rules:

- Tool name must appear in the current process `tools/list`.
- Arguments are validated with a **minimal** JSON Schema subset (`type`, `properties`, `required`, `additionalProperties`, `enum`, `items`, `oneOf`/`anyOf` when present).
- `cwd` is fixed to the resolved project root; callers cannot set an arbitrary host cwd for the Claude process.

## 6. Fake tests (default suite)

```bash
cargo test --bin webcodex-agent experimental_ -- --nocapture
cargo test --bin webcodex-agent external_tools -- --nocapture
```

Uses the standalone fake stdio MCP binary compiled from `src/bin/webcodex_agent/fake_claude_mcp.rs`.

## 7. Opt-in real Claude tests

```bash
WEBCODEX_EXPERIMENTAL_CLAUDE_TOOLS=1 \
cargo test --bin webcodex-agent opt_in_experimental_real_claude_tools_smoke -- --nocapture
```

Requires a logged-in local `claude` that supports `claude mcp serve`. Default suite skips this test.

Related existing opt-ins (production provider path):

```bash
WEBCODEX_PROBE_CLAUDE_PROVIDER=1 cargo test --bin webcodex-agent opt_in_real_claude_mcp_probe -- --nocapture
WEBCODEX_TEST_CLAUDE_MCP=1 cargo test --bin webcodex-agent opt_in_real_claude_mcp_smoke -- --nocapture
```

## 8. Bounds and timeouts

| Bound | Value |
|---|---|
| MCP message size | 1 MiB |
| Experimental schema size | 64 KiB |
| Experimental description | 4096 chars |
| Experimental result | 256 KiB (soft truncate + `result_truncated`; hard fail if ~2×) |
| Max discovered tools | 64 |
| Timeout | `min(request.timeout_secs, policy.max_timeout_secs, config.timeout_secs)` |

## 9. Explicitly unsupported orchestration tools

Do **not** research or adapt via this harness as product features:

`Agent`, `Workflow`, `TaskCreate`, `TaskGet`, `TaskList`, `TaskUpdate`, `TaskOutput`, `TaskStop`, `Monitor`, `ScheduleWakeup`, `CronCreate`, `CronList`, `CronDelete`, `SendMessage`, `PushNotification`, `NotebookEdit`, `DesignSync`, `Skill`, `ToolSearch`

Optional harness call-through of `WebFetch` / `WebSearch` / worktree tools is not required for phase 1.

## 10. No production compatibility promise

- Error codes (`claude_tool_not_found`, `claude_arguments_invalid`, …) are branch-local.
- Schema hashes are only meaningful for a given Claude version + process discovery snapshot.
- Production default strategy remains `native`; experimental kinds work when `claude_code.enabled = true` even if strategy stays `native`.

## 11. Process model

- One Claude MCP child per canonical project root (reuse existing provider).
- First call may start; later calls reuse while alive.
- Process exit → next call restarts lazily.
- Discovery snapshot is bound to that process; version comes from `initialize.serverInfo`.
- Agent/provider shutdown kills the process group.

## 12. Fixture cleanup

Real and write tests must:

- use tempdir / dedicated fixtures only;
- restore Edit targets;
- delete Write temps;
- leave no Claude process leak after shutdown;
- leave the main repo worktree clean of fixture files.

## 13. Native comparison notes (smoke observations)

| Capability | Claude | WebCodex Native | Takeaway |
|---|---|---|---|
| Read | `file_path` (+ optional offset/limit); MCP content array | `read_file` with start_line/limit/line numbers | Claude path is simpler for models; Native pagination/line numbers are richer for edit loops |
| Edit | exact `old_string`/`new_string`; **requires prior Read** in-process | `replace_in_file` with expected_replacements + post-write verification | Claude session memory is surprising; Native is stateless and verifies SHA |
| Write | create/overwrite via path+content | `write_project_file` with path policy | Both overwrite; Native stays inside project policy more explicitly |
| Bash | command string; non-zero → `isError` + text | `run_shell` with exit_code/stdout/stderr split | Native exit codes are clearer; Claude packs status into tool text/`isError` |
| Lifecycle | one MCP child per project root | no child process | Reuse is fast; shutdown must reap process groups |
| Schema drift | live `tools/list` + schema hash | stable WebCodex ToolSpec | Hash is useful for detecting Claude upgrades |

Design worth borrowing: keep public tool names fixed, observe third-party schemas via discovery hashes, and always bound results. Do **not** mirror Claude orchestration tools into WebCodex.
