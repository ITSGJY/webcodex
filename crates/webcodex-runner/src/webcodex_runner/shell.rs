use super::config::{validate_shell_config, AgentPolicy, ShellConfig, ShellProfileConfig};
use super::output::CommandResult;
use super::projects::find_project_shell_context;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const SHELL_PROFILE_PREPARE_TIMEOUT_SECS: u64 = 30;
const PROCESS_GROUP_TERMINATION_GRACE: Duration = Duration::from_millis(50);
const PROFILE_PREPARE_PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PreparedShellProfileKey {
    generation: u64,
    project_key: String,
    profile_name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedShellProfile {
    pub(crate) profile_name: String,
    program: String,
    args: Vec<String>,
    env_snapshot: HashMap<String, String>,
}

/// Lazily prepared shell environment snapshots. Snapshots are keyed by
/// config generation, project/cwd, and profile name because inline init
/// scripts such as `. .venv/bin/activate` are intentionally resolved from the
/// project cwd. A successful hot reload retires older cached generations after
/// the new generation prepares its first snapshot.
#[derive(Debug, Clone, Default)]
pub(crate) struct PreparedShellProfileCache {
    profiles: Arc<Mutex<HashMap<PreparedShellProfileKey, Arc<PreparedShellProfile>>>>,
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn should_inherit_env_key(key: &str) -> bool {
    !matches!(
        key,
        "WEBCODEX_TOKEN" | "WEBCODEX_AGENT_TOKEN" | "WEBCODEX_USER_TOKEN" | "AUTHORIZATION"
    )
}

fn shell_command_text(shell: &ShellConfig, command: &str) -> String {
    match shell.init_script.as_ref() {
        Some(path) => format!(
            ". {} && (\n{}\n)",
            shell_quote(&path.to_string_lossy()),
            command
        ),
        None => command.to_string(),
    }
}

fn apply_shell_environment(cmd: &mut Command, shell: &ShellConfig) -> Result<(), String> {
    for key in [
        "WEBCODEX_TOKEN",
        "WEBCODEX_AGENT_TOKEN",
        "WEBCODEX_USER_TOKEN",
        "AUTHORIZATION",
    ] {
        cmd.env_remove(key);
    }
    if !shell.path_prepend.is_empty() {
        let mut paths = shell.path_prepend.clone();
        if let Some(current) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&current));
        }
        let joined = std::env::join_paths(paths)
            .map_err(|e| format!("failed to build shell PATH from shell.path_prepend: {}", e))?;
        cmd.env("PATH", joined);
    }
    for (key, value) in &shell.env {
        cmd.env(key, value);
    }
    Ok(())
}

fn apply_env_snapshot(cmd: &mut Command, env_snapshot: &HashMap<String, String>) {
    cmd.env_clear();
    for (key, value) in env_snapshot {
        cmd.env(key, value);
    }
}

fn configured_shell_command(shell: &ShellConfig, command: &str) -> Result<Command, String> {
    validate_shell_config(shell)?;
    let mut cmd = Command::new(&shell.program);
    for arg in &shell.args {
        cmd.arg(arg);
    }
    cmd.arg(shell_command_text(shell, command));
    configure_direct_process_group(&mut cmd);
    apply_shell_environment(&mut cmd, shell)?;
    Ok(cmd)
}

fn configured_prepared_shell_command(
    profile: &PreparedShellProfile,
    command: &str,
) -> Result<Command, String> {
    let mut cmd = Command::new(&profile.program);
    for arg in &profile.args {
        cmd.arg(arg);
    }
    cmd.arg(command);
    configure_direct_process_group(&mut cmd);
    apply_env_snapshot(&mut cmd, &profile.env_snapshot);
    Ok(cmd)
}

pub(crate) fn configured_shell_job_command(
    shell: &ShellConfig,
    command: &str,
) -> Result<Command, String> {
    validate_shell_config(shell)?;
    let mut cmd = Command::new(&shell.program);
    for arg in &shell.args {
        cmd.arg(arg);
    }
    cmd.arg(shell_command_text(shell, command));
    // Establish the private group before `Command::spawn` returns. Executing
    // an external `setsid` wrapper left a race where shutdown could signal a
    // group that the wrapper had not created yet, then lose the group id.
    configure_direct_process_group(&mut cmd);
    apply_shell_environment(&mut cmd, shell)?;
    Ok(cmd)
}

pub(crate) fn configured_prepared_shell_job_command(
    profile: &PreparedShellProfile,
    command: &str,
) -> Result<Command, String> {
    let mut cmd = Command::new(&profile.program);
    for arg in &profile.args {
        cmd.arg(arg);
    }
    cmd.arg(command);
    configure_direct_process_group(&mut cmd);
    apply_env_snapshot(&mut cmd, &profile.env_snapshot);
    Ok(cmd)
}

pub(crate) fn configured_validation_job_command(
    shell: &ShellConfig,
    profile: Option<&PreparedShellProfile>,
    program: &str,
    args: &[String],
) -> Result<Command, String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    configure_direct_process_group(&mut cmd);
    match profile {
        Some(profile) => apply_env_snapshot(&mut cmd, &profile.env_snapshot),
        None => {
            validate_shell_config(shell)?;
            apply_shell_environment(&mut cmd, shell)?;
        }
    }
    Ok(cmd)
}

fn configure_direct_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe and touches no Rust-managed
        // memory in the post-fork child.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
}

fn base_shell_env(
    shell: &ShellConfig,
    profile: &ShellProfileConfig,
) -> Result<HashMap<String, String>, String> {
    let mut env: HashMap<String, String> = std::env::vars()
        .filter(|(key, _)| should_inherit_env_key(key))
        .collect();
    if !shell.path_prepend.is_empty() {
        let mut paths = shell.path_prepend.clone();
        if let Some(current) = env.get("PATH") {
            paths.extend(std::env::split_paths(current));
        }
        let joined = std::env::join_paths(paths)
            .map_err(|e| format!("failed to build shell PATH from shell.path_prepend: {}", e))?;
        env.insert("PATH".to_string(), joined.to_string_lossy().to_string());
    }
    for (key, value) in &shell.env {
        env.insert(key.clone(), value.clone());
    }
    for (key, value) in &profile.env {
        env.insert(key.clone(), value.clone());
    }
    for key in [
        "WEBCODEX_TOKEN",
        "WEBCODEX_AGENT_TOKEN",
        "WEBCODEX_USER_TOKEN",
        "AUTHORIZATION",
    ] {
        env.remove(key);
    }
    Ok(env)
}

fn stderr_tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).to_string();
    const MAX_ERR: usize = 4096;
    if text.len() <= MAX_ERR {
        return text;
    }
    let mut start = text.len() - MAX_ERR;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[stderr truncated]\n{}", &text[start..])
}

struct ProfilePreparePipeReader {
    stream_name: &'static str,
    result_rx: mpsc::Receiver<Result<Vec<u8>, String>>,
    handle: std::thread::JoinHandle<()>,
}

impl ProfilePreparePipeReader {
    fn finish_until(self, deadline: Instant) -> Result<Vec<u8>, String> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.result_rx.recv_timeout(remaining) {
            Ok(result) => {
                join_profile_prepare_reader_until(self.handle, deadline, self.stream_name)?;
                result
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
                "profile prepare {} reader did not finish before the cleanup deadline",
                self.stream_name
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                join_profile_prepare_reader_until(self.handle, deadline, self.stream_name)?;
                Err(format!(
                    "profile prepare {} reader exited without a result",
                    self.stream_name
                ))
            }
        }
    }
}

fn join_profile_prepare_reader_until(
    handle: std::thread::JoinHandle<()>,
    deadline: Instant,
    stream_name: &'static str,
) -> Result<(), String> {
    while !handle.is_finished() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "profile prepare {stream_name} reader did not join before the cleanup deadline"
            ));
        }
        std::thread::sleep(Duration::from_millis(5).min(remaining));
    }
    handle
        .join()
        .map_err(|_| format!("profile prepare {stream_name} reader panicked"))
}

fn spawn_profile_prepare_pipe_reader(
    stream_name: &'static str,
    mut pipe: impl Read + Send + 'static,
) -> ProfilePreparePipeReader {
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let result = pipe
            .read_to_end(&mut buf)
            .map(|_| buf)
            .map_err(|e| format!("failed to read profile prepare {stream_name}: {e}"));
        let _ = result_tx.send(result);
    });
    ProfilePreparePipeReader {
        stream_name,
        result_rx,
        handle,
    }
}

fn collect_profile_prepare_output(
    stdout: ProfilePreparePipeReader,
    stderr: ProfilePreparePipeReader,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let deadline = Instant::now() + PROFILE_PREPARE_PIPE_DRAIN_TIMEOUT;
    let stdout = stdout.finish_until(deadline)?;
    let stderr = stderr.finish_until(deadline)?;
    Ok((stdout, stderr))
}

fn run_prepare_command(
    mut cmd: Command,
    timeout: Duration,
    stop_requested: Option<&AtomicBool>,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), String> {
    if stop_requested.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        return Err("profile prepare stopped during runner shutdown".to_string());
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn profile prepare command: {}", e))?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let cleanup = terminate_child_without_output(child).err();
            return Err(with_cleanup_error(
                "profile prepare stdout pipe missing",
                cleanup,
            ));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(stdout);
            let cleanup = terminate_child_without_output(child).err();
            return Err(with_cleanup_error(
                "profile prepare stderr pipe missing",
                cleanup,
            ));
        }
    };
    let stdout_reader = spawn_profile_prepare_pipe_reader("stdout", stdout);
    let stderr_reader = spawn_profile_prepare_pipe_reader("stderr", stderr);
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if stop_requested.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                    let cleanup = terminate_child_process_tree(&mut child).err();
                    let output = collect_profile_prepare_output(stdout_reader, stderr_reader).err();
                    return Err(with_cleanup_error(
                        output.map_or_else(
                            || "profile prepare stopped during runner shutdown".to_string(),
                            |error| {
                                format!(
                                    "profile prepare stopped during runner shutdown; failed to collect output: {error}"
                                )
                            },
                        ),
                        cleanup,
                    ));
                }
                if start.elapsed() >= timeout {
                    let cleanup = terminate_child_process_tree(&mut child).err();
                    return match collect_profile_prepare_output(stdout_reader, stderr_reader) {
                        Ok((_stdout, stderr)) => Err(format!(
                            "profile prepare timed out after {} seconds; stderr tail: {}{}",
                            timeout.as_secs(),
                            stderr_tail(&stderr),
                            cleanup
                                .as_deref()
                                .map(|error| format!("; cleanup failed: {error}"))
                                .unwrap_or_default(),
                        )),
                        Err(error) => Err(with_cleanup_error(
                            format!(
                                "profile prepare timed out after {} seconds; failed to collect output: {}",
                                timeout.as_secs(),
                                error
                            ),
                            cleanup,
                        )),
                    };
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let cleanup = terminate_child_process_tree(&mut child).err();
                let output = collect_profile_prepare_output(stdout_reader, stderr_reader).err();
                let base = match output {
                    Some(error) => {
                        format!("failed to wait profile prepare command: {}; failed to collect output: {}", e, error)
                    }
                    None => format!("failed to wait profile prepare command: {}", e),
                };
                return Err(with_cleanup_error(base, cleanup));
            }
        }
    };
    // The direct child has already exited, but its private process group can
    // still contain background descendants that inherited these pipe handles.
    // Reap that group before waiting on the readers so they see EOF promptly.
    let cleanup = terminate_child_process_tree(&mut child).err();
    let output = collect_profile_prepare_output(stdout_reader, stderr_reader);
    match (cleanup, output) {
        (None, Ok((stdout, stderr))) => Ok((status, stdout, stderr)),
        (Some(cleanup), Ok(_)) => Err(format!(
            "failed to clean up profile prepare command process group: {cleanup}"
        )),
        (None, Err(error)) => Err(format!("failed to collect profile prepare output: {error}")),
        (Some(cleanup), Err(error)) => Err(format!(
            "failed to clean up profile prepare command process group: {cleanup}; failed to collect output: {error}"
        )),
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_env_payload(
    payload: &[u8],
    profile_name: &str,
) -> Result<HashMap<String, String>, String> {
    let mut env = HashMap::new();
    for entry in payload.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        let Some(eq) = entry.iter().position(|byte| *byte == b'=') else {
            return Err(format!(
                "failed to parse env snapshot for profile '{}': entry missing '='",
                profile_name
            ));
        };
        let key = std::str::from_utf8(&entry[..eq]).map_err(|_| {
            format!(
                "failed to parse env snapshot for profile '{}': key is not UTF-8",
                profile_name
            )
        })?;
        if key.is_empty() {
            return Err(format!(
                "failed to parse env snapshot for profile '{}': empty env key",
                profile_name
            ));
        }
        let value = std::str::from_utf8(&entry[eq + 1..]).map_err(|_| {
            format!(
                "failed to parse env snapshot for profile '{}': value is not UTF-8",
                profile_name
            )
        })?;
        if should_inherit_env_key(key) {
            env.insert(key.to_string(), value.to_string());
        }
    }
    Ok(env)
}

fn capture_profile_env_snapshot(
    profile_name: &str,
    profile: &ShellProfileConfig,
    program: &str,
    args: &[String],
    prepare_cwd: &Path,
    initial_env: HashMap<String, String>,
    stop_requested: Option<&AtomicBool>,
) -> Result<HashMap<String, String>, String> {
    let Some(init_script) = profile.init_script.as_deref() else {
        return Ok(initial_env);
    };
    let marker = format!("__WEBCODEX_ENV_START_{}__", uuid::Uuid::new_v4().simple());
    let prepare_script = format!(
        "set -e\n{}\nprintf '\\n{}\\n'\nenv -0\n",
        init_script, marker
    );
    let mut cmd = Command::new(program);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.arg(prepare_script).current_dir(prepare_cwd).env_clear();
    configure_direct_process_group(&mut cmd);
    for (key, value) in initial_env {
        cmd.env(key, value);
    }
    let (status, stdout, stderr) = run_prepare_command(
        cmd,
        Duration::from_secs(SHELL_PROFILE_PREPARE_TIMEOUT_SECS),
        stop_requested,
    )
    .map_err(|e| {
        format!(
            "failed to prepare shell profile '{}' at {}: {}",
            profile_name,
            prepare_cwd.display(),
            e
        )
    })?;
    if !status.success() {
        return Err(format!(
            "failed to prepare shell profile '{}' at {}: exit code {}; stderr tail: {}",
            profile_name,
            prepare_cwd.display(),
            status.code().unwrap_or(-1),
            stderr_tail(&stderr)
        ));
    }
    let marker_pos = find_bytes(&stdout, marker.as_bytes()).ok_or_else(|| {
        format!(
            "failed to prepare shell profile '{}' at {}: env marker not found",
            profile_name,
            prepare_cwd.display()
        )
    })?;
    let mut payload_start = marker_pos + marker.len();
    while stdout
        .get(payload_start)
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        payload_start += 1;
    }
    let mut snapshot = parse_env_payload(&stdout[payload_start..], profile_name)?;
    for key in [
        "WEBCODEX_TOKEN",
        "WEBCODEX_AGENT_TOKEN",
        "WEBCODEX_USER_TOKEN",
        "AUTHORIZATION",
    ] {
        snapshot.remove(key);
    }
    Ok(snapshot)
}

impl PreparedShellProfileCache {
    /// Number of currently prepared snapshots. Used only for the sanitized
    /// observability summary; never exposes snapshot contents.
    pub(crate) fn len(&self) -> usize {
        self.profiles.lock().unwrap().len()
    }

    fn get_or_prepare(
        &self,
        generation: u64,
        shell: &ShellConfig,
        profile_name: &str,
        project_key: String,
        prepare_cwd: &Path,
        stop_requested: Option<&AtomicBool>,
    ) -> Result<Arc<PreparedShellProfile>, String> {
        let key = PreparedShellProfileKey {
            generation,
            project_key,
            profile_name: profile_name.to_string(),
        };
        let profiles = self.profiles.lock().unwrap();
        if let Some(prepared) = profiles.get(&key).cloned() {
            return Ok(prepared);
        }
        drop(profiles);
        let profile = shell.profiles.get(profile_name).ok_or_else(|| {
            format!(
                "shell profile '{}' is not configured for project/cwd {}",
                profile_name,
                prepare_cwd.display()
            )
        })?;
        let program = profile
            .program
            .clone()
            .unwrap_or_else(|| shell.program.clone());
        let args = profile.args.clone().unwrap_or_else(|| shell.args.clone());
        let initial_env = base_shell_env(shell, profile)?;
        let env_snapshot = capture_profile_env_snapshot(
            profile_name,
            profile,
            &program,
            &args,
            prepare_cwd,
            initial_env,
            stop_requested,
        )?;
        let prepared = Arc::new(PreparedShellProfile {
            profile_name: profile_name.to_string(),
            program,
            args,
            env_snapshot,
        });
        let mut profiles = self.profiles.lock().unwrap();
        if let Some(cached) = profiles.get(&key).cloned() {
            return Ok(cached);
        }
        if profiles.keys().any(|cached| cached.generation > generation) {
            return Ok(prepared);
        }
        profiles.retain(|cached, _| cached.generation == generation);
        profiles.insert(key, prepared.clone());
        Ok(prepared)
    }
}

fn shell_profile_project_key(project_id: Option<&str>, path: &Path) -> String {
    let path = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string();
    match project_id {
        Some(id) => format!("project:{}:{}", id, path),
        None => format!("cwd:{}", path),
    }
}

pub(crate) fn resolve_prepared_shell_profile(
    generation: u64,
    shell: &ShellConfig,
    projects_dir: &Path,
    cwd_path: &Path,
    request_has_cwd: bool,
    cache: &PreparedShellProfileCache,
    stop_requested: Option<&AtomicBool>,
) -> Result<Option<Arc<PreparedShellProfile>>, String> {
    let project = request_has_cwd
        .then(|| find_project_shell_context(projects_dir, cwd_path))
        .flatten();
    let profile_name = project
        .as_ref()
        .and_then(|project| project.shell_profile.as_deref())
        .or(shell.default_profile.as_deref());
    let Some(profile_name) = profile_name else {
        return Ok(None);
    };
    let prepare_cwd = project
        .as_ref()
        .map(|project| PathBuf::from(&project.path))
        .unwrap_or_else(|| cwd_path.to_path_buf());
    if let Some(project) = &project {
        if project.shell_profile.as_deref() == Some(profile_name)
            && !shell.profiles.contains_key(profile_name)
        {
            return Err(format!(
                "project '{}' shell_profile '{}' does not match any shell.profiles entry",
                project.id, profile_name
            ));
        }
    }
    let project_key = shell_profile_project_key(
        project.as_ref().map(|project| project.id.as_str()),
        &prepare_cwd,
    );
    cache
        .get_or_prepare(
            generation,
            shell,
            profile_name,
            project_key,
            &prepare_cwd,
            stop_requested,
        )
        .map(Some)
}

pub(crate) fn canonicalize_existing(path: &Path) -> Result<PathBuf, String> {
    path.canonicalize()
        .map_err(|e| format!("failed to access {}: {}", path.display(), e))
}

pub(crate) fn cwd_allowed(policy: &AgentPolicy, cwd: &Path) -> Result<(), String> {
    if policy.allow_cwd_anywhere {
        return Ok(());
    }
    let cwd = canonicalize_existing(cwd)?;
    for root in &policy.allowed_roots {
        let root = canonicalize_existing(root)?;
        if cwd == root || cwd.starts_with(&root) {
            return Ok(());
        }
    }
    Err(format!(
        "cwd {} is outside allowed_roots",
        cwd.to_string_lossy()
    ))
}

fn truncate_bytes(bytes: &[u8], max: usize) -> String {
    let text = String::from_utf8_lossy(bytes).to_string();
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "[output truncated to last {} bytes]\n{}",
        max,
        &text[start..]
    )
}

fn with_cleanup_error(base: impl Into<String>, cleanup: Option<String>) -> String {
    match cleanup {
        Some(cleanup) => format!("{}; cleanup failed: {}", base.into(), cleanup),
        None => base.into(),
    }
}

#[cfg(unix)]
fn signal_process_group(pgid: u32, signal: i32) -> Result<bool, String> {
    let target = i32::try_from(pgid)
        .map_err(|_| format!("process-group id {pgid} exceeds the supported range"))?;
    // SAFETY: callers use only the private session/process group created for
    // this command by `configure_direct_process_group`. A negative target is
    // required by POSIX to signal the whole group, not just its leader.
    if unsafe { libc::kill(-target, signal) } == 0 {
        Ok(true)
    } else {
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Err(format!(
                "permission denied signaling command process group {pgid} with signal {signal}"
            )),
            _ => Err(format!(
                "failed to signal command process group {pgid} with signal {signal}: {}",
                std::io::Error::last_os_error()
            )),
        }
    }
}

/// Terminate a command and all members of its private process group, then
/// reap the direct child. Callers do this before waiting for output pipes, so
/// descendants cannot keep them open after a timeout, stop, executor failure,
/// or direct-child exit.
fn terminate_child_process_tree(child: &mut std::process::Child) -> Result<(), String> {
    terminate_child_process_tree_until(child, Instant::now() + Duration::from_secs(1))
}

fn terminate_child_process_tree_until(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Result<(), String> {
    let mut errors = Vec::new();
    #[cfg(unix)]
    {
        // `configure_direct_process_group` calls `setsid` before exec, making
        // this pid the private session and process-group leader. Guard zero so
        // a malformed Child can never turn into a signal for the runner's own
        // process group.
        let pgid = child.id();
        if pgid == 0 {
            errors.push("command child has invalid process-group id 0".to_string());
        } else {
            let sent_sigterm = match signal_process_group(pgid, libc::SIGTERM) {
                Ok(true) => {
                    let grace_deadline =
                        deadline.min(Instant::now() + PROCESS_GROUP_TERMINATION_GRACE);
                    while Instant::now() < grace_deadline {
                        match signal_process_group(pgid, 0) {
                            Ok(false) => break,
                            Ok(true) => {}
                            Err(error) => {
                                errors.push(error);
                                break;
                            }
                        }
                        std::thread::sleep(
                            Duration::from_millis(10)
                                .min(grace_deadline.saturating_duration_since(Instant::now())),
                        );
                    }
                    true
                }
                // If the group is already gone, do not probe this numeric ID
                // again: a later probe could observe an unrelated, reused
                // process-group ID.
                Ok(false) => false,
                Err(error) => {
                    errors.push(error);
                    false
                }
            };
            if sent_sigterm {
                match signal_process_group(pgid, 0) {
                    Ok(true) => {
                        if let Err(error) = signal_process_group(pgid, libc::SIGKILL) {
                            errors.push(error);
                        }
                    }
                    Ok(false) => {}
                    Err(error) => errors.push(error),
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Non-Unix platforms retain the existing direct-child behavior; they
        // do not have the POSIX process-group signalling used above.
        if let Err(error) = child.kill() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => errors.push(format!("failed to kill command: {error}")),
            }
        }
    }
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    errors.push("command child reap timed out".to_string());
                    break;
                }
                std::thread::sleep(Duration::from_millis(10).min(remaining));
            }
            Err(error) => {
                errors.push(format!("failed to reap command child: {error}"));
                break;
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn terminate_child_without_output(mut child: std::process::Child) -> Result<(), String> {
    let result = terminate_child_process_tree(&mut child);
    // The direct child has been reaped above. Closing the local pipe handles
    // is sufficient on error paths where the response intentionally has no
    // command output.
    drop(child.stdout.take());
    drop(child.stderr.take());
    result
}

fn terminate_and_read_pipes(
    mut child: std::process::Child,
    max_output_bytes: usize,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), String> {
    let deadline = Instant::now() + Duration::from_secs(1);
    let cleanup = terminate_child_process_tree_until(&mut child, deadline).err();
    let output = read_pipes_until(child, max_output_bytes, deadline);
    match (cleanup, output) {
        (None, Ok(output)) => Ok(output),
        (Some(cleanup), Ok(_)) => Err(format!(
            "failed to terminate command process tree: {cleanup}"
        )),
        (None, Err(error)) => Err(error),
        (Some(cleanup), Err(error)) => Err(format!(
            "failed to terminate command process tree: {cleanup}; failed to collect output: {error}"
        )),
    }
}

fn read_pipes_until(
    mut child: std::process::Child,
    max_output_bytes: usize,
    deadline: Instant,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "stdout pipe missing".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "stderr pipe missing".to_string())?;
    let (stdout_tx, stdout_rx) = mpsc::sync_channel(1);
    let stdout_handle = std::thread::spawn(move || {
        let _ = stdout_tx.send(read_bounded_pipe_tail(stdout, max_output_bytes, "stdout"));
    });
    let (stderr_tx, stderr_rx) = mpsc::sync_channel(1);
    let stderr_handle = std::thread::spawn(move || {
        let _ = stderr_tx.send(read_bounded_pipe_tail(stderr, max_output_bytes, "stderr"));
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err("command child wait timed out".to_string());
                }
                std::thread::sleep(Duration::from_millis(10).min(remaining));
            }
            Err(error) => return Err(format!("failed to wait command: {error}")),
        }
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let stdout = stdout_rx
        .recv_timeout(remaining)
        .map_err(|_| "stdout reader did not finish before cleanup deadline".to_string())??;
    let remaining = deadline.saturating_duration_since(Instant::now());
    let stderr = stderr_rx
        .recv_timeout(remaining)
        .map_err(|_| "stderr reader did not finish before cleanup deadline".to_string())??;
    if stdout_handle.is_finished() {
        let _ = stdout_handle.join();
    }
    if stderr_handle.is_finished() {
        let _ = stderr_handle.join();
    }
    Ok((status, stdout, stderr))
}

fn read_bounded_pipe_tail(
    mut pipe: impl Read,
    max_bytes: usize,
    stream_name: &'static str,
) -> Result<Vec<u8>, String> {
    let retained_limit = max_bytes.saturating_add(1);
    let mut output = Vec::with_capacity(retained_limit.min(64 * 1024));
    let mut chunk = [0_u8; 8192];
    loop {
        let read = pipe
            .read(&mut chunk)
            .map_err(|error| format!("failed to read {stream_name}: {error}"))?;
        if read == 0 {
            return Ok(output);
        }
        output.extend_from_slice(&chunk[..read]);
        if output.len() > retained_limit {
            let discard = output.len() - retained_limit;
            output.drain(..discard);
        }
    }
}

// Test-only wrapper for callers that do not need prepared shell profiles; the
// production request path uses `run_shell_with_profiles` directly.
#[cfg(test)]
pub(crate) fn run_shell(
    policy: &AgentPolicy,
    shell: &ShellConfig,
    cwd: Option<&str>,
    command: &str,
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
) -> CommandResult {
    run_shell_impl(
        policy,
        shell,
        None,
        cwd,
        command,
        stdin,
        timeout_secs,
        stop_requested,
        None,
    )
}

#[cfg(test)]
pub(crate) fn run_shell_with_profiles(
    generation: u64,
    policy: &AgentPolicy,
    shell: &ShellConfig,
    projects_dir: &Path,
    cache: &PreparedShellProfileCache,
    cwd: Option<&str>,
    command: &str,
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
) -> CommandResult {
    run_shell_with_profiles_in_sandbox(
        generation,
        policy,
        shell,
        projects_dir,
        cache,
        cwd,
        command,
        stdin,
        timeout_secs,
        stop_requested,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_shell_with_profiles_in_sandbox(
    generation: u64,
    policy: &AgentPolicy,
    shell: &ShellConfig,
    projects_dir: &Path,
    cache: &PreparedShellProfileCache,
    cwd: Option<&str>,
    command: &str,
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
    sandbox: Option<&str>,
) -> CommandResult {
    run_shell_impl(
        policy,
        shell,
        Some((generation, projects_dir, cache)),
        cwd,
        command,
        stdin,
        timeout_secs,
        stop_requested,
        sandbox,
    )
}

fn run_shell_impl(
    policy: &AgentPolicy,
    shell: &ShellConfig,
    profiles: Option<(u64, &Path, &PreparedShellProfileCache)>,
    cwd: Option<&str>,
    command: &str,
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
    sandbox: Option<&str>,
) -> CommandResult {
    if !policy.allow_raw_shell {
        return CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(0),
            error: Some("raw shell is disabled by local agent policy".to_string()),
        };
    }
    let cwd_path = cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
    if let Err(e) = cwd_allowed(policy, &cwd_path) {
        return CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(0),
            error: Some(e),
        };
    }
    let timeout_secs = timeout_secs.min(policy.max_timeout_secs).max(1);
    let start = Instant::now();
    let inspect_scratch = match sandbox {
        None => None,
        Some(crate::command_sandbox::INSPECT_SANDBOX_MODE) => {
            match crate::command_sandbox::InspectScratch::create() {
                Ok(scratch) => Some(scratch),
                Err(error) => {
                    return CommandResult {
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: Some(start.elapsed().as_millis() as u64),
                        error: Some(format!("inspect sandbox unavailable: {error}")),
                    }
                }
            }
        }
        Some(other) => {
            return CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(format!("unknown sandbox mode '{other}'")),
            }
        }
    };
    let mut prepared_profile_name = None;
    // Preparing a profile executes its init script. In inspect mode that
    // preparation must not happen outside Landlock, so use the base configured
    // shell and run its optional init script as part of the sandboxed command.
    let mut cmd = match profiles.filter(|_| inspect_scratch.is_none()) {
        Some((generation, projects_dir, cache)) => {
            match resolve_prepared_shell_profile(
                generation,
                shell,
                projects_dir,
                &cwd_path,
                cwd.is_some(),
                cache,
                stop_requested,
            ) {
                Ok(Some(profile)) => match configured_prepared_shell_command(&profile, command) {
                    Ok(cmd) => {
                        prepared_profile_name = Some(profile.profile_name.clone());
                        cmd
                    }
                    Err(e) => {
                        return CommandResult {
                            exit_code: None,
                            stdout: None,
                            stderr: None,
                            duration_ms: Some(start.elapsed().as_millis() as u64),
                            error: Some(format!(
                                "failed to configure shell profile '{}': {}",
                                profile.profile_name, e
                            )),
                        };
                    }
                },
                Ok(None) => match configured_shell_command(shell, command) {
                    Ok(cmd) => cmd,
                    Err(e) => {
                        return CommandResult {
                            exit_code: None,
                            stdout: None,
                            stderr: None,
                            duration_ms: Some(start.elapsed().as_millis() as u64),
                            error: Some(e),
                        };
                    }
                },
                Err(e) => {
                    return CommandResult {
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: Some(start.elapsed().as_millis() as u64),
                        error: Some(e),
                    };
                }
            }
        }
        None => match configured_shell_command(shell, command) {
            Ok(cmd) => cmd,
            Err(e) => {
                return CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some(e),
                };
            }
        },
    };
    cmd.current_dir(&cwd_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    if let Some(scratch) = inspect_scratch.as_ref() {
        if let Err(error) = crate::command_sandbox::sandbox_command_inspect(&mut cmd, scratch) {
            return CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(format!("inspect sandbox unavailable: {error}")),
            };
        }
    }
    let spawn = cmd.spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(e) => {
            let error = prepared_profile_name
                .as_deref()
                .map(|profile_name| {
                    format!("failed to spawn shell profile '{}': {}", profile_name, e)
                })
                .unwrap_or_else(|| format!("failed to spawn command: {}", e));
            return CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(error),
            };
        }
    };
    if let Some(input) = stdin {
        match child.stdin.take() {
            Some(mut child_stdin) => {
                if let Err(e) = child_stdin.write_all(input.as_bytes()) {
                    // A command may reject a request or report a missing
                    // capability before consuming its payload. Once it closes
                    // stdin, BrokenPipe says nothing about the command's own
                    // result, so preserve its exit status and output. Other
                    // write failures still belong to the executor.
                    if e.kind() != std::io::ErrorKind::BrokenPipe {
                        let cleanup = terminate_child_without_output(child).err();
                        return CommandResult {
                            exit_code: None,
                            stdout: None,
                            stderr: None,
                            duration_ms: Some(start.elapsed().as_millis() as u64),
                            error: Some(with_cleanup_error(
                                format!("failed to write command stdin: {}", e),
                                cleanup,
                            )),
                        };
                    }
                }
            }
            None => {
                let cleanup = terminate_child_without_output(child).err();
                return CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some(with_cleanup_error("stdin pipe missing", cleanup)),
                };
            }
        }
    }
    loop {
        if stop_requested
            .map(|flag| flag.load(Ordering::SeqCst))
            .unwrap_or(false)
        {
            let duration_ms = start.elapsed().as_millis() as u64;
            return match terminate_and_read_pipes(child, policy.max_output_bytes) {
                Ok((_status, stdout, stderr)) => CommandResult {
                    exit_code: Some(-1),
                    stdout: Some(truncate_bytes(&stdout, policy.max_output_bytes)),
                    stderr: Some(format!(
                        "{}{}job stopped by request",
                        truncate_bytes(&stderr, policy.max_output_bytes),
                        if stderr.is_empty() { "" } else { "\n" },
                    )),
                    duration_ms: Some(duration_ms),
                    error: Some("job stopped".to_string()),
                },
                Err(e) => CommandResult {
                    exit_code: Some(-1),
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(duration_ms),
                    error: Some(format!("job stopped; failed to collect output: {}", e)),
                },
            };
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() >= Duration::from_secs(timeout_secs) {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    return match terminate_and_read_pipes(child, policy.max_output_bytes) {
                        Ok((_status, stdout, stderr)) => CommandResult {
                            exit_code: Some(-1),
                            stdout: Some(truncate_bytes(&stdout, policy.max_output_bytes)),
                            stderr: Some(format!(
                                "{}{}command timed out after {} seconds",
                                truncate_bytes(&stderr, policy.max_output_bytes),
                                if stderr.is_empty() { "" } else { "\n" },
                                timeout_secs
                            )),
                            duration_ms: Some(duration_ms),
                            error: Some("command timed out".to_string()),
                        },
                        Err(e) => CommandResult {
                            exit_code: Some(-1),
                            stdout: None,
                            stderr: None,
                            duration_ms: Some(duration_ms),
                            error: Some(format!(
                                "command timed out; failed to collect output: {}",
                                e
                            )),
                        },
                    };
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let cleanup = terminate_child_without_output(child).err();
                return CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some(with_cleanup_error(
                        format!("failed to wait command: {}", e),
                        cleanup,
                    )),
                };
            }
        }
    }
    match terminate_and_read_pipes(child, policy.max_output_bytes) {
        Ok((status, stdout, stderr)) => CommandResult {
            exit_code: Some(status.code().unwrap_or(-1)),
            stdout: Some(truncate_bytes(&stdout, policy.max_output_bytes)),
            stderr: Some(truncate_bytes(&stderr, policy.max_output_bytes)),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        },
        Err(e) => CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(e),
        },
    }
}
