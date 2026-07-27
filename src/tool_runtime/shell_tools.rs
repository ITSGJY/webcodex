//! Runtime dispatch adapter for shell tool calls.

use super::{ToolCall, ToolResult, ToolRuntime};

impl ToolRuntime {
    pub(crate) async fn dispatch_shell_tool(&self, call: ToolCall) -> ToolResult {
        match call {
            ToolCall::RunShell {
                project,
                command,
                session_id: _,
                timeout_secs,
                cwd,
                purpose,
                shell,
            } => {
                self.run_shell_with_contract(project, command, timeout_secs, cwd, purpose, shell)
                    .await
            }
            _ => unreachable!("non-shell tool routed to shell dispatcher"),
        }
    }
}
