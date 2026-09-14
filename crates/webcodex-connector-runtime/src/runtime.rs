//! Durable, transport-neutral Connector task and execution runtime.

mod operations;
mod task_lifecycle;

use crate::context::ConnectorContext;
use crate::execution;
use crate::projections::{
    connector_window_binding, context_refresh_payload, host_review_projection, invalid_input,
    navigation_payload, parse_input, project_brief, project_brief_from_fingerprint,
    store_error_outcome,
};
use crate::surface;
use crate::wire_models::{TaskCancelInput, TaskReviewInput, TaskStartInput};
use crate::workspace::{LocalResultDecision, PreparedWorkspace, WorkspaceManager};
use crate::{
    ConnectorCallContext, ConnectorCallOutcome, ConnectorJobHostError, ConnectorPermission,
    ConnectorProjectRegistration, ConnectorTransport, ConnectorWindowId,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use webcodex_runner_registry::RunnerRegistry;
use webcodex_store::{
    ConnectorBinding, ConnectorExecution, ConnectorRunState, ConnectorTaskContinuation,
    ConnectorTaskMode, ConnectorTaskResult, ConnectorTaskSnapshot, ConnectorTaskState,
    ConnectorTaskStoreError, ConnectorWorkspaceTransition, Database, NewConnectorTask,
};
use webcodex_workspace::project_context::{
    capture_project_context, compare_project_context, ContextRefreshSummary,
    ProjectContextFingerprint,
};

#[cfg(test)]
type FinishTestHook = (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);

pub struct ConnectorRuntime {
    runner_registry: Arc<RunnerRegistry>,
    pub(crate) db: Arc<Database>,
    context: ConnectorContext,
    workspace: crate::workspace::WorkspaceManager,
    executions: execution::ExecutionService,
    workspace_ops: tokio::sync::Mutex<()>,
    task_locks: StdMutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
    context_locks: StdMutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
    #[cfg(test)]
    finish_after_fingerprint: StdMutex<Option<FinishTestHook>>,
    #[cfg(test)]
    mutation_before_task_lock: StdMutex<Option<Arc<tokio::sync::Semaphore>>>,
}

impl ConnectorRuntime {
    pub fn new(
        runner_registry: Arc<RunnerRegistry>,
        db: Arc<Database>,
        context: ConnectorContext,
    ) -> Result<Self, String> {
        context.validate()?;
        let workspace = WorkspaceManager::new(&context)?;
        WorkspaceManager::recover_result_decisions(
            &db,
            &context.project_id,
            Path::new(&context.executor_root),
            chrono::Utc::now().timestamp(),
        )
        .map_err(|error| format!("failed to recover local result decision: {error}"))?;
        let executions = execution::ExecutionService::new(
            runner_registry.clone(),
            db.clone(),
            workspace.clone(),
        );
        let (runs_recovered, executions_recovered) = executions
            .reconcile_startup(&context.project_id, chrono::Utc::now().timestamp())
            .map_err(|error| format!("failed to recover connector runs: {error}"))?;
        if runs_recovered > 0 || executions_recovered > 0 {
            tracing::warn!(
                project_id = %context.project_id,
                runs = runs_recovered,
                executions = executions_recovered,
                "Recovered unfinished connector executions as interrupted"
            );
        }
        let preserved = db
            .connector_preserved_workspaces(&context.project_id)
            .map_err(|error| format!("failed to inspect connector workspaces: {error}"))?;
        for warning in workspace.recover(&context, &preserved) {
            tracing::warn!(project_id = %context.project_id, warning = %warning, "Connector workspace recovery was incomplete");
        }
        Ok(Self {
            runner_registry,
            db,
            context,
            workspace,
            executions,
            workspace_ops: tokio::sync::Mutex::new(()),
            task_locks: StdMutex::new(HashMap::new()),
            context_locks: StdMutex::new(HashMap::new()),
            #[cfg(test)]
            finish_after_fingerprint: StdMutex::new(None),
            #[cfg(test)]
            mutation_before_task_lock: StdMutex::new(None),
        })
    }

    pub fn context(&self) -> &ConnectorContext {
        &self.context
    }

    fn project_access_allowed(&self, auth: &ConnectorCallContext) -> bool {
        auth.access
            .project_access_allowed(&self.context.project_grant_id)
    }

    /// Build the readiness projection and record the probe outcome as the
    /// connector endpoint observation (real activity, not config inference).

    pub async fn host_review(
        &self,
        auth: &ConnectorCallContext,
        input: TaskReviewInput,
    ) -> ConnectorCallOutcome {
        let task = match self
            .db
            .local_connector_task(&input.task_id, &self.context.project_id)
        {
            Ok(task) => task,
            Err(error) => return store_error_outcome(error, None),
        };
        let mut outcome = self
            .task_review(
                json!(input),
                &task.owner_subject_id,
                auth,
                ConnectorTransport::Api,
                false,
            )
            .await;
        if outcome.ok {
            // Read-only guidance read-state for the console timeline: the
            // watermark the model has claimed, and the newest still-pending
            // guidance. This never advances the watermark — opening the host
            // review page must not consume guidance the model has yet to read.
            let read_state = self
                .db
                .connector_guidance_read_state(&input.task_id, &self.context.project_id)
                .unwrap_or(None);
            outcome.body = host_review_projection(&outcome.body, read_state);
        }
        outcome
    }

    pub async fn host_cancel(
        &self,
        auth: &ConnectorCallContext,
        input: TaskCancelInput,
    ) -> ConnectorCallOutcome {
        let task = match self
            .db
            .local_connector_task(&input.task_id, &self.context.project_id)
        {
            Ok(task) => task,
            Err(error) => return store_error_outcome(error, None),
        };
        self.task_cancel(json!(input), &task.owner_subject_id, auth)
            .await
    }

    pub fn host_decide(
        &self,
        task_id: &str,
        result_id: Option<&str>,
        decision: LocalResultDecision,
        reason: Option<&str>,
        now: i64,
    ) -> Result<ConnectorTaskResult, ConnectorTaskStoreError> {
        WorkspaceManager::decide_connector_result_local(
            &self.db,
            &self.context.project_id,
            task_id,
            result_id,
            Path::new(&self.context.executor_root),
            decision,
            "local_console",
            reason,
            now,
        )
    }

    fn execution_task_not_found() -> ConnectorCallOutcome {
        store_error_outcome(ConnectorTaskStoreError::NotFound, None)
    }

    fn execution_for_auth(
        &self,
        execution_id: &str,
        auth: &ConnectorCallContext,
    ) -> Result<(ConnectorTaskSnapshot, ConnectorExecution), ConnectorCallOutcome> {
        if !auth.access.allows(ConnectorPermission::JobRun) {
            return Err(ConnectorCallOutcome::scope_denied(
                ConnectorPermission::JobRun,
            ));
        }
        if !self.project_access_allowed(auth) {
            return Err(Self::execution_task_not_found());
        }
        let subject_id = auth.access.principal.as_str();
        self.db
            .connector_execution_for_subject(execution_id, &self.context.project_id, subject_id)
            .map_err(|error| match error {
                ConnectorTaskStoreError::NotFound => Self::execution_task_not_found(),
                other => store_error_outcome(other, None),
            })
    }

    fn execution_task_for_auth(
        &self,
        execution_id: &str,
        auth: &ConnectorCallContext,
    ) -> Result<(ConnectorTaskSnapshot, ConnectorExecution), ConnectorCallOutcome> {
        let (task, execution) = self.execution_for_auth(execution_id, auth)?;
        if !execution.mcp_task_is_materialized() {
            return Err(Self::execution_task_not_found());
        }
        Ok((task, execution))
    }

    pub async fn ordinary_execution_result_for_auth(
        &self,
        execution_id: &str,
        auth: &ConnectorCallContext,
    ) -> Result<ConnectorCallOutcome, ConnectorCallOutcome> {
        let (mut task, execution) = self.execution_for_auth(execution_id, auth)?;
        task.run_id = execution.run_id.clone();
        let runner_access = auth.access.runner_access.clone();
        let projection = self
            .executions
            .projection(&execution, &runner_access, true)
            .await;
        let mut data = json!({ "execution": projection });
        self.attach_pending_guidance(&task, &mut data);
        Ok(ConnectorCallOutcome::success_blocking_at(
            &task,
            task.event_cursor,
            data,
            execution.blocks_finish(),
        ))
    }

    pub fn materialize_execution_task_for_auth(
        &self,
        execution_id: &str,
        auth: &ConnectorCallContext,
    ) -> Result<ConnectorExecution, ConnectorCallOutcome> {
        if !auth.access.allows(ConnectorPermission::JobRun) {
            return Err(ConnectorCallOutcome::scope_denied(
                ConnectorPermission::JobRun,
            ));
        }
        if !self.project_access_allowed(auth) {
            return Err(Self::execution_task_not_found());
        }
        let subject_id = auth.access.principal.as_str();
        self.db
            .materialize_connector_execution_mcp_task_for_subject(
                execution_id,
                &self.context.project_id,
                subject_id,
                chrono::Utc::now().timestamp(),
            )
            .map_err(|error| match error {
                ConnectorTaskStoreError::NotFound => Self::execution_task_not_found(),
                other => store_error_outcome(other, None),
            })
    }

    pub async fn execution_task_result_for_auth(
        &self,
        execution_id: &str,
        auth: &ConnectorCallContext,
    ) -> Result<
        (
            ConnectorTaskSnapshot,
            ConnectorExecution,
            ConnectorCallOutcome,
        ),
        ConnectorCallOutcome,
    > {
        let (mut task, execution) = self.execution_task_for_auth(execution_id, auth)?;
        if execution.is_terminal() && !execution.mcp_task_result_is_finalized() {
            return Err(store_error_outcome(
                ConnectorTaskStoreError::InvalidState(
                    "materialized MCP task is terminal before its durable result was finalized"
                        .to_string(),
                ),
                Some(&task),
            ));
        }
        // A Connector task may have been resumed into a later run. MCP task
        // identity is the exact durable execution, so never project a newer
        // run id onto an older execution handle.
        task.run_id = execution.run_id.clone();
        let execution_event_cursor = self
            .db
            .connector_execution_event_cursor(&execution)
            .map_err(|error| store_error_outcome(error, Some(&task)))?;
        let projection = self.executions.durable_task_projection(&execution);
        let outcome = ConnectorCallOutcome::success_blocking_at(
            &task,
            execution_event_cursor,
            json!({ "execution": projection }),
            execution.blocks_finish(),
        );
        Ok((task, execution, outcome))
    }

    pub async fn cancel_execution_task_for_auth(
        &self,
        execution_id: &str,
        auth: &ConnectorCallContext,
    ) -> Result<(), ConnectorCallOutcome> {
        let (task, execution) = self.execution_task_for_auth(execution_id, auth)?;
        if execution.is_terminal() {
            return Ok(());
        }
        let task_lock = self.task_lock(&task.task_id);
        let _task_guard = task_lock.lock().await;
        let (task, execution) = self.execution_task_for_auth(execution_id, auth)?;
        if execution.is_terminal() {
            return Ok(());
        }
        if task.run_id != execution.run_id {
            return Err(Self::execution_task_not_found());
        }
        let current = self
            .db
            .latest_connector_execution(
                &task.task_id,
                &self.context.project_id,
                &task.owner_subject_id,
                None,
            )
            .map_err(|error| store_error_outcome(error, Some(&task)))?;
        if current
            .as_ref()
            .is_none_or(|current| current.execution_id != execution.execution_id)
        {
            return Err(Self::execution_task_not_found());
        }
        let host = auth.host.clone();
        let runner_access = auth.access.runner_access.clone();
        self.executions
            .cancel_task(task.clone(), None, host, runner_access)
            .await
            .map(|_| ())
            .map_err(|error| store_error_outcome(error, Some(&task)))
    }

    pub async fn call_for_window(
        &self,
        capability: &str,
        arguments: Value,
        auth: Option<&ConnectorCallContext>,
        transport: ConnectorTransport,
        window: Option<&ConnectorWindowId>,
    ) -> ConnectorCallOutcome {
        self.call_for_window_inner(capability, arguments, auth, transport, window, false)
            .await
    }

    pub async fn call_for_window_with_task_polling(
        &self,
        capability: &str,
        arguments: Value,
        auth: Option<&ConnectorCallContext>,
        transport: ConnectorTransport,
        window: Option<&ConnectorWindowId>,
    ) -> ConnectorCallOutcome {
        self.call_for_window_inner(capability, arguments, auth, transport, window, true)
            .await
    }

    async fn call_for_window_inner(
        &self,
        capability: &str,
        arguments: Value,
        auth: Option<&ConnectorCallContext>,
        transport: ConnectorTransport,
        window: Option<&ConnectorWindowId>,
        defer_execution_guidance: bool,
    ) -> ConnectorCallOutcome {
        if surface::capability_spec(capability).is_none() {
            return ConnectorCallOutcome::error(
                400,
                "unknown_capability",
                format!(
                    "'{capability}' is not available in the project connector; use one of: {}",
                    surface::CAPABILITY_NAMES.join(", ")
                ),
                false,
                false,
                Some("Call task_start first, then use the returned task_id."),
                None,
                true,
            );
        }

        let Some(auth) = auth else {
            return ConnectorCallOutcome::error(
                401,
                "authentication_required",
                "connector capabilities require an authenticated identity",
                false,
                true,
                Some("Configure Bearer authentication in the connector client."),
                None,
                false,
            );
        };
        let access = &auth.access;
        if !access.project_access_allowed(&self.context.project_grant_id) {
            return ConnectorCallOutcome::error(
                403,
                "project_credential_rejected",
                "the authenticated credential is not authorized for this project",
                false,
                true,
                Some("Use the credential generated by setup for this project."),
                None,
                false,
            );
        }
        let required_permission = ConnectorPermission::for_capability(capability);
        if !access.allows(required_permission) {
            return ConnectorCallOutcome::scope_denied(required_permission);
        }
        let subject_id = access.principal.as_str().to_string();

        let now = chrono::Utc::now().timestamp();
        if let Err(error) = self.db.ensure_connector_binding(ConnectorBinding {
            project_id: &self.context.project_id,
            project_name: &self.context.project_name,
            workspace_id: &self.context.workspace_id,
            executor_ref: &self.context.executor_project,
            subject_id: &subject_id,
            profile: &self.context.profile,
            now,
        }) {
            return store_error_outcome(error, None);
        }

        // Read operations coordinate with lifecycle transitions, while every
        // mutation/reservation method owns its narrower task-lock boundary.
        let task_lock = if matches!(
            capability,
            "files_read" | "files_search" | "code_navigate" | "code_impact"
        ) {
            arguments
                .get("task_id")
                .and_then(Value::as_str)
                .map(|task_id| self.task_lock(task_id))
        } else {
            None
        };
        let _task_guard = match task_lock.as_ref() {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        let outcome = match capability {
            "task_start" => {
                self.task_start(arguments, &subject_id, auth, transport, window, now)
                    .await
            }
            "task_list" => self.task_list(arguments, &subject_id).await,
            "task_resume" => self.task_resume(arguments, &subject_id, window, now).await,
            "files_list" => {
                self.files_list(arguments, &subject_id, auth, transport, now)
                    .await
            }
            "files_read" => {
                self.files_read(arguments, &subject_id, auth, transport, now)
                    .await
            }
            "files_search" => {
                self.files_search(arguments, &subject_id, auth, transport, now)
                    .await
            }
            "code_navigate" => {
                self.code_navigate(arguments, &subject_id, auth, transport, now)
                    .await
            }
            "code_impact" => {
                self.code_impact(arguments, &subject_id, auth, transport, now)
                    .await
            }
            "edits_apply" => {
                self.edits_apply(arguments, &subject_id, auth, transport, now)
                    .await
            }
            "checks_run" => {
                self.checks_run(
                    arguments,
                    &subject_id,
                    auth,
                    transport,
                    now,
                    defer_execution_guidance,
                )
                .await
            }
            "commands_run" => {
                self.commands_run(
                    arguments,
                    &subject_id,
                    auth,
                    transport,
                    now,
                    defer_execution_guidance,
                )
                .await
            }
            "task_review" => {
                self.task_review(arguments, &subject_id, auth, transport, true)
                    .await
            }
            "task_cancel" => self.task_cancel(arguments, &subject_id, auth).await,
            "task_finish" => {
                self.task_finish(arguments, &subject_id, auth, transport, now)
                    .await
            }
            _ => unreachable!("capability registry checked before dispatch"),
        };
        outcome
    }

    pub(in crate::runtime) fn task_lock(&self, task_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.task_locks.lock().unwrap();
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(task_id).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(task_id.to_string(), Arc::downgrade(&lock));
        lock
    }

    fn context_lock(&self, subject_id: &str, window_key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let key = format!("{subject_id}:{window_key}");
        let mut locks = self.context_locks.lock().unwrap();
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        locks.retain(|_, lock| lock.strong_count() > 0);
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    pub(in crate::runtime) async fn workspace_fingerprint(
        &self,
        task: &ConnectorTaskSnapshot,
        capability: &'static str,
    ) -> Result<String, ConnectorCallOutcome> {
        let manager = self.workspace.clone();
        let task_for_fingerprint = task.clone();
        match tokio::task::spawn_blocking(move || {
            manager.action_precondition(&task_for_fingerprint)
        })
        .await
        {
            Ok(Ok(fingerprint)) => Ok(fingerprint),
            Ok(Err(message)) => Err(ConnectorCallOutcome::error_for_task(
                409,
                "workspace_fingerprint_failed",
                self.sanitize_task_string(task, &message),
                false,
                true,
                Some("Resolve the Git workspace issue, then retry the operation."),
                task,
                Value::Null,
            )),
            Err(error) => {
                tracing::error!(error = %error, capability, "connector workspace fingerprint task failed");
                Err(ConnectorCallOutcome::error_for_task(
                    500,
                    "workspace_fingerprint_failed",
                    "connector could not fingerprint the current workspace",
                    false,
                    true,
                    Some("Inspect server logs before retrying the operation."),
                    task,
                    Value::Null,
                ))
            }
        }
    }

    async fn task_start(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        _transport: ConnectorTransport,
        window: Option<&ConnectorWindowId>,
        now: i64,
    ) -> ConnectorCallOutcome {
        if arguments.get("mode").and_then(Value::as_str) == Some("inspect") {
            return ConnectorCallOutcome::error(
                400,
                "inspect_mode_retired",
                "inspect mode was retired before v0.4 and is no longer executable",
                false,
                true,
                Some("Use read_only for analysis, or normal for writable work, command execution, and validation."),
                None,
                true,
            );
        }
        let input: TaskStartInput = match parse_input("task_start", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        let goal = input.goal.trim();
        if goal.is_empty() || goal.len() > 4000 {
            return invalid_input("task_start", "goal must be 1..=4000 bytes");
        }
        let mode = input.mode.as_str();
        if mode == "normal" && !auth.access.allows(ConnectorPermission::ProjectWrite) {
            return ConnectorCallOutcome::scope_denied(ConnectorPermission::ProjectWrite);
        }

        let normalized_target =
            match webcodex_workspace::project_overview::normalize_project_overview_path(
                input.target_path.as_deref().unwrap_or(""),
            ) {
                Ok(path) => path,
                Err(message) => return invalid_input("task_start", message),
            };
        let fingerprint = match self
            .capture_connector_context(normalized_target.clone())
            .await
        {
            Ok(fingerprint) => fingerprint,
            Err(outcome) => return outcome,
        };

        // Serialize get-or-create for one stable window/project. Without this,
        // two simultaneous first turns could both observe no mapping and
        // create duplicate durable tasks.
        let context_lock = window.map(|window| self.context_lock(subject_id, window.key()));
        let _context_guard = match context_lock.as_ref() {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        let project_identity = format!(
            "{}:{}",
            self.context.project_id, fingerprint.project_root_sha256
        );
        let existing_context = if let Some(window) = window {
            match self.db.connector_window_context(
                window.key(),
                &self.context.project_id,
                subject_id,
                &fingerprint.project_root_sha256,
            ) {
                Ok(context) => context,
                Err(error) => return store_error_outcome(error, None),
            }
        } else {
            None
        };

        if let Some(existing_context) = existing_context.as_ref() {
            let existing = match self.db.connector_task(
                &existing_context.task_id,
                &self.context.project_id,
                subject_id,
            ) {
                Ok(task) => Some(task),
                Err(ConnectorTaskStoreError::NotFound) => None,
                Err(error) => return store_error_outcome(error, None),
            };
            if let Some(task) = existing {
                if task.mode == ConnectorTaskMode::InspectLegacy {
                    return Self::retired_inspect_task_outcome(&task);
                }
                if let Some(outcome) = Self::invalid_mode_transition_outcome(&task, mode) {
                    return outcome;
                }
                let refresh =
                    compare_project_context(Some(&existing_context.fingerprint), &fingerprint);
                if task.task_status == ConnectorTaskState::Active
                    && task.run_status == ConnectorRunState::Running
                {
                    return self
                        .continue_window_task(
                            task,
                            goal,
                            mode,
                            auth,
                            window.expect("existing window context has a window"),
                            &fingerprint,
                            &refresh,
                            now,
                        )
                        .await;
                }
                if task.run_status == ConnectorRunState::Interrupted
                    && task.task_status == ConnectorTaskState::NeedsAttention
                {
                    let window = window.expect("existing window context has a window");
                    let cursor = match self.db.append_interrupted_connector_instruction_and_bind(
                        &task.task_id,
                        &self.context.project_id,
                        subject_id,
                        goal,
                        mode,
                        now,
                        connector_window_binding(&window.clone(), &fingerprint, now),
                    ) {
                        Ok(cursor) => cursor,
                        Err(error) => return store_error_outcome(error, Some(&task)),
                    };
                    let navigation = self.db.activate_window_project(
                        subject_id,
                        window.key(),
                        &project_identity,
                    );
                    return ConnectorCallOutcome::error_for_task_at(
                        409,
                        "task_interrupted",
                        "the previous project context was recovered, but its execution was interrupted and cannot be resumed by a chat request",
                        false,
                        true,
                        Some("Review the task, then resume or reject it from the WebCodex host."),
                        &task,
                        cursor,
                        json!({
                            "continuation": "recovered",
                            "instruction_appended": true,
                            "context": context_refresh_payload(&refresh),
                            "project_switch": navigation_payload(Some(&navigation), true),
                            "local_command": format!("webcodex task resume {}", task.task_id)
                        }),
                    );
                }
                // A reviewed/closed task remains durable history. The mapping
                // may advance to a new task without deleting the old row.
            }
        }

        let task_id = format!("wc_task_{}", uuid::Uuid::new_v4().simple());
        let run_id = format!("wc_run_{}", uuid::Uuid::new_v4().simple());
        let non_writable = mode != "normal";
        let prepared = match self
            .prepare_connector_workspace(&task_id, &run_id, non_writable, auth)
            .await
        {
            Ok(prepared) => prepared,
            Err(outcome) => return outcome,
        };
        let new_task = NewConnectorTask {
            task_id: &task_id,
            run_id: &run_id,
            project_id: &self.context.project_id,
            workspace_id: &self.context.workspace_id,
            subject_id,
            goal,
            mode,
            target_executor_ref: &self.context.executor_project,
            execution_executor_ref: &prepared.execution_executor_ref,
            target_root: &self.context.executor_root,
            execution_root: &prepared.execution_root,
            baseline_commit: prepared.baseline_commit.as_deref(),
            baseline_tree: prepared.baseline_tree.as_deref(),
            isolated: prepared.isolated,
            now,
        };
        let stored = match window {
            Some(window) => {
                let window = window.clone();
                self.db.start_connector_task_and_bind(
                    new_task,
                    connector_window_binding(&window, &fingerprint, now),
                )
            }
            None => self.db.start_connector_task(new_task),
        };
        let task = match stored {
            Ok(task) => task,
            Err(error) => {
                if let Some(cleanup) = self
                    .workspace
                    .discard_prepared(&self.context.executor_root, &prepared)
                {
                    tracing::warn!(cleanup = %cleanup, "failed to fully clean unpersisted workspace");
                }
                return store_error_outcome(error, None);
            }
        };
        let navigation = window.map(|window| {
            self.db
                .activate_window_project(subject_id, window.key(), &project_identity)
        });
        let brief = project_brief(
            &task,
            prepared.project_overview.as_ref(),
            prepared.git_dirty,
            prepared.git_conflict_count,
        );
        ConnectorCallOutcome::success(
            &task,
            json!({
                "project": {
                    "id": self.context.project_id,
                    "name": self.context.project_name
                },
                "goal": goal,
                "mode": mode,
                "status": task.task_status,
                "continuation": "created",
                "instruction_appended": true,
                "history": {
                    "preserved": true,
                    "event_cursor_before": 0,
                    "event_cursor_after": task.event_cursor
                },
                "context": context_refresh_payload(&compare_project_context(None, &fingerprint)),
                "project_switch": navigation_payload(navigation.as_ref(), false),
                "brief": brief,
                "next": "Use the brief to choose the first targeted read; edit with returned sha256 guards, validate, review, and finish."
            }),
        )
    }

    async fn capture_connector_context(
        &self,
        target_path: String,
    ) -> Result<ProjectContextFingerprint, ConnectorCallOutcome> {
        let root = self.context.executor_root.clone();
        match tokio::task::spawn_blocking(move || {
            capture_project_context(Path::new(&root), Some(&target_path))
        })
        .await
        {
            Ok(Ok(fingerprint)) => Ok(fingerprint),
            Ok(Err(message)) => Err(ConnectorCallOutcome::error(
                409,
                "project_context_unavailable",
                self.sanitize_executor_string(&message),
                false,
                true,
                Some("Resolve the repository path or Git state, then retry the instruction."),
                None,
                false,
            )),
            Err(error) => {
                tracing::error!(error = %error, "connector context fingerprint task failed");
                Err(ConnectorCallOutcome::error(
                    500,
                    "project_context_unavailable",
                    "connector could not fingerprint the project context",
                    false,
                    true,
                    Some("Inspect server logs before retrying the instruction."),
                    None,
                    false,
                ))
            }
        }
    }

    async fn prepare_connector_workspace(
        &self,
        task_id: &str,
        run_id: &str,
        non_writable: bool,
        auth: &ConnectorCallContext,
    ) -> Result<PreparedWorkspace, ConnectorCallOutcome> {
        let _workspace_guard = if non_writable {
            None
        } else {
            Some(self.workspace_ops.lock().await)
        };
        let manager = self.workspace.clone();
        let context = self.context.clone();
        let task_for_prepare = task_id.to_string();
        let run_for_prepare = run_id.to_string();
        let prepared = match tokio::task::spawn_blocking(move || {
            manager.prepare(&context, &task_for_prepare, &run_for_prepare, non_writable)
        })
        .await
        {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => {
                let guidance = if error.reason_code == "writable_slot_occupied" {
                    "Finish, resume, or reject the task occupying the writable slot."
                } else {
                    "Resolve the reported Git/private-state issue; normal mode never falls back to the target checkout."
                };
                return Err(ConnectorCallOutcome::error_with_data(
                    409,
                    "workspace_preparation_failed",
                    error.message,
                    false,
                    true,
                    Some(guidance),
                    json!({
                        "stage": error.stage,
                        "reason_code": error.reason_code,
                    }),
                    None,
                    false,
                ));
            }
            Err(error) => {
                tracing::error!(error = %error, "connector workspace preparation task failed");
                return Err(ConnectorCallOutcome::error(
                    500,
                    "workspace_preparation_failed",
                    "connector could not prepare the isolated execution workspace",
                    false,
                    true,
                    Some("Inspect server logs, then retry the instruction."),
                    None,
                    false,
                ));
            }
        };
        if prepared.isolated {
            let host = auth.host.clone();
            let registration = host
                .register_isolated_project(ConnectorProjectRegistration {
                    client_id: prepared.agent_client_id.clone(),
                    project_id: prepared.agent_project_id.clone(),
                    name: format!("WebCodex {}", prepared.agent_project_id),
                    path: prepared.execution_root.clone(),
                    description: Some("WebCodex managed isolated task worktree".to_string()),
                })
                .await;
            if let Err(registration_error) = registration {
                let cleanup = self
                    .workspace
                    .discard_prepared(&self.context.executor_root, &prepared);
                let registration_message = match registration_error {
                    ConnectorJobHostError::Rejected(message)
                    | ConnectorJobHostError::OutcomeUnknown(message) => message,
                    ConnectorJobHostError::Adapter(message) => Some(message),
                };
                if let Some(error) = registration_message.as_deref() {
                    tracing::warn!(
                        error = %self.sanitize_executor_string(error),
                        "temporary Runner project registration failed"
                    );
                }
                if let Some(cleanup) = cleanup {
                    tracing::warn!(cleanup = %cleanup, "failed to fully clean rejected workspace preparation");
                }
                return Err(ConnectorCallOutcome::error_with_data(
                    409,
                    "workspace_preparation_failed",
                    "the isolated writable workspace could not be registered with the Runner",
                    false,
                    true,
                    Some("Resolve the Runner project-registration policy, then retry; the target checkout was not used as a writable fallback."),
                    json!({
                        "stage": "runner_project_registration",
                        "reason_code": "runner_project_registration_failed",
                    }),
                    None,
                    false,
                ));
            }
        }
        Ok(prepared)
    }

    #[allow(clippy::too_many_arguments)]
    async fn continue_window_task(
        &self,
        task: ConnectorTaskSnapshot,
        instruction: &str,
        mode: &str,
        auth: &ConnectorCallContext,
        window: &ConnectorWindowId,
        fingerprint: &ProjectContextFingerprint,
        refresh: &ContextRefreshSummary,
        now: i64,
    ) -> ConnectorCallOutcome {
        let event_cursor_before = task.event_cursor;
        if task.mode == ConnectorTaskMode::InspectLegacy {
            return Self::retired_inspect_task_outcome(&task);
        }
        if let Some(outcome) = Self::invalid_mode_transition_outcome(&task, mode) {
            return outcome;
        }
        let prepared = if mode == "normal" && !task.isolated {
            match self
                .prepare_connector_workspace(&task.task_id, &task.run_id, false, auth)
                .await
            {
                Ok(prepared) => Some(prepared),
                Err(outcome) => return outcome,
            }
        } else {
            None
        };
        let workspace = prepared
            .as_ref()
            .map(|prepared| ConnectorWorkspaceTransition {
                target_executor_ref: &self.context.executor_project,
                execution_executor_ref: &prepared.execution_executor_ref,
                target_root: &self.context.executor_root,
                execution_root: &prepared.execution_root,
                baseline_commit: prepared.baseline_commit.as_deref().unwrap_or_default(),
                baseline_tree: prepared.baseline_tree.as_deref().unwrap_or_default(),
            });
        let (continued, cursor, previous_mode) = match self.db.continue_connector_task_and_bind(
            ConnectorTaskContinuation {
                task_id: &task.task_id,
                project_id: &self.context.project_id,
                subject_id: &task.owner_subject_id,
                instruction,
                mode,
                workspace,
                now,
            },
            connector_window_binding(&window.clone(), fingerprint, now),
        ) {
            Ok(continued) => continued,
            Err(error) => {
                if let Some(prepared) = prepared.as_ref() {
                    if let Some(cleanup) = self
                        .workspace
                        .discard_prepared(&self.context.executor_root, prepared)
                    {
                        tracing::warn!(cleanup = %cleanup, "failed to fully clean rejected workspace upgrade");
                    }
                }
                return store_error_outcome(error, Some(&task));
            }
        };
        let navigation = self.db.activate_window_project(
            &continued.owner_subject_id,
            window.key(),
            &format!(
                "{}:{}",
                self.context.project_id, fingerprint.project_root_sha256
            ),
        );
        let brief = match prepared.as_ref() {
            Some(prepared) => project_brief(
                &continued,
                prepared.project_overview.as_ref(),
                prepared.git_dirty,
                prepared.git_conflict_count,
            ),
            None => project_brief_from_fingerprint(&continued, fingerprint),
        };
        ConnectorCallOutcome::success_at(
            &continued,
            cursor,
            json!({
                "project": {
                    "id": self.context.project_id,
                    "name": self.context.project_name
                },
                "goal": continued.goal,
                "instruction": instruction,
                "mode": continued.mode,
                "status": continued.task_status,
                "continuation": "continued",
                "instruction_appended": true,
                "history": {
                    "preserved": true,
                    "event_cursor_before": event_cursor_before,
                    "event_cursor_after": cursor
                },
                "capability": {
                    "changed": previous_mode != continued.mode,
                    "previous_mode": previous_mode,
                    "mode": continued.mode,
                    "write_scope_verified": mode == "normal",
                    "workspace_upgraded": prepared.is_some()
                },
                "context": context_refresh_payload(refresh),
                "project_switch": navigation_payload(Some(&navigation), true),
                "brief": brief,
                "next": "Continue from the preserved history; read only context reported as refreshed before editing or validating."
            }),
        )
    }

    pub(in crate::runtime) fn persist_window_context(
        &self,
        window: &ConnectorWindowId,
        subject_id: &str,
        task_id: &str,
        fingerprint: &ProjectContextFingerprint,
        now: i64,
    ) -> Result<(), ConnectorTaskStoreError> {
        self.db.bind_connector_window_context(
            window.key(),
            window.source(),
            &self.context.project_id,
            subject_id,
            &fingerprint.project_root_sha256,
            task_id,
            &fingerprint.target_directory,
            fingerprint,
            now,
        )
    }

    fn sanitize_executor_string(&self, value: &str) -> String {
        value
            .replace(&self.context.executor_project, &self.context.project_id)
            .replace(&self.context.executor_root, ".")
            .replace(&self.context.runs_root, "<managed-runs>")
            .replace(&self.context.results_root, "<managed-results>")
            .replace(
                &self.context.project_registry_dir,
                "<managed-project-registry>",
            )
    }
}
