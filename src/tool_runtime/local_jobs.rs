//! Local job records and process-group termination support.

use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub(crate) struct LocalJobRecord {
    pub(crate) project: String,
    pub(crate) dir: PathBuf,
    terminal_snapshot: Arc<Mutex<Option<LocalJobTerminalSnapshot>>>,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalJobTerminalSnapshot {
    files: HashMap<String, String>,
}

impl LocalJobRecord {
    pub(crate) fn new(project: String, dir: PathBuf) -> Self {
        Self {
            project,
            dir,
            terminal_snapshot: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn terminal_snapshot_handle(&self) -> Arc<Mutex<Option<LocalJobTerminalSnapshot>>> {
        self.terminal_snapshot.clone()
    }

    pub(crate) fn read_text(&self, name: &str) -> Option<String> {
        if let Some(snapshot) = self.terminal_snapshot.lock().unwrap().as_ref() {
            return snapshot.files.get(name).cloned();
        }
        std::fs::read_to_string(self.dir.join(name)).ok()
    }

    pub(crate) fn read_json(&self, name: &str) -> Value {
        self.read_text(name)
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Value::Null)
    }
}

/// Capture an inspect job's terminal files in memory, then release its only
/// scratch owner so the private directory is removed. Active jobs keep the
/// scratch alive; terminal status is published only after `finished_at`.
pub(crate) fn retain_inspect_job_until_terminal(
    dir: PathBuf,
    snapshot: Arc<Mutex<Option<LocalJobTerminalSnapshot>>>,
    scratch: crate::command_sandbox::InspectScratch,
    mut child: std::process::Child,
) {
    std::thread::spawn(move || {
        let exit = child.wait();
        let status = read_file(&dir.join("status")).unwrap_or_default();
        let finished = dir.join("finished_at").is_file();
        if ACTIVE_LOCAL_STATUSES.contains(&status.trim()) || !finished {
            // A signal or wrapper failure may prevent its terminal writes.
            // The child is gone, so publish a durable fallback before
            // snapshotting and deleting the scratch instead of leaking it.
            if !dir.join("exit_code").is_file() {
                if let Ok(exit) = &exit {
                    if let Some(code) = exit.code() {
                        let _ = std::fs::write(dir.join("exit_code"), code.to_string());
                    }
                }
            }
            let _ = std::fs::write(
                dir.join("finished_at"),
                chrono::Utc::now().timestamp().to_string(),
            );
            let _ = std::fs::write(dir.join("status"), "lost");
        }

        let mut files = HashMap::new();
        for name in [
            "metadata.json",
            "status",
            "exit_code",
            "finished_at",
            "stdout.log",
            "stderr.log",
            "pid",
        ] {
            if let Some(value) = read_file(&dir.join(name)) {
                files.insert(name.to_string(), value);
            }
        }
        *snapshot.lock().unwrap() = Some(LocalJobTerminalSnapshot { files });
        drop(scratch);
    });
}

fn read_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Local job statuses that are still active (not yet terminal). A stop/timeout
/// only acts on these; terminal jobs (`completed`/`failed`/`stopped`/`lost`)
/// are left untouched.
pub(crate) const ACTIVE_LOCAL_STATUSES: &[&str] = &["running", "queued", "started"];

/// Statuses counted as broadly "active" by runtime observability and bounded
/// summaries. `stop_requested` remains active for compatibility, but
/// lifecycle summaries classify it as nonblocking terminal-pending state.
pub(crate) const ACTIVE_JOB_STATUSES: &[&str] = &[
    "running",
    "queued",
    "started",
    "agent_queued",
    "stop_requested",
];

/// Outcome of attempting to terminate a local job's process group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TerminateOutcome {
    /// The process group was alive and was signalled. `escalated_to_kill` is
    /// true when SIGTERM did not suffice within the grace window and SIGKILL
    /// was sent to the whole group.
    Terminated { pgid: i64, escalated_to_kill: bool },
    /// No live process was found for the recorded pid (already exited).
    AlreadyGone,
}

/// Abstraction over terminating a local job's process group.
///
/// The production implementation shells out to `kill -TERM/-KILL -<pgid>`
/// (negative pid => whole process group). Local jobs are spawned with
/// `setsid`, which makes the wrapper shell a session and process-group
/// leader, so `-<pgid>` reaches the wrapper and every descendant it spawned
/// in a single signal, reliably reclaiming the whole subtree.
///
/// Tests inject a fake to assert the runtime targets the correct pgid without
/// spawning real processes. The runtime only ever passes pids/pgids read from
/// its own on-disk job files, never caller-supplied pids.
pub(crate) trait LocalJobKiller: Send + Sync {
    /// Terminate the process group led by `pid`/`pgid`. Sends SIGTERM, waits
    /// briefly, and escalates to SIGKILL if the leader is still alive. Never
    /// panics; a failure to signal is reflected as a `Terminated` outcome
    /// without escalation.
    fn terminate_group(&self, pid: i64, pgid: i64) -> TerminateOutcome;
}

/// Production `LocalJobKiller` backed by the `kill` shell command.
pub(crate) struct SystemJobKiller;

impl SystemJobKiller {
    /// True if a process with `pid` is currently alive (`kill -0`).
    fn is_alive(pid: i64) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Send `signal` (e.g. `-TERM`/`-KILL`) to the whole process group `pgid`
    /// (negative pid). Failures are swallowed: a non-existent group yields a
    /// non-zero exit which we treat as nothing left to signal.
    fn signal_group(pgid: i64, signal: &str) {
        match std::process::Command::new("kill")
            .arg(signal)
            .arg("--")
            .arg(format!("-{}", pgid))
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => {
                tracing::debug!(
                    pgid,
                    signal,
                    status = %status,
                    "local job process-group signal did not report success"
                );
            }
            Err(e) => {
                tracing::warn!(
                    pgid,
                    signal,
                    error = %e,
                    "failed to signal local job process group"
                );
            }
        }
    }
}

impl LocalJobKiller for SystemJobKiller {
    fn terminate_group(&self, pid: i64, pgid: i64) -> TerminateOutcome {
        if !Self::is_alive(pid) {
            return TerminateOutcome::AlreadyGone;
        }
        Self::signal_group(pgid, "-TERM");
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            if !Self::is_alive(pid) {
                return TerminateOutcome::Terminated {
                    pgid,
                    escalated_to_kill: false,
                };
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let escalated = Self::is_alive(pid);
        if escalated {
            Self::signal_group(pgid, "-KILL");
        }
        TerminateOutcome::Terminated {
            pgid,
            escalated_to_kill: escalated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspect_job_child_exit_publishes_fallback_and_cleans_scratch() {
        let scratch = crate::command_sandbox::InspectScratch::create().unwrap();
        let scratch_path = scratch.path().to_path_buf();
        let dir = scratch.path().join("job");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("status"), "running").unwrap();
        std::fs::write(dir.join("metadata.json"), "{}").unwrap();
        let record = LocalJobRecord::new("project".to_string(), dir.clone());
        let snapshot = record.terminal_snapshot_handle();
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 7")
            .spawn()
            .unwrap();

        retain_inspect_job_until_terminal(dir, snapshot, scratch, child);

        let deadline = Instant::now() + Duration::from_secs(2);
        while record.read_text("status").as_deref() != Some("lost") || scratch_path.exists() {
            assert!(
                Instant::now() < deadline,
                "terminal fallback or scratch cleanup timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(record.read_text("exit_code").as_deref(), Some("7"));
        assert!(record.read_text("finished_at").is_some());
    }
}
