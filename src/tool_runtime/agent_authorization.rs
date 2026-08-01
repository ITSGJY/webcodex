use super::tool_definition::{runtime_tool_agent_capability, AgentCapability};
use super::{ProjectResolverError, ToolCall, ToolResult, ToolRuntime};
use crate::auth::AuthContext;
use crate::shell_protocol::{
    SHELL_CLIENT_CAPABILITY_SSH_PERSISTENT_SHELL, SHELL_CLIENT_CAPABILITY_SSH_SHELL,
};

/// The capability an agent-backed tool variant requires from the agent
/// client. Non-agent tools (and tools without a project) require nothing.
pub(crate) fn required_agent_capability(call: &ToolCall) -> Option<AgentCapability> {
    runtime_tool_agent_capability(call.tool_name())
}

/// True when a tool call routes through the persistent-shell manager. These
/// are the SSH-capable Session shell operations; one-shot `run_shell` /
/// `run_job` over an SSH resource use the Runner SSH connection pool instead.
fn is_ssh_persistent_shell_call(call: &ToolCall) -> bool {
    matches!(
        call,
        ToolCall::OpenSessionShell { .. }
            | ToolCall::SessionShellExec { .. }
            | ToolCall::SessionShellStatus { .. }
            | ToolCall::CloseSessionShell { .. }
    )
}

impl ToolRuntime {
    /// Enforce the owner boundary and capability requirements for agent-backed
    /// runtime tools before dispatching. This is the single place where the
    /// runtime paths (`/api/tools/call`, `/api/projects/*`, `/mcp`) check that
    /// the caller is allowed to drive an agent.
    /// `/api/shell/*` handlers keep their own `assert_shell_client_owner`
    /// checks; this method closes the gap for the runtime paths.
    ///
    /// Returns `Ok(())` for project-less tools so they are unaffected.
    pub(crate) async fn authorize_agent_tool(
        &self,
        call: &ToolCall,
        ssh_resource: Option<&str>,
        auth: Option<&AuthContext>,
    ) -> Result<(), ToolResult> {
        let Some(project) = call.project() else {
            return Ok(());
        };
        let required = required_agent_capability(call);
        if required.is_none() && ssh_resource.is_none() {
            return Ok(());
        }
        let proj = self
            .resolve_project_for_auth(project, auth)
            .await
            .map_err(ProjectResolverError::into_tool_result)?;
        if !proj.is_agent() {
            if ssh_resource.is_some() {
                return Err(ToolResult::err(
                    "ssh_resource_requires_agent_project: SSH resources require a project owned by a connected Runner"
                        .to_string(),
                ));
            }
            return Ok(());
        }
        let client_id = proj.agent_client_id().map_err(ToolResult::err)?.to_string();
        if self
            .shell_clients
            .get_client_view_for_auth(&client_id, auth)
            .await
            .is_none()
        {
            return Err(ToolResult::err(format!(
                "unknown shell client: {}",
                client_id
            )));
        }
        self.shell_clients
            .assert_client_access(auth, &client_id)
            .await
            .map_err(ToolResult::err)?;
        if let Some(required) = required {
            if !required.is_owner_only() {
                // Capability check via the registry helper so the requirement is
                // expressed as a named capability, not a raw struct field access.
                let mut supported = false;
                for capability in required.registry_capabilities() {
                    if self
                        .shell_clients
                        .client_supports_for_auth(&client_id, capability, auth)
                        .await
                        .map_err(ToolResult::err)?
                    {
                        supported = true;
                        break;
                    }
                }
                if !supported {
                    let message = format!(
                        "agent client {} does not support {}",
                        client_id,
                        required.label()
                    );
                    if matches!(required, AgentCapability::LspReadOnlyNavigation) {
                        return Err(ToolResult::err(format!(
                            "{}: {}",
                            crate::lsp_bridge::error_codes::AGENT_CAPABILITY_UNAVAILABLE,
                            message
                        )));
                    }
                    if matches!(required, AgentCapability::PersistentShell) {
                        return Err(ToolResult::err(format!(
                            "agent_capability_unavailable: {}",
                            message
                        )));
                    }
                    return Err(ToolResult::err(message));
                }
            }
        }
        if ssh_resource.is_some() {
            // Every SSH-resource request routes to a Runner-local SSH
            // connection pool (`run_ssh_shell` / `start_ssh_shell_job`).
            // `ssh_shell` was introduced together with SSH resource routing in
            // the same change, so it is both necessary and sufficient to
            // guarantee a legacy Runner understands SSH resources instead of
            // silently executing the command on the Runner host.
            if !self
                .shell_clients
                .client_supports_for_auth(&client_id, SHELL_CLIENT_CAPABILITY_SSH_SHELL, auth)
                .await
                .map_err(ToolResult::err)?
            {
                return Err(ToolResult::err(format!(
                    "agent_capability_unavailable: agent client {} does not support {}",
                    client_id, SHELL_CLIENT_CAPABILITY_SSH_SHELL
                )));
            }
            // Persistent-shell operations additionally route through the
            // process-local persistent shell manager. `persistent_shell` is
            // already enforced for these tools through the required
            // `AgentCapability::PersistentShell`; the SSH-specific variant is
            // the extra gate that fails closed for a Runner predating
            // `ssh_persistent_shell` instead of opening a local shell.
            if is_ssh_persistent_shell_call(call)
                && !self
                    .shell_clients
                    .client_supports_for_auth(
                        &client_id,
                        SHELL_CLIENT_CAPABILITY_SSH_PERSISTENT_SHELL,
                        auth,
                    )
                    .await
                    .map_err(ToolResult::err)?
            {
                return Err(ToolResult::err(format!(
                    "agent_capability_unavailable: agent client {} does not support {}",
                    client_id, SHELL_CLIENT_CAPABILITY_SSH_PERSISTENT_SHELL
                )));
            }
        }
        Ok(())
    }
}
