//! Local job records and process-group termination support.

use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::helpers::{DEFAULT_JOB_LOG_TAIL_LINES, MAX_LOCAL_LOG_LINES};

pub(crate) const MAX_LOCAL_LOG_BYTES_PER_STREAM: usize = 1024 * 1024;
const LOCAL_LOG_READ_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct LocalJobRecord {
    pub(crate) project: String,
    pub(crate) dir: PathBuf,
    terminal_snapshot: Arc<Mutex<Option<LocalJobTerminalSnapshot>>>,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalJobTerminalSnapshot {
    files: HashMap<String, String>,
    logs: HashMap<String, LocalJobLogSnapshot>,
}

#[derive(Debug, Clone)]
struct LocalJobLogSnapshot {
    retained_text: String,
    total_lines: usize,
    first_retained_line: usize,
    truncated: bool,
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
            if let Some(log) = snapshot.logs.get(name) {
                return Some(log.retained_text.clone());
            }
            return snapshot.files.get(name).cloned();
        }
        std::fs::read_to_string(self.dir.join(name)).ok()
    }

    pub(crate) fn read_log_lines(
        &self,
        name: &str,
        offset: Option<usize>,
        tail_lines: Option<usize>,
    ) -> (String, usize, usize, bool) {
        if let Some(snapshot) = self.terminal_snapshot.lock().unwrap().as_ref() {
            return snapshot
                .logs
                .get(name)
                .map(|log| log.read_lines(offset, tail_lines))
                .unwrap_or_else(|| (String::new(), 1, 0, false));
        }
        super::helpers::read_lines_from_text(
            &std::fs::read_to_string(self.dir.join(name)).unwrap_or_default(),
            offset,
            tail_lines,
        )
    }

    pub(crate) fn read_json(&self, name: &str) -> Value {
        self.read_text(name)
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Value::Null)
    }
}

impl LocalJobLogSnapshot {
    fn read_lines(
        &self,
        offset: Option<usize>,
        tail_lines: Option<usize>,
    ) -> (String, usize, usize, bool) {
        let lines: Vec<&str> = self.retained_text.lines().collect();
        if self.total_lines == 0 {
            return (String::new(), 1, 0, false);
        }

        let (start_idx, limit) = if let Some(requested) = offset {
            let first_available = self.first_retained_line;
            let effective = requested.max(1).max(first_available);
            let start = if effective > self.total_lines {
                lines.len()
            } else {
                effective.saturating_sub(first_available).min(lines.len())
            };
            (start, MAX_LOCAL_LOG_LINES)
        } else {
            let tail = tail_lines
                .filter(|lines| *lines > 0)
                .map(|lines| lines.min(MAX_LOCAL_LOG_LINES))
                .unwrap_or(DEFAULT_JOB_LOG_TAIL_LINES);
            (lines.len().saturating_sub(tail), tail)
        };
        let end_idx = start_idx.saturating_add(limit).min(lines.len());
        let selected = lines[start_idx..end_idx].join("\n");
        let next_line = if start_idx >= lines.len() {
            self.total_lines.saturating_add(1)
        } else {
            self.first_retained_line.saturating_add(end_idx)
        };
        (
            selected,
            next_line,
            self.total_lines,
            self.truncated || start_idx > 0 || end_idx < lines.len(),
        )
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
        for name in ["metadata.json", "status", "exit_code", "finished_at", "pid"] {
            if let Some(value) = read_file(&dir.join(name)) {
                files.insert(name.to_string(), value);
            }
        }
        let logs = ["stdout.log", "stderr.log"]
            .into_iter()
            .map(|name| {
                (
                    name.to_string(),
                    read_bounded_log(&dir.join(name)).unwrap_or_default(),
                )
            })
            .collect();
        *snapshot.lock().unwrap() = Some(LocalJobTerminalSnapshot { files, logs });
        drop(scratch);
    });
}

fn read_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn read_bounded_log(path: &Path) -> std::io::Result<LocalJobLogSnapshot> {
    let mut file = File::open(path)?;
    let mut retained = VecDeque::with_capacity(MAX_LOCAL_LOG_BYTES_PER_STREAM);
    let mut buffer = [0_u8; LOCAL_LOG_READ_CHUNK_BYTES];
    let mut total_newlines = 0_usize;
    let mut last_byte = None;
    let mut truncated = false;

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        for &byte in &buffer[..read] {
            total_newlines = total_newlines.saturating_add(usize::from(byte == b'\n'));
            last_byte = Some(byte);
            if retained.len() == MAX_LOCAL_LOG_BYTES_PER_STREAM {
                retained.pop_front();
                truncated = true;
            }
            retained.push_back(byte);
        }
    }

    let total_lines =
        total_newlines.saturating_add(usize::from(last_byte.is_some_and(|byte| byte != b'\n')));
    let mut retained: Vec<u8> = retained.into_iter().collect();
    let retained_lines = byte_line_count(&retained);
    if retained_lines > MAX_LOCAL_LOG_LINES {
        let lines_to_drop = retained_lines - MAX_LOCAL_LOG_LINES;
        if let Some(end) = nth_newline(&retained, lines_to_drop) {
            retained.drain(..=end);
            truncated = true;
        }
    }

    let mut retained_text = String::from_utf8_lossy(&retained).into_owned();
    if retained_text.len() > MAX_LOCAL_LOG_BYTES_PER_STREAM {
        let mut start = retained_text.len() - MAX_LOCAL_LOG_BYTES_PER_STREAM;
        while !retained_text.is_char_boundary(start) {
            start += 1;
        }
        retained_text = retained_text[start..].to_string();
        truncated = true;
    }
    let retained_lines = retained_text.lines().count();
    let first_retained_line = if retained_lines == 0 {
        total_lines.saturating_add(1)
    } else {
        total_lines.saturating_sub(retained_lines).saturating_add(1)
    };

    Ok(LocalJobLogSnapshot {
        retained_text,
        total_lines,
        first_retained_line,
        truncated,
    })
}

fn byte_line_count(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .filter(|&&byte| byte == b'\n')
        .count()
        .saturating_add(usize::from(bytes.last().is_some_and(|byte| *byte != b'\n')))
}

fn nth_newline(bytes: &[u8], count: usize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let mut seen = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            seen += 1;
            if seen == count {
                return Some(index);
            }
        }
    }
    None
}

impl Default for LocalJobLogSnapshot {
    fn default() -> Self {
        Self {
            retained_text: String::new(),
            total_lines: 0,
            first_retained_line: 1,
            truncated: false,
        }
    }
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

    #[test]
    fn inspect_job_terminal_snapshot_bounds_multimegabyte_logs() {
        let scratch = crate::command_sandbox::InspectScratch::create().unwrap();
        let scratch_path = scratch.path().to_path_buf();
        let dir = scratch.path().join("job");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("status"), "completed").unwrap();
        std::fs::write(dir.join("finished_at"), "100").unwrap();
        std::fs::write(dir.join("exit_code"), "0").unwrap();
        std::fs::write(dir.join("metadata.json"), "{}").unwrap();

        let stdout_lines = 20_000;
        let stdout = (1..=stdout_lines)
            .map(|line| format!("{line:05}:{}\n", "x".repeat(120)))
            .collect::<String>();
        let stderr = format!("{}THE-END", "y".repeat(2 * MAX_LOCAL_LOG_BYTES_PER_STREAM));
        assert!(stdout.len() > 2 * MAX_LOCAL_LOG_BYTES_PER_STREAM);
        assert!(stderr.len() > 2 * MAX_LOCAL_LOG_BYTES_PER_STREAM);
        std::fs::write(dir.join("stdout.log"), &stdout).unwrap();
        std::fs::write(dir.join("stderr.log"), &stderr).unwrap();

        let record = LocalJobRecord::new("project".to_string(), dir.clone());
        let snapshot = record.terminal_snapshot_handle();
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .unwrap();
        retain_inspect_job_until_terminal(dir, snapshot.clone(), scratch, child);

        let deadline = Instant::now() + Duration::from_secs(3);
        while snapshot.lock().unwrap().is_none() || scratch_path.exists() {
            assert!(
                Instant::now() < deadline,
                "terminal snapshot or scratch cleanup timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        {
            let snapshot = snapshot.lock().unwrap();
            let snapshot = snapshot.as_ref().unwrap();
            assert!(!snapshot.files.contains_key("stdout.log"));
            assert!(!snapshot.files.contains_key("stderr.log"));
            let stdout_snapshot = snapshot.logs.get("stdout.log").unwrap();
            assert!(stdout_snapshot.retained_text.len() <= MAX_LOCAL_LOG_BYTES_PER_STREAM);
            assert!(stdout_snapshot.retained_text.lines().count() <= MAX_LOCAL_LOG_LINES);
            assert_eq!(stdout_snapshot.total_lines, stdout_lines);
            assert!(stdout_snapshot.first_retained_line > 1);
            assert!(stdout_snapshot.truncated);
            assert!(stdout_snapshot.retained_text.contains("20000:"));
            assert!(!stdout_snapshot.retained_text.contains("00001:"));

            let stderr_snapshot = snapshot.logs.get("stderr.log").unwrap();
            assert!(stderr_snapshot.retained_text.len() <= MAX_LOCAL_LOG_BYTES_PER_STREAM);
            assert_eq!(stderr_snapshot.total_lines, 1);
            assert_eq!(stderr_snapshot.first_retained_line, 1);
            assert!(stderr_snapshot.truncated);
            assert!(stderr_snapshot.retained_text.ends_with("THE-END"));
        }

        let (stdout_tail, cursor, total, truncated) =
            record.read_log_lines("stdout.log", Some(1), None);
        assert_eq!(stdout_tail.lines().count(), MAX_LOCAL_LOG_LINES);
        assert!(stdout_tail.starts_with("19501:"));
        assert_eq!(cursor, stdout_lines + 1);
        assert_eq!(total, stdout_lines);
        assert!(truncated);

        let (stderr_tail, cursor, total, truncated) =
            record.read_log_lines("stderr.log", Some(1), None);
        assert!(stderr_tail.len() <= MAX_LOCAL_LOG_BYTES_PER_STREAM);
        assert!(stderr_tail.ends_with("THE-END"));
        assert_eq!(cursor, 2);
        assert_eq!(total, 1);
        assert!(truncated);
        assert!(!scratch_path.exists());
    }
}
