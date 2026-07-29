use super::job_updates::ShellJobStartMetadata;
use super::reconciliation::validate_job_inventory;
use super::{now_ts, ShellClientRegistry, JOB_RECOVERY_GRACE_SECS, MAX_OUTPUT_BYTES};
use crate::shell_protocol::{
    ShellAgentJobUpdateRequest, ShellAgentPollRequest, ShellAgentProjectSummary,
    ShellClientCapabilities, ShellClientRegisterRequest, ShellJobContext, ShellJobInventory,
    ShellJobLogSnapshot, ShellJobOpRequest, ShellJobSnapshot, ShellJobStreamSnapshot,
    ShellJobValidationProgress, JOB_INVENTORY_MAX_TERMINAL_JOBS, JOB_SNAPSHOT_STREAM_MAX_BYTES,
};

const CLIENT_ID: &str = "oe";
const INSTANCE_A: &str = "instance-reconcile-a";
const INSTANCE_B: &str = "instance-reconcile-b";
const PROJECT_ID: &str = "demo";
const RUNTIME_PROJECT_ID: &str = "agent:oe:demo";
const SESSION_ID: &str = "wc_sess_job_reconciliation";

fn reconciliation_capabilities() -> ShellClientCapabilities {
    ShellClientCapabilities {
        jobs: true,
        async_jobs: true,
        async_shell_jobs: true,
        structured_validation_argv: true,
        job_state_reconciliation: true,
        ..Default::default()
    }
}

fn project_summary() -> ShellAgentProjectSummary {
    ShellAgentProjectSummary {
        id: PROJECT_ID.to_string(),
        name: Some("Demo".to_string()),
        path: "/srv/demo".to_string(),
        allow_patch: true,
        kind: Some("rust".to_string()),
        description: None,
        hooks: Vec::new(),
        disabled: false,
        revision: None,
        git_branch: Some("main".to_string()),
        git_head: None,
        git_dirty: None,
        updated_at: now_ts(),
        shell_profile: None,
    }
}

fn empty_inventory() -> ShellJobInventory {
    ShellJobInventory {
        active_complete: true,
        jobs: Vec::new(),
    }
}

fn register_request(instance: &str, inventory: ShellJobInventory) -> ShellClientRegisterRequest {
    ShellClientRegisterRequest {
        client_id: CLIENT_ID.to_string(),
        agent_instance_id: instance.to_string(),
        display_name: Some("reconciliation test runner".to_string()),
        owner: Some("tester".to_string()),
        hostname: None,
        capabilities: Some(reconciliation_capabilities()),
        projects: Some(vec![project_summary()]),
        agent_protocol_version: Some("polling-v1".to_string()),
        policy: None,
        process_started_at: Some(1_700_000_000),
        build: None,
        job_inventory: Some(inventory),
    }
}

async fn register(registry: &ShellClientRegistry, instance: &str, inventory: ShellJobInventory) {
    registry
        .register(register_request(instance, inventory))
        .await
        .unwrap();
}

fn start_request(command: &str) -> ShellJobOpRequest {
    ShellJobOpRequest {
        op: "start".to_string(),
        client_id: Some(CLIENT_ID.to_string()),
        cwd: Some("/srv/demo".to_string()),
        command: Some(command.to_string()),
        timeout_secs: Some(120),
        job_id: None,
        since_stdout_line: None,
        since_stderr_line: None,
        tail_lines: None,
        limit: None,
        codex: None,
    }
}

async fn start_and_take_over(
    registry: &ShellClientRegistry,
    instance: &str,
) -> (
    crate::shell_protocol::ShellJobInfo,
    crate::shell_protocol::ShellAgentShellRequest,
) {
    let job = registry
        .start_job_with_metadata(
            start_request("printf 'one\\ntwo\\n'; sleep 30"),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(RUNTIME_PROJECT_ID.to_string()),
                session_id: Some(SESSION_ID.to_string()),
                project_cwd: Some("/srv/demo".to_string()),
                purpose: Some("test".to_string()),
                shell: Some("bash".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let request = registry
        .poll(ShellAgentPollRequest {
            client_id: CLIENT_ID.to_string(),
            agent_instance_id: instance.to_string(),
            projects: None,
        })
        .await
        .unwrap()
        .expect("start request");
    assert_eq!(request.kind, "start_job");
    assert_eq!(request.job_id.as_deref(), Some(job.job_id.as_str()));
    let context = request.job_context.as_ref().expect("safe recovery context");
    assert_eq!(
        context.runtime_project_id.as_deref(),
        Some(RUNTIME_PROJECT_ID)
    );
    assert_eq!(context.workflow_session_id.as_deref(), Some(SESSION_ID));
    assert_eq!(context.command_preview, "printf 'one\\ntwo\\n'; sleep 30");
    assert!(!context.command_preview.contains("Authorization"));
    (job, request)
}

fn stream(tail: &str, first_retained_line: usize, truncated: bool) -> ShellJobStreamSnapshot {
    ShellJobStreamSnapshot {
        tail: tail.to_string(),
        first_retained_line,
        next_line: first_retained_line.saturating_add(tail.lines().count()),
        truncated,
    }
}

fn snapshot_from_request(
    job: &crate::shell_protocol::ShellJobInfo,
    request: &crate::shell_protocol::ShellAgentShellRequest,
    status: &str,
    update_seq: u64,
    stdout: ShellJobStreamSnapshot,
) -> ShellJobSnapshot {
    let terminal = matches!(
        status,
        "completed" | "failed" | "stopped" | "timeout" | "timed_out" | "cancelled" | "lost"
    );
    ShellJobSnapshot {
        job_id: job.job_id.clone(),
        request_id: request.request_id.clone(),
        status: status.to_string(),
        update_seq,
        created_at: job.created_at,
        started_at: Some(job.created_at + 1),
        ended_at: terminal.then_some(job.created_at + 2),
        exit_code: terminal.then_some(0),
        duration_ms: terminal.then_some(2_000),
        error: None,
        context: request.job_context.clone().expect("job context"),
        stdout,
        stderr: ShellJobStreamSnapshot::default(),
        validation_progress: None,
    }
}

fn update(
    instance: &str,
    job_id: &str,
    sequence: u64,
    status: &str,
    stdout_chunk: Option<&str>,
    finished: bool,
) -> ShellAgentJobUpdateRequest {
    ShellAgentJobUpdateRequest {
        client_id: CLIENT_ID.to_string(),
        agent_instance_id: instance.to_string(),
        job_id: job_id.to_string(),
        request_id: None,
        update_seq: Some(sequence),
        status: status.to_string(),
        stdout_chunk: stdout_chunk.map(str::to_string),
        stderr_chunk: None,
        stdout_tail: None,
        stderr_tail: None,
        log_snapshot: None,
        exit_code: finished.then_some(0),
        duration_ms: finished.then_some(2_000),
        error: None,
        validation_progress: None,
        finished,
    }
}

#[tokio::test]
async fn job_reconciliation_server_restart_restores_running_job_and_completion() {
    let registry_a = ShellClientRegistry::default();
    register(&registry_a, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&registry_a, INSTANCE_A).await;
    let snapshot = snapshot_from_request(&job, &request, "running", 2, stream("one\n", 1, false));

    let registry_b = ShellClientRegistry::default();
    register(
        &registry_b,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![snapshot],
        },
    )
    .await;

    let restored = registry_b.get_job(&job.job_id).await.unwrap();
    assert_eq!(restored.job_id, job.job_id);
    assert_eq!(restored.status, "running");
    assert_eq!(restored.project_id.as_deref(), Some(RUNTIME_PROJECT_ID));
    assert_eq!(restored.session_id.as_deref(), Some(SESSION_ID));
    assert_eq!(restored.project_cwd.as_deref(), Some("/srv/demo"));
    assert_eq!(restored.cwd.as_deref(), Some("/srv/demo"));
    assert_eq!(restored.purpose.as_deref(), Some("test"));
    assert_eq!(restored.shell.as_deref(), Some("bash"));
    assert!(restored.recovered_after_server_restart);
    assert_eq!(restored.last_update_seq, Some(2));
    assert_eq!(registry_b.list_jobs(Some(10)).await.len(), 1);

    registry_b
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            3,
            "completed",
            Some("two\n"),
            true,
        ))
        .await
        .unwrap();
    let completed = registry_b.get_job(&job.job_id).await.unwrap();
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.exit_code, Some(0));
    assert_eq!(completed.last_update_seq, Some(3));
    let (_, stdout, _, next_stdout, _) = registry_b
        .job_log(&job.job_id, Some(1), None, None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("one\ntwo\n"));
    assert_eq!(next_stdout, 3);
}

#[tokio::test]
async fn job_reconciliation_restores_terminal_snapshot_and_replay_is_idempotent() {
    let registry_a = ShellClientRegistry::default();
    register(&registry_a, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&registry_a, INSTANCE_A).await;
    let mut snapshot = snapshot_from_request(
        &job,
        &request,
        "completed",
        7,
        stream("offline output\n", 4, true),
    );
    snapshot.context.command_preview = "validation: check".to_string();
    snapshot.context.validation_steps = vec!["check".to_string()];
    snapshot.validation_progress = Some(ShellJobValidationProgress {
        completed: 1,
        current_step: None,
        failed_step: None,
    });
    let inventory = ShellJobInventory {
        active_complete: true,
        jobs: vec![snapshot],
    };

    let registry_b = ShellClientRegistry::default();
    register(&registry_b, INSTANCE_A, inventory.clone()).await;
    let first = registry_b.get_job(&job.job_id).await.unwrap();
    assert_eq!(first.status, "completed");
    assert_eq!(first.exit_code, Some(0));
    assert_eq!(first.duration_ms, Some(2_000));
    assert_eq!(
        first.validation_progress,
        Some(ShellJobValidationProgress {
            completed: 1,
            current_step: None,
            failed_step: None,
        })
    );
    assert_eq!(first.stdout_retained_from_line, Some(4));
    let first_reconciled_at = first.reconciled_at;
    let first_ended_at = first.ended_at;

    register(&registry_b, INSTANCE_A, inventory).await;
    let replayed = registry_b.get_job(&job.job_id).await.unwrap();
    assert_eq!(replayed.status, "completed");
    assert_eq!(replayed.ended_at, first_ended_at);
    assert_eq!(replayed.reconciled_at, first_reconciled_at);
    let (_, stdout, _, next, _) = registry_b
        .job_log(&job.job_id, Some(1), None, None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("offline output\n"));
    assert_eq!(next, 5);
}

#[tokio::test]
async fn job_reconciliation_same_instance_replaces_tail_without_duplicates() {
    let registry = ShellClientRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&registry, INSTANCE_A).await;
    registry
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            1,
            "running",
            Some("one\n"),
            false,
        ))
        .await
        .unwrap();
    registry.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "recovering"
    );

    let reconciled =
        snapshot_from_request(&job, &request, "running", 2, stream("one\ntwo\n", 1, false));
    register(
        &registry,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![reconciled],
        },
    )
    .await;
    let running = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(running.status, "running");
    assert_eq!(running.recovery_state.as_deref(), Some("reconciled"));
    assert_eq!(
        running.recovery_reason_code.as_deref(),
        Some("same_instance_reconciliation")
    );

    registry
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            1,
            "running",
            Some("one\n"),
            false,
        ))
        .await
        .unwrap();
    registry
        .update_job(update(
            INSTANCE_A,
            &job.job_id,
            3,
            "running",
            Some("three\n"),
            false,
        ))
        .await
        .unwrap();
    let (_, stdout, _, next, _) = registry
        .job_log(&job.job_id, Some(1), None, None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("one\ntwo\nthree\n"));
    assert_eq!(next, 4);

    let authoritative = snapshot_from_request(
        &job,
        &request,
        "running",
        4,
        stream("two\nthree\nfour\n", 2, true),
    );
    register(
        &registry,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![authoritative],
        },
    )
    .await;
    let (_, stdout, _, next, _) = registry
        .job_log(&job.job_id, Some(1), None, None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("two\nthree\nfour\n"));
    assert_eq!(next, 5);
    let status = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(status.stdout_retained_from_line, Some(2));
    assert_eq!(status.last_update_seq, Some(4));

    let mut replay = update(INSTANCE_A, &job.job_id, 5, "running", None, false);
    replay.log_snapshot = Some(ShellJobLogSnapshot {
        stdout: stream("two\nthree\nfour\nfive\n", 2, true),
        stderr: ShellJobStreamSnapshot::default(),
    });
    registry.update_job(replay).await.unwrap();
    let mut stale_replay = update(INSTANCE_A, &job.job_id, 4, "running", None, false);
    stale_replay.log_snapshot = Some(ShellJobLogSnapshot {
        stdout: stream("two\nthree\nfour\n", 2, true),
        stderr: ShellJobStreamSnapshot::default(),
    });
    registry.update_job(stale_replay).await.unwrap();
    let (_, stdout, _, next, _) = registry
        .job_log(&job.job_id, Some(1), None, None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("two\nthree\nfour\nfive\n"));
    assert_eq!(next, 6);

    let mut cursor_regression = update(INSTANCE_A, &job.job_id, 6, "running", None, false);
    cursor_regression.log_snapshot = Some(ShellJobLogSnapshot {
        stdout: ShellJobStreamSnapshot::default(),
        stderr: ShellJobStreamSnapshot::default(),
    });
    assert!(registry
        .update_job(cursor_regression)
        .await
        .unwrap_err()
        .contains("regresses an absolute cursor"));
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().last_update_seq,
        Some(5)
    );
}

#[tokio::test]
async fn job_reconciliation_same_instance_stale_connection_disconnect_is_noop() {
    let registry = ShellClientRegistry::default();
    registry
        .register_with_auth_connection(
            register_request(INSTANCE_A, empty_inventory()),
            None,
            Some("connection-a"),
        )
        .await
        .unwrap();
    let (job, request) = start_and_take_over(&registry, INSTANCE_A).await;
    let snapshot = snapshot_from_request(
        &job,
        &request,
        "running",
        1,
        ShellJobStreamSnapshot::default(),
    );
    registry
        .register_with_auth_connection(
            register_request(
                INSTANCE_A,
                ShellJobInventory {
                    active_complete: true,
                    jobs: vec![snapshot],
                },
            ),
            None,
            Some("connection-b"),
        )
        .await
        .unwrap();

    registry
        .reconcile_disconnect_for_connection(CLIENT_ID, INSTANCE_A, "connection-a")
        .await;
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "running"
    );
    assert!(registry
        .get_client_view_for_connection(CLIENT_ID, INSTANCE_A, "connection-b")
        .await
        .is_some());

    registry
        .reconcile_disconnect_for_connection(CLIENT_ID, INSTANCE_A, "connection-b")
        .await;
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "recovering"
    );
}

#[tokio::test]
async fn job_reconciliation_instance_replacement_fences_old_runner() {
    let registry = ShellClientRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    registry
        .update_job(update(INSTANCE_A, &job.job_id, 1, "running", None, false))
        .await
        .unwrap();
    registry.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;

    register(&registry, INSTANCE_B, empty_inventory()).await;
    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert_eq!(
        lost.recovery_reason_code.as_deref(),
        Some("runner_instance_replaced")
    );
    assert!(registry
        .update_job(update(INSTANCE_A, &job.job_id, 2, "running", None, false,))
        .await
        .unwrap_err()
        .contains("no longer the active instance"));
    assert!(registry
        .register(register_request(INSTANCE_A, empty_inventory()))
        .await
        .unwrap_err()
        .contains("instance was replaced"));
    assert_eq!(
        registry
            .get_client_view(CLIENT_ID)
            .await
            .unwrap()
            .pending_requests,
        0
    );
}

#[tokio::test]
async fn job_reconciliation_instance_replacement_does_not_redispatch_server_queue() {
    let registry = ShellClientRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let queued = registry
        .start_job(start_request("echo must-not-run"), "tester".to_string())
        .await
        .unwrap();
    assert_eq!(queued.status, "queued");
    registry
        .set_last_seen_for_test(CLIENT_ID, now_ts() - 120)
        .await;

    register(&registry, INSTANCE_B, empty_inventory()).await;
    let lost = registry.get_job(&queued.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert_eq!(
        lost.recovery_reason_code.as_deref(),
        Some("runner_instance_replaced")
    );
    assert!(registry
        .poll(ShellAgentPollRequest {
            client_id: CLIENT_ID.to_string(),
            agent_instance_id: INSTANCE_B.to_string(),
            projects: None,
        })
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn job_reconciliation_complete_inventory_missing_marks_job_lost() {
    let registry = ShellClientRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, _) = start_and_take_over(&registry, INSTANCE_A).await;
    registry
        .update_job(update(INSTANCE_A, &job.job_id, 1, "running", None, false))
        .await
        .unwrap();
    registry.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;

    register(&registry, INSTANCE_A, empty_inventory()).await;
    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert_eq!(
        lost.recovery_reason_code.as_deref(),
        Some("runner_inventory_missing")
    );
    let inner = registry.inner.lock().await;
    assert!(inner.pending_by_id.is_empty());
    assert!(inner.request_to_job.is_empty());
    assert!(inner
        .queues_by_client
        .get(CLIENT_ID)
        .is_none_or(|queue| queue.is_empty()));
}

#[tokio::test]
async fn job_reconciliation_recovery_deadline_and_unavailable_stop_are_explicit() {
    let registry = ShellClientRegistry::default();
    register(&registry, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&registry, INSTANCE_A).await;
    registry
        .update_job(update(INSTANCE_A, &job.job_id, 1, "running", None, false))
        .await
        .unwrap();
    registry.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;
    let recovering = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(recovering.status, "recovering");
    assert!(recovering.ended_at.is_none());
    let stop_error = registry
        .stop_job(&job.job_id, "tester".to_string())
        .await
        .unwrap_err();
    assert!(stop_error.contains("runner_unavailable_recovering"));

    {
        let mut inner = registry.inner.lock().await;
        inner
            .jobs_by_id
            .get_mut(&job.job_id)
            .unwrap()
            .recovering_since = Some(now_ts() - JOB_RECOVERY_GRACE_SECS);
    }
    let late_snapshot = snapshot_from_request(
        &job,
        &request,
        "running",
        2,
        ShellJobStreamSnapshot::default(),
    );
    register(
        &registry,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![late_snapshot],
        },
    )
    .await;
    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert_eq!(
        lost.recovery_reason_code.as_deref(),
        Some("runner_recovery_deadline_exceeded")
    );
    let first_ended_at = lost.ended_at;
    assert!(first_ended_at.is_some());
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().ended_at,
        first_ended_at
    );
}

#[tokio::test]
async fn job_reconciliation_stop_restored_job_targets_original_id() {
    let registry_a = ShellClientRegistry::default();
    register(&registry_a, INSTANCE_A, empty_inventory()).await;
    let (job, request) = start_and_take_over(&registry_a, INSTANCE_A).await;
    let snapshot = snapshot_from_request(
        &job,
        &request,
        "running",
        4,
        ShellJobStreamSnapshot::default(),
    );
    let registry_b = ShellClientRegistry::default();
    register(
        &registry_b,
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![snapshot],
        },
    )
    .await;

    let requested = registry_b
        .stop_job(&job.job_id, "tester".to_string())
        .await
        .unwrap();
    assert_eq!(requested.status, "stop_requested");
    let stop = registry_b
        .poll(ShellAgentPollRequest {
            client_id: CLIENT_ID.to_string(),
            agent_instance_id: INSTANCE_A.to_string(),
            projects: None,
        })
        .await
        .unwrap()
        .expect("stop request");
    assert_eq!(stop.kind, "stop_job");
    assert_eq!(stop.job_id.as_deref(), Some(job.job_id.as_str()));
    registry_b
        .update_job(update(INSTANCE_A, &job.job_id, 5, "stopped", None, true))
        .await
        .unwrap();
    assert_eq!(
        registry_b.get_job(&job.job_id).await.unwrap().status,
        "stopped"
    );
    assert_eq!(registry_b.list_jobs(Some(10)).await.len(), 1);
}

fn standalone_snapshot(job_id: &str, status: &str) -> ShellJobSnapshot {
    let terminal = status == "completed";
    ShellJobSnapshot {
        job_id: job_id.to_string(),
        request_id: format!("request-{job_id}"),
        status: status.to_string(),
        update_seq: 1,
        created_at: 1_700_000_000,
        started_at: Some(1_700_000_001),
        ended_at: terminal.then_some(1_700_000_002),
        exit_code: terminal.then_some(0),
        duration_ms: terminal.then_some(1_000),
        error: None,
        context: ShellJobContext {
            runtime_project_id: None,
            workflow_session_id: None,
            project_cwd: None,
            cwd: Some("/tmp".to_string()),
            purpose: Some("test".to_string()),
            shell: Some("bash".to_string()),
            command_preview: "safe preview".to_string(),
            validation_steps: Vec::new(),
        },
        stdout: ShellJobStreamSnapshot::default(),
        stderr: ShellJobStreamSnapshot::default(),
        validation_progress: None,
    }
}

#[test]
fn job_reconciliation_inventory_validation_is_bounded_and_atomic() {
    let projects = vec![project_summary()];
    let duplicate = standalone_snapshot("duplicate-job", "running");
    let error = validate_job_inventory(
        CLIENT_ID,
        &projects,
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![duplicate.clone(), duplicate],
        },
    )
    .unwrap_err();
    assert!(error.contains("duplicate job_id"));

    let mut invalid_status = standalone_snapshot("bad-status", "running");
    invalid_status.status = "mystery".to_string();
    assert!(validate_job_inventory(
        CLIENT_ID,
        &projects,
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![invalid_status],
        },
    )
    .unwrap_err()
    .contains("status"));

    let mut incomplete_validation = standalone_snapshot("incomplete-validation", "completed");
    incomplete_validation.context.validation_steps = vec!["check".to_string()];
    incomplete_validation.validation_progress = Some(ShellJobValidationProgress {
        completed: 0,
        current_step: None,
        failed_step: None,
    });
    assert!(validate_job_inventory(
        CLIENT_ID,
        &projects,
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![incomplete_validation],
        },
    )
    .unwrap_err()
    .contains("does not match status"));

    let mut oversized_stream = standalone_snapshot("oversized-stream", "running");
    oversized_stream.stdout = stream(&"x".repeat(JOB_SNAPSHOT_STREAM_MAX_BYTES + 1), 1, true);
    assert!(validate_job_inventory(
        CLIENT_ID,
        &projects,
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![oversized_stream],
        },
    )
    .unwrap_err()
    .contains("stdout exceeds"));

    let too_many_terminal = (0..=JOB_INVENTORY_MAX_TERMINAL_JOBS)
        .map(|index| standalone_snapshot(&format!("terminal-{index}"), "completed"))
        .collect();
    assert!(validate_job_inventory(
        CLIENT_ID,
        &projects,
        &ShellJobInventory {
            active_complete: true,
            jobs: too_many_terminal,
        },
    )
    .unwrap_err()
    .contains("terminal records"));

    let large_tail = "x\n".repeat(JOB_SNAPSHOT_STREAM_MAX_BYTES / 2);
    let large_inventory = ShellJobInventory {
        active_complete: true,
        jobs: (0..JOB_INVENTORY_MAX_TERMINAL_JOBS)
            .map(|index| {
                let mut snapshot =
                    standalone_snapshot(&format!("large-terminal-{index}"), "completed");
                snapshot.stdout = stream(&large_tail, 1, false);
                snapshot.stderr = stream(&large_tail, 1, false);
                snapshot
            })
            .collect(),
    };
    assert!(
        validate_job_inventory(CLIENT_ID, &projects, &large_inventory)
            .unwrap_err()
            .contains("serialized bytes")
    );

    let mut wrong_project = standalone_snapshot("wrong-project", "running");
    wrong_project.context.runtime_project_id = Some("agent:oe:missing".to_string());
    assert!(validate_job_inventory(
        CLIENT_ID,
        &projects,
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![wrong_project],
        },
    )
    .unwrap_err()
    .contains("not registered"));

    let mut invalid_session = standalone_snapshot("invalid-session", "running");
    invalid_session.context.runtime_project_id = Some(RUNTIME_PROJECT_ID.to_string());
    invalid_session.context.workflow_session_id = Some("foreign-session".to_string());
    assert!(validate_job_inventory(
        CLIENT_ID,
        &projects,
        &ShellJobInventory {
            active_complete: true,
            jobs: vec![invalid_session],
        },
    )
    .unwrap_err()
    .contains("workflow_session_id"));

    let safe = standalone_snapshot("no-raw-command", "running");
    let encoded = serde_json::to_value(&safe).unwrap();
    let snapshot = encoded.as_object().unwrap();
    for forbidden_field in ["command", "raw_command", "stdin", "env", "token", "config"] {
        assert!(!snapshot.contains_key(forbidden_field));
        assert!(!snapshot["context"]
            .as_object()
            .unwrap()
            .contains_key(forbidden_field));
    }
    assert!(serde_json::to_vec(&encoded).unwrap().len() < MAX_OUTPUT_BYTES);
}

#[tokio::test]
async fn job_reconciliation_malformed_inventory_does_not_mutate_registry() {
    let registry = ShellClientRegistry::default();
    let duplicate = standalone_snapshot("duplicate-job", "running");
    let mut request = register_request(
        INSTANCE_A,
        ShellJobInventory {
            active_complete: true,
            jobs: vec![duplicate.clone(), duplicate],
        },
    );
    request.display_name = Some("must not be installed".to_string());
    assert!(registry.register(request).await.is_err());
    assert!(registry.get_client_view(CLIENT_ID).await.is_none());
    assert!(registry.list_jobs(Some(10)).await.is_empty());

    register(&registry, INSTANCE_A, empty_inventory()).await;
    let queued = registry
        .start_job(
            start_request("echo request-id-collision"),
            "tester".to_string(),
        )
        .await
        .unwrap();
    let queued_request_id = {
        let inner = registry.inner.lock().await;
        inner
            .jobs_by_id
            .get(&queued.job_id)
            .and_then(|job| job.request_id.clone())
            .unwrap()
    };
    let mut collision = standalone_snapshot("different-job", "completed");
    collision.request_id = queued_request_id.clone();
    assert!(registry
        .register(register_request(
            INSTANCE_A,
            ShellJobInventory {
                active_complete: true,
                jobs: vec![collision],
            },
        ))
        .await
        .unwrap_err()
        .contains("belongs to a different job"));
    let inner = registry.inner.lock().await;
    assert_eq!(
        inner.request_to_job.get(&queued_request_id),
        Some(&queued.job_id)
    );
    assert_eq!(
        inner.jobs_by_id.get(&queued.job_id).unwrap().status,
        "queued"
    );
}

#[tokio::test]
async fn job_reconciliation_legacy_capability_keeps_immediate_lost_semantics() {
    let mismatch_registry = ShellClientRegistry::default();
    let mut missing_inventory = register_request(INSTANCE_A, empty_inventory());
    missing_inventory.job_inventory = None;
    assert!(mismatch_registry
        .register(missing_inventory)
        .await
        .unwrap_err()
        .contains("requires job_inventory"));
    let mut unexpected_inventory = register_request(INSTANCE_A, empty_inventory());
    unexpected_inventory.capabilities = Some(ShellClientCapabilities::default());
    assert!(mismatch_registry
        .register(unexpected_inventory)
        .await
        .unwrap_err()
        .contains("requires job_state_reconciliation"));
    let downgrade_registry = ShellClientRegistry::default();
    register(&downgrade_registry, INSTANCE_A, empty_inventory()).await;
    let mut downgraded = register_request(INSTANCE_A, empty_inventory());
    downgraded.capabilities = Some(ShellClientCapabilities::default());
    downgraded.job_inventory = None;
    assert!(downgrade_registry
        .register(downgraded)
        .await
        .unwrap_err()
        .contains("cannot downgrade"));

    let registry = ShellClientRegistry::default();
    let mut request = register_request(INSTANCE_A, empty_inventory());
    request.capabilities = Some(ShellClientCapabilities {
        jobs: true,
        async_jobs: true,
        async_shell_jobs: true,
        ..Default::default()
    });
    request.job_inventory = None;
    registry.register(request).await.unwrap();
    let job = registry
        .start_job(start_request("sleep 30"), "tester".to_string())
        .await
        .unwrap();
    registry
        .poll(ShellAgentPollRequest {
            client_id: CLIENT_ID.to_string(),
            agent_instance_id: INSTANCE_A.to_string(),
            projects: None,
        })
        .await
        .unwrap()
        .unwrap();
    registry.reconcile_disconnect(CLIENT_ID, INSTANCE_A).await;
    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert_eq!(
        lost.recovery_reason_code.as_deref(),
        Some("legacy_runner_disconnected")
    );
}
