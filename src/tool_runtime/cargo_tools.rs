//! Runtime dispatch adapters for Cargo tool calls.

use super::{ToolCall, ToolResult, ToolRuntime};

impl ToolRuntime {
    pub(crate) async fn dispatch_cargo_tool(
        &self,
        call: ToolCall,
        sandbox: Option<&str>,
    ) -> ToolResult {
        match call {
            ToolCall::CargoFmt {
                project,
                session_id: _,
                cwd,
                check,
                timeout_secs,
            } => {
                self.cargo_fmt_in_sandbox(project, cwd, check, timeout_secs, sandbox)
                    .await
            }
            ToolCall::CargoCheck {
                project,
                session_id: _,
                cwd,
                all_targets,
                all_features,
                no_default_features,
                features,
                package,
                timeout_secs,
            } => {
                self.cargo_check_in_sandbox(
                    project,
                    cwd,
                    all_targets,
                    all_features,
                    no_default_features,
                    features,
                    package,
                    timeout_secs,
                    sandbox,
                )
                .await
            }
            ToolCall::CargoTest {
                project,
                session_id: _,
                cwd,
                filter,
                all_targets,
                all_features,
                no_default_features,
                features,
                package,
                no_run,
                timeout_secs,
            } => {
                self.cargo_test_in_sandbox(
                    project,
                    cwd,
                    filter,
                    all_targets,
                    all_features,
                    no_default_features,
                    features,
                    package,
                    no_run,
                    timeout_secs,
                    sandbox,
                )
                .await
            }
            _ => unreachable!("non-cargo tool routed to cargo dispatcher"),
        }
    }
}
