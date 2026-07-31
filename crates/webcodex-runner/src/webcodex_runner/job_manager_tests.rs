use super::*;
use serde_json::json;

fn retained_terminal_job(job_id: &str, ended_at: i64) -> RunningJob {
    let mut snapshot = test_job_snapshot(job_id);
    snapshot.status = "completed".to_string();
    snapshot.ended_at = Some(ended_at);
    snapshot.exit_code = Some(0);
    snapshot.duration_ms = Some(1);
    RunningJob {
        client_id: "test-agent".to_string(),
        agent_instance_id: "test-instance".to_string(),
        snapshot,
        child: None,
        process_group_id: None,
        stop_requested: Arc::new(AtomicBool::new(false)),
        slot_reserved: false,
    }
}

#[test]
fn job_reconciliation_inventory_prioritizes_active_and_bounds_terminal_history() {
    let manager = JobManager::new(1);
    let now = chrono::Utc::now().timestamp();
    let mut active = test_job_snapshot("active-original-job");
    active.created_at = now - 100;
    active.context.command_preview = "safe preview".to_string();
    lock_unpoison(&manager.jobs).insert(
        active.job_id.clone(),
        RunningJob {
            client_id: "test-agent".to_string(),
            agent_instance_id: "test-instance".to_string(),
            snapshot: active,
            child: None,
            process_group_id: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
            slot_reserved: true,
        },
    );
    for index in 0..(JOB_INVENTORY_MAX_TERMINAL_JOBS + 8) {
        let job_id = format!("terminal-{index}");
        lock_unpoison(&manager.jobs).insert(
            job_id.clone(),
            retained_terminal_job(&job_id, now - index as i64),
        );
    }
    let expired_id = "terminal-expired";
    lock_unpoison(&manager.jobs).insert(
        expired_id.to_string(),
        retained_terminal_job(expired_id, now - JOB_TERMINAL_RETENTION_SECS),
    );

    let inventory = manager.inventory();
    assert!(inventory.active_complete);
    assert_eq!(inventory.jobs[0].job_id, "active-original-job");
    assert_eq!(
        inventory
            .jobs
            .iter()
            .filter(|snapshot| runner_job_is_terminal(&snapshot.status))
            .count(),
        JOB_INVENTORY_MAX_TERMINAL_JOBS
    );
    assert_eq!(
        inventory
            .jobs
            .iter()
            .filter(|snapshot| runner_job_is_active(&snapshot.status))
            .count(),
        1
    );
    assert!(!inventory
        .jobs
        .iter()
        .any(|snapshot| snapshot.job_id == expired_id));
    assert!(inventory
        .jobs
        .iter()
        .skip(1)
        .all(|snapshot| runner_job_is_terminal(&snapshot.status)));
}

#[test]
fn job_reconciliation_inventory_drops_terminal_payload_before_active_jobs() {
    let manager = JobManager::new(1);
    let now = chrono::Utc::now().timestamp();
    let mut active = test_job_snapshot("active-safe-metadata");
    active.context.command_preview = "safe preview".to_string();
    lock_unpoison(&manager.jobs).insert(
        active.job_id.clone(),
        RunningJob {
            client_id: "test-agent".to_string(),
            agent_instance_id: "test-instance".to_string(),
            snapshot: active,
            child: None,
            process_group_id: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
            slot_reserved: true,
        },
    );
    let tail = "x\n".repeat(JOB_SNAPSHOT_STREAM_MAX_BYTES / 2);
    for index in 0..JOB_INVENTORY_MAX_TERMINAL_JOBS {
        let job_id = format!("large-terminal-{index}");
        let mut job = retained_terminal_job(&job_id, now - index as i64);
        job.snapshot.stdout = ShellJobStreamSnapshot {
            tail: tail.clone(),
            first_retained_line: 1,
            next_line: 1 + tail.lines().count(),
            truncated: false,
        };
        job.snapshot.stderr = job.snapshot.stdout.clone();
        lock_unpoison(&manager.jobs).insert(job_id, job);
    }

    let inventory = manager.inventory();
    assert_eq!(inventory.jobs[0].job_id, "active-safe-metadata");
    assert!(
        inventory.jobs.len() < JOB_INVENTORY_MAX_TERMINAL_JOBS + 1,
        "terminal snapshots must yield before the active record"
    );
    let encoded = serde_json::to_vec(&inventory).unwrap();
    assert!(encoded.len() <= JOB_INVENTORY_MAX_SERIALIZED_BYTES);
    assert!(!String::from_utf8(encoded)
        .unwrap()
        .contains("super-secret-raw-command"));
}

#[test]
fn job_reconciliation_local_snapshot_advances_before_best_effort_send() {
    let manager = JobManager::new(1);
    let mut snapshot = test_job_snapshot("offline-terminal-job");
    snapshot.context.validation_steps = vec!["check".to_string()];
    lock_unpoison(&manager.jobs).insert(
        snapshot.job_id.clone(),
        RunningJob {
            client_id: "test-agent".to_string(),
            agent_instance_id: "test-instance".to_string(),
            snapshot,
            child: None,
            process_group_id: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
            slot_reserved: true,
        },
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    manager.install_sink(AgentSink::WebSocket {
        tx,
        client_id: "test-agent".to_string(),
        agent_instance_id: "test-instance".to_string(),
    });
    manager.update_and_send(
        "offline-terminal-job",
        RunnerJobDelta {
            status: "running".to_string(),
            stdout_chunk: Some("one\n".to_string()),
            validation_progress: Some(ShellJobValidationProgress {
                completed: 0,
                current_step: Some("check".to_string()),
                failed_step: None,
            }),
            ..Default::default()
        },
    );
    let first = rx.try_recv().expect("incremental update");
    let AgentEnvelope::JobUpdate { payload: first } = first else {
        panic!("expected job update");
    };
    assert_eq!(first.update_seq, Some(2));
    assert!(first.stdout_chunk.is_none());
    let first_logs = first
        .log_snapshot
        .expect("sequenced update has authoritative logs");
    assert_eq!(first_logs.stdout.tail, "one\n");
    assert_eq!(first_logs.stdout.next_line, 2);

    drop(rx);
    manager.update_and_send(
        "offline-terminal-job",
        RunnerJobDelta {
            status: "completed".to_string(),
            stdout_chunk: Some("two\n".to_string()),
            exit_code: Some(0),
            duration_ms: Some(25),
            validation_progress: Some(ShellJobValidationProgress {
                completed: 1,
                current_step: None,
                failed_step: None,
            }),
            finished: true,
            ..Default::default()
        },
    );
    let inventory = manager.inventory();
    let retained = inventory
        .jobs
        .iter()
        .find(|snapshot| snapshot.job_id == "offline-terminal-job")
        .expect("terminal snapshot remains after transport send fails");
    assert_eq!(retained.status, "completed");
    assert_eq!(retained.update_seq, 3);
    assert_eq!(retained.stdout.tail, "one\ntwo\n");
    assert_eq!(retained.stdout.next_line, 3);
    assert_eq!(retained.exit_code, Some(0));
    assert_eq!(retained.duration_ms, Some(25));
    assert_eq!(
        retained.validation_progress,
        Some(ShellJobValidationProgress {
            completed: 1,
            current_step: None,
            failed_step: None,
        })
    );
    let record = lock_unpoison(&manager.jobs)
        .get("offline-terminal-job")
        .cloned()
        .unwrap();
    assert!(record.child.is_none());
    assert!(record.process_group_id.is_none());
    assert!(!record.slot_reserved);

    manager.update_and_send(
        "offline-terminal-job",
        RunnerJobDelta {
            status: "running".to_string(),
            stdout_chunk: Some("late\n".to_string()),
            ..Default::default()
        },
    );
    let immutable_terminal = manager
        .inventory()
        .jobs
        .into_iter()
        .find(|snapshot| snapshot.job_id == "offline-terminal-job")
        .unwrap();
    assert_eq!(immutable_terminal.status, "completed");
    assert_eq!(immutable_terminal.update_seq, 3);
    assert_eq!(immutable_terminal.stdout.tail, "one\ntwo\n");

    let mut registered_inventory = manager.inventory();
    let (reconnected_tx, mut reconnected_rx) = tokio::sync::mpsc::channel(4);
    manager.install_sink(AgentSink::WebSocket {
        tx: reconnected_tx,
        client_id: "test-agent".to_string(),
        agent_instance_id: "test-instance".to_string(),
    });
    manager.replay_snapshots_since(&registered_inventory);
    assert!(
        reconnected_rx.try_recv().is_err(),
        "unchanged register snapshots need no network replay"
    );
    registered_inventory
        .jobs
        .iter_mut()
        .find(|snapshot| snapshot.job_id == "offline-terminal-job")
        .unwrap()
        .update_seq -= 1;
    manager.replay_snapshots_since(&registered_inventory);
    let replay = reconnected_rx.try_recv().expect("post-register replay");
    let AgentEnvelope::JobUpdate { payload: replay } = replay else {
        panic!("expected replay job update");
    };
    assert_eq!(replay.job_id, "offline-terminal-job");
    assert_eq!(replay.update_seq, Some(3));
    assert!(replay.finished);
    let logs = replay.log_snapshot.expect("authoritative replay logs");
    assert_eq!(logs.stdout.tail, "one\ntwo\n");
    assert_eq!(logs.stdout.next_line, 3);

    manager.stop("offline-terminal-job").unwrap();
    let stopped_race = reconnected_rx
        .try_recv()
        .expect("stop racing a lost terminal update replays the terminal snapshot");
    let AgentEnvelope::JobUpdate {
        payload: stopped_race,
    } = stopped_race
    else {
        panic!("expected replay job update");
    };
    assert_eq!(stopped_race.status, "completed");
    assert_eq!(stopped_race.update_seq, Some(3));
    assert!(stopped_race.finished);
}

#[test]
fn job_reconciliation_utf8_log_tail_is_bounded_with_absolute_cursor() {
    let emoji = "🙂".as_bytes();
    let mut split_scalar = emoji[..2].to_vec();
    assert!(take_utf8_output(&mut split_scalar, false).is_empty());
    assert_eq!(split_scalar, emoji[..2]);
    split_scalar.extend_from_slice(&emoji[2..]);
    assert_eq!(take_utf8_output(&mut split_scalar, false), "🙂");
    assert!(split_scalar.is_empty());

    let mut stream = ShellJobStreamSnapshot::default();
    let chunk = "🙂\n".repeat(JOB_SNAPSHOT_STREAM_MAX_BYTES / 2);
    append_runner_stream(&mut stream, Some(&chunk));
    assert!(stream.tail.len() <= JOB_SNAPSHOT_STREAM_MAX_BYTES);
    assert!(stream.truncated);
    assert!(stream.first_retained_line > 1);
    assert_eq!(
        stream.next_line,
        stream
            .first_retained_line
            .saturating_add(stream.tail.lines().count())
    );
    assert!(std::str::from_utf8(stream.tail.as_bytes()).is_ok());

    let mut long_partial = ShellJobStreamSnapshot::default();
    append_runner_stream(
        &mut long_partial,
        Some(&format!(
            "first\nsecond\n{}",
            "z".repeat(JOB_SNAPSHOT_STREAM_MAX_BYTES + 1)
        )),
    );
    assert_eq!(long_partial.first_retained_line, 3);
    assert_eq!(long_partial.next_line, 4);
    let observed_next = long_partial.next_line;
    trim_runner_stream_to(&mut long_partial, 0);
    assert!(long_partial.tail.is_empty());
    assert_eq!(long_partial.first_retained_line, observed_next);
    assert_eq!(long_partial.next_line, observed_next);
}

#[test]
fn validation_wait_failure_is_executor_owned_without_a_failed_check() {
    let error = std::io::Error::other("synthetic wait failure");
    let encoded = wait_failure_error(true, &error);
    assert_eq!(encoded, VALIDATION_STEP_WAIT_FAILED_CODE);
    assert_eq!(
        validation_failed_step("failed", Some(&encoded), "check"),
        None
    );

    let ordinary = wait_failure_error(false, &error);
    assert_eq!(ordinary, "failed to wait job: synthetic wait failure");
    assert_eq!(
        validation_failed_step("failed", Some("check exited non-zero"), "check"),
        Some("check".to_string())
    );
}

#[cfg(target_os = "linux")]
#[test]
fn job_manager_stop_terminates_the_process_group() {
    let temp = tempfile::tempdir().unwrap();
    let mut command = configured_shell_job_command(
        &ShellConfig::default(),
        "sleep 60 & echo $! > descendant.pid; wait",
    )
    .unwrap();
    let child = Arc::new(Mutex::new(
        command
            .current_dir(temp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    ));
    let leader_pid = child.lock().unwrap().id();
    let pid_file = temp.path().join("descendant.pid");
    for _ in 0..200 {
        if pid_file.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let descendant_pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    assert!(process_running(leader_pid));
    assert!(process_running(descendant_pid));

    let manager = JobManager::new(1);
    let stop_requested = Arc::new(AtomicBool::new(false));
    manager.jobs.lock().unwrap().insert(
        "process-group-job".into(),
        RunningJob {
            client_id: "test-agent".into(),
            agent_instance_id: "test-instance".into(),
            snapshot: test_job_snapshot("process-group-job"),
            child: Some(child.clone()),
            process_group_id: Some(leader_pid),
            stop_requested: stop_requested.clone(),
            slot_reserved: true,
        },
    );
    manager.stop("process-group-job").unwrap();
    assert!(stop_requested.load(Ordering::SeqCst));

    for _ in 0..200 {
        let leader_exited = child.lock().unwrap().try_wait().unwrap().is_some();
        if leader_exited && !process_running(descendant_pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(child.lock().unwrap().try_wait().unwrap().is_some());
    assert!(
        !process_running(descendant_pid),
        "descendant {descendant_pid} survived process-group cancellation"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn job_shutdown_reaps_a_sigterm_responsive_child() {
    let temp = tempfile::tempdir().unwrap();
    let ready = temp.path().join("ready");
    let mut command = configured_shell_job_command(
        &ShellConfig::default(),
        "trap 'exit 0' TERM; : > ready; while :; do sleep 1; done",
    )
    .unwrap();
    let child = Arc::new(Mutex::new(
        command
            .current_dir(temp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    ));
    let leader_pid = child.lock().unwrap().id();
    assert!(wait_until(Duration::from_secs(1), || ready.exists()));
    let manager = JobManager::new(1);
    let stop_requested = Arc::new(AtomicBool::new(false));
    lock_unpoison(&manager.jobs).insert(
        "term-responsive".into(),
        RunningJob {
            client_id: "test-agent".into(),
            agent_instance_id: "test-instance".into(),
            snapshot: test_job_snapshot("term-responsive"),
            child: Some(Arc::clone(&child)),
            process_group_id: Some(leader_pid),
            stop_requested: Arc::clone(&stop_requested),
            slot_reserved: true,
        },
    );

    manager.stop_accepting_work();
    let batch = manager.signal_all_for_shutdown();
    let outcome = manager.drain_shutdown(batch, Instant::now() + Duration::from_millis(800));
    assert_eq!(outcome.resources, 1);
    assert_eq!(outcome.timed_out, 0);
    assert!(stop_requested.load(Ordering::SeqCst));
    assert!(child.lock().unwrap().try_wait().unwrap().is_some());
    assert!(!process_running(leader_pid));
}

#[cfg(target_os = "linux")]
#[test]
fn job_shutdown_escalates_ignored_sigterm_for_parent_and_descendant() {
    let temp = tempfile::tempdir().unwrap();
    let mut command = configured_shell_job_command(
        &ShellConfig::default(),
        "trap '' TERM; sleep 60 & echo $! > descendant.pid; wait",
    )
    .unwrap();
    let child = Arc::new(Mutex::new(
        command
            .current_dir(temp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    ));
    let leader_pid = child.lock().unwrap().id();
    let pid_file = temp.path().join("descendant.pid");
    assert!(wait_until(Duration::from_secs(2), || pid_file.exists()));
    let descendant_pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    assert!(process_running(leader_pid));
    assert!(process_running(descendant_pid));

    let manager = JobManager::new(1);
    let stop_requested = Arc::new(AtomicBool::new(false));
    lock_unpoison(&manager.jobs).insert(
        "term-ignoring".into(),
        RunningJob {
            client_id: "test-agent".into(),
            agent_instance_id: "test-instance".into(),
            snapshot: test_job_snapshot("term-ignoring"),
            child: Some(Arc::clone(&child)),
            process_group_id: Some(leader_pid),
            stop_requested: Arc::clone(&stop_requested),
            slot_reserved: true,
        },
    );
    let started = Instant::now();
    manager.stop_accepting_work();
    let batch = manager.signal_all_for_shutdown();
    let outcome = manager.drain_shutdown(batch, Instant::now() + Duration::from_millis(900));
    let elapsed = started.elapsed();

    assert_eq!(outcome.resources, 1);
    assert_eq!(outcome.timed_out, 0);
    assert!(stop_requested.load(Ordering::SeqCst));
    assert!(
        elapsed < Duration::from_millis(1100),
        "job shutdown exceeded its absolute deadline: {elapsed:?}"
    );
    assert!(child.lock().unwrap().try_wait().unwrap().is_some());
    assert!(!process_running(leader_pid));
    assert!(
        wait_until(Duration::from_secs(1), || !process_running(descendant_pid)),
        "descendant survived process-group SIGKILL"
    );
}

#[test]
fn poisoned_job_mutex_does_not_panic_shutdown() {
    let manager = JobManager::new(1);
    let jobs = Arc::clone(&manager.jobs);
    let poisoned = std::thread::spawn(move || {
        let _guard = jobs.lock().unwrap();
        panic!("poison jobs mutex");
    });
    assert!(poisoned.join().is_err());

    manager.stop_accepting_work();
    assert_eq!(manager.cancel_queued_for_shutdown(), 0);
    let batch = manager.signal_all_for_shutdown();
    let outcome = manager.drain_shutdown(batch, Instant::now() + Duration::from_millis(50));
    assert_eq!(outcome.resources, 0);
    assert_eq!(outcome.timed_out, 0);
}

#[cfg(unix)]
#[test]
fn process_group_signal_errors_distinguish_gone_permission_and_other_failures() {
    assert_eq!(
        classify_process_group_signal_error(
            42,
            libc::SIGTERM,
            std::io::Error::from_raw_os_error(libc::ESRCH),
        ),
        Ok(false)
    );
    let permission = classify_process_group_signal_error(
        42,
        libc::SIGTERM,
        std::io::Error::from_raw_os_error(libc::EPERM),
    )
    .unwrap_err();
    assert!(permission.contains("permission"));
    let other = classify_process_group_signal_error(
        42,
        libc::SIGTERM,
        std::io::Error::from_raw_os_error(libc::EINVAL),
    )
    .unwrap_err();
    assert!(other.contains("Invalid argument"));
}

/// One run of the fail-fast plan, plus the side effect the plan must not have.
#[cfg(unix)]
struct FailFastAttempt {
    updates: Vec<ShellAgentJobUpdateRequest>,
    test_step_ran: bool,
}

/// Drain job updates until the job reports `finished`, or the deadline passes.
///
/// The deadline is wall-clock rather than a sleep count: under a loaded machine
/// a 10ms sleep is not 10ms, so a counting loop silently shortens its own
/// patience exactly when the job needs more of it.
#[cfg(unix)]
fn collect_job_updates(
    rx: &mut tokio::sync::mpsc::Receiver<AgentEnvelope>,
    deadline: Duration,
) -> Vec<ShellAgentJobUpdateRequest> {
    let started = Instant::now();
    let mut updates: Vec<ShellAgentJobUpdateRequest> = Vec::new();
    while started.elapsed() < deadline {
        while let Ok(envelope) = rx.try_recv() {
            if let AgentEnvelope::JobUpdate { payload } = envelope {
                updates.push(payload);
            }
        }
        if updates.last().is_some_and(|update| update.finished) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    updates
}

#[cfg(target_os = "linux")]
#[test]
fn inspect_job_manager_path_landlocks_commands_and_descendants() {
    if crate::command_sandbox::inspect_sandbox_available().is_err() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let tracked = project.join("tracked.txt");
    std::fs::write(&tracked, "original\n").unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let sink = AgentSink::WebSocket {
        tx,
        client_id: "inspect-agent".into(),
        agent_instance_id: "inspect-instance".into(),
    };
    let manager = JobManager::new(1);
    manager.enqueue(
        sink,
        1,
        AgentPolicy {
            allow_cwd_anywhere: true,
            ..AgentPolicy::default()
        },
        ShellConfig::default(),
        SshConfig::default(),
        temp.path().join("projects.d"),
        serde_json::from_value(json!({
            "request_id": "inspect-job-request",
            "client_id": "inspect-agent",
            "kind": "start_job",
            "job_id": "inspect-job",
            "cwd": project,
            "command": "set -eu; cat tracked.txt; printf ok > \"$TMPDIR/proof\"; test \"$(cat \"$TMPDIR/proof\")\" = ok; ! touch created.txt; ! truncate -s 0 tracked.txt; ! sh -c 'printf child > child.txt'",
            "timeout_secs": 30,
            "requested_by": "test",
            "created_at": 1,
            "sandbox": crate::command_sandbox::INSPECT_SANDBOX_MODE,
            "job_context": test_job_context(&project, Vec::new())
        }))
        .unwrap(),
    );

    let updates = collect_job_updates(&mut rx, Duration::from_secs(30));
    let final_update = updates.last().expect("inspect job should finish");
    assert!(final_update.finished, "{final_update:?}");
    assert_eq!(final_update.status, "completed", "{final_update:?}");
    assert_eq!(final_update.exit_code, Some(0), "{final_update:?}");
    assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "original\n");
    assert!(!project.join("created.txt").exists());
    assert!(!project.join("child.txt").exists());
}

/// The outcome the executor emits when a step could not be spawned at all.
///
/// This is a modeled result, not a bug — `validation_spawn_failure_is_
/// infrastructure_without_failed_assertion` pins it, and the connector treats
/// it as infrastructure rather than as a failed assertion. Recognising it here
/// keeps a machine-level spawn failure from being read as a fail-fast
/// regression.
#[cfg(unix)]
fn is_validation_spawn_failure(update: &ShellAgentJobUpdateRequest) -> bool {
    update.finished
        && update.status == "failed"
        && update.exit_code.is_none()
        && update.error.as_deref() == Some(VALIDATION_STEP_SPAWN_FAILED_CODE)
}

#[cfg(unix)]
fn describe_update(update: &ShellAgentJobUpdateRequest) -> String {
    format!(
        "status={:?} finished={} exit_code={:?} error={:?} progress={:?}",
        update.status, update.finished, update.exit_code, update.error, update.validation_progress
    )
}

#[cfg(unix)]
fn run_fail_fast_validation_job(attempt: usize) -> FailFastAttempt {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let cargo = bin.join("cargo");
    std::fs::write(
        &cargo,
        "#!/bin/sh\ncase \"$1\" in\nfmt) echo 'format passed';;\ncheck) exit 7;;\ntest) touch should-not-run;;\nesac\n",
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let sink = AgentSink::WebSocket {
        tx,
        client_id: "validation-agent".into(),
        agent_instance_id: "validation-instance".into(),
    };
    let steps = vec![
        ShellJobValidationStep {
            name: "format".into(),
            program: "cargo".into(),
            args: vec!["fmt".into(), "--".into(), "--check".into()],
            env: Vec::new(),
        },
        ShellJobValidationStep {
            name: "check".into(),
            program: "cargo".into(),
            args: vec!["check".into(), "--all-targets".into()],
            env: Vec::new(),
        },
        ShellJobValidationStep {
            name: "test".into(),
            program: "cargo".into(),
            args: vec!["test".into()],
            env: Vec::new(),
        },
    ];
    let mut shell = ShellConfig::default();
    shell.path_prepend.push(bin);
    let manager = JobManager::new(1);
    manager.enqueue(
        sink,
        1,
        AgentPolicy {
            // These tests run jobs in a temp dir; the boundary itself is
            // covered separately, and AgentPolicy::default() is fail-closed.
            allow_cwd_anywhere: true,
            ..AgentPolicy::default()
        },
        shell,
        SshConfig::default(),
        temp.path().join("projects.d"),
        serde_json::from_value(json!({
            "request_id": format!("validation-request-{attempt}"),
            "client_id": "validation-agent",
            "kind": "start_validation_job",
            "job_id": format!("validation-job-{attempt}"),
            "cwd": temp.path(),
            "command": serde_json::to_string(&steps).unwrap(),
            // Two `sh` one-liners. A timeout here would mean a hang, not a busy
            // machine, which is the point of the gap between this and the
            // collector deadline below.
            "timeout_secs": 60,
            "requested_by": "test",
            "created_at": 1,
            "job_context": test_job_context(
                temp.path(),
                steps.iter().map(|step| step.name.clone()).collect(),
            )
        }))
        .unwrap(),
    );
    let updates = collect_job_updates(&mut rx, Duration::from_secs(120));
    FailFastAttempt {
        test_step_ran: temp.path().join("should-not-run").exists(),
        updates,
    }
}

#[cfg(unix)]
#[test]
fn validation_job_progress_is_executor_owned_and_fail_fast() {
    // Spawning a step can fail for reasons that belong to the machine rather
    // than to the state machine — `fork` returning EAGAIN under a loaded test
    // suite, or ETXTBSY on a script written moments earlier. The executor
    // reports that as `validation_step_spawn_failed` with no exit code, which
    // is the correct answer to a question this test is not asking. Retrying
    // those attempts is what keeps a busy machine from deciding whether the
    // fail-fast contract holds; asserting on the first attempt is not.
    const ATTEMPTS: usize = 3;
    let mut spawn_failures = Vec::new();

    for attempt in 0..ATTEMPTS {
        let FailFastAttempt {
            updates,
            test_step_ran,
        } = run_fail_fast_validation_job(attempt);
        let final_update = updates.last().unwrap_or_else(|| {
            panic!("attempt {attempt}: validation job emitted no updates before the deadline")
        });
        assert!(
            final_update.finished,
            "attempt {attempt}: job never finished; last update was {}",
            describe_update(final_update)
        );
        if is_validation_spawn_failure(final_update) {
            spawn_failures.push(format!(
                "attempt {attempt}: {}",
                describe_update(final_update)
            ));
            continue;
        }

        assert_eq!(
            final_update.status,
            "failed",
            "attempt {attempt}: {}",
            describe_update(final_update)
        );
        assert_eq!(
            final_update.exit_code,
            Some(7),
            "attempt {attempt}: the failing step's exit code must reach the final update: {}",
            describe_update(final_update)
        );
        assert_eq!(
            final_update.validation_progress,
            Some(ShellJobValidationProgress {
                completed: 1,
                current_step: None,
                failed_step: Some("check".into()),
            }),
            "attempt {attempt}: {}",
            describe_update(final_update)
        );
        assert!(
            updates.iter().any(|update| {
                update.validation_progress
                    == Some(ShellJobValidationProgress {
                        completed: 1,
                        current_step: Some("check".into()),
                        failed_step: None,
                    })
            }),
            "attempt {attempt}: no update announced 'check' as the running step; saw {:?}",
            updates.iter().map(describe_update).collect::<Vec<_>>()
        );
        assert!(
            !test_step_ran,
            "attempt {attempt}: the plan ran 'test' after 'check' failed"
        );
        return;
    }

    panic!(
        "every attempt failed to spawn a validation step, so the fail-fast path \
         was never exercised:\n{}",
        spawn_failures.join("\n")
    );
}

#[cfg(unix)]
#[test]
fn validation_spawn_failure_is_infrastructure_without_failed_assertion() {
    let temp = tempfile::tempdir().unwrap();
    let mut shell = ShellConfig::default();
    shell.env.insert(
        "PATH".to_string(),
        temp.path().to_string_lossy().into_owned(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let sink = AgentSink::WebSocket {
        tx,
        client_id: "validation-agent".into(),
        agent_instance_id: "validation-instance".into(),
    };
    let manager = JobManager::new(1);
    manager.enqueue(
        sink,
        1,
        AgentPolicy {
            // These tests run jobs in a temp dir; the boundary itself is
            // covered separately, and AgentPolicy::default() is fail-closed.
            allow_cwd_anywhere: true,
            ..AgentPolicy::default()
        },
        shell,
        SshConfig::default(),
        temp.path().join("projects.d"),
        serde_json::from_value(json!({
            "request_id": "spawn-failure-request",
            "client_id": "validation-agent",
            "kind": "start_validation_job",
            "job_id": "spawn-failure-job",
            "cwd": temp.path(),
            "command": serde_json::to_string(&[ShellJobValidationStep {
                name: "check".into(),
                program: "cargo".into(),
                args: vec!["check".into(), "--all-targets".into()],
            env: Vec::new(),
            }]).unwrap(),
            "timeout_secs": 10,
            "requested_by": "test",
            "created_at": 1,
            "job_context": test_job_context(temp.path(), vec!["check".to_string()])
        }))
        .unwrap(),
    );
    let update = (0..100)
        .find_map(|_| {
            let update = rx.try_recv().ok().and_then(|envelope| match envelope {
                AgentEnvelope::JobUpdate { payload } if payload.finished => Some(payload),
                _ => None,
            });
            if update.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            update
        })
        .expect("validation spawn failure update");
    assert!(update.finished);
    assert_eq!(update.status, "failed");
    assert_eq!(update.exit_code, None);
    assert_eq!(
        update.error.as_deref(),
        Some(VALIDATION_STEP_SPAWN_FAILED_CODE)
    );
    assert_eq!(
        update.validation_progress,
        Some(ShellJobValidationProgress {
            completed: 0,
            current_step: None,
            failed_step: None,
        })
    );
}

#[cfg(unix)]
#[test]
fn python_module_probe_reports_tool_unavailable_without_running_recipe() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let python = temp.path().join("python");
    let probe_output = temp.path().join("module");
    std::fs::write(
        &python,
        "#!/bin/sh\nprintf '%s' \"$4\" > \"$PROBE_OUTPUT\"\nexit 42\n",
    )
    .unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut shell = ShellConfig::default();
    shell.env.insert(
        "PATH".to_string(),
        temp.path().to_string_lossy().into_owned(),
    );
    shell.env.insert(
        "PROBE_OUTPUT".to_string(),
        probe_output.to_string_lossy().into_owned(),
    );
    let step = ShellJobValidationStep {
        name: "test".into(),
        program: "python".into(),
        args: ["-B", "-m", "unittest", "discover", "-v"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        env: Vec::new(),
    };
    assert!(!validation_module_available(
        &shell,
        None,
        temp.path(),
        &step,
        None,
        None,
    ));
    assert_eq!(std::fs::read_to_string(&probe_output).unwrap(), "unittest");
    assert!(!temp.path().join("recipe-ran").exists());

    std::fs::remove_file(&probe_output).unwrap();
    let scratch = crate::command_sandbox::InspectScratch::create().unwrap();
    assert!(!validation_module_available(
        &shell,
        None,
        temp.path(),
        &step,
        Some(&scratch),
        None,
    ));
    assert!(
        !probe_output.exists(),
        "the inspect validation probe must not write outside scratch"
    );
}

#[cfg(target_os = "linux")]
pub(super) fn process_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(") ")
        .and_then(|(_, rest)| rest.chars().next())
        .is_some_and(|state| state != 'Z')
}

fn wait_until(timeout: Duration, condition: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    condition()
}
