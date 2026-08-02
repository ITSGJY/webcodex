use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::super::system::discover_internal_binary;
use super::profile::{
    atomic_write, ensure_private_directory, protect_secret_file, sha256_hex,
    validate_existing_regular_file, ProfileLock,
};

const CONNECT_MARKER_FILE: &str = "hosted-connect";
const RUNNER_STATE_FILE: &str = "runner.toml";
const RUNNER_LOG_FILE: &str = "runner.log";
static OWNED_RUNNER_CHILDREN: OnceLock<Mutex<HashMap<u32, Child>>> = OnceLock::new();

fn owned_runner_children() -> &'static Mutex<HashMap<u32, Child>> {
    OWNED_RUNNER_CHILDREN.get_or_init(|| Mutex::new(HashMap::new()))
}

fn reap_owned_runner(pid: u32) {
    let child = owned_runner_children().lock().unwrap().remove(&pid);
    if let Some(mut child) = child {
        let _ = child.wait();
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct RunnerState {
    pid: u32,
    process_start: String,
    executable: String,
    config: String,
    config_sha256: String,
    started_at: String,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalRunnerStateSummary {
    pub(crate) managed: bool,
    pub(crate) running: bool,
    pub(crate) pid: Option<u32>,
    pub(crate) log_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalRunnerServiceAction {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunnerStart {
    Started,
    Reused,
}

pub(crate) fn local_runner_profile_marker(state_dir: &Path) -> PathBuf {
    state_dir.join(CONNECT_MARKER_FILE)
}

fn local_runner_state_path(state_dir: &Path) -> PathBuf {
    state_dir.join(RUNNER_STATE_FILE)
}

pub(crate) fn local_runner_log_path(state_dir: &Path) -> PathBuf {
    state_dir.join(RUNNER_LOG_FILE)
}

pub(super) fn load_runner_state(state_dir: &Path) -> Result<Option<RunnerState>, String> {
    let path = local_runner_state_path(state_dir);
    if !path.exists() {
        return Ok(None);
    }
    validate_existing_regular_file(&path)?;
    let content = std::fs::read_to_string(&path)
        .map_err(|error| format!("failed to read runner state {}: {error}", path.display()))?;
    toml::from_str(&content)
        .map(Some)
        .map_err(|error| format!("failed to parse runner state {}: {error}", path.display()))
}

#[cfg(target_os = "linux")]
fn linux_process_start(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let remaining = stat.get(close + 2..)?;
    let fields = remaining.split_whitespace().collect::<Vec<_>>();
    if fields.first().copied() == Some("Z") {
        return None;
    }
    fields.get(19).map(|value| (*value).to_string())
}

#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<String> {
    linux_process_start(pid)
}

#[cfg(target_os = "linux")]
fn process_executable(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|path| path.to_string_lossy().to_string())
}

#[cfg(not(target_os = "linux"))]
fn process_start(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(not(target_os = "linux"))]
fn process_executable(_pid: u32) -> Option<String> {
    None
}

pub(super) fn process_matches(state: &RunnerState) -> bool {
    if state.pid <= 1 || process_start(state.pid).as_deref() != Some(&state.process_start) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        return process_executable(state.pid).as_deref() == Some(&state.executable);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = Command::new("ps")
            .args(["-p", &state.pid.to_string(), "-o", "command="])
            .output();
        output.is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains(&state.executable)
                && String::from_utf8_lossy(&output.stdout).contains(&state.config)
        })
    }
}

fn remove_stale_state(state_dir: &Path) -> Result<(), String> {
    let path = local_runner_state_path(state_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "failed to remove stale runner state {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn local_runner_state_summary(
    state_dir: &Path,
) -> Result<LocalRunnerStateSummary, String> {
    let state = load_runner_state(state_dir)?;
    let running = state.as_ref().is_some_and(process_matches);
    Ok(LocalRunnerStateSummary {
        managed: local_runner_profile_marker(state_dir).is_file(),
        running,
        pid: running.then(|| state.as_ref().unwrap().pid),
        log_path: local_runner_log_path(state_dir),
    })
}

fn open_runner_log(state_dir: &Path) -> Result<File, String> {
    let path = local_runner_log_path(state_dir);
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(|error| format!("failed to open runner log {}: {error}", path.display()))?;
    protect_secret_file(&path)?;
    Ok(file)
}

fn start_runner(
    runner_bin: &Path,
    config: &Path,
    state_dir: &Path,
    config_sha256: String,
) -> Result<(), String> {
    let executable = runner_bin.canonicalize().map_err(|error| {
        format!(
            "failed to resolve Runner binary {}: {error}",
            runner_bin.display()
        )
    })?;
    let config = config.canonicalize().map_err(|error| {
        format!(
            "failed to resolve Runner config {}: {error}",
            config.display()
        )
    })?;
    let log = open_runner_log(state_dir)?;
    let stderr = log
        .try_clone()
        .map_err(|error| format!("failed to clone Runner log handle: {error}"))?;
    let mut command = Command::new(&executable);
    command
        .arg("--config")
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .env("RUST_LOG", "info");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command.spawn().map_err(|error| {
        format!(
            "failed to start webcodex-runner {}: {error}",
            executable.display()
        )
    })?;
    std::thread::sleep(Duration::from_millis(150));
    if let Some(status) = child
        .try_wait()
        .map_err(|error| format!("failed to inspect new Runner process: {error}"))?
    {
        return Err(format!("webcodex-runner exited immediately with {status}"));
    }
    let pid = child.id();
    let process_start = (0..20)
        .find_map(|_| {
            let marker = process_start(pid);
            if marker.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            marker
        })
        .ok_or_else(|| "failed to capture the new Runner process identity".to_string())?;
    let process_executable =
        process_executable(pid).unwrap_or_else(|| executable.to_string_lossy().to_string());
    let state = RunnerState {
        pid,
        process_start,
        executable: process_executable,
        config: config.to_string_lossy().to_string(),
        config_sha256,
        started_at: chrono::Utc::now().to_rfc3339(),
    };
    let content = toml::to_string(&state)
        .map_err(|error| format!("failed to render Runner state: {error}"))?;
    if let Err(error) = atomic_write(
        &local_runner_state_path(state_dir),
        content.as_bytes(),
        true,
    ) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    owned_runner_children().lock().unwrap().insert(pid, child);
    Ok(())
}

#[cfg(unix)]
fn signal_process(pid: u32, signal: i32) -> Result<(), String> {
    if pid <= 1 {
        return Err("refusing to signal an invalid Runner pid".to_string());
    }
    let result = unsafe { libc::kill(pid as i32, signal) };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(format!("failed to signal Runner pid {pid}: {error}"))
        }
    }
}

#[cfg(not(unix))]
fn signal_process(pid: u32, _signal: i32) -> Result<(), String> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T"])
        .status()
        .map_err(|error| format!("failed to run taskkill for Runner pid {pid}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("taskkill failed for Runner pid {pid}"))
    }
}

pub(super) fn stop_runner_unlocked(state_dir: &Path) -> Result<bool, String> {
    let Some(state) = load_runner_state(state_dir)? else {
        return Ok(false);
    };
    if !process_matches(&state) {
        remove_stale_state(state_dir)?;
        return Ok(false);
    }
    #[cfg(unix)]
    signal_process(state.pid, libc::SIGTERM)?;
    #[cfg(not(unix))]
    signal_process(state.pid, 0)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !process_matches(&state) {
            reap_owned_runner(state.pid);
            remove_stale_state(state_dir)?;
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    #[cfg(unix)]
    signal_process(state.pid, libc::SIGKILL)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && process_matches(&state) {
        std::thread::sleep(Duration::from_millis(25));
    }
    reap_owned_runner(state.pid);
    remove_stale_state(state_dir)?;
    Ok(true)
}

pub(super) fn ensure_runner_unlocked(
    runner_bin: &Path,
    config: &Path,
    state_dir: &Path,
) -> Result<RunnerStart, String> {
    let config_bytes = std::fs::read(config)
        .map_err(|error| format!("failed to read Runner config {}: {error}", config.display()))?;
    let config_sha256 = sha256_hex(&config_bytes);
    if let Some(state) = load_runner_state(state_dir)? {
        if process_matches(&state) && state.config_sha256 == config_sha256 {
            return Ok(RunnerStart::Reused);
        }
        if process_matches(&state) {
            stop_runner_unlocked(state_dir)?;
        } else {
            remove_stale_state(state_dir)?;
        }
    }
    start_runner(runner_bin, config, state_dir, config_sha256)?;
    Ok(RunnerStart::Started)
}

pub(crate) fn run_local_runner_service(
    action: LocalRunnerServiceAction,
    config: &Path,
    state_dir: &Path,
    runner_bin: Option<&Path>,
) -> Result<String, String> {
    ensure_private_directory(state_dir)?;
    let _lock = ProfileLock::acquire(state_dir)?;
    match action {
        LocalRunnerServiceAction::Stop => {
            let stopped = stop_runner_unlocked(state_dir)?;
            Ok(if stopped {
                "Hosted Runner stopped.\n".to_string()
            } else {
                "Hosted Runner is not running.\n".to_string()
            })
        }
        LocalRunnerServiceAction::Start | LocalRunnerServiceAction::Restart => {
            if action == LocalRunnerServiceAction::Restart {
                stop_runner_unlocked(state_dir)?;
            }
            let runner = runner_bin
                .map(Path::to_path_buf)
                .or_else(|| discover_internal_binary("webcodex-runner"))
                .ok_or_else(|| {
                    "webcodex-runner was not found beside webcodex or in an absolute PATH entry"
                        .to_string()
                })?;
            let started = ensure_runner_unlocked(&runner, config, state_dir)?;
            Ok(format!(
                "Hosted Runner {}.\n  config: {}\n  logs:   {}\n",
                if started == RunnerStart::Started {
                    "started"
                } else {
                    "was already running"
                },
                config.display(),
                local_runner_log_path(state_dir).display()
            ))
        }
    }
}

pub(crate) fn run_local_runner_logs(
    state_dir: &Path,
    lines: u32,
    since: Option<&str>,
    follow: bool,
) -> Result<String, String> {
    let path = local_runner_log_path(state_dir);
    if since.is_some() {
        return Err(format!(
            "--since is available for systemd journal logs only; local Runner logs are at {}",
            path.display()
        ));
    }
    if follow {
        let status = Command::new("tail")
            .arg("-n")
            .arg(lines.to_string())
            .arg("-f")
            .arg(&path)
            .status()
            .map_err(|error| format!("failed to follow {}: {error}", path.display()))?;
        if !status.success() {
            return Err(format!("tail failed for {}", path.display()));
        }
        return Ok(String::new());
    }
    let mut content = String::new();
    File::open(&path)
        .and_then(|mut file| file.read_to_string(&mut content))
        .map_err(|error| format!("failed to read Runner log {}: {error}", path.display()))?;
    let all = content.lines().collect::<Vec<_>>();
    let start = all.len().saturating_sub(lines as usize);
    let mut output = all[start..].join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn local_runner_reuses_process_recovers_stale_pid_and_stops() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let runner = tmp.path().join("webcodex-runner");
        std::fs::write(
            &runner,
            "#!/bin/sh\ntrap 'exit 0' TERM INT\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = tmp.path().join("agent.toml");
        std::fs::write(&config, "server_url='http://example.test'\n").unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();

        assert_eq!(
            ensure_runner_unlocked(&runner, &config, &state).unwrap(),
            RunnerStart::Started
        );
        let first = load_runner_state(&state).unwrap().unwrap();
        assert_eq!(
            ensure_runner_unlocked(&runner, &config, &state).unwrap(),
            RunnerStart::Reused
        );
        assert_eq!(load_runner_state(&state).unwrap().unwrap().pid, first.pid);

        std::fs::write(
            &config,
            "server_url='http://example.test'\ntransport='websocket'\n",
        )
        .unwrap();
        assert_eq!(
            ensure_runner_unlocked(&runner, &config, &state).unwrap(),
            RunnerStart::Started
        );
        let restarted = load_runner_state(&state).unwrap().unwrap();
        assert_ne!(restarted.pid, first.pid);
        assert!(stop_runner_unlocked(&state).unwrap());
        let mut stale = restarted;
        stale.pid = std::process::id();
        stale.process_start = "not-this-process".to_string();
        atomic_write(
            &local_runner_state_path(&state),
            toml::to_string(&stale).unwrap().as_bytes(),
            true,
        )
        .unwrap();
        assert_eq!(
            ensure_runner_unlocked(&runner, &config, &state).unwrap(),
            RunnerStart::Started
        );
        assert_ne!(load_runner_state(&state).unwrap().unwrap().pid, stale.pid);
        assert!(stop_runner_unlocked(&state).unwrap());
        assert!(!local_runner_state_summary(&state).unwrap().running);
    }

    #[cfg(unix)]
    #[test]
    fn immediate_runner_failure_does_not_leave_active_state() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let runner = tmp.path().join("webcodex-runner");
        std::fs::write(&runner, "#!/bin/sh\nexit 23\n").unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = tmp.path().join("agent.toml");
        std::fs::write(&config, "server_url='http://example.test'\n").unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();

        let error = ensure_runner_unlocked(&runner, &config, &state).unwrap_err();
        assert!(error.contains("exited immediately"), "{error}");
        assert!(!local_runner_state_path(&state).exists());
    }
}
