use super::*;
use serde_json::json;

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
            child: Some(child.clone()),
            stop_requested: stop_requested.clone(),
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
            "created_at": 1
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
            "created_at": 1
        }))
        .unwrap(),
    );
    let update = (0..100)
        .find_map(|_| {
            let update = rx.try_recv().ok().and_then(|envelope| match envelope {
                AgentEnvelope::JobUpdate { payload } => Some(payload),
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
        &step
    ));
    assert_eq!(std::fs::read_to_string(probe_output).unwrap(), "unittest");
    assert!(!temp.path().join("recipe-ran").exists());
}

#[cfg(target_os = "linux")]
fn process_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(") ")
        .and_then(|(_, rest)| rest.chars().next())
        .is_some_and(|state| state != 'Z')
}
