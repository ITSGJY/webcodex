//! Bounded process execution for validation adapters.

use crate::validation_bridge::{
    sanitize_bridge_text, MAX_VALIDATION_STDERR_CAPTURE_BYTES, MAX_VALIDATION_STDERR_SUMMARY_CHARS,
    MAX_VALIDATION_STDOUT_BYTES,
};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct CapturedProcess {
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stdout_capped: bool,
    pub(crate) stderr_capped: bool,
    pub(crate) stderr_summary: Option<String>,
    pub(crate) duration_ms: u64,
    pub(crate) timed_out: bool,
    pub(crate) spawn_error: Option<String>,
    pub(crate) wait_error: Option<String>,
}

/// Run argv with bounded stdout capture. When stdout exceeds the hard byte cap,
/// `stdout_capped` is true and `stdout` is empty (complete JSON only — never a
/// truncated body intended for parsing).
pub(crate) fn run_bounded(
    program: &Path,
    args: &[String],
    cwd: &Path,
    timeout_secs: u64,
    shutdown: Option<&AtomicBool>,
) -> CapturedProcess {
    let start = Instant::now();
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("PYTHONSTARTUP")
        .env_remove("PYTHONPATH");
    #[cfg(unix)]
    command.process_group(0);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return CapturedProcess {
                exit_code: None,
                stdout: Vec::new(),
                stdout_capped: false,
                stderr_capped: false,
                stderr_summary: Some(bound_stderr(&format!("spawn failed: {error}"))),
                duration_ms: start.elapsed().as_millis() as u64,
                timed_out: false,
                spawn_error: Some(format!("spawn failed: {error}")),
                wait_error: None,
            };
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (stdout_tx, stdout_rx) = mpsc::channel::<(Vec<u8>, bool)>();
    if let Some(mut out) = stdout {
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut collected = Vec::new();
            let mut capped = false;
            loop {
                match out.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if collected.len() + n > MAX_VALIDATION_STDOUT_BYTES {
                            capped = true;
                            let mut discard = [0u8; 8192];
                            while let Ok(m) = out.read(&mut discard) {
                                if m == 0 {
                                    break;
                                }
                            }
                            break;
                        }
                        collected.extend_from_slice(&buf[..n]);
                    }
                    Err(_) => break,
                }
            }
            let _ = stdout_tx.send((if capped { Vec::new() } else { collected }, capped));
        });
    } else {
        let _ = stdout_tx.send((Vec::new(), false));
    }

    let (stderr_tx, stderr_rx) = mpsc::channel::<(Vec<u8>, bool)>();
    if let Some(mut err) = stderr {
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut collected = Vec::with_capacity(MAX_VALIDATION_STDERR_CAPTURE_BYTES);
            let mut capped = false;
            loop {
                match err.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let remaining =
                            MAX_VALIDATION_STDERR_CAPTURE_BYTES.saturating_sub(collected.len());
                        let retained = remaining.min(n);
                        collected.extend_from_slice(&buf[..retained]);
                        if retained < n {
                            capped = true;
                        }
                        // Keep draining after the cap so the child cannot block
                        // on a full stderr pipe. No further bytes are retained.
                    }
                    Err(_) => break,
                }
            }
            let _ = stderr_tx.send((collected, capped));
        });
    } else {
        let _ = stderr_tx.send((Vec::new(), false));
    }

    let process_group_id = child.id();
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let mut wait_error = None;
    let mut exit_status = None;
    let mut stopped = false;
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_status = Some(status);
                break false;
            }
            Ok(None) => {
                if shutdown.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                    stopped = true;
                    break false;
                }
                if start.elapsed() >= timeout {
                    break true;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                wait_error = Some(format!("wait failed: {error}"));
                break false;
            }
        }
    };

    let cleanup_deadline = Instant::now() + Duration::from_secs(1);
    match terminate_validation_child(&mut child, process_group_id, cleanup_deadline) {
        Ok(status) => {
            if exit_status.is_none() {
                exit_status = status;
            }
        }
        Err(error) => {
            if wait_error.is_none() {
                wait_error = Some(error);
            }
        }
    }
    if stopped && wait_error.is_none() {
        wait_error = Some("validation stopped during runner shutdown".to_string());
    }

    let exit_code = if timed_out {
        Some(-1)
    } else if wait_error.is_some() {
        None
    } else {
        exit_status.and_then(|status| status.code())
    };

    // Drain readers against one shared cleanup deadline rather than giving
    // stdout and stderr independent five-second waits.
    let remaining = cleanup_deadline.saturating_duration_since(Instant::now());
    let (stdout_bytes, stdout_capped) = stdout_rx
        .recv_timeout(remaining)
        .unwrap_or((Vec::new(), false));
    let remaining = cleanup_deadline.saturating_duration_since(Instant::now());
    let (stderr_bytes, stderr_capped) = stderr_rx
        .recv_timeout(remaining)
        .unwrap_or((Vec::new(), false));
    let stderr_text = String::from_utf8_lossy(&stderr_bytes);

    if timed_out {
        return CapturedProcess {
            exit_code,
            stdout: Vec::new(),
            stdout_capped: false,
            stderr_capped,
            stderr_summary: Some(bound_stderr(&format!(
                "command timed out after {timeout_secs} seconds"
            ))),
            duration_ms: start.elapsed().as_millis() as u64,
            timed_out: true,
            spawn_error: None,
            wait_error: None,
        };
    }

    if let Some(error) = wait_error {
        return CapturedProcess {
            exit_code: None,
            stdout: stdout_bytes,
            stdout_capped,
            stderr_capped,
            stderr_summary: Some(bound_stderr(&error)),
            duration_ms: start.elapsed().as_millis() as u64,
            timed_out: false,
            spawn_error: None,
            wait_error: Some(error),
        };
    }

    CapturedProcess {
        exit_code,
        stdout: stdout_bytes,
        stdout_capped,
        stderr_capped,
        stderr_summary: if stderr_text.trim().is_empty() {
            None
        } else {
            Some(bound_stderr(&stderr_text))
        },
        duration_ms: start.elapsed().as_millis() as u64,
        timed_out: false,
        spawn_error: None,
        wait_error: None,
    }
}

fn terminate_validation_child(
    child: &mut std::process::Child,
    process_group_id: u32,
    deadline: Instant,
) -> Result<Option<std::process::ExitStatus>, String> {
    let mut errors = Vec::new();
    #[cfg(unix)]
    {
        let grace_deadline = deadline.min(Instant::now() + Duration::from_millis(100));
        if signal_validation_group(process_group_id, libc::SIGTERM).is_err() {
            errors.push("validation process group SIGTERM failed");
        }
        while Instant::now() < grace_deadline && validation_group_exists(process_group_id) {
            thread::sleep(
                Duration::from_millis(10)
                    .min(grace_deadline.saturating_duration_since(Instant::now())),
            );
        }
        if validation_group_exists(process_group_id)
            && signal_validation_group(process_group_id, libc::SIGKILL).is_err()
        {
            errors.push("validation process group SIGKILL failed");
        }
    }
    #[cfg(not(unix))]
    if child.kill().is_err() {
        errors.push("validation child kill failed");
    }

    let mut status = None;
    loop {
        match child.try_wait() {
            Ok(Some(exit_status)) => {
                status = Some(exit_status);
                break;
            }
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    errors.push("validation child reap timed out");
                    break;
                }
                thread::sleep(Duration::from_millis(10).min(remaining));
            }
            Err(_) => {
                errors.push("validation child reap failed");
                break;
            }
        }
    }
    #[cfg(unix)]
    if validation_group_exists(process_group_id) {
        errors.push("validation process group did not exit before deadline");
    }
    if errors.is_empty() {
        Ok(status)
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(unix)]
fn signal_validation_group(process_group_id: u32, signal: i32) -> Result<bool, ()> {
    if process_group_id == 0 {
        return Err(());
    }
    let process_group_id = i32::try_from(process_group_id).map_err(|_| ())?;
    // SAFETY: run_bounded places each validation child in its own session.
    if unsafe { libc::kill(-process_group_id, signal) } == 0 {
        Ok(true)
    } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(())
    }
}

#[cfg(unix)]
fn validation_group_exists(process_group_id: u32) -> bool {
    signal_validation_group(process_group_id, 0).unwrap_or(true)
}

fn bound_stderr(text: &str) -> String {
    sanitize_bridge_text(text)
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .take(MAX_VALIDATION_STDERR_SUMMARY_CHARS)
        .collect()
}

/// Resolve an executable by env override then PATH search. Callers must not
/// expose the absolute executable path across the bridge.
pub(crate) fn resolve_executable(env_override: &str, executable_name: &str) -> Option<PathBuf> {
    if let Ok(value) = std::env::var(env_override) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            return crate::webcodex_runner::util::is_executable_file(&path).then_some(path);
        }
    }
    let path_var = std::env::var_os("PATH")?;
    crate::webcodex_runner::util::find_executable_in_path(executable_name, &path_var)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn env_override_requires_an_executable_file() {
        use std::os::unix::fs::PermissionsExt;

        const ENV: &str = "WEBCODEX_TEST_VALIDATION_EXECUTABLE";
        const MISSING_NAME: &str = "webcodex-validation-executable-that-does-not-exist";
        let temp = tempfile::tempdir().unwrap();

        std::env::set_var(ENV, temp.path());
        assert!(resolve_executable(ENV, MISSING_NAME).is_none());

        let file = temp.path().join("tool");
        std::fs::write(&file, "#!/bin/sh\nexit 0\n").unwrap();
        std::env::set_var(ENV, &file);
        assert!(resolve_executable(ENV, MISSING_NAME).is_none());

        let mut permissions = std::fs::metadata(&file).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&file, permissions).unwrap();
        assert_eq!(resolve_executable(ENV, MISSING_NAME), Some(file));
        std::env::remove_var(ENV);
    }
}
