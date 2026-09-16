//! Connector durable task lifecycle and review operations.

use super::ConnectorRuntime;
use crate::execution;
use crate::projections::{
    bounded_goal, checks_stale_outcome, context_refresh_payload, durable_task_review_projection,
    invalid_input, model_next_action, navigation_payload, parse_input, store_error_outcome,
    validate_task_id, validation_projection, DEFAULT_TASK_LIST_LIMIT, MAX_TASK_LIST_LIMIT,
};
use crate::wire_models::{
    TaskCancelInput, TaskFinishInput, TaskListInput, TaskResumeInput, TaskReviewInput,
};
use crate::workspace::WorkspaceManager;
use crate::{ConnectorCallContext, ConnectorCallOutcome, ConnectorTransport, ConnectorWindowId};
use serde_json::{json, Value};
use std::path::Path;
use webcodex_store::{
    ConnectorExecution, ConnectorExecutionState, ConnectorResultDecisionStatus, ConnectorRunState,
    ConnectorTaskMode, ConnectorTaskSnapshot, ConnectorTaskState, NewConnectorResult,
};
use webcodex_workspace::project_context::compare_project_context;

const MAX_EVENT_COUNT: usize = 50;
const MAX_GUIDANCE_PER_RESPONSE: usize = 16;
const MAX_REVIEW_APPLIED_PATHS: usize = 200;
const CONNECTOR_PATCH_PREVIEW_BYTES: usize = 128 * 1024;

impl ConnectorRuntime {
    pub(in crate::runtime) async fn task_review(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
        deliver_guidance: bool,
    ) -> ConnectorCallOutcome {
        let input: TaskReviewInput = match parse_input("task_review", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if input.after_cursor.is_some_and(|cursor| cursor < 0) {
            return invalid_input("task_review", "after_cursor must be non-negative");
        }
        if input.wait_ms.is_some_and(|wait| wait > 15_000) {
            return invalid_input("task_review", "wait_ms must be 0..=15000");
        }
        if input
            .max_events
            .is_some_and(|count| count == 0 || count > MAX_EVENT_COUNT)
        {
            return invalid_input("task_review", "max_events must be 1..=50");
        }
        let initial_task = match self.task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let review = match self
            .executions
            .wait_for_review(initial_task, input.after_cursor, input.wait_ms.unwrap_or(0))
            .await
        {
            Ok(review) => review,
            Err(error) => return store_error_outcome(error, None),
        };
        let task = review.task;
        let result =
            match self
                .db
                .connector_task_result(&task.task_id, &self.context.project_id, subject_id)
            {
                Ok(result) => result,
                Err(error) => return store_error_outcome(error, Some(&task)),
            };
        let changes = if let Some(result) = result.as_ref() {
            let diff_preview = if input.include_diff.unwrap_or(false) {
                match WorkspaceManager::patch_preview(result, CONNECTOR_PATCH_PREVIEW_BYTES) {
                    Ok(preview) => preview,
                    Err(message) => {
                        return ConnectorCallOutcome::error_for_task(
                            409,
                            "result_artifact_unavailable",
                            self.sanitize_task_string(&task, &message),
                            false,
                            true,
                            Some("Inspect the local task state before accepting this result."),
                            &task,
                            Value::Null,
                        )
                    }
                }
            } else {
                None
            };
            self.sanitize_task_value(
                &task,
                json!({
                    "source": "stable_task_result",
                    "patch_sha256": result.patch_sha256,
                    "patch_bytes": result.patch_bytes,
                    "changed_paths": result.changed_paths,
                    "warnings": result.warnings,
                    "diff_preview": diff_preview
                }),
            )
        } else if task.task_status == ConnectorTaskState::Cancelled {
            json!({
                "source": "cancelled_task",
                "changed_paths": [],
                "diff_preview": null
            })
        } else if review
            .execution
            .as_ref()
            .is_some_and(ConnectorExecution::is_active)
        {
            // The diff stays deferred while a command runs — a synchronous
            // workspace scan here would stall the review long-poll behind the
            // executor. The paths this task has applied are already durable
            // facts in its event log, so surface those instead of going dark.
            // Queried straight from the applied-edit events rather than
            // filtered out of the recent timeline, so a path applied early in a
            // long task is still reported.
            let applied = match self.db.connector_task_applied_paths(
                &task.task_id,
                &task.project_id,
                &task.owner_subject_id,
                MAX_REVIEW_APPLIED_PATHS,
            ) {
                Ok(applied) => applied,
                Err(error) => return store_error_outcome(error, Some(&task)),
            };
            json!({
                "source": "live_workspace_deferred",
                "reason": "execution_active",
                "changed_paths": applied.paths,
                "changed_paths_source": "applied_edits",
                "changed_paths_complete": applied.complete,
                "changed_paths_total": applied.total,
                "diff_preview": null
            })
        } else {
            match self
                .invoke_kernel(
                    "show_changes",
                    json!({
                        "project": task.execution_executor_ref,
                        "include_diff": input.include_diff.unwrap_or(false),
                        "max_hunks": 20,
                        "max_hunk_lines": 80,
                        "session_event_limit": 0
                    }),
                    &task,
                    auth,
                    transport,
                )
                .await
            {
                Ok(output) => output,
                Err(_) => {
                    // Never blind the reviewer because the workspace scan
                    // failed (e.g. the slot is wedged after a terminal
                    // failure): degrade to the durable applied-path record
                    // instead of failing the whole review.
                    // Same durable source the active-execution branch uses, so
                    // a path applied early in a long task is still reported and
                    // a bounded list never claims to be the whole set. A
                    // lookup that also fails reports nothing rather than
                    // implying the task changed nothing.
                    let applied = self.db.connector_task_applied_paths(
                        &task.task_id,
                        &task.project_id,
                        &task.owner_subject_id,
                        MAX_REVIEW_APPLIED_PATHS,
                    );
                    let (paths, total, complete) = match applied {
                        Ok(applied) => (applied.paths, applied.total, applied.complete),
                        Err(_) => (Vec::new(), 0, false),
                    };
                    json!({
                        "source": "workspace_scan_failed",
                        "changed_paths": paths,
                        "changed_paths_source": "applied_edits",
                        "changed_paths_complete": complete,
                        "changed_paths_total": total,
                        "diff_preview": null
                    })
                }
            }
        };
        let events = match self.db.connector_task_events(
            &task.task_id,
            &task.project_id,
            &task.owner_subject_id,
            MAX_EVENT_COUNT,
        ) {
            Ok(events) => events,
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        let max_events = input.max_events.unwrap_or(MAX_EVENT_COUNT);
        let mut events = events
            .into_iter()
            .filter(|event| {
                input
                    .after_cursor
                    .is_none_or(|cursor| event.sequence > cursor)
            })
            .collect::<Vec<_>>();
        events.drain(..events.len().saturating_sub(max_events));
        let execution = match review.execution.as_ref() {
            Some(execution) => {
                let runner_access = auth.access.runner_access.clone();
                Some(
                    self.executions
                        .projection(
                            execution,
                            &runner_access,
                            input.include_output_tail.unwrap_or(false),
                        )
                        .await,
                )
            }
            None => None,
        };
        let blocking = review
            .execution
            .as_ref()
            .is_some_and(|execution| execution.blocks_finish());
        let next_action = execution
            .as_ref()
            .and_then(|value| value["next_action"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if task.task_status == ConnectorTaskState::Cancelled {
                    "start_a_new_task"
                } else if task.run_status == ConnectorRunState::Interrupted {
                    "resume_or_reject_on_the_host"
                } else {
                    "continue_or_finish"
                }
                .to_string()
            });
        let mut data = durable_task_review_projection(&task, result.as_ref());
        data["changes"] = changes;
        data["active_execution"] = execution
            .as_ref()
            .filter(|_| {
                review
                    .execution
                    .as_ref()
                    .is_some_and(ConnectorExecution::is_active)
            })
            .cloned()
            .unwrap_or(Value::Null);
        data["recent_execution"] = execution.unwrap_or(Value::Null);
        data["recent_events"] = json!(events);
        data["heartbeat"] = json!(review.heartbeat);
        data["next_action"] = json!(next_action);
        if deliver_guidance {
            self.attach_pending_guidance(&task, &mut data);
        }
        ConnectorCallOutcome::success_blocking_at(&task, task.event_cursor, data, blocking)
    }

    /// Recovery/diagnostic listing for durable tasks this credential may
    /// continue, most actionable first. Ordinary same-window continuation is
    /// resolved by task_start without duplicating the durable context.
    pub(in crate::runtime) async fn task_list(
        &self,
        arguments: Value,
        subject_id: &str,
    ) -> ConnectorCallOutcome {
        let input: TaskListInput = match parse_input("task_list", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        let limit = input.limit.unwrap_or(DEFAULT_TASK_LIST_LIMIT);
        if !(1..=MAX_TASK_LIST_LIMIT).contains(&limit) {
            return invalid_input("task_list", "limit must be 1..=20");
        }
        let tasks =
            match self
                .db
                .connector_tasks_for_subject(&self.context.project_id, subject_id, limit)
            {
                Ok(tasks) => tasks,
                Err(error) => return store_error_outcome(error, None),
            };
        let items: Vec<Value> = tasks
            .iter()
            .map(|task| {
                json!({
                    "task_id": task.task_id,
                    "goal": bounded_goal(&task.goal),
                    "task_status": task.task_status,
                    "updated_at": task.updated_at,
                    "execution_status": task.execution_status,
                    "validation_status": task.validation_status,
                    "next_action": model_next_action(task.task_status, &task.next_action),
                })
            })
            .collect();
        ConnectorCallOutcome::success_project(json!({
            "tasks": items,
            "count": items.len(),
            "note": "Ordinary work starts or continues with task_start. Use task_resume only to recover a specific durable task after window identity was lost."
        }))
    }

    /// Explicit recovery for a task whose automatic window binding is no
    /// longer available. Claims pending human guidance exactly like
    /// task_review does.
    pub(in crate::runtime) async fn task_resume(
        &self,
        arguments: Value,
        subject_id: &str,
        window: Option<&ConnectorWindowId>,
        now: i64,
    ) -> ConnectorCallOutcome {
        let input: TaskResumeInput = match parse_input("task_resume", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        let task = match self.task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        if task.mode == ConnectorTaskMode::InspectLegacy {
            return Self::retired_inspect_task_outcome(&task);
        }
        let context_lock = window.map(|window| self.context_lock(subject_id, window.key()));
        let _context_guard = match context_lock.as_ref() {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        let result =
            match self
                .db
                .connector_task_result(&task.task_id, &self.context.project_id, subject_id)
            {
                Ok(result) => result,
                Err(error) => return store_error_outcome(error, Some(&task)),
            };
        let applied = match self.db.connector_task_applied_paths(
            &task.task_id,
            &task.project_id,
            &task.owner_subject_id,
            MAX_REVIEW_APPLIED_PATHS,
        ) {
            Ok(applied) => applied,
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        let execution = match self.db.latest_connector_execution(
            &task.task_id,
            &self.context.project_id,
            subject_id,
            None,
        ) {
            Ok(execution) => execution,
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        let execution_value =
            execution.map(|execution| execution::execution_projection(&execution, now, None));
        // A local decision outranks stale execution advice: an accepted or
        // rejected result decides the story, whatever the last run said.
        let decision_action = result.as_ref().and_then(|result| match result.decision_status {
            ConnectorResultDecisionStatus::Accepted => {
                Some("the result was accepted locally; start the next piece of work with task_start")
            }
            ConnectorResultDecisionStatus::Rejected => Some(
                "the result was rejected; apply the guidance and start a corrected task with task_start",
            ),
            ConnectorResultDecisionStatus::Pending => None,
        });
        let next_action = decision_action
            .map(str::to_string)
            .or_else(|| {
                execution_value
                    .as_ref()
                    .and_then(|value| value["next_action"].as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| {
                if result.is_some() {
                    "task_review, then ask the project owner to accept or reject locally"
                } else if task.task_status == ConnectorTaskState::Cancelled {
                    "start_a_new_task"
                } else if task.run_status == ConnectorRunState::Interrupted {
                    "ask the project owner to resume or reject this task on the host"
                } else {
                    "continue with files_read/edits_apply, then task_review"
                }
                .to_string()
            });
        let result_value = result
            .as_ref()
            .map(|result| {
                json!({
                    "result_id": result.result_id,
                    "summary": result.summary,
                    "changed_paths": result.changed_paths,
                    "patch_bytes": result.patch_bytes,
                    "decision_status": result.decision_status,
                })
            })
            .unwrap_or(Value::Null);
        let continuity = if let Some(window) = window {
            let previous = match self.db.connector_window_context_for_task(
                &task.task_id,
                &self.context.project_id,
                subject_id,
            ) {
                Ok(previous) => previous,
                Err(error) => return store_error_outcome(error, Some(&task)),
            };
            let target_path = previous
                .as_ref()
                .map(|context| context.target_path.clone())
                .unwrap_or_default();
            let fingerprint = match self.capture_connector_context(target_path).await {
                Ok(fingerprint) => fingerprint,
                Err(outcome) => return outcome,
            };
            if previous.as_ref().is_some_and(|context| {
                context.fingerprint.project_root_sha256 != fingerprint.project_root_sha256
            }) {
                return ConnectorCallOutcome::error_for_task(
                    409,
                    "project_context_mismatch",
                    "the durable task repository identity no longer matches the configured path",
                    false,
                    true,
                    Some("Recover the task only after restoring its original repository identity."),
                    &task,
                    json!({ "window_rebound": false }),
                );
            }
            let refresh = compare_project_context(
                previous.as_ref().map(|context| &context.fingerprint),
                &fingerprint,
            );
            if let Err(error) =
                self.persist_window_context(window, subject_id, &task.task_id, &fingerprint, now)
            {
                return store_error_outcome(error, Some(&task));
            }
            let navigation = self.db.activate_window_project(
                subject_id,
                window.key(),
                &format!(
                    "{}:{}",
                    self.context.project_id, fingerprint.project_root_sha256
                ),
            );
            json!({
                "window_rebound": true,
                "context": context_refresh_payload(&refresh),
                "project_switch": navigation_payload(Some(&navigation), true)
            })
        } else {
            json!({
                "window_rebound": false,
                "recovery_boundary": "no stable transport window identity was available"
            })
        };
        let mut data = json!({
            "goal": task.goal,
            "mode": task.mode,
            "task_status": task.task_status,
            "run_status": task.run_status,
            "isolated": task.isolated,
            "created_at": task.created_at,
            "updated_at": task.updated_at,
            "result": result_value,
            "applied_paths": applied.paths,
            "applied_paths_total": applied.total,
            "applied_paths_complete": applied.complete,
            "recent_execution": execution_value.unwrap_or(Value::Null),
            "next_action": next_action,
            "continuity": continuity,
            "resume_note": "This window is now the task's continuation when a stable transport identity was available. Trust this bootstrap over assumptions from earlier windows, and apply any guidance below before acting."
        });
        self.attach_pending_guidance(&task, &mut data);
        // Timeline visibility for the console; terminal tasks skip the
        // running-only event guard on purpose, and a failed advisory event
        // must not fail the bootstrap.
        let cursor = if task.task_status == ConnectorTaskState::Active
            && task.run_status == ConnectorRunState::Running
        {
            match self.record_event(
                &task,
                "task_resume",
                json!({ "window_rebound": window.is_some() }),
                now,
            ) {
                Ok(cursor) => cursor,
                Err(_) => task.event_cursor,
            }
        } else {
            task.event_cursor
        };
        ConnectorCallOutcome::success_at(&task, cursor, self.sanitize_task_value(&task, data))
    }

    pub(in crate::runtime) async fn task_cancel(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
    ) -> ConnectorCallOutcome {
        let input: TaskCancelInput = match parse_input("task_cancel", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if input
            .reason
            .as_deref()
            .is_some_and(|reason| reason.trim().is_empty() || reason.len() > 500)
        {
            return invalid_input("task_cancel", "reason must be 1..=500 bytes when provided");
        }
        let task_lock = self.task_lock(&input.task_id);
        let _task_guard = task_lock.lock().await;
        let task = match self.task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let host = auth.host.clone();
        let runner_access = auth.access.runner_access.clone();
        let execution = match self
            .executions
            .cancel_task(
                task.clone(),
                input.reason.as_deref(),
                host,
                runner_access.clone(),
            )
            .await
        {
            Ok(execution) => execution,
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        let current = self.task(&task.task_id, subject_id).unwrap_or(task);
        let projection = match execution.as_ref() {
            Some(execution) => Some(
                self.executions
                    .projection(execution, &runner_access, true)
                    .await,
            ),
            None => None,
        };
        let blocking = execution
            .as_ref()
            .is_some_and(|execution| execution.blocks_finish());
        ConnectorCallOutcome::success_blocking_at(
            &current,
            current.event_cursor,
            json!({
                "status": current.task_status,
                "run_status": current.run_status,
                "execution": projection,
                "cancellation": if blocking { "requested" } else { "terminal" },
                "next_action": if blocking {
                    "wait_with_task_review"
                } else {
                    "start_a_new_task_if_more_work_is_needed"
                }
            }),
            blocking,
        )
    }

    pub(in crate::runtime) async fn task_finish(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        _transport: ConnectorTransport,
        now: i64,
    ) -> ConnectorCallOutcome {
        let input: TaskFinishInput = match parse_input("task_finish", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if input.summary.trim().is_empty() || input.summary.len() > 4000 {
            return invalid_input("task_finish", "summary must be 1..=4000 bytes");
        }
        let task_lock = self.task_lock(&input.task_id);
        let task_guard = task_lock.lock().await;
        let visible_task = match self.task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        if visible_task.mode == ConnectorTaskMode::InspectLegacy {
            return Self::retired_inspect_task_outcome(&visible_task);
        }
        if let Some(outcome) = Self::invalid_task_workspace_outcome(&visible_task) {
            return outcome;
        }
        let blocker = match self.db.connector_finish_blocker(&input.task_id) {
            Ok(blocker) => blocker,
            Err(error) => return store_error_outcome(error, Some(&visible_task)),
        };
        if let Some(execution) = blocker {
            let runner_access = auth.access.runner_access.clone();
            let projection = self
                .executions
                .projection(&execution, &runner_access, true)
                .await;
            return ConnectorCallOutcome::error_for_task(
                409,
                "execution_not_terminal",
                "task_finish is blocked until the active execution reaches a known terminal state",
                true,
                execution.state == ConnectorExecutionState::Unknown,
                Some(if execution.state == ConnectorExecutionState::Unknown {
                    "Inspect the executor state on the host before finishing this task."
                } else {
                    "Use task_review to wait for completion or task_cancel to stop the execution."
                }),
                &visible_task,
                json!({ "execution": projection }),
            );
        }
        let task = match self.active_task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let _workspace_guard = if task.isolated {
            Some(self.workspace_ops.lock().await)
        } else {
            None
        };
        let check_execution = match self.db.latest_connector_execution_by_kind(
            &task.task_id,
            &self.context.project_id,
            subject_id,
            "check",
        ) {
            Ok(execution) => execution,
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        if task.isolated && check_execution.is_none() {
            return ConnectorCallOutcome::error_for_task(
                409,
                "checks_required",
                "an isolated writable coding result must run structured checks before task_finish",
                false,
                true,
                Some("Call checks_run with a new operation_id, then retry task_finish."),
                &task,
                json!({}),
            );
        }
        if let Some(check) = check_execution
            .as_ref()
            .filter(|check| check.state == ConnectorExecutionState::Succeeded)
        {
            let Some(validated) = check.validated_workspace_sha256.as_deref() else {
                return checks_stale_outcome(
                    &task,
                    check,
                    "the latest successful check has no trusted workspace provenance",
                );
            };
            let current = match self.workspace_fingerprint(&task, "task_finish").await {
                Ok(current) => current,
                Err(outcome) => return outcome,
            };
            if current != validated {
                return checks_stale_outcome(
                    &task,
                    check,
                    "the workspace changed after the latest successful check",
                );
            }
            #[cfg(test)]
            let finish_hook = { self.finish_after_fingerprint.lock().unwrap().clone() };
            #[cfg(test)]
            if let Some((reached, resume)) = finish_hook {
                reached.notify_one();
                resume.notified().await;
            }
        }
        let manager = self.workspace.clone();
        let task_for_capture = task.clone();
        let captured =
            match tokio::task::spawn_blocking(move || manager.capture_result(&task_for_capture))
                .await
            {
                Ok(Ok(captured)) => captured,
                Ok(Err(message)) => {
                    let cursor = self.record_event(
                        &task,
                        "task_finish",
                        json!({ "ok": false, "stage": "capture_result" }),
                        now,
                    );
                    let cursor = cursor.unwrap_or(task.event_cursor);
                    return ConnectorCallOutcome::error_for_task_at(
                        409,
                        "result_capture_failed",
                        self.sanitize_task_string(&task, &message),
                        false,
                        true,
                        Some("Resolve the reported workspace issue, then retry task_finish."),
                        &task,
                        cursor,
                        Value::Null,
                    );
                }
                Err(error) => {
                    tracing::error!(error = %error, "connector result capture task failed");
                    return ConnectorCallOutcome::error_for_task(
                        500,
                        "result_capture_failed",
                        "connector could not capture a stable task result",
                        false,
                        true,
                        Some("Inspect server logs before retrying task_finish."),
                        &task,
                        Value::Null,
                    );
                }
            };
        let validation = validation_projection(check_execution.as_ref());
        let mut allocation_attempt = 0;
        let (result_id, mut cursor) = loop {
            let result_id = format!(
                "wc_result_{}",
                webcodex_core::compact::random_suffix::<12>()
            );
            let cursor = match self.db.finish_connector_task(
                &task.task_id,
                &self.context.project_id,
                subject_id,
                NewConnectorResult {
                    result_id: &result_id,
                    summary: input.summary.trim(),
                    patch_artifact: captured.patch_artifact.as_deref(),
                    patch_sha256: captured.patch_sha256.as_deref(),
                    patch_bytes: captured.patch_bytes,
                    changed_paths: &captured.changed_paths,
                    validation: &validation,
                    warnings: &captured.warnings,
                },
                now,
            ) {
                Ok(cursor) => cursor,
                Err(error)
                    if error.is_generated_identity_collision() && allocation_attempt < 15 =>
                {
                    allocation_attempt += 1;
                    continue;
                }
                Err(error) => return store_error_outcome(error, Some(&task)),
            };
            break (result_id, cursor);
        };
        drop(task_guard);
        let cleanup_warning = if task.isolated {
            let manager = self.workspace.clone();
            let task_for_release = task.clone();
            match tokio::task::spawn_blocking(move || {
                manager.release_task_workspace(&task_for_release)
            })
            .await
            {
                Ok(warning) => warning,
                Err(error) => {
                    tracing::error!(error = %error, "connector workspace release task failed");
                    Some("connector could not release the reusable execution workspace".to_string())
                }
            }
        } else {
            None
        }
        .map(|warning| self.sanitize_task_string(&task, &warning));
        if task.isolated {
            match self.db.record_connector_workspace_release(
                &task.task_id,
                &self.context.project_id,
                subject_id,
                cleanup_warning.is_none(),
                cleanup_warning.as_deref(),
                now,
            ) {
                Ok(release_cursor) => cursor = release_cursor,
                Err(error) => {
                    tracing::warn!(error = %error, task_id = %task.task_id, "Could not record connector workspace release");
                }
            }
        }
        let workspace_released = !task.isolated || cleanup_warning.is_none();
        ConnectorCallOutcome::success_at(
            &task,
            cursor,
            json!({
                "status": "ready_for_review",
                "run_status": "completed",
                "summary": input.summary.trim(),
                "result": {
                    "result_id": result_id,
                    "patch_sha256": captured.patch_sha256,
                    "patch_bytes": captured.patch_bytes,
                    "changed_paths": captured.changed_paths,
                    "validation": validation,
                    "warnings": captured.warnings,
                    "decision_status": "pending",
                    "cleanup_warning": cleanup_warning.clone()
                },
                "workspace": {
                    "strategy": if task.isolated { "reusable_slot" } else { "target_checkout" },
                    "released": workspace_released
                },
                "human_action": format!(
                    "Run 'webcodex task show {}', then accept or reject the result locally.",
                    task.task_id
                )
            }),
        )
    }

    pub(in crate::runtime) fn task(
        &self,
        task_id: &str,
        subject_id: &str,
    ) -> Result<ConnectorTaskSnapshot, ConnectorCallOutcome> {
        validate_task_id(task_id).map_err(|message| invalid_input("task", message))?;
        let task = self
            .db
            .connector_task(task_id, &self.context.project_id, subject_id)
            .map_err(|error| store_error_outcome(error, None))?;
        match (
            Path::new(&task.target_root).canonicalize(),
            Path::new(&self.context.executor_root).canonicalize(),
        ) {
            (Ok(recorded), Ok(configured)) if recorded == configured => Ok(task),
            (Ok(_), Ok(_)) => Err(ConnectorCallOutcome::error_for_task(
                409,
                "project_context_mismatch",
                "the durable task belongs to a different repository path",
                false,
                true,
                Some("Use the Connector configured for the task's original repository."),
                &task,
                json!({ "window_rebound": false }),
            )),
            _ => Err(ConnectorCallOutcome::error_for_task(
                409,
                "project_context_unavailable",
                "the durable task repository identity cannot be verified",
                false,
                true,
                Some("Restore the configured repository path before continuing this task."),
                &task,
                json!({ "window_rebound": false }),
            )),
        }
    }

    pub(in crate::runtime) fn retired_inspect_task_outcome(
        task: &ConnectorTaskSnapshot,
    ) -> ConnectorCallOutcome {
        ConnectorCallOutcome::error_for_task(
            409,
            "inspect_mode_retired",
            "this pre-0.4 inspect task can no longer execute",
            false,
            true,
            Some("Reject or clean up this legacy task, then start a new read_only task for analysis or a new normal task for writable work."),
            task,
            Value::Null,
        )
    }

    pub(in crate::runtime) fn invalid_mode_transition_outcome(
        task: &ConnectorTaskSnapshot,
        requested_mode: &str,
    ) -> Option<ConnectorCallOutcome> {
        (task.mode == ConnectorTaskMode::Normal && requested_mode == "read_only").then(|| {
            ConnectorCallOutcome::error_for_task(
                409,
                "mode_transition_invalid",
                "a writable normal task cannot transition to read_only",
                false,
                true,
                Some("Finish or reject the current writable task, then start a new read_only task for analysis."),
                task,
                json!({
                    "previous_mode": task.mode,
                    "requested_mode": requested_mode,
                }),
            )
        })
    }

    fn invalid_task_workspace_outcome(
        task: &ConnectorTaskSnapshot,
    ) -> Option<ConnectorCallOutcome> {
        let message = match task.mode {
            ConnectorTaskMode::Normal
                if !task.isolated
                    || task.execution_root == task.target_root
                    || task.baseline_commit.as_deref().is_none_or(str::is_empty)
                    || task.baseline_tree.as_deref().is_none_or(str::is_empty) =>
            {
                "normal task has an invalid isolated writable-workspace state"
            }
            ConnectorTaskMode::ReadOnly
                if task.isolated || task.execution_root != task.target_root =>
            {
                "read_only task has an invalid workspace state"
            }
            ConnectorTaskMode::Normal
            | ConnectorTaskMode::ReadOnly
            | ConnectorTaskMode::InspectLegacy => return None,
        };
        Some(ConnectorCallOutcome::error_for_task(
            409,
            "task_state_invalid",
            message,
            false,
            true,
            Some("Reject or clean up this inconsistent task, then start a new normal or read_only task."),
            task,
            json!({
                "mode": task.mode,
                "isolated": task.isolated,
            }),
        ))
    }

    pub(in crate::runtime) fn active_task(
        &self,
        task_id: &str,
        subject_id: &str,
    ) -> Result<ConnectorTaskSnapshot, ConnectorCallOutcome> {
        let task = self.task(task_id, subject_id)?;
        if task.mode != ConnectorTaskMode::InspectLegacy {
            if let Some(outcome) = Self::invalid_task_workspace_outcome(&task) {
                return Err(outcome);
            }
        }
        if task.run_status == ConnectorRunState::Interrupted {
            return Err(ConnectorCallOutcome::error_for_task(
                409,
                "task_interrupted",
                "this task was interrupted when the local connector runtime stopped",
                false,
                true,
                Some("Review the task, then resume it from the WebCodex host before continuing."),
                &task,
                json!({
                    "local_command": format!("webcodex task resume {}", task.task_id)
                }),
            ));
        }
        if task.task_status != ConnectorTaskState::Active
            || task.run_status != ConnectorRunState::Running
        {
            return Err(ConnectorCallOutcome::error_for_task(
                409,
                "task_not_active",
                "this task is already ready for review; start a new task for additional work",
                false,
                true,
                Some("Call task_start with the next requested outcome."),
                &task,
                Value::Null,
            ));
        }
        Ok(task)
    }

    pub(in crate::runtime) fn active_writable_task(
        &self,
        task_id: &str,
        subject_id: &str,
        capability: &str,
        now: i64,
    ) -> Result<ConnectorTaskSnapshot, ConnectorCallOutcome> {
        let task = self.active_task(task_id, subject_id)?;
        if task.mode == ConnectorTaskMode::InspectLegacy {
            return Err(Self::retired_inspect_task_outcome(&task));
        }
        if task.mode == ConnectorTaskMode::ReadOnly {
            let cursor = self.record_event(
                &task,
                capability,
                json!({ "ok": false, "denied": "read_only" }),
                now,
            );
            return Err(ConnectorCallOutcome::error_for_task_at(
                403,
                "read_only_task",
                format!("{capability} is unavailable because this task is read_only"),
                false,
                true,
                Some("Start a normal task only after the user authorizes changes or execution."),
                &task,
                cursor.unwrap_or(task.event_cursor),
                Value::Null,
            ));
        }
        Ok(task)
    }

    pub(in crate::runtime) fn active_executable_task(
        &self,
        task_id: &str,
        subject_id: &str,
        capability: &str,
        now: i64,
    ) -> Result<ConnectorTaskSnapshot, ConnectorCallOutcome> {
        let task = self.active_task(task_id, subject_id)?;
        if task.mode == ConnectorTaskMode::InspectLegacy {
            return Err(Self::retired_inspect_task_outcome(&task));
        }
        if task.mode == ConnectorTaskMode::ReadOnly {
            let cursor = self.record_event(
                &task,
                capability,
                json!({ "ok": false, "denied": "read_only" }),
                now,
            );
            return Err(ConnectorCallOutcome::error_for_task_at(
                403,
                "read_only_task",
                format!("{capability} is unavailable because this task is read_only"),
                false,
                true,
                Some("Start a normal task only after the user authorizes command execution."),
                &task,
                cursor.unwrap_or(task.event_cursor),
                Value::Null,
            ));
        }
        Ok(task)
    }

    /// Attach human guidance to a model-facing capability response.
    /// The task watermark provides an atomic, single-consumer claim across
    /// concurrent server responses. It does not provide an end-to-end delivery
    /// acknowledgement if the response is lost after the transaction commits.
    pub(in crate::runtime) fn attach_pending_guidance(
        &self,
        task: &ConnectorTaskSnapshot,
        data: &mut Value,
    ) {
        // One transaction claims the guidance and advances the watermark, so a
        // second capability response running concurrently cannot claim the same
        // message, and guidance older than the timeline window is still found.
        let claimed = match self.db.claim_pending_connector_guidance(
            &task.task_id,
            &self.context.project_id,
            &task.owner_subject_id,
            MAX_GUIDANCE_PER_RESPONSE,
        ) {
            Ok(claimed) => claimed,
            Err(error) => {
                // A claim that failed delivered nothing and advanced nothing;
                // say so rather than letting the message look consumed.
                tracing::warn!(
                    task_id = %task.task_id,
                    error = %error,
                    "guidance claim failed; guidance stays pending",
                );
                return;
            }
        };
        if claimed.is_empty() {
            return;
        }
        let pending: Vec<Value> = claimed
            .iter()
            .map(|event| {
                json!({
                    "sequence": event.sequence,
                    "message": event.payload["message"],
                    "created_at": event.created_at,
                })
            })
            .collect();
        data["guidance"] = json!(pending);
        data["guidance_note"] =
            json!("Human guidance from the project owner — adjust course before continuing.");
    }

    /// Host-side entry: record a human guidance message on a task. Delivered
    /// to the model inside its next capability response for this task.
    pub fn host_guide(&self, task_id: &str, message: &str) -> ConnectorCallOutcome {
        let task = match self
            .db
            .local_connector_task(task_id, &self.context.project_id)
        {
            Ok(task) => task,
            Err(error) => return store_error_outcome(error, None),
        };
        let now = chrono::Utc::now().timestamp();
        match self.record_event(
            &task,
            "human_guidance",
            json!({ "message": message, "source": "host" }),
            now,
        ) {
            Ok(cursor) => ConnectorCallOutcome::success_at(
                &task,
                cursor,
                json!({ "recorded": true, "sequence": cursor }),
            ),
            Err(outcome) => outcome,
        }
    }

    pub(in crate::runtime) fn record_event(
        &self,
        task: &ConnectorTaskSnapshot,
        capability: &str,
        payload: Value,
        now: i64,
    ) -> Result<i64, ConnectorCallOutcome> {
        self.db
            .append_connector_task_event(
                &task.task_id,
                &self.context.project_id,
                &task.owner_subject_id,
                capability,
                &payload,
                now,
            )
            .map_err(|error| store_error_outcome(error, Some(task)))
    }
}
