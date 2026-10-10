use super::support::*;
use crate::runner_http::RunnerRegistry;
use crate::runner_protocol::{
    RunnerCapabilities, RunnerJobUpdateRequest, RunnerPollRequest, RunnerResultPayload,
    RunnerResultRequest, ShellCommandExecutionState,
};
use crate::tool_runtime::{RuntimeInfo, ToolCall, ToolRuntime};
use serde_json::json;
use std::sync::Arc;
use webcodex_core::project_format::{
    ProjectFormatAdapter, ProjectFormatFileWitness, ProjectFormatMutationReport, ProjectFormatPlan,
    ProjectFormatPlanningResult, ProjectFormatProfile, ProjectFormatRequest,
};
use webcodex_runner_registry::{JobReceiptStore, NoopRunnerRegistryTelemetry, RetainedJobReceipt};

const CLIENT: &str = "project-format";

#[derive(Debug, Default)]
struct MemoryReceiptStore(std::sync::Mutex<Vec<RetainedJobReceipt>>);

impl JobReceiptStore for MemoryReceiptStore {
    fn upsert(&self, receipt: &RetainedJobReceipt) -> Result<(), String> {
        self.0.lock().unwrap().push(receipt.clone());
        Ok(())
    }
    fn load(&self, _now: i64) -> Result<Vec<RetainedJobReceipt>, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn prune(&self, _now: i64) -> Result<(), String> {
        Ok(())
    }
}

async fn setup(grace_ms: u64, with_store: bool) -> ToolRuntime {
    let registry = if with_store {
        RunnerRegistry::with_job_receipt_store(
            Arc::new(NoopRunnerRegistryTelemetry),
            Arc::new(MemoryReceiptStore::default()),
        )
        .await
    } else {
        RunnerRegistry::default()
    };
    let runtime = ToolRuntime::new(Arc::new(registry), Arc::new(RuntimeInfo::default()))
        .with_structured_execution_sync_wait(std::time::Duration::from_millis(grace_ms));
    register_agent(
        &runtime,
        CLIENT,
        None,
        RunnerCapabilities {
            project_format_v1: true,
            ..Default::default()
        },
    )
    .await;
    runtime
}

fn call() -> ToolCall {
    ToolCall::ProjectFormat {
        project: agent_test_project_id(CLIENT),
        session_id: None,
        cwd: None,
        adapter: Some(ProjectFormatAdapter::Rust),
        files: vec!["src/main.rs".into()],
        timeout_secs: Some(60),
    }
}

async fn wait_for_runner_request(runtime: &ToolRuntime) -> crate::runner_protocol::RunnerRequest {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(request) = probe_patch_agent_request(runtime, CLIENT).await {
            return request;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Runner request was not enqueued within 10 seconds"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn reply_plan(runtime: &ToolRuntime) -> (crate::runner_protocol::RunnerRequest, String) {
    let request = wait_for_runner_request(runtime).await;
    assert_eq!(request.kind, "plan_project_format");
    let semantic: ProjectFormatRequest =
        serde_json::from_str(request.content.as_deref().unwrap()).unwrap();
    assert_eq!(semantic.project_id, "agent-proj");
    let plan = ProjectFormatPlan {
        request: semantic,
        recipe_root: ".".into(),
        root_digest: "a".repeat(64),
        manifest_digest: "b".repeat(64),
        profile: ProjectFormatProfile::Rust {
            edition: "2021".into(),
        },
        files: vec![ProjectFormatFileWitness {
            path: "src/main.rs".into(),
            bytes: 15,
            sha256: "c".repeat(64),
            identity_digest: "d".repeat(64),
        }],
    };
    runtime
        .runner_registry
        .complete(RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: CLIENT.into(),
                runner_instance_id: "inst".into(),
                request_id: request.request_id,
                exit_code: Some(0),
                stdout: Some(
                    serde_json::to_string(&ProjectFormatPlanningResult::Ready { plan }).unwrap(),
                ),
                stderr: Some(String::new()),
                stdout_truncated: false,
                stderr_truncated: false,
                duration_ms: Some(5),
                error: None,
            },
            command_execution_state: Some(ShellCommandExecutionState::Completed),
            mcp_gateway: None,
            plugin_gateway: None,
            coding_agent: None,
        })
        .await
        .unwrap();
    let start = wait_for_runner_request(runtime).await;
    assert_eq!(start.kind, "start_format_job");
    let job_id = start.job_id.clone().expect("start_format_job job id");
    (start, job_id)
}

#[allow(clippy::too_many_arguments)]
async fn finish_format_job(
    runtime: &ToolRuntime,
    request: &crate::runner_protocol::RunnerRequest,
    job_id: &str,
    status: &str,
    state: ShellCommandExecutionState,
    exit_code: Option<i32>,
    error: Option<&str>,
    mutation: Option<ProjectFormatMutationReport>,
) {
    runtime
        .runner_registry
        .update_job(RunnerJobUpdateRequest {
            client_id: CLIENT.into(),
            runner_instance_id: "inst".into(),
            update_seq: None,
            job_id: job_id.into(),
            request_id: Some(request.request_id.clone()),
            status: status.into(),
            stdout_chunk: None,
            stderr_chunk: None,
            log_snapshot: None,
            exit_code,
            duration_ms: Some(25),
            error: error.map(str::to_string),
            command_execution_state: Some(state),
            validation_progress: None,
            test_count_evidence: None,
            format_mutation: mutation,
            activity: None,
            finished: true,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn project_format_fast_success_commits_and_returns_state_changed_true() {
    let runtime = setup(500, true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(call(), Some(&auth_context(None, true)))
                .await
        }
    });

    let (request, job_id) = reply_plan(&runtime).await;
    let operation = request.decode_operation().unwrap();
    let webcodex_core::runner_operation::RunnerOperation::Job(
        webcodex_core::runner_operation::RunnerJobOperation::StartFormat(format_op),
    ) = operation
    else {
        panic!("expected typed StartFormat");
    };
    assert_eq!(format_op.plan.recipe_root, ".");
    assert_eq!(
        format_op
            .context
            .structured_execution
            .as_ref()
            .unwrap()
            .execution_source,
        "project_format"
    );

    finish_format_job(
        &runtime,
        &request,
        &job_id,
        "completed",
        ShellCommandExecutionState::Completed,
        Some(0),
        None,
        Some(ProjectFormatMutationReport::Changed),
    )
    .await;

    let result = task.await.unwrap();
    assert!(result.success, "expected Ok result: {:?}", result.output);
    assert_eq!(result.output["state_changed"], json!(true));
}

#[tokio::test]
async fn project_format_fast_success_noop_returns_state_changed_false() {
    let runtime = setup(500, true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(call(), Some(&auth_context(None, true)))
                .await
        }
    });

    let (request, job_id) = reply_plan(&runtime).await;
    finish_format_job(
        &runtime,
        &request,
        &job_id,
        "completed",
        ShellCommandExecutionState::Completed,
        Some(0),
        None,
        Some(ProjectFormatMutationReport::Unchanged),
    )
    .await;

    let result = task.await.unwrap();
    assert!(result.success, "expected Ok result: {:?}", result.output);
    assert_eq!(result.output["state_changed"], json!(false));
}

#[tokio::test]
async fn project_format_missing_capability_rejects_without_dispatch() {
    let registry = RunnerRegistry::with_job_receipt_store(
        Arc::new(NoopRunnerRegistryTelemetry),
        Arc::new(MemoryReceiptStore::default()),
    )
    .await;
    let runtime = ToolRuntime::new(Arc::new(registry), Arc::new(RuntimeInfo::default()));
    register_agent(
        &runtime,
        CLIENT,
        None,
        RunnerCapabilities {
            project_format_v1: false,
            ..Default::default()
        },
    )
    .await;

    let result = runtime
        .dispatch_with_auth(call(), Some(&auth_context(None, true)))
        .await;
    assert!(!result.success);
    assert_eq!(
        result.output["failure_kind"],
        json!("capability_unavailable")
    );
    assert!(probe_patch_agent_request(&runtime, CLIENT).await.is_none());
}

#[tokio::test]
async fn project_format_runner_replaced_rejects_without_executing() {
    let runtime = setup(500, true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(call(), Some(&auth_context(None, true)))
                .await
        }
    });

    let request = wait_for_runner_request(&runtime).await;
    assert_eq!(request.kind, "plan_project_format");
    let semantic: ProjectFormatRequest =
        serde_json::from_str(request.content.as_deref().unwrap()).unwrap();
    let plan = ProjectFormatPlan {
        request: semantic,
        recipe_root: ".".into(),
        root_digest: "a".repeat(64),
        manifest_digest: "b".repeat(64),
        profile: ProjectFormatProfile::Rust {
            edition: "2021".into(),
        },
        files: vec![ProjectFormatFileWitness {
            path: "src/main.rs".into(),
            bytes: 15,
            sha256: "c".repeat(64),
            identity_digest: "d".repeat(64),
        }],
    };

    // Simulate runner restart/replacement with new runner_instance_id before completing plan
    register_agent_with_instance(
        &runtime,
        CLIENT,
        "inst-2",
        None,
        RunnerCapabilities {
            project_format_v1: true,
            ..Default::default()
        },
    )
    .await;

    let complete_result = runtime
        .runner_registry
        .complete(RunnerResultPayload {
            result: RunnerResultRequest {
                client_id: CLIENT.into(),
                runner_instance_id: "inst".into(),
                request_id: request.request_id,
                exit_code: Some(0),
                stdout: Some(
                    serde_json::to_string(&ProjectFormatPlanningResult::Ready { plan }).unwrap(),
                ),
                stderr: Some(String::new()),
                stdout_truncated: false,
                stderr_truncated: false,
                duration_ms: Some(5),
                error: None,
            },
            command_execution_state: Some(ShellCommandExecutionState::Completed),
            mcp_gateway: None,
            plugin_gateway: None,
            coding_agent: None,
        })
        .await;
    assert!(
        complete_result.is_err(),
        "expected stale complete rejection"
    );
    assert!(
        complete_result
            .as_ref()
            .unwrap_err()
            .contains("stale or replaced"),
        "expected stale or replaced error, got: {:?}",
        complete_result
    );

    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(
        result.output["failure_kind"],
        json!("format_runner_replaced")
    );
    let replacement_request = runtime
        .runner_registry
        .poll(RunnerPollRequest {
            client_id: CLIENT.to_string(),
            runner_instance_id: "inst-2".to_string(),
        })
        .await
        .unwrap();
    assert!(
        replacement_request.is_none(),
        "no StartFormat dispatch to replacement Runner"
    );
    assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
}

#[tokio::test]
async fn project_format_failed_job_returns_failure_kind_and_continuation() {
    let runtime = setup(500, true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(call(), Some(&auth_context(None, true)))
                .await
        }
    });

    let (request, job_id) = reply_plan(&runtime).await;
    finish_format_job(
        &runtime,
        &request,
        &job_id,
        "failed",
        ShellCommandExecutionState::OutcomeUnknown,
        Some(1),
        Some("format_write_conflict"),
        Some(ProjectFormatMutationReport::Unknown),
    )
    .await;

    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(
        result.output["failure_kind"],
        json!("format_write_conflict")
    );
    assert_eq!(result.output["execution_state"], json!("outcome_unknown"));
    assert_eq!(result.output["state_changed"], json!(null));
    assert_eq!(result.output["job_id"], json!(job_id));
    assert!(result.output.get("continuation").is_some());
}

#[tokio::test]
async fn project_format_contradictory_completed_error_mutation_fails_closed() {
    let runtime = setup(500, true).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(call(), Some(&auth_context(None, true)))
                .await
        }
    });

    let (request, job_id) = reply_plan(&runtime).await;
    finish_format_job(
        &runtime,
        &request,
        &job_id,
        "completed",
        ShellCommandExecutionState::Completed,
        Some(0),
        Some("format_write_conflict"),
        Some(ProjectFormatMutationReport::Changed),
    )
    .await;

    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(result.output["execution_state"], json!("outcome_unknown"));
}

#[tokio::test]
async fn project_format_invalid_arguments_reject_without_dispatch() {
    let runtime = setup(500, true).await;

    // 0 files
    let zero_files = ToolCall::ProjectFormat {
        project: agent_test_project_id(CLIENT),
        session_id: None,
        cwd: None,
        adapter: Some(ProjectFormatAdapter::Rust),
        files: vec![],
        timeout_secs: Some(60),
    };
    let result = runtime
        .dispatch_with_auth(zero_files, Some(&auth_context(None, true)))
        .await;
    assert!(!result.success);
    assert_eq!(result.output["failure_kind"], json!("invalid_arguments"));

    // 9 files (max is 8)
    let too_many_files = ToolCall::ProjectFormat {
        project: agent_test_project_id(CLIENT),
        session_id: None,
        cwd: None,
        adapter: Some(ProjectFormatAdapter::Rust),
        files: (0..9).map(|i| format!("src/file_{i}.rs")).collect(),
        timeout_secs: Some(60),
    };
    let result = runtime
        .dispatch_with_auth(too_many_files, Some(&auth_context(None, true)))
        .await;
    assert!(!result.success);
    assert_eq!(result.output["failure_kind"], json!("invalid_arguments"));

    // Invalid timeout
    let invalid_timeout = ToolCall::ProjectFormat {
        project: agent_test_project_id(CLIENT),
        session_id: None,
        cwd: None,
        adapter: Some(ProjectFormatAdapter::Rust),
        files: vec!["src/main.rs".into()],
        timeout_secs: Some(0),
    };
    let result = runtime
        .dispatch_with_auth(invalid_timeout, Some(&auth_context(None, true)))
        .await;
    assert!(!result.success);
    assert_eq!(result.output["failure_kind"], json!("invalid_arguments"));

    assert!(probe_patch_agent_request(&runtime, CLIENT).await.is_none());
}

#[tokio::test]
async fn project_format_without_receipt_store_fails_closed() {
    let runtime = setup(500, false).await;
    let result = runtime
        .dispatch_with_auth(call(), Some(&auth_context(None, true)))
        .await;
    assert!(!result.success);
    assert_eq!(
        result.output["failure_kind"],
        json!("format_execution_unavailable")
    );
    assert!(probe_patch_agent_request(&runtime, CLIENT).await.is_none());
    assert!(runtime.runner_registry.list_jobs(Some(10)).await.is_empty());
}
