use super::{RecoveryKind, ToolResult, ToolRuntime};
use crate::auth::{AuthContext, AuthKind};
use chrono::Utc;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;
use webcodex_core::coding_agent::{
    CodingAgentCancelRequest, CodingAgentConfigValue, CodingAgentDispatchState, CodingAgentEvent,
    CodingAgentExecutionState, CodingAgentObserveRequest, CodingAgentObserveResult,
    CodingAgentRequest, CodingAgentResponse, CodingAgentResponsePayload, CodingAgentRunSnapshot,
    CodingAgentRunState, CodingAgentStartRequest, CodingAgentTerminal,
    CODING_AGENT_MAX_CONFIG_OPTIONS, CODING_AGENT_MAX_EVENTS_PER_RESPONSE,
    CODING_AGENT_MAX_INVENTORY_RUNS, CODING_AGENT_OBSERVE_WAIT_MAX_SECS,
    CODING_AGENT_TIMEOUT_MAX_SECS, CODING_AGENT_TIMEOUT_MIN_SECS,
};

const IDEMPOTENCY_KEY_MAX_BYTES: usize = 256;
const START_RESPONSE_WAIT_SECS: u64 = 32;
const CONTROL_RESPONSE_WAIT_SECS: u64 = 65;
const DEFAULT_RUN_TIMEOUT_SECS: u64 = 300;
const PUBLIC_TOKEN_PREFIX: &str = "wcar1";
const SERVER_TERMINAL_RETENTION_SECS: i64 = 15 * 60;
const SERVER_MAX_TERMINAL_RUNS: usize = CODING_AGENT_MAX_INVENTORY_RUNS;

#[derive(Debug, Clone)]
pub(crate) struct ServerRunBinding {
    authority_fingerprint: String,
    client_id: String,
    agent_instance_id: String,
    runtime_project_id: String,
    provider_id: String,
    provider_instance_id: String,
    recording_session_id: Option<String>,
    recorded_lifecycle_mask: u8,
    snapshot: CodingAgentRunSnapshot,
}

#[derive(Debug)]
pub(crate) struct CodingAgentServerState {
    epoch: String,
    runs: Mutex<HashMap<String, ServerRunBinding>>,
}

fn prune_server_runs_locked(runs: &mut HashMap<String, ServerRunBinding>, now: i64) {
    let cutoff = now.saturating_sub(SERVER_TERMINAL_RETENTION_SECS);
    runs.retain(|_, binding| {
        !binding.snapshot.state.terminal() || binding.snapshot.updated_at >= cutoff
    });
    let mut terminals = runs
        .iter()
        .filter(|(_, binding)| binding.snapshot.state.terminal())
        .map(|(run_id, binding)| (run_id.clone(), binding.snapshot.updated_at))
        .collect::<Vec<_>>();
    if terminals.len() <= SERVER_MAX_TERMINAL_RUNS {
        return;
    }
    terminals.sort_by_key(|(_, updated_at)| *updated_at);
    let remove_count = terminals.len().saturating_sub(SERVER_MAX_TERMINAL_RUNS);
    for (run_id, _) in terminals.into_iter().take(remove_count) {
        runs.remove(&run_id);
    }
}

impl Default for CodingAgentServerState {
    fn default() -> Self {
        Self {
            epoch: Uuid::new_v4().simple().to_string(),
            runs: Mutex::new(HashMap::new()),
        }
    }
}

impl CodingAgentServerState {
    async fn bind(
        &self,
        client: &crate::shell_protocol::ShellClientView,
        run: CodingAgentRunSnapshot,
        recording_session_id: Option<String>,
    ) {
        let mut runs = self.runs.lock().await;
        prune_server_runs_locked(&mut runs, Utc::now().timestamp());
        let existing = runs.get(&run.run_id);
        let recording_session_id = existing
            .and_then(|binding| binding.recording_session_id.clone())
            .or(recording_session_id);
        let recorded_lifecycle_mask = existing
            .map(|binding| binding.recorded_lifecycle_mask)
            .unwrap_or_default();
        runs.insert(
            run.run_id.clone(),
            ServerRunBinding {
                authority_fingerprint: run.authority_fingerprint.clone(),
                client_id: client.client_id.clone(),
                agent_instance_id: client.agent_instance_id.clone(),
                runtime_project_id: run.runtime_project_id.clone(),
                provider_id: run.provider_id.clone(),
                provider_instance_id: run.provider_instance_id.clone(),
                recording_session_id,
                recorded_lifecycle_mask,
                snapshot: run,
            },
        );
    }

    async fn attach_recorder(&self, run_id: &str, recording_session_id: Option<String>) {
        let Some(recording_session_id) = recording_session_id else {
            return;
        };
        let mut runs = self.runs.lock().await;
        prune_server_runs_locked(&mut runs, Utc::now().timestamp());
        if let Some(binding) = runs.get_mut(run_id) {
            if binding.recording_session_id.is_none() {
                binding.recording_session_id = Some(recording_session_id);
            }
        }
    }

    async fn take_lifecycle_evidence(
        &self,
        run_id: &str,
    ) -> Option<(String, CodingAgentRunSnapshot, &'static str)> {
        let mut runs = self.runs.lock().await;
        prune_server_runs_locked(&mut runs, Utc::now().timestamp());
        let binding = runs.get_mut(run_id)?;
        let session_id = binding.recording_session_id.clone()?;
        let (bit, kind) = match binding.snapshot.state {
            CodingAgentRunState::Starting | CodingAgentRunState::Running => {
                (1, "coding_agent_started")
            }
            CodingAgentRunState::WaitingPermission => (2, "coding_agent_waiting_permission"),
            CodingAgentRunState::Completed
            | CodingAgentRunState::Failed
            | CodingAgentRunState::Cancelled
            | CodingAgentRunState::Lost => (4, "coding_agent_terminal"),
        };
        if binding.recorded_lifecycle_mask & bit != 0 {
            return None;
        }
        binding.recorded_lifecycle_mask |= bit;
        Some((session_id, binding.snapshot.clone(), kind))
    }

    async fn get(&self, run_id: &str) -> Option<ServerRunBinding> {
        let mut runs = self.runs.lock().await;
        prune_server_runs_locked(&mut runs, Utc::now().timestamp());
        runs.get(run_id).cloned()
    }
}

impl ToolRuntime {
    pub(crate) async fn coding_agent_start(
        &self,
        project: String,
        provider_id: String,
        idempotency_key: String,
        instruction: String,
        config: Option<BTreeMap<String, CodingAgentConfigValue>>,
        timeout_secs: Option<u64>,
        recording_session_id: Option<String>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if let Err(error) = validate_start_input(
            &provider_id,
            &idempotency_key,
            &instruction,
            config.as_ref(),
            timeout_secs,
        ) {
            return coding_agent_error(
                "invalid_coding_agent_start",
                error,
                "not_started",
                RecoveryKind::FixInput,
                None,
            );
        }
        let principal = match stable_principal(auth) {
            Ok(principal) => principal,
            Err(error) => {
                return coding_agent_error(
                    "coding_agent_identity_unavailable",
                    error,
                    "not_started",
                    RecoveryKind::FixInput,
                    None,
                )
            }
        };
        let authority_fingerprint = authority_fingerprint(&principal);
        let run_id = deterministic_run_id(&principal, &idempotency_key);
        let resolved = match self.resolve_project_input_for_auth(&project, auth).await {
            Ok(resolved) => resolved,
            Err(error) => return error.into_tool_result(),
        };
        let client_id = match resolved.config.agent_client_id() {
            Ok(value) => value.to_string(),
            Err(error) => {
                return coding_agent_error(
                    "invalid_project",
                    error,
                    "not_started",
                    RecoveryKind::FixInput,
                    Some(&run_id),
                )
            }
        };
        let client = match self
            .shell_clients
            .get_client_view_for_auth(&client_id, auth)
            .await
        {
            Some(client) if client.connected => client,
            _ => {
                return coding_agent_error(
                    "coding_agent_runner_unavailable",
                    "exact Project Runner is offline or unauthorized",
                    "not_started",
                    RecoveryKind::Wait,
                    Some(&run_id),
                )
            }
        };
        if !client.capabilities.coding_agent_runs {
            return coding_agent_error(
                "coding_agent_unsupported",
                "exact Project Runner does not advertise CodingAgentRun",
                "not_started",
                RecoveryKind::Reobserve,
                Some(&run_id),
            );
        }
        let providers = client.coding_agent_providers.as_deref().unwrap_or(&[]);
        let provider = match providers
            .iter()
            .find(|provider| provider.provider_id == provider_id)
        {
            Some(provider) => provider,
            None => {
                return coding_agent_error(
                    "coding_agent_provider_unavailable",
                    "logical ACP provider is not advertised by the exact Project Runner",
                    "not_started",
                    RecoveryKind::Reobserve,
                    Some(&run_id),
                )
            }
        };
        let timeout_secs = timeout_secs.unwrap_or(DEFAULT_RUN_TIMEOUT_SECS);
        let config = config.unwrap_or_default();
        let intent_fingerprint = intent_fingerprint(
            &resolved.resolved_id,
            &provider_id,
            &instruction,
            &config,
            timeout_secs,
        );

        if let Some(existing) = self
            .reconcile_run(&run_id, &authority_fingerprint, auth)
            .await
        {
            if existing.snapshot.intent_fingerprint != intent_fingerprint {
                return coding_agent_error(
                    "idempotency_conflict",
                    "idempotency_key is already bound to a different CodingAgentRun intent",
                    "not_started",
                    RecoveryKind::FixInput,
                    Some(&run_id),
                );
            }
            if existing.runtime_project_id != resolved.resolved_id {
                return coding_agent_error(
                    "idempotency_conflict",
                    "idempotency_key is already bound to another Project",
                    "not_started",
                    RecoveryKind::FixInput,
                    Some(&run_id),
                );
            }
            self.coding_agent_runs
                .attach_recorder(&run_id, recording_session_id.clone())
                .await;
            self.record_coding_agent_lifecycle_if_needed(&run_id).await;
            return ToolResult::ok(start_projection(
                &existing.snapshot,
                observation_token(&self.coding_agent_runs.epoch, &run_id, 0),
            ));
        }

        let operation = CodingAgentRequest::Start(CodingAgentStartRequest {
            run_id: run_id.clone(),
            intent_fingerprint: intent_fingerprint.clone(),
            authority_fingerprint: authority_fingerprint.clone(),
            runtime_project_id: resolved.resolved_id.clone(),
            project_root: resolved.config.path.clone(),
            provider_id: provider_id.clone(),
            provider_instance_id: provider.provider_instance_id.clone(),
            instruction,
            config,
            timeout_secs,
        });
        let (request_id, receiver) = match self
            .shell_clients
            .enqueue_coding_agent(
                &client.client_id,
                &client.agent_instance_id,
                &provider_id,
                &provider.provider_instance_id,
                operation,
                auth,
                authority_fingerprint.clone(),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return coding_agent_error(
                    "coding_agent_dispatch_rejected",
                    error,
                    "not_started",
                    RecoveryKind::Reobserve,
                    Some(&run_id),
                )
            }
        };
        let response =
            match tokio::time::timeout(Duration::from_secs(START_RESPONSE_WAIT_SECS), receiver)
                .await
            {
                Ok(Ok(response)) => response,
                Ok(Err(_)) | Err(_) => {
                    return self
                        .start_waiter_lost(
                            &request_id,
                            &run_id,
                            &authority_fingerprint,
                            recording_session_id.clone(),
                            auth,
                        )
                        .await;
                }
            };
        match response.payload {
            Some(CodingAgentResponsePayload::Start { run }) => {
                if run.authority_fingerprint != authority_fingerprint
                    || run.intent_fingerprint != intent_fingerprint
                    || run.runtime_project_id != resolved.resolved_id
                {
                    return coding_agent_error(
                        "invalid_runner_response",
                        "Runner returned mismatched CodingAgentRun identity",
                        "outcome_unknown",
                        RecoveryKind::Reconcile,
                        Some(&run_id),
                    );
                }
                self.coding_agent_runs
                    .bind(&client, run.clone(), recording_session_id)
                    .await;
                self.record_coding_agent_lifecycle_if_needed(&run_id).await;
                ToolResult::ok(start_projection(
                    &run,
                    observation_token(&self.coding_agent_runs.epoch, &run_id, 0),
                ))
            }
            _ => response_to_tool_error(response, Some(&run_id)),
        }
    }

    pub(crate) async fn coding_agent_observe(
        &self,
        run_id: String,
        after_observation_token: Option<String>,
        wait_secs: Option<u64>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let wait_secs = wait_secs.unwrap_or(0);
        if wait_secs > CODING_AGENT_OBSERVE_WAIT_MAX_SECS {
            return coding_agent_error(
                "invalid_wait_secs",
                "wait_secs exceeds CodingAgentRun bounded wait",
                "not_started",
                RecoveryKind::FixInput,
                Some(&run_id),
            );
        }
        let authority = match stable_principal(auth) {
            Ok(principal) => authority_fingerprint(&principal),
            Err(error) => {
                return coding_agent_error(
                    "coding_agent_identity_unavailable",
                    error,
                    "not_started",
                    RecoveryKind::FixInput,
                    Some(&run_id),
                )
            }
        };
        let Some(binding) = self.reconcile_run(&run_id, &authority, auth).await else {
            return coding_agent_error(
                "unknown_coding_agent_run",
                "CodingAgentRun is not visible to this caller",
                "not_started",
                RecoveryKind::Reobserve,
                Some(&run_id),
            );
        };
        if binding.authority_fingerprint != authority {
            return coding_agent_error(
                "unknown_coding_agent_run",
                "CodingAgentRun is not visible to this caller",
                "not_started",
                RecoveryKind::Reobserve,
                Some(&run_id),
            );
        }
        self.record_coding_agent_lifecycle_if_needed(&run_id).await;
        let (after_sequence, token_reset) = match after_observation_token.as_deref() {
            None => (None, false),
            Some(token) => {
                match parse_observation_token(&self.coding_agent_runs.epoch, &run_id, token) {
                    Ok(sequence) => (Some(sequence), false),
                    Err(TokenError::StaleEpoch) => (None, true),
                    Err(TokenError::Invalid) => {
                        return coding_agent_error(
                            "invalid_observation_token",
                            "observation token is invalid or belongs to another Run",
                            "not_started",
                            RecoveryKind::FixInput,
                            Some(&run_id),
                        )
                    }
                }
            }
        };
        if binding.snapshot.state.terminal()
            && binding.agent_instance_id
                != self
                    .current_agent_instance(&binding.client_id, auth)
                    .await
                    .unwrap_or_default()
        {
            return ToolResult::ok(observe_projection(
                CodingAgentObserveResult {
                    run: binding.snapshot.clone(),
                    events: Vec::new(),
                    first_retained_sequence: 1,
                    next_sequence: after_sequence.unwrap_or(0),
                    has_more: false,
                    history_lost: true,
                },
                &self.coding_agent_runs.epoch,
                token_reset,
            ));
        }
        let operation = CodingAgentRequest::Observe(CodingAgentObserveRequest {
            run_id: run_id.clone(),
            after_sequence,
            limit: CODING_AGENT_MAX_EVENTS_PER_RESPONSE,
            wait_secs,
        });
        let (request_id, receiver) = match self
            .shell_clients
            .enqueue_coding_agent(
                &binding.client_id,
                &binding.agent_instance_id,
                &binding.provider_id,
                &binding.provider_instance_id,
                operation,
                auth,
                authority.clone(),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                if binding.snapshot.state.terminal() {
                    return ToolResult::ok(observe_projection(
                        CodingAgentObserveResult {
                            run: binding.snapshot.clone(),
                            events: Vec::new(),
                            first_retained_sequence: 1,
                            next_sequence: after_sequence.unwrap_or(0),
                            has_more: false,
                            history_lost: true,
                        },
                        &self.coding_agent_runs.epoch,
                        true,
                    ));
                }
                return coding_agent_error(
                    "coding_agent_runner_unavailable",
                    error,
                    "outcome_unknown",
                    RecoveryKind::Reobserve,
                    Some(&run_id),
                );
            }
        };
        let response =
            match tokio::time::timeout(Duration::from_secs(CONTROL_RESPONSE_WAIT_SECS), receiver)
                .await
            {
                Ok(Ok(response)) => response,
                _ => {
                    let _ = self
                        .shell_clients
                        .cancel_request_dispatch_state(&request_id)
                        .await;
                    return coding_agent_error(
                        "coding_agent_observe_timeout",
                        "timed out waiting for bounded CodingAgentRun observation",
                        "outcome_unknown",
                        RecoveryKind::Reobserve,
                        Some(&run_id),
                    );
                }
            };
        match response.payload {
            Some(CodingAgentResponsePayload::Observe { mut observation }) => {
                if observation.run.authority_fingerprint != authority {
                    return coding_agent_error(
                        "invalid_runner_response",
                        "Runner CodingAgentRun authority fingerprint mismatch",
                        "outcome_unknown",
                        RecoveryKind::Reconcile,
                        Some(&run_id),
                    );
                }
                if token_reset {
                    observation.history_lost = true;
                }
                let client = match self
                    .shell_clients
                    .get_client_view_for_auth(&binding.client_id, auth)
                    .await
                {
                    Some(client) => client,
                    None => {
                        return coding_agent_error(
                            "coding_agent_runner_unavailable",
                            "exact Runner became unavailable",
                            "outcome_unknown",
                            RecoveryKind::Reobserve,
                            Some(&run_id),
                        )
                    }
                };
                self.coding_agent_runs
                    .bind(&client, observation.run.clone(), None)
                    .await;
                self.record_coding_agent_lifecycle_if_needed(&run_id).await;
                ToolResult::ok(observe_projection(
                    observation,
                    &self.coding_agent_runs.epoch,
                    token_reset,
                ))
            }
            _ => response_to_tool_error(response, Some(&run_id)),
        }
    }

    pub(crate) async fn coding_agent_cancel(
        &self,
        run_id: String,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let authority = match stable_principal(auth) {
            Ok(principal) => authority_fingerprint(&principal),
            Err(error) => {
                return coding_agent_error(
                    "coding_agent_identity_unavailable",
                    error,
                    "not_started",
                    RecoveryKind::FixInput,
                    Some(&run_id),
                )
            }
        };
        let Some(binding) = self.reconcile_run(&run_id, &authority, auth).await else {
            return coding_agent_error(
                "unknown_coding_agent_run",
                "CodingAgentRun is not visible to this caller",
                "not_started",
                RecoveryKind::Reobserve,
                Some(&run_id),
            );
        };
        self.record_coding_agent_lifecycle_if_needed(&run_id).await;
        if binding.snapshot.state.terminal() {
            return ToolResult::ok(cancel_projection(&binding.snapshot));
        }
        let operation = CodingAgentRequest::Cancel(CodingAgentCancelRequest {
            run_id: run_id.clone(),
        });
        let (request_id, receiver) = match self
            .shell_clients
            .enqueue_coding_agent(
                &binding.client_id,
                &binding.agent_instance_id,
                &binding.provider_id,
                &binding.provider_instance_id,
                operation,
                auth,
                authority.clone(),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return coding_agent_error(
                    "coding_agent_cancel_unavailable",
                    error,
                    "outcome_unknown",
                    RecoveryKind::Reobserve,
                    Some(&run_id),
                )
            }
        };
        let response =
            match tokio::time::timeout(Duration::from_secs(START_RESPONSE_WAIT_SECS), receiver)
                .await
            {
                Ok(Ok(response)) => response,
                _ => {
                    let _ = self
                        .shell_clients
                        .cancel_request_dispatch_state(&request_id)
                        .await;
                    return coding_agent_error(
                        "coding_agent_cancel_timeout",
                        "cancel outcome is not yet authoritative; observe the same Run",
                        "outcome_unknown",
                        RecoveryKind::Reobserve,
                        Some(&run_id),
                    );
                }
            };
        match response.payload {
            Some(CodingAgentResponsePayload::Cancel { run }) => {
                if run.authority_fingerprint != authority {
                    return coding_agent_error(
                        "invalid_runner_response",
                        "Runner CodingAgentRun authority fingerprint mismatch",
                        "outcome_unknown",
                        RecoveryKind::Reconcile,
                        Some(&run_id),
                    );
                }
                if let Some(client) = self
                    .shell_clients
                    .get_client_view_for_auth(&binding.client_id, auth)
                    .await
                {
                    self.coding_agent_runs
                        .bind(&client, run.clone(), None)
                        .await;
                }
                self.record_coding_agent_lifecycle_if_needed(&run_id).await;
                ToolResult::ok(cancel_projection(&run))
            }
            _ => response_to_tool_error(response, Some(&run_id)),
        }
    }

    async fn start_waiter_lost(
        &self,
        request_id: &str,
        run_id: &str,
        authority: &str,
        recording_session_id: Option<String>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let dispatched = self
            .shell_clients
            .cancel_request_dispatch_state(request_id)
            .await;
        if let Some(binding) = self.reconcile_run(run_id, authority, auth).await {
            self.coding_agent_runs
                .attach_recorder(run_id, recording_session_id)
                .await;
            self.record_coding_agent_lifecycle_if_needed(run_id).await;
            return ToolResult::ok(start_projection(
                &binding.snapshot,
                observation_token(&self.coding_agent_runs.epoch, run_id, 0),
            ));
        }
        match dispatched {
            Some(false) => coding_agent_error("coding_agent_start_timeout", "Run admission timed out before Runner dispatch", "not_started", RecoveryKind::RetrySame, Some(run_id)),
            Some(true) | None => coding_agent_error("coding_agent_start_outcome_unknown", "Run dispatch may have reached the Runner; do not use a new idempotency key, reobserve/retry the same initiation", "outcome_unknown", RecoveryKind::Reconcile, Some(run_id)),
        }
    }

    async fn record_coding_agent_lifecycle_if_needed(&self, run_id: &str) {
        let Some((session_id, snapshot, kind)) =
            self.coding_agent_runs.take_lifecycle_evidence(run_id).await
        else {
            return;
        };
        self.sessions.record_coding_agent_lifecycle_evidence(
            &session_id,
            &snapshot.runtime_project_id,
            &snapshot.run_id,
            &snapshot.provider_id,
            kind,
            state_name(&snapshot.state),
            execution_name(snapshot.execution_state),
            snapshot
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.stop_reason.as_deref()),
            snapshot
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.error_code.as_deref()),
        );
    }

    async fn reconcile_run(
        &self,
        run_id: &str,
        authority: &str,
        auth: Option<&AuthContext>,
    ) -> Option<ServerRunBinding> {
        // A live Runner inventory is authoritative over the Server's process-local
        // projection. This is what lets a Server restart (or a Runner restart that
        // recovered a durable record as `lost`) rebaseline without redispatching.
        if let Some((client, run)) = self
            .shell_clients
            .coding_agent_run_for_auth(auth, run_id)
            .await
        {
            if run.authority_fingerprint != authority {
                return None;
            }
            self.coding_agent_runs.bind(&client, run, None).await;
            return self.coding_agent_runs.get(run_id).await;
        }

        let mut binding = self.coding_agent_runs.get(run_id).await?;
        if binding.authority_fingerprint != authority {
            return None;
        }
        if binding.snapshot.state.terminal() {
            return Some(binding);
        }

        // A temporary disconnect of the same Runner is not proof of loss: keep
        // the active projection so callers get wait/reobserve semantics. A live
        // replacement instance, however, is a positive fence crossing. If that
        // replacement does not advertise the durable Run, the old prompt may have
        // executed and P1 must close it `lost` rather than retrying blindly.
        if let Some(current) = self
            .shell_clients
            .get_client_view_for_auth(&binding.client_id, auth)
            .await
        {
            let instance_replaced = current.agent_instance_id != binding.agent_instance_id;
            let provider_replaced = !instance_replaced
                && current
                    .coding_agent_providers
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .all(|provider| provider.provider_instance_id != binding.provider_instance_id);
            if instance_replaced || provider_replaced {
                let code = if instance_replaced {
                    "runner_replaced_uncertain"
                } else {
                    "provider_replaced_uncertain"
                };
                mark_server_binding_lost(&mut binding, code);
                self.coding_agent_runs
                    .runs
                    .lock()
                    .await
                    .insert(run_id.to_string(), binding.clone());
            }
        }
        Some(binding)
    }

    async fn current_agent_instance(
        &self,
        client_id: &str,
        auth: Option<&AuthContext>,
    ) -> Option<String> {
        self.shell_clients
            .get_client_view_for_auth(client_id, auth)
            .await
            .map(|client| client.agent_instance_id)
    }
}

fn mark_server_binding_lost(binding: &mut ServerRunBinding, code: &str) {
    if binding.snapshot.state.terminal() {
        return;
    }
    let completed_at = chrono::Utc::now().timestamp();
    binding.snapshot.state = CodingAgentRunState::Lost;
    binding.snapshot.execution_state = CodingAgentExecutionState::OutcomeUnknown;
    binding.snapshot.updated_at = completed_at;
    binding.snapshot.observation_revision = binding.snapshot.observation_revision.saturating_add(1);
    binding.snapshot.terminal = Some(CodingAgentTerminal {
        stop_reason: None,
        error_code: Some(code.to_string()),
        message: Some(
            "owning Runner/provider instance was replaced while prompt outcome was uncertain; do not redispatch"
                .to_string(),
        ),
        completed_at,
    });
}

fn validate_start_input(
    provider_id: &str,
    idempotency_key: &str,
    instruction: &str,
    config: Option<&BTreeMap<String, CodingAgentConfigValue>>,
    timeout_secs: Option<u64>,
) -> Result<(), String> {
    webcodex_core::coding_agent::validate_provider_id(provider_id)?;
    if idempotency_key.is_empty()
        || idempotency_key.len() > IDEMPOTENCY_KEY_MAX_BYTES
        || idempotency_key.contains(['\0', '\r', '\n'])
    {
        return Err(format!(
            "idempotency_key must contain 1..={IDEMPOTENCY_KEY_MAX_BYTES} bytes and no NUL/CR/LF"
        ));
    }
    if instruction.is_empty()
        || instruction.len() > webcodex_core::coding_agent::CODING_AGENT_MAX_INSTRUCTION_BYTES
        || instruction.contains('\0')
    {
        return Err("instruction is empty, too large, or contains NUL".to_string());
    }
    if config.is_some_and(|config| config.len() > CODING_AGENT_MAX_CONFIG_OPTIONS) {
        return Err("too many CodingAgentRun config overrides".to_string());
    }
    if let Some(timeout) = timeout_secs {
        if !(CODING_AGENT_TIMEOUT_MIN_SECS..=CODING_AGENT_TIMEOUT_MAX_SECS).contains(&timeout) {
            return Err("timeout_secs is outside the supported range".to_string());
        }
    }
    Ok(())
}

fn stable_principal(auth: Option<&AuthContext>) -> Result<String, String> {
    let Some(auth) = auth else {
        return Ok("local-dev:local-dev".to_string());
    };
    if auth.kind == AuthKind::Bootstrap || auth.is_bootstrap {
        return Ok("bootstrap:server-bootstrap".to_string());
    }
    if auth.is_oauth_shared_key_subject() || auth.is_shared_key() {
        let shared_key_hash = auth
            .shared_key_hash
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                "shared-key authority has no stable CodingAgentRun group identity".to_string()
            })?;
        return Ok(format!("shared-key-group:{shared_key_hash}"));
    }
    if auth.is_oauth_project_subject() || auth.is_project_credential() || auth.is_agent_token() {
        let project_grant_id = auth
            .project_grant_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                "project-grant authority has no stable CodingAgentRun grant identity".to_string()
            })?;
        return Ok(format!("project-grant:{project_grant_id}"));
    }
    if auth.is_open_anonymous() {
        return Ok("open-anonymous:open-anonymous".to_string());
    }
    if matches!(
        auth.kind,
        AuthKind::ApiToken | AuthKind::AccountCredential | AuthKind::OAuth2Token
    ) {
        let user_id = auth
            .user_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                "managed authority has no stable CodingAgentRun user identity".to_string()
            })?;
        return Ok(format!("managed-user:{user_id}"));
    }
    Err("authenticated credential has no canonical CodingAgentRun authority identity".to_string())
}

fn authority_fingerprint(principal: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-coding-agent-authority-v1\0");
    hasher.update(principal.as_bytes());
    format!("auth_{:x}", hasher.finalize())
}

fn deterministic_run_id(principal: &str, key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-coding-agent-run-v1\0");
    hasher.update(principal.as_bytes());
    hasher.update(b"\0");
    hasher.update(key.as_bytes());
    format!("wc_agent_run_{:x}", hasher.finalize())
}

fn intent_fingerprint(
    project: &str,
    provider: &str,
    instruction: &str,
    config: &BTreeMap<String, CodingAgentConfigValue>,
    timeout_secs: u64,
) -> String {
    let canonical = serde_json::to_vec(&json!({
        "project": project,
        "provider": provider,
        "instruction": instruction,
        "config": config,
        "timeout_secs": timeout_secs,
    }))
    .unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-coding-agent-intent-v1\0");
    hasher.update(&canonical);
    format!("{:x}", hasher.finalize())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenError {
    Invalid,
    StaleEpoch,
}

fn run_token_hash(run_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-coding-agent-token-run-v1\0");
    hasher.update(run_id.as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

fn observation_token(epoch: &str, run_id: &str, sequence: u64) -> String {
    format!(
        "{PUBLIC_TOKEN_PREFIX}:{epoch}:{}:{sequence}",
        run_token_hash(run_id)
    )
}

fn parse_observation_token(epoch: &str, run_id: &str, token: &str) -> Result<u64, TokenError> {
    if token.len() > 192 {
        return Err(TokenError::Invalid);
    }
    let mut parts = token.split(':');
    if parts.next() != Some(PUBLIC_TOKEN_PREFIX) {
        return Err(TokenError::Invalid);
    }
    let Some(token_epoch) = parts.next() else {
        return Err(TokenError::Invalid);
    };
    let Some(run_hash) = parts.next() else {
        return Err(TokenError::Invalid);
    };
    let Some(sequence) = parts.next() else {
        return Err(TokenError::Invalid);
    };
    if parts.next().is_some() || run_hash != run_token_hash(run_id) {
        return Err(TokenError::Invalid);
    }
    if token_epoch != epoch {
        return Err(TokenError::StaleEpoch);
    }
    sequence.parse().map_err(|_| TokenError::Invalid)
}

fn start_projection(run: &CodingAgentRunSnapshot, token: String) -> Value {
    json!({
        "run_id": run.run_id,
        "project": run.runtime_project_id,
        "provider_id": run.provider_id,
        "state": state_name(&run.state),
        "execution_state": execution_name(run.execution_state),
        "observation_token": token,
        "terminal": terminal_projection(run),
    })
}

fn cancel_projection(run: &CodingAgentRunSnapshot) -> Value {
    json!({
        "run_id": run.run_id,
        "project": run.runtime_project_id,
        "provider_id": run.provider_id,
        "state": state_name(&run.state),
        "execution_state": execution_name(run.execution_state),
        "cancel_requested": !run.state.terminal(),
        "terminal": terminal_projection(run),
    })
}

fn observe_projection(observation: CodingAgentObserveResult, epoch: &str, reset: bool) -> Value {
    let run = &observation.run;
    let token = observation_token(epoch, &run.run_id, observation.next_sequence);
    let events = observation
        .events
        .iter()
        .map(event_projection)
        .collect::<Vec<_>>();
    json!({
        "run_id": run.run_id,
        "project": run.runtime_project_id,
        "provider_id": run.provider_id,
        "state": state_name(&run.state),
        "execution_state": execution_name(run.execution_state),
        "events": events,
        "observation_token": token,
        "has_more": observation.has_more,
        "history_lost": observation.history_lost || reset,
        "first_retained_sequence": observation.first_retained_sequence,
        "terminal": terminal_projection(run),
        "recovery_kind": run_recovery_kind(run),
    })
}

fn event_projection(event: &CodingAgentEvent) -> Value {
    json!({
        "sequence": event.sequence,
        "kind": format!("{:?}", event.kind).to_ascii_lowercase(),
        "text": event.text,
        "label": event.label,
        "status": event.status,
        "usage": event.usage,
    })
}

fn terminal_projection(run: &CodingAgentRunSnapshot) -> Value {
    run.terminal
        .as_ref()
        .map(|terminal| {
            json!({
                "stop_reason": terminal.stop_reason,
                "error_code": terminal.error_code,
                "message": terminal.message,
                "completed_at": terminal.completed_at,
            })
        })
        .unwrap_or(Value::Null)
}

fn state_name(state: &CodingAgentRunState) -> &'static str {
    match state {
        CodingAgentRunState::Starting => "starting",
        CodingAgentRunState::Running => "running",
        CodingAgentRunState::WaitingPermission => "waiting_permission",
        CodingAgentRunState::Completed => "completed",
        CodingAgentRunState::Failed => "failed",
        CodingAgentRunState::Cancelled => "cancelled",
        CodingAgentRunState::Lost => "lost",
    }
}

fn execution_name(state: CodingAgentExecutionState) -> &'static str {
    match state {
        CodingAgentExecutionState::NotStarted => "not_started",
        CodingAgentExecutionState::Started => "started",
        CodingAgentExecutionState::OutcomeUnknown => "outcome_unknown",
        CodingAgentExecutionState::Completed => "completed",
    }
}

fn run_recovery_kind(run: &CodingAgentRunSnapshot) -> &'static str {
    match run.state {
        CodingAgentRunState::Starting | CodingAgentRunState::Running => "reobserve",
        CodingAgentRunState::WaitingPermission => "wait",
        CodingAgentRunState::Lost => "reconcile",
        CodingAgentRunState::Completed | CodingAgentRunState::Cancelled => "none",
        CodingAgentRunState::Failed
            if run.execution_state == CodingAgentExecutionState::NotStarted =>
        {
            "retry_same"
        }
        CodingAgentRunState::Failed => "none",
    }
}

fn coding_agent_error(
    kind: &str,
    message: impl Into<String>,
    execution_state: &str,
    recovery: RecoveryKind,
    run_id: Option<&str>,
) -> ToolResult {
    ToolResult::err_with_output(
        message.into(),
        json!({
            "error_kind": kind,
            "run_id": run_id,
            "execution_state": execution_state,
        }),
    )
    .with_recovery(recovery, None)
}

fn response_to_tool_error(response: CodingAgentResponse, run_id: Option<&str>) -> ToolResult {
    let dispatch = response.dispatch_state;
    let Some(error) = response.error else {
        return coding_agent_error(
            "invalid_runner_response",
            "Runner CodingAgentRun response contained no result",
            if dispatch == CodingAgentDispatchState::NotStarted {
                "not_started"
            } else {
                "outcome_unknown"
            },
            RecoveryKind::Reobserve,
            run_id,
        );
    };
    let recovery = match error.recovery_kind.as_deref() {
        Some("fix_input") => RecoveryKind::FixInput,
        Some("retry_same") => RecoveryKind::RetrySame,
        Some("reconcile") => RecoveryKind::Reconcile,
        Some("wait") => RecoveryKind::Wait,
        Some("user_action") => RecoveryKind::UserAction,
        Some("none") => RecoveryKind::NoAction,
        _ => RecoveryKind::Reobserve,
    };
    coding_agent_error(
        &error.code,
        error.message,
        if dispatch == CodingAgentDispatchState::NotStarted {
            "not_started"
        } else {
            "outcome_unknown"
        },
        recovery,
        run_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_server_binding(
        run_id: String,
        state: CodingAgentRunState,
        updated_at: i64,
    ) -> ServerRunBinding {
        let terminal = state.terminal().then(|| CodingAgentTerminal {
            stop_reason: Some("end_turn".to_string()),
            error_code: None,
            message: None,
            completed_at: updated_at,
        });
        ServerRunBinding {
            authority_fingerprint: "auth_test".to_string(),
            client_id: "client".to_string(),
            agent_instance_id: "instance".to_string(),
            runtime_project_id: "agent:test:demo".to_string(),
            provider_id: "codex".to_string(),
            provider_instance_id: "provider".to_string(),
            recording_session_id: None,
            recorded_lifecycle_mask: 0,
            snapshot: CodingAgentRunSnapshot {
                run_id,
                intent_fingerprint: "fingerprint".to_string(),
                authority_fingerprint: "auth_test".to_string(),
                runtime_project_id: "agent:test:demo".to_string(),
                provider_id: "codex".to_string(),
                provider_instance_id: "provider".to_string(),
                state,
                execution_state: if terminal.is_some() {
                    CodingAgentExecutionState::Completed
                } else {
                    CodingAgentExecutionState::Started
                },
                observation_revision: 0,
                created_at: updated_at,
                updated_at,
                terminal,
            },
        }
    }

    #[test]
    fn server_run_registry_prunes_expired_and_bounds_recent_terminals() {
        let now = 10_000;
        let mut runs = HashMap::new();
        runs.insert(
            "wc_agent_run_active".to_string(),
            test_server_binding(
                "wc_agent_run_active".to_string(),
                CodingAgentRunState::Running,
                1,
            ),
        );
        runs.insert(
            "wc_agent_run_expired".to_string(),
            test_server_binding(
                "wc_agent_run_expired".to_string(),
                CodingAgentRunState::Completed,
                now - SERVER_TERMINAL_RETENTION_SECS - 1,
            ),
        );
        for index in 0..SERVER_MAX_TERMINAL_RUNS + 2 {
            let run_id = format!("wc_agent_run_recent_{index:03}");
            runs.insert(
                run_id.clone(),
                test_server_binding(run_id, CodingAgentRunState::Completed, now - index as i64),
            );
        }
        prune_server_runs_locked(&mut runs, now);
        assert!(runs.contains_key("wc_agent_run_active"));
        assert!(!runs.contains_key("wc_agent_run_expired"));
        assert_eq!(
            runs.values()
                .filter(|binding| binding.snapshot.state.terminal())
                .count(),
            SERVER_MAX_TERMINAL_RUNS
        );
    }

    #[test]
    fn stable_principal_canonicalizes_equivalent_credential_transports() {
        let direct_shared = crate::auth::shared_key_context("coding-agent-shared-key");
        let shared_hash = direct_shared.shared_key_hash.clone().unwrap();
        let oauth_shared = AuthContext {
            token_kind: Some("oauth2_shared_key".to_string()),
            shared_key_hash: Some(shared_hash),
            ..AuthContext::new(AuthKind::OAuth2Token)
        };
        assert_eq!(
            stable_principal(Some(&direct_shared)).unwrap(),
            stable_principal(Some(&oauth_shared)).unwrap()
        );

        let pat = AuthContext {
            user_id: Some("user-1".to_string()),
            api_key_id: Some("pat-1".to_string()),
            ..AuthContext::new(AuthKind::ApiToken)
        };
        let oauth_user = AuthContext {
            user_id: Some("user-1".to_string()),
            api_key_id: Some("oauth-access-1".to_string()),
            token_kind: Some("oauth2".to_string()),
            ..AuthContext::new(AuthKind::OAuth2Token)
        };
        assert_eq!(
            stable_principal(Some(&pat)).unwrap(),
            stable_principal(Some(&oauth_user)).unwrap()
        );

        let project = AuthContext {
            project_grant_id: Some("grant-1".to_string()),
            ..AuthContext::new(AuthKind::ProjectCredential)
        };
        let oauth_project = AuthContext {
            token_kind: Some(crate::auth::PROJECT_SHARE_OAUTH_TOKEN_KIND.to_string()),
            project_grant_id: Some("grant-1".to_string()),
            ..AuthContext::new(AuthKind::OAuth2Token)
        };
        assert_eq!(
            stable_principal(Some(&project)).unwrap(),
            stable_principal(Some(&oauth_project)).unwrap()
        );
    }

    #[test]
    fn identities_are_domain_separated_and_tokens_are_run_bound() {
        let principal = "oauth2:shared-key:abc";
        let run = deterministic_run_id(principal, "same-key");
        assert!(run.starts_with("wc_agent_run_"));
        assert_ne!(authority_fingerprint(principal), run);
        let token = observation_token("epoch", &run, 7);
        assert_eq!(parse_observation_token("epoch", &run, &token), Ok(7));
        assert_eq!(
            parse_observation_token("other", &run, &token),
            Err(TokenError::StaleEpoch)
        );
        assert_eq!(
            parse_observation_token("epoch", "wc_agent_run_other", &token),
            Err(TokenError::Invalid)
        );
    }

    #[test]
    fn intent_fingerprint_is_stable_over_sorted_config_and_changes_with_execution_intent() {
        let config = BTreeMap::from([(
            "mode".to_string(),
            CodingAgentConfigValue::String("agent".to_string()),
        )]);
        let a = intent_fingerprint("agent:x:p", "codex", "inspect", &config, 30);
        let b = intent_fingerprint("agent:x:p", "codex", "inspect", &config, 30);
        let c = intent_fingerprint("agent:x:p", "codex", "different", &config, 30);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
