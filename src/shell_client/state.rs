use super::auth::ShellClientAuthGroup;
use crate::shell_protocol::{
    AgentBuildInfo, AgentPolicySummary, ShellAgentProjectSummary, ShellAgentShellRequest,
    ShellClientCapabilities, ShellJobCodexMetadata, ShellJobValidationProgress, ShellRunResponse,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{oneshot, Notify};

#[derive(Debug, Clone)]
pub(super) struct ShellClientRecord {
    pub(super) client_id: String,
    /// Active agent process identity (UUID). Replacing this value is the lease
    /// hand-off: once changed, the previous instance can no longer poll or
    /// submit results/job_updates.
    pub(super) agent_instance_id: String,
    pub(super) display_name: Option<String>,
    pub(super) owner: Option<String>,
    pub(super) hostname: Option<String>,
    pub(super) capabilities: ShellClientCapabilities,
    pub(super) projects: Vec<ShellAgentProjectSummary>,
    pub(super) last_seen: i64,
    pub(super) agent_protocol_version: String,
    /// How this client is currently connected: `"polling"`, `"websocket"`,
    /// or `"quic"`.
    pub(super) transport: String,
    /// Sanitized agent policy summary reported at registration. `None` for
    /// older agents that did not report a policy. Exposed in
    /// `runtime_status` / `listAgents`; never carries token/env/init_script.
    pub(super) policy: Option<AgentPolicySummary>,
    /// Lightweight quick-start isolation group captured at registration. This
    /// is intentionally not exposed in `ShellClientView`.
    pub(super) auth_group: Option<ShellClientAuthGroup>,
    /// When the current agent instance first registered under this client_id.
    /// Preserved across same-instance re-registrations (transport reconnects).
    pub(super) registered_at: i64,
    /// When the current transport connection was established (latest register
    /// for this instance).
    pub(super) connected_at: i64,
    /// Server-generated lease for one concrete WebSocket/QUIC connection.
    /// Polling registrations use `None`. This is internal and prevents a late
    /// disconnect from an older same-instance transport from tearing down the
    /// newer connection.
    pub(super) connection_id: Option<String>,
    /// When the server observed the last transport disconnect for the current
    /// instance. Cleared on re-register.
    pub(super) disconnected_at: Option<i64>,
    /// Runner-reported process start timestamp (register payload).
    pub(super) process_started_at: Option<i64>,
    /// Runner-reported build identity (register payload).
    pub(super) build: Option<AgentBuildInfo>,
}

#[derive(Debug)]
pub(super) struct PendingShellRequest {
    pub(super) request: ShellAgentShellRequest,
    pub(super) waiter: Option<oneshot::Sender<ShellRunResponse>>,
    pub(super) job_id: Option<String>,
    pub(super) dispatched: bool,
}

#[derive(Debug, Clone)]
pub(super) struct ShellJobRecord {
    pub(super) job_id: String,
    pub(super) request_id: Option<String>,
    pub(super) client_id: String,
    /// Internal lease owner. Never exposed through public job tools.
    pub(super) agent_instance_id: String,
    pub(super) kind: String,
    pub(super) project_id: Option<String>,
    pub(super) session_id: Option<String>,
    pub(super) cwd: Option<String>,
    pub(super) project_cwd: Option<String>,
    pub(super) purpose: Option<String>,
    pub(super) shell: Option<String>,
    pub(super) command_preview: String,
    pub(super) status: String,
    pub(super) created_at: i64,
    pub(super) started_at: Option<i64>,
    pub(super) ended_at: Option<i64>,
    pub(super) exit_code: Option<i32>,
    pub(super) duration_ms: Option<u64>,
    pub(super) stdout: ShellJobLogState,
    pub(super) stderr: ShellJobLogState,
    pub(super) error: Option<String>,
    pub(super) codex: Option<ShellJobCodexMetadata>,
    pub(super) validation_steps: Vec<String>,
    pub(super) validation_progress: Option<ShellJobValidationProgress>,
    pub(super) last_update_seq: u64,
    pub(super) recovery_state: Option<String>,
    pub(super) recovered_after_server_restart: bool,
    pub(super) reconciled_at: Option<i64>,
    pub(super) recovery_reason_code: Option<String>,
    pub(super) recovering_since: Option<i64>,
    pub(super) recovery_original_status: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ShellJobLogState {
    pub(super) tail: String,
    pub(super) first_retained_line: usize,
    pub(super) next_line: usize,
    pub(super) truncated: bool,
}

impl Default for ShellJobLogState {
    fn default() -> Self {
        Self {
            tail: String::new(),
            first_retained_line: 1,
            next_line: 1,
            truncated: false,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct ShellClientRegistryInner {
    pub(super) clients: HashMap<String, ShellClientRecord>,
    pub(super) pending_by_id: HashMap<String, PendingShellRequest>,
    pub(super) queues_by_client: HashMap<String, VecDeque<String>>,
    pub(super) jobs_by_id: HashMap<String, ShellJobRecord>,
    pub(super) request_to_job: HashMap<String, String>,
    /// Bounded stale-instance tombstones prevent a replaced runner process
    /// from reclaiming the same client lease after the replacement later
    /// becomes stale.
    pub(super) retired_instances: HashMap<String, VecDeque<String>>,
    /// Runtime project ids temporarily fenced while unregister validates and
    /// removes the Agent registry entry. Job enqueue checks this set while
    /// holding the same registry mutex, closing the check/start TOCTOU window.
    pub(super) unregistering_projects: HashMap<String, usize>,
    /// Optional push notifiers for agents connected over a long-lived
    /// transport (WebSocket). When a request is enqueued for a client that
    /// has a registered notifier, the server pumps the request immediately
    /// instead of waiting for the agent to poll. Polling agents never
    /// register a notifier and are unaffected.
    ///
    /// The stored instance and connection ids record which concrete transport
    /// owns the notifier. Disconnect cleanup is applied only when both leases
    /// still match, so neither a replaced process nor an older same-process
    /// socket can tear down the current notifier and jobs.
    pub(super) notifiers: HashMap<String, NotifierEntry>,
}

/// A registered push notifier plus the agent instance id that installed it.
#[derive(Debug, Clone)]
pub(super) struct NotifierEntry {
    pub(super) notify: Arc<Notify>,
    pub(super) agent_instance_id: String,
    pub(super) connection_id: Option<String>,
}
