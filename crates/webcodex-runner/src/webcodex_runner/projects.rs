use super::config::{
    default_true, projects_dir, validate_shell_profile_name, AgentConfig, AgentPolicy,
};
use super::shell::canonicalize_existing;
use crate::shell_protocol::{ShellAgentProjectSummary, ShellAgentShellRequest};
use crate::{err_cmd, ok_cmd, write_created_file};
use crate::{CommandResult, CreatedProjectPaths};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const PROJECT_SCAN_CACHE_MS: u64 = 5000;
const PROJECT_GIT_TIMEOUT: Duration = Duration::from_secs(2);
const PROJECT_GIT_CLEANUP_TIMEOUT: Duration = Duration::from_millis(500);
const PROJECT_GIT_OUTPUT_MAX_BYTES: usize = 64 * 1024;
const MANAGED_TEMPORARY_PROJECT_KIND: &str = "managed_temporary";
const AUTO_REGISTERED_PROJECT_KIND: &str = "auto_registered";
const DEFAULT_MANAGED_TEMPORARY_PROJECT_NAME: &str = "Temporary Project";
const MANAGED_TEMPORARY_PROJECT_ID_PREFIX: &str = "temporary";
const MANAGED_TEMPORARY_PROJECT_CREATE_ATTEMPTS: usize = 16;
const AUTO_PROJECT_HASH_PREFIX_LENGTHS: &[usize] = &[8, 12, 16, 24, 32, 48, 64];
static PROJECT_REGISTRY_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn project_registry_write_lock() -> &'static Mutex<()> {
    PROJECT_REGISTRY_WRITE_LOCK.get_or_init(|| Mutex::new(()))
}

fn project_error_cmd(start: Instant, error_code: &'static str) -> CommandResult {
    CommandResult {
        exit_code: Some(1),
        stdout: Some(
            serde_json::to_string(&serde_json::json!({"error_code": error_code}))
                .unwrap_or_else(|_| r#"{"error_code":"operation_failed"}"#.to_string()),
        ),
        stderr: Some(String::new()),
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: None,
    }
}

fn structured_project_error_cmd(
    start: Instant,
    error_kind: &'static str,
    state_changed: bool,
    fields: serde_json::Value,
) -> CommandResult {
    let mut output = serde_json::json!({
        "error_code": error_kind,
        "error_kind": error_kind,
        "failure_kind": error_kind,
        "state_changed": state_changed,
    });
    if let (Some(output), Some(fields)) = (output.as_object_mut(), fields.as_object()) {
        output.extend(fields.clone());
    }
    CommandResult {
        exit_code: Some(1),
        stdout: Some(
            serde_json::to_string(&output)
                .unwrap_or_else(|_| r#"{"error_code":"operation_failed"}"#.to_string()),
        ),
        stderr: Some(String::new()),
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AgentProjectFile {
    pub(crate) id: String,
    pub(crate) path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) shell_profile: Option<String>,
    #[serde(default = "default_true")]
    pub(crate) allow_patch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) disabled: bool,
    #[serde(default)]
    pub(crate) hooks: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AgentProjectCache {
    projects: Vec<ShellAgentProjectSummary>,
    refreshed_at: Option<Instant>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentProjectShellContext {
    pub(crate) id: String,
    pub(crate) path: String,
    pub(crate) shell_profile: Option<String>,
}

fn validate_project_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("id cannot be empty".to_string());
    }
    if id == "." || id == ".." {
        return Err("id cannot be '.' or '..'".to_string());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err("id may only contain ASCII letters, digits, '-', '_', and '.'".to_string());
    }
    Ok(())
}

fn trim_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn agent_project_server_format_hint(content: &str, err: &str) -> Option<String> {
    let normalized = err.replace('`', "");
    if normalized.contains("missing field id") && content.contains("[projects.") {
        Some(
            "looks like a server projects.toml entry. Agent projects.d files must use top-level fields:\n\
             id = \"smoke\"\n\
             path = \"/path/to/repo\""
                .to_string(),
        )
    } else {
        None
    }
}

pub(crate) fn parse_agent_project_toml(content: &str) -> Result<AgentProjectFile, String> {
    let mut project: AgentProjectFile = toml::from_str(content).map_err(|e| {
        let err = e.to_string();
        let base = format!("failed to parse project toml: {}", err);
        match agent_project_server_format_hint(content, &err) {
            Some(hint) => format!("{}; {}", base, hint),
            None => base,
        }
    })?;
    project.id = project.id.trim().to_string();
    validate_project_id(&project.id)?;
    project.path = project.path.trim().to_string();
    if project.path.is_empty() {
        return Err("path cannot be empty".to_string());
    }
    project.name = trim_optional(project.name);
    project.kind = trim_optional(project.kind);
    project.description = trim_optional(project.description);
    if let Some(shell_profile) = &project.shell_profile {
        validate_shell_profile_name("project.shell_profile", shell_profile)?;
    }
    let mut hooks = HashMap::new();
    for (name, commands) in project.hooks {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err("hook name cannot be empty".to_string());
        }
        hooks.insert(name, commands);
    }
    project.hooks = hooks;
    Ok(project)
}

fn load_agent_project_shell_contexts_from_dir(dir: &Path) -> Vec<AgentProjectShellContext> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            files.push(path);
        }
    }
    files.sort();
    let mut seen = HashSet::new();
    let mut projects = Vec::new();
    for file in files {
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(project) = parse_agent_project_toml(&content) else {
            continue;
        };
        if project.disabled || !seen.insert(project.id.clone()) {
            continue;
        }
        projects.push(AgentProjectShellContext {
            id: project.id,
            path: project.path,
            shell_profile: project.shell_profile,
        });
    }
    projects
}

pub(crate) fn find_project_shell_context(
    projects_dir: &Path,
    cwd_path: &Path,
) -> Option<AgentProjectShellContext> {
    let cwd = cwd_path.canonicalize().ok()?;
    load_agent_project_shell_contexts_from_dir(projects_dir)
        .into_iter()
        .filter_map(|project| {
            let project_path = PathBuf::from(&project.path).canonicalize().ok()?;
            if cwd == project_path || cwd.starts_with(&project_path) {
                Some((project_path.components().count(), project))
            } else {
                None
            }
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, project)| project)
}

/// Resolve one enabled project by its Runner-local id. Persistent shells use
/// the id from the authenticated runtime-project binding rather than choosing
/// a project solely from a caller-controlled cwd.
pub(crate) fn find_project_shell_context_by_id(
    projects_dir: &Path,
    project_id: &str,
) -> Option<AgentProjectShellContext> {
    load_agent_project_shell_contexts_from_dir(projects_dir)
        .into_iter()
        .find(|project| project.id == project_id)
}

struct BoundedGitOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_capped: bool,
    stderr_capped: bool,
}

fn spawn_bounded_git_reader(
    mut pipe: impl Read + Send + 'static,
) -> (mpsc::Receiver<(Vec<u8>, bool)>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        let mut retained = Vec::with_capacity(PROJECT_GIT_OUTPUT_MAX_BYTES.min(8192));
        let mut chunk = [0_u8; 8192];
        let mut capped = false;
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    let remaining = PROJECT_GIT_OUTPUT_MAX_BYTES.saturating_sub(retained.len());
                    let keep = remaining.min(read);
                    retained.extend_from_slice(&chunk[..keep]);
                    capped |= keep < read;
                }
                Err(_) => break,
            }
        }
        let _ = tx.send((retained, capped));
    });
    (rx, handle)
}

#[cfg(unix)]
fn signal_project_git_group(process_group_id: u32, signal: i32) -> bool {
    let Ok(process_group_id) = i32::try_from(process_group_id) else {
        return false;
    };
    if process_group_id == 0 {
        return false;
    }
    // SAFETY: each helper below places Git in a private process group.
    (unsafe { libc::kill(-process_group_id, signal) }) == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn terminate_project_git_child(
    child: &mut std::process::Child,
    process_group_id: u32,
    deadline: Instant,
) {
    #[cfg(unix)]
    {
        let _ = signal_project_git_group(process_group_id, libc::SIGTERM);
        let grace = deadline.min(Instant::now() + Duration::from_millis(50));
        while Instant::now() < grace {
            if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                break;
            }
            thread::sleep(
                Duration::from_millis(10).min(grace.saturating_duration_since(Instant::now())),
            );
        }
        let _ = signal_project_git_group(process_group_id, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child.kill();

    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
            return;
        }
        thread::sleep(
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn run_git_bounded(
    path: &Path,
    args: &[&str],
    timeout: Duration,
    shutdown: Option<&AtomicBool>,
) -> Result<BoundedGitOutput, String> {
    if shutdown.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        return Err("git stopped during runner shutdown".to_string());
    }
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to spawn git: {error}"))?;
    let process_group_id = child.id();
    let Some(stdout) = child.stdout.take() else {
        terminate_project_git_child(
            &mut child,
            process_group_id,
            Instant::now() + PROJECT_GIT_CLEANUP_TIMEOUT,
        );
        return Err("git stdout pipe was unavailable".to_string());
    };
    let Some(stderr) = child.stderr.take() else {
        drop(stdout);
        terminate_project_git_child(
            &mut child,
            process_group_id,
            Instant::now() + PROJECT_GIT_CLEANUP_TIMEOUT,
        );
        return Err("git stderr pipe was unavailable".to_string());
    };
    let (stdout_rx, stdout_reader) = spawn_bounded_git_reader(stdout);
    let (stderr_rx, stderr_reader) = spawn_bounded_git_reader(stderr);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let stopping = shutdown.is_some_and(|flag| flag.load(Ordering::SeqCst));
                if stopping || Instant::now() >= deadline {
                    terminate_project_git_child(
                        &mut child,
                        process_group_id,
                        Instant::now() + PROJECT_GIT_CLEANUP_TIMEOUT,
                    );
                    return Err(if stopping {
                        "git stopped during runner shutdown".to_string()
                    } else {
                        "git command timed out".to_string()
                    });
                }
                thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(error) => {
                terminate_project_git_child(
                    &mut child,
                    process_group_id,
                    Instant::now() + PROJECT_GIT_CLEANUP_TIMEOUT,
                );
                return Err(format!("failed to wait for git: {error}"));
            }
        }
    };

    // A helper descendant must not keep either pipe open after Git itself
    // exits. The private group makes this cleanup local to this command.
    #[cfg(unix)]
    let _ = signal_project_git_group(process_group_id, libc::SIGKILL);
    let drain_deadline = Instant::now() + PROJECT_GIT_CLEANUP_TIMEOUT;
    let stdout = stdout_rx
        .recv_timeout(drain_deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| "git stdout reader timed out".to_string())?;
    let stderr = stderr_rx
        .recv_timeout(drain_deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| "git stderr reader timed out".to_string())?;
    if stdout_reader.is_finished() {
        let _ = stdout_reader.join();
    }
    if stderr_reader.is_finished() {
        let _ = stderr_reader.join();
    }
    Ok(BoundedGitOutput {
        status,
        stdout: stdout.0,
        stderr: stderr.0,
        stdout_capped: stdout.1,
        stderr_capped: stderr.1,
    })
}

fn run_git_capture(path: &str, args: &[&str], shutdown: Option<&AtomicBool>) -> Option<String> {
    let output = run_git_bounded(Path::new(path), args, PROJECT_GIT_TIMEOUT, shutdown).ok()?;
    if !output.status.success() || output.stdout_capped {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn project_revision(project: &AgentProjectFile) -> String {
    let normalized = toml::to_string(project).unwrap_or_default();
    format!("sha256:{:x}", Sha256::digest(normalized.as_bytes()))
}

fn agent_project_summary_with_shutdown(
    project: &AgentProjectFile,
    updated_at: i64,
    include_git: bool,
    shutdown: Option<&AtomicBool>,
) -> ShellAgentProjectSummary {
    let mut hooks = project.hooks.keys().cloned().collect::<Vec<_>>();
    hooks.sort();
    // The server uses the reported path as part of its repository continuity
    // identity. Report the actual root, not a mutable symlink alias, so a
    // retargeted project registration cannot inherit another repository's
    // current Workflow Session.
    let resolved_path = canonicalize_existing(Path::new(&project.path))
        .ok()
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| PathBuf::from(&project.path));
    let resolved_path = resolved_path.to_string_lossy().to_string();
    let (git_branch, git_head, git_dirty) = if include_git {
        let branch = run_git_capture(
            &resolved_path,
            &["rev-parse", "--abbrev-ref", "HEAD"],
            shutdown,
        );
        let head = run_git_capture(
            &resolved_path,
            &["log", "-1", "--pretty=format:%h"],
            shutdown,
        );
        let dirty = run_git_capture(&resolved_path, &["status", "--short"], shutdown)
            .map(|status| !status.trim().is_empty());
        (branch, head, dirty)
    } else {
        (None, None, None)
    };
    ShellAgentProjectSummary {
        id: project.id.clone(),
        name: project.name.clone().or_else(|| Some(project.id.clone())),
        path: resolved_path,
        allow_patch: project.allow_patch,
        kind: project.kind.clone(),
        description: project.description.clone(),
        hooks,
        disabled: project.disabled,
        revision: Some(project_revision(project)),
        git_branch,
        git_head,
        git_dirty,
        updated_at,
        shell_profile: project.shell_profile.clone(),
    }
}

#[cfg(test)]
pub(crate) fn agent_project_summary(
    project: &AgentProjectFile,
    updated_at: i64,
    include_git: bool,
) -> ShellAgentProjectSummary {
    agent_project_summary_with_shutdown(project, updated_at, include_git, None)
}

fn warn_empty_hook_commands(source: &Path, project: &AgentProjectFile) {
    for (hook, commands) in &project.hooks {
        for (idx, command) in commands.iter().enumerate() {
            if command.trim().is_empty() {
                eprintln!(
                    "webcodex-runner project warning: {} hook {} command {} is empty",
                    source.display(),
                    hook,
                    idx
                );
            }
        }
    }
}

fn load_agent_project_summaries_from_dir_with_shutdown(
    dir: &Path,
    shutdown: Option<&AtomicBool>,
) -> Vec<ShellAgentProjectSummary> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            eprintln!(
                "webcodex-runner project warning: failed to read {}: {}",
                dir.display(),
                e
            );
            return Vec::new();
        }
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            files.push(path);
        }
    }
    files.sort();

    let updated_at = chrono::Utc::now().timestamp();
    let mut seen = HashSet::new();
    let mut projects = Vec::new();
    for file in files {
        if shutdown.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            break;
        }
        let content = match std::fs::read_to_string(&file) {
            Ok(content) => content,
            Err(e) => {
                eprintln!(
                    "webcodex-runner project warning: failed to read {}: {}",
                    file.display(),
                    e
                );
                continue;
            }
        };
        let project = match parse_agent_project_toml(&content) {
            Ok(project) => project,
            Err(e) => {
                eprintln!(
                    "webcodex-runner project warning: skipping {}: {}",
                    file.display(),
                    e
                );
                continue;
            }
        };
        if !seen.insert(project.id.clone()) {
            eprintln!(
                "webcodex-runner project warning: duplicate project id {} in {}; skipping",
                project.id,
                file.display()
            );
            continue;
        }
        warn_empty_hook_commands(&file, &project);
        projects.push(agent_project_summary_with_shutdown(
            &project, updated_at, true, shutdown,
        ));
    }
    projects.sort_by(|a, b| a.id.cmp(&b.id));
    projects
}

pub(crate) fn load_agent_project_summaries_from_dir(dir: &Path) -> Vec<ShellAgentProjectSummary> {
    load_agent_project_summaries_from_dir_with_shutdown(dir, None)
}

fn load_agent_project_summaries(
    cfg: &AgentConfig,
    shutdown: Option<&AtomicBool>,
) -> Vec<ShellAgentProjectSummary> {
    load_agent_project_summaries_from_dir_with_shutdown(&projects_dir(cfg), shutdown)
}

impl AgentProjectCache {
    #[cfg(test)]
    pub(crate) fn get(&mut self, cfg: &AgentConfig) -> Vec<ShellAgentProjectSummary> {
        self.get_with_shutdown(cfg, None)
    }

    pub(crate) fn get_with_shutdown(
        &mut self,
        cfg: &AgentConfig,
        shutdown: Option<&AtomicBool>,
    ) -> Vec<ShellAgentProjectSummary> {
        if self.refreshed_at.is_some_and(|refreshed_at| {
            refreshed_at.elapsed() < Duration::from_millis(PROJECT_SCAN_CACHE_MS)
        }) {
            return self.projects.clone();
        }
        self.projects = load_agent_project_summaries(cfg, shutdown);
        self.refreshed_at = Some(Instant::now());
        self.projects.clone()
    }

    pub(crate) fn invalidate(&mut self) {
        self.projects.clear();
        self.refreshed_at = None;
    }
}

/// System directories that must never be used as a project root unless they are
/// explicitly under an `allowed_roots` entry. Even when `allow_cwd_anywhere`
/// is true, these roots are rejected to prevent accidental registration of
/// critical system paths.
const DANGEROUS_PROJECT_ROOTS: &[&str] = &[
    "/", "/etc", "/bin", "/sbin", "/usr", "/var", "/proc", "/sys", "/dev", "/run", "/boot",
];

/// Escape a string for use as a TOML basic string (double-quoted). NUL is
/// rejected up front by validation, so we only handle backslash, quote, and
/// common control characters.
fn toml_basic_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{}\"", escaped)
}

/// Build a deterministic project TOML string compatible with the existing
/// `parse_agent_project_toml` parser. The field order is fixed so the output
/// is reproducible.
fn build_project_toml(
    id: &str,
    name: &str,
    path: &str,
    description: &Option<String>,
    allow_patch: bool,
) -> String {
    build_project_toml_with_kind(id, name, path, None, description, allow_patch)
}

fn build_project_toml_with_kind(
    id: &str,
    name: &str,
    path: &str,
    kind: Option<&str>,
    description: &Option<String>,
    allow_patch: bool,
) -> String {
    let mut toml = String::new();
    toml.push_str(&format!("id = {}\n", toml_basic_string(id)));
    toml.push_str(&format!("name = {}\n", toml_basic_string(name)));
    toml.push_str(&format!("path = {}\n", toml_basic_string(path)));
    if let Some(kind) = kind {
        toml.push_str(&format!("kind = {}\n", toml_basic_string(kind)));
    }
    if let Some(desc) = description {
        toml.push_str(&format!("description = {}\n", toml_basic_string(desc)));
    }
    toml.push_str(&format!("allow_patch = {}\n", allow_patch));
    toml
}

/// Validate the project `id` for project-management operations. Stricter than
/// the existing `validate_project_id`: no dots (prevents any path-like
/// interpretation), only ASCII letters/digits/dash/underscore.
fn validate_project_op_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("id cannot be empty".to_string());
    }
    if id.contains('\0') {
        return Err("id must not contain NUL".to_string());
    }
    if id.len() > 64 {
        return Err("id must be at most 64 characters".to_string());
    }
    if id.contains('/') || id.contains('\\') {
        return Err("id must not contain slash or backslash".to_string());
    }
    if id == ".." || id == "." || id.contains("..") {
        return Err("id must not contain dot-dot traversal".to_string());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("id may only contain ASCII letters, digits, '-', and '_'".to_string());
    }
    Ok(())
}

/// Validate the project `name`: non-empty after trim, <= 120 chars, no NUL.
fn validate_project_op_name(name: &str) -> Result<(), String> {
    if name.contains('\0') {
        return Err("name must not contain NUL".to_string());
    }
    if name.trim().is_empty() {
        return Err("name cannot be empty".to_string());
    }
    if name.len() > 120 {
        return Err("name must be at most 120 characters".to_string());
    }
    Ok(())
}

/// A managed temporary project name is persisted as display metadata, never
/// used as a filesystem path. Still reject path-looking input at the Runner
/// boundary so callers cannot mistake it for a directory selector.
fn validate_managed_temporary_project_name(name: &str) -> Result<(), String> {
    validate_project_op_name(name)?;
    let name = name.trim();
    if name == "." || name == ".." || name.contains("..") {
        return Err("name must not contain dot-dot traversal".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("name must not contain slash or backslash".to_string());
    }
    Ok(())
}

/// Validate the optional `description`: <= 500 chars, no NUL.
fn validate_project_op_description(desc: &str) -> Result<(), String> {
    if desc.contains('\0') {
        return Err("description must not contain NUL".to_string());
    }
    if desc.len() > 500 {
        return Err("description must be at most 500 characters".to_string());
    }
    Ok(())
}

/// Check whether a canonicalized project path is allowed by the agent policy.
/// Returns Ok(()) if the path is safe, Err otherwise.
///
/// - If `allow_cwd_anywhere` is false, the path must be under an explicit
///   `allowed_roots` entry.
/// - If `allow_cwd_anywhere` is true, the path is allowed unless it is one of
///   the `DANGEROUS_PROJECT_ROOTS` (and not under an explicit `allowed_roots`).
pub(crate) fn validate_project_path_policy(
    policy: &AgentPolicy,
    canonical_path: &Path,
) -> Result<(), String> {
    let path_str = canonical_path.to_string_lossy().to_string();
    // If under an explicit allowed_root, always allow.
    for root in &policy.allowed_roots {
        if let Ok(canonical_root) = canonicalize_existing(root) {
            if canonical_path == &canonical_root || canonical_path.starts_with(&canonical_root) {
                return Ok(());
            }
        }
    }
    if !policy.allow_cwd_anywhere {
        return Err(format!(
            "path {} is outside allowed_roots and allow_cwd_anywhere is false",
            path_str
        ));
    }
    // allow_cwd_anywhere is true: reject dangerous system roots.
    for &dangerous in DANGEROUS_PROJECT_ROOTS {
        let dangerous_root = Path::new(dangerous);
        let is_dangerous = if dangerous_root == Path::new("/") {
            canonical_path == dangerous_root
        } else {
            canonical_path == dangerous_root || canonical_path.starts_with(dangerous_root)
        };
        if is_dangerous {
            return Err(format!(
                "path {} is under a dangerous system root; register it under an explicit allowed_roots entry if intended",
                path_str
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ProjectTomlWriteResult {
    config_path: PathBuf,
    created_config: bool,
    overwritten: bool,
}

#[derive(Debug)]
enum ProjectTomlWriteError {
    BeforeRename,
    AfterRename,
}

#[cfg(test)]
thread_local! {
    static FAIL_PARENT_SYNC_AFTER_PROJECT_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_PROJECT_PUBLISH_BEFORE_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_project_parent_sync_after_rename() {
    FAIL_PARENT_SYNC_AFTER_PROJECT_RENAME.set(true);
}

#[cfg(test)]
pub(crate) fn fail_next_project_publish_before_rename() {
    FAIL_PROJECT_PUBLISH_BEFORE_RENAME.set(true);
}

fn sync_project_parent_after_rename(path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if FAIL_PARENT_SYNC_AFTER_PROJECT_RENAME.replace(false) {
        return Err("injected parent directory sync failure".to_string());
    }
    sync_parent_dir(path)
}

/// Write a project TOML file atomically into `projects_dir`. Creates
/// `projects_dir` if missing. Returns write metadata on success.
/// The temp file is written and fsynced, then atomically published as
/// `<id>.toml`.
fn sync_parent_dir(path: &Path) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| "project config has no parent".to_string())?;
    std::fs::File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|e| format!("failed to sync project registry directory: {e}"))
}

fn unique_registry_temp(dir: &Path, id: &str, suffix: &str) -> PathBuf {
    dir.join(format!(".{id}.{}.{}", uuid::Uuid::new_v4(), suffix))
}

fn write_project_toml_atomic(
    projects_dir: &Path,
    id: &str,
    toml_content: &str,
    overwrite: bool,
) -> Result<ProjectTomlWriteResult, ProjectTomlWriteError> {
    std::fs::create_dir_all(projects_dir).map_err(|_| ProjectTomlWriteError::BeforeRename)?;
    let canonical_dir =
        canonicalize_existing(projects_dir).map_err(|_| ProjectTomlWriteError::BeforeRename)?;
    let config_path = canonical_dir.join(format!("{id}.toml"));
    if !config_path.starts_with(&canonical_dir) {
        return Err(ProjectTomlWriteError::BeforeRename);
    }
    let existed_before = config_path.exists();
    if existed_before && !overwrite {
        return Err(ProjectTomlWriteError::BeforeRename);
    }
    let temp_path = unique_registry_temp(&canonical_dir, id, "toml.tmp");
    let mut published = false;
    let before = (|| -> Result<(), String> {
        let mut file = std::fs::File::create(&temp_path).map_err(|e| e.to_string())?;
        file.write_all(toml_content.as_bytes())
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        #[cfg(test)]
        if FAIL_PROJECT_PUBLISH_BEFORE_RENAME.replace(false) {
            return Err("injected project publish failure".to_string());
        }
        if overwrite {
            std::fs::rename(&temp_path, &config_path).map_err(|e| e.to_string())?;
            published = true;
        } else {
            // Publish a complete, synced same-directory temp file without the
            // overwrite-on-rename race. A concurrent creator wins cleanly and
            // the caller can rescan the registry to converge.
            std::fs::hard_link(&temp_path, &config_path).map_err(|e| e.to_string())?;
            published = true;
            std::fs::remove_file(&temp_path).map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    if before.is_err() {
        let _ = std::fs::remove_file(&temp_path);
        return Err(if published {
            ProjectTomlWriteError::AfterRename
        } else {
            ProjectTomlWriteError::BeforeRename
        });
    }
    sync_project_parent_after_rename(&config_path)
        .map_err(|_| ProjectTomlWriteError::AfterRename)?;
    Ok(ProjectTomlWriteResult {
        config_path,
        created_config: !existed_before,
        overwritten: existed_before && overwrite,
    })
}

fn load_project_files_for_path_resolution(
    projects_dir: &Path,
) -> Result<Vec<AgentProjectFile>, &'static str> {
    let entries = match std::fs::read_dir(projects_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("project_registry_unavailable"),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| "project_registry_unavailable")?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("toml") {
            files.push(path);
        }
    }
    files.sort();

    let mut projects = Vec::with_capacity(files.len());
    for file in files {
        let content = std::fs::read_to_string(&file).map_err(|_| "project_registry_unavailable")?;
        let project =
            parse_agent_project_toml(&content).map_err(|_| "project_registry_unavailable")?;
        projects.push(project);
    }
    Ok(projects)
}

fn projects_matching_canonical_path(
    projects: &[AgentProjectFile],
    canonical_path: &Path,
) -> Vec<AgentProjectFile> {
    projects
        .iter()
        .filter_map(|project| {
            let registered_path = canonicalize_existing(Path::new(&project.path)).ok()?;
            (registered_path.is_dir() && registered_path == canonical_path).then(|| project.clone())
        })
        .collect()
}

fn bounded_project_name(canonical_path: &Path) -> String {
    let raw = canonical_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("Project")
        .trim();
    let mut name = String::new();
    for character in raw.chars() {
        if name.len() + character.len_utf8() > 120 {
            break;
        }
        name.push(character);
    }
    if name.is_empty() {
        "Project".to_string()
    } else {
        name
    }
}

fn sanitized_project_basename(canonical_path: &Path) -> String {
    let raw = canonical_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    let mut sanitized = String::new();
    let mut separator_pending = false;
    for character in raw.chars() {
        if character.is_ascii_alphanumeric() {
            if separator_pending && !sanitized.is_empty() {
                sanitized.push('-');
            }
            sanitized.push(character.to_ascii_lowercase());
            separator_pending = false;
        } else {
            separator_pending = true;
        }
    }
    if sanitized.is_empty() {
        "project".to_string()
    } else {
        sanitized
    }
}

fn canonical_project_path_hash(canonical_path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        return format!(
            "{:x}",
            Sha256::digest(canonical_path.as_os_str().as_bytes())
        );
    }
    #[cfg(not(unix))]
    format!(
        "{:x}",
        Sha256::digest(canonical_path.to_string_lossy().as_bytes())
    )
}

fn auto_project_id_candidate(
    canonical_path: &Path,
    hash_prefix_length: usize,
) -> Result<String, &'static str> {
    let digest = canonical_project_path_hash(canonical_path);
    let hash_prefix = digest
        .get(..hash_prefix_length.min(digest.len()))
        .ok_or("project_id_collision")?;
    let max_basename_length = 64usize.saturating_sub(hash_prefix.len() + 1);
    if max_basename_length == 0 {
        return Err("project_id_collision");
    }
    let basename = sanitized_project_basename(canonical_path);
    let basename = basename
        .chars()
        .take(max_basename_length)
        .collect::<String>();
    let candidate = format!("{basename}-{hash_prefix}");
    validate_project_op_id(&candidate).map_err(|_| "project_id_collision")?;
    Ok(candidate)
}

fn choose_auto_project_id(
    projects_dir: &Path,
    projects: &[AgentProjectFile],
    canonical_path: &Path,
) -> Result<String, &'static str> {
    let configured_ids = projects
        .iter()
        .map(|project| project.id.as_str())
        .collect::<HashSet<_>>();
    for &prefix_length in AUTO_PROJECT_HASH_PREFIX_LENGTHS {
        let candidate = auto_project_id_candidate(canonical_path, prefix_length)?;
        if configured_ids.contains(candidate.as_str())
            || projects_dir.join(format!("{candidate}.toml")).exists()
        {
            continue;
        }
        return Ok(candidate);
    }
    Err("project_id_collision")
}

fn path_resolution_success(
    request: &ShellAgentShellRequest,
    project: &AgentProjectFile,
    canonical_path: &Path,
    outcome: &'static str,
    registered: bool,
    projects_config_path: Option<&Path>,
) -> serde_json::Value {
    serde_json::json!({
        "id": format!("agent:{}:{}", request.client_id, project.id),
        "agent_project_id": project.id,
        "client_id": request.client_id,
        "name": project.name,
        "path": canonical_path.to_string_lossy(),
        "kind": project.kind,
        "description": project.description,
        "allow_patch": project.allow_patch,
        "disabled": project.disabled,
        "revision": project_revision(project),
        "source": "path",
        "outcome": outcome,
        "registered": registered,
        "created_config": registered,
        "changed": registered,
        "recovered": !registered,
        "projects_config_path": projects_config_path.map(|path| path.to_string_lossy().to_string()),
    })
}

fn existing_path_resolution_result(
    start: Instant,
    request: &ShellAgentShellRequest,
    canonical_path: &Path,
    matches: Vec<AgentProjectFile>,
) -> Option<CommandResult> {
    if matches.len() > 1 {
        let mut matching_project_ids = matches
            .iter()
            .map(|project| project.id.clone())
            .collect::<Vec<_>>();
        matching_project_ids.sort();
        matching_project_ids.dedup();
        return Some(structured_project_error_cmd(
            start,
            "ambiguous_project_path",
            false,
            serde_json::json!({"matching_project_ids": matching_project_ids}),
        ));
    }
    let project = matches.into_iter().next()?;
    if project.disabled {
        return Some(structured_project_error_cmd(
            start,
            "project_disabled",
            false,
            serde_json::json!({"matching_project_id": project.id}),
        ));
    }
    Some(ok_cmd(
        start,
        path_resolution_success(
            request,
            &project,
            canonical_path,
            "reused_existing_registration",
            false,
            None,
        ),
    ))
}

/// Resolve an existing Runner registration by canonical path or atomically
/// persist a new one. This is an internal Server↔Runner operation, not a
/// model-visible runtime tool.
pub(crate) fn handle_resolve_or_register_project(
    policy: &AgentPolicy,
    projects_dir: &Path,
    request: &ShellAgentShellRequest,
) -> CommandResult {
    let start = Instant::now();
    let _registry_guard = match project_registry_write_lock().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return structured_project_error_cmd(
                start,
                "operation_failed",
                false,
                serde_json::json!({}),
            )
        }
    };
    let payload = match request
        .stdin
        .as_deref()
        .and_then(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
        .and_then(|payload| payload.as_object().cloned())
    {
        Some(payload) => payload,
        None => {
            return structured_project_error_cmd(
                start,
                "invalid_request",
                false,
                serde_json::json!({}),
            )
        }
    };
    if payload.len() != 1 {
        return structured_project_error_cmd(
            start,
            "invalid_request",
            false,
            serde_json::json!({}),
        );
    }
    let path = match payload.get("path").and_then(serde_json::Value::as_str) {
        Some(path) if !path.is_empty() && !path.contains('\0') && Path::new(path).is_absolute() => {
            path
        }
        _ => {
            return structured_project_error_cmd(
                start,
                "invalid_project_path",
                false,
                serde_json::json!({"field": "path"}),
            )
        }
    };
    let canonical_path = match canonicalize_existing(Path::new(path)) {
        Ok(path) => path,
        Err(_) => {
            return structured_project_error_cmd(
                start,
                "project_path_not_found",
                false,
                serde_json::json!({"field": "path"}),
            )
        }
    };
    if !canonical_path.is_dir() {
        return structured_project_error_cmd(
            start,
            "project_path_not_directory",
            false,
            serde_json::json!({"field": "path"}),
        );
    }
    if canonical_path.to_str().is_none() {
        return structured_project_error_cmd(
            start,
            "invalid_project_path",
            false,
            serde_json::json!({"field": "path"}),
        );
    }
    if validate_project_path_policy(policy, &canonical_path).is_err() {
        return structured_project_error_cmd(
            start,
            "path_outside_allowed_roots",
            false,
            serde_json::json!({"field": "path"}),
        );
    }

    let projects = match load_project_files_for_path_resolution(projects_dir) {
        Ok(projects) => projects,
        Err(error_kind) => {
            return structured_project_error_cmd(start, error_kind, false, serde_json::json!({}))
        }
    };
    let matches = projects_matching_canonical_path(&projects, &canonical_path);
    if let Some(result) = existing_path_resolution_result(start, request, &canonical_path, matches)
    {
        return result;
    }

    let project_id = match choose_auto_project_id(projects_dir, &projects, &canonical_path) {
        Ok(project_id) => project_id,
        Err(error_kind) => {
            return structured_project_error_cmd(start, error_kind, false, serde_json::json!({}))
        }
    };
    let canonical_path_string = canonical_path
        .to_str()
        .expect("validated UTF-8 canonical project path")
        .to_string();
    let name = bounded_project_name(&canonical_path);
    let description = None;
    let toml_content = build_project_toml_with_kind(
        &project_id,
        &name,
        &canonical_path_string,
        Some(AUTO_REGISTERED_PROJECT_KIND),
        &description,
        true,
    );
    let write_result =
        match write_project_toml_atomic(projects_dir, &project_id, &toml_content, false) {
            Ok(result) => result,
            Err(ProjectTomlWriteError::BeforeRename) => {
                // A different process may have won publication. Rescan under
                // our process-local lock and converge if it registered the
                // same canonical directory.
                if let Ok(projects) = load_project_files_for_path_resolution(projects_dir) {
                    let matches = projects_matching_canonical_path(&projects, &canonical_path);
                    if let Some(result) =
                        existing_path_resolution_result(start, request, &canonical_path, matches)
                    {
                        return result;
                    }
                }
                return structured_project_error_cmd(
                    start,
                    "operation_failed",
                    false,
                    serde_json::json!({}),
                );
            }
            Err(ProjectTomlWriteError::AfterRename) => {
                return structured_project_error_cmd(
                    start,
                    "operation_indeterminate",
                    true,
                    serde_json::json!({}),
                )
            }
        };
    let project = match parse_agent_project_toml(&toml_content) {
        Ok(project) => project,
        Err(_) => {
            return structured_project_error_cmd(
                start,
                "operation_indeterminate",
                true,
                serde_json::json!({}),
            )
        }
    };
    ok_cmd(
        start,
        path_resolution_success(
            request,
            &project,
            &canonical_path,
            "auto_registered",
            true,
            Some(&write_result.config_path),
        ),
    )
}

fn lifecycle_config_path(projects_dir: &Path, id: &str) -> Result<PathBuf, String> {
    validate_project_op_id(id)?;
    let canonical_dir = canonicalize_existing(projects_dir)?;
    let path = canonical_dir.join(format!("{id}.toml"));
    if !path.starts_with(&canonical_dir) {
        return Err("project config path would escape projects_dir".to_string());
    }
    Ok(path)
}

fn write_existing_project_atomic(path: &Path, content: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| "project config has no parent".to_string())?;
    let id = path
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("project");
    let temp = unique_registry_temp(dir, id, "toml.tmp");
    let result = (|| {
        let mut file = std::fs::File::create(&temp)
            .map_err(|e| format!("failed to create lifecycle temp file: {e}"))?;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("failed to write lifecycle temp file: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("failed to sync lifecycle temp file: {e}"))?;
        std::fs::rename(&temp, path)
            .map_err(|e| format!("failed to atomically replace project config: {e}"))?;
        sync_parent_dir(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn cleanup_unregister_tombstones(projects_dir: &Path, id: &str) -> Result<(), String> {
    let prefix = format!(".{id}.");
    let suffix = ".toml.unregistering";
    let mut changed = false;
    for entry in std::fs::read_dir(projects_dir)
        .map_err(|e| format!("failed to inspect project registry tombstones: {e}"))?
    {
        let entry = entry.map_err(|e| format!("failed to inspect project registry entry: {e}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(suffix) {
            std::fs::remove_file(entry.path())
                .map_err(|e| format!("failed to remove stale unregister tombstone: {e}"))?;
            changed = true;
        }
    }
    if changed {
        std::fs::File::open(projects_dir)
            .and_then(|file| file.sync_all())
            .map_err(|e| format!("failed to sync project registry directory: {e}"))?;
    }
    Ok(())
}

fn unregister_project_config(path: &Path) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| "project config has no parent".to_string())?;
    let id = path
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("project");
    let tombstone = unique_registry_temp(dir, id, "toml.unregistering");
    std::fs::rename(path, &tombstone)
        .map_err(|e| format!("failed to stage project unregister: {e}"))?;
    sync_parent_dir(path)?;
    std::fs::remove_file(&tombstone)
        .map_err(|e| format!("failed to remove project registry tombstone: {e}"))?;
    sync_parent_dir(path)
}

/// Structured, non-shell project lifecycle mutation. Unregister only removes
/// the registry TOML and never touches the project path or Git data.
pub(crate) fn handle_project_lifecycle_op(
    policy: &AgentPolicy,
    projects_dir: &Path,
    request: &ShellAgentShellRequest,
) -> CommandResult {
    let _registry_guard = match project_registry_write_lock().lock() {
        Ok(guard) => guard,
        Err(_) => return project_error_cmd(Instant::now(), "operation_failed"),
    };
    let start = Instant::now();
    let action = request
        .kind
        .strip_prefix("project_lifecycle_")
        .unwrap_or("");
    if !matches!(action, "enable" | "disable" | "unregister") {
        return project_error_cmd(start, "unsupported_runner_version");
    }
    let payload: serde_json::Value = match request
        .stdin
        .as_deref()
        .and_then(|v| serde_json::from_str(v).ok())
    {
        Some(v) => v,
        None => return project_error_cmd(start, "invalid_request"),
    };
    let id = match payload.get("project_id").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return project_error_cmd(start, "invalid_request"),
    };
    let expected_revision = match payload.get("expected_revision").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return project_error_cmd(start, "invalid_request"),
    };
    let config_path = match lifecycle_config_path(projects_dir, id) {
        Ok(v) => v,
        Err(e) => return err_cmd(start, e),
    };
    if !config_path.exists() {
        if action == "unregister" {
            if cleanup_unregister_tombstones(projects_dir, id).is_err() {
                return project_error_cmd(start, "operation_failed");
            }
            return ok_cmd(
                start,
                serde_json::json!({
                    "operation": action, "agent_project_id": id,
                    "outcome": "already_unregistered", "changed": false,
                    "revision": serde_json::Value::Null
                }),
            );
        }
        return project_error_cmd(start, "project_not_found");
    }
    let content = match std::fs::read_to_string(&config_path) {
        Ok(v) => v,
        Err(_) => return project_error_cmd(start, "operation_failed"),
    };
    let mut project = match parse_agent_project_toml(&content) {
        Ok(v) => v,
        Err(_) => return project_error_cmd(start, "operation_failed"),
    };
    let current_revision = project_revision(&project);
    let desired_disabled = action == "disable";
    if action != "unregister" && project.disabled == desired_disabled {
        return ok_cmd(
            start,
            serde_json::json!({
                "operation": action, "agent_project_id": id,
                "outcome": if desired_disabled {"already_disabled"} else {"already_enabled"},
                "changed": false, "revision": current_revision,
                "disabled": project.disabled, "path": project.path,
                "name": project.name, "description": project.description,
                "allow_patch": project.allow_patch
            }),
        );
    }
    if expected_revision != current_revision {
        return project_error_cmd(start, "revision_conflict");
    }
    if action == "unregister" {
        if unregister_project_config(&config_path).is_err() {
            return project_error_cmd(start, "operation_failed");
        }
        return ok_cmd(
            start,
            serde_json::json!({
                "operation": action, "agent_project_id": id,
                "outcome": "unregistered", "changed": true,
                "revision": serde_json::Value::Null
            }),
        );
    }
    if !desired_disabled {
        let canonical = match canonicalize_existing(Path::new(&project.path)) {
            Ok(v) if v.is_dir() => v,
            _ => return project_error_cmd(start, "project_not_found"),
        };
        if let Err(_) = validate_project_path_policy(policy, &canonical) {
            return project_error_cmd(start, "path_outside_allowed_roots");
        }
    }
    project.disabled = desired_disabled;
    let serialized = match toml::to_string_pretty(&project) {
        Ok(v) => v,
        Err(_) => return project_error_cmd(start, "operation_failed"),
    };
    if write_existing_project_atomic(&config_path, &serialized).is_err() {
        return project_error_cmd(start, "operation_failed");
    }
    let revision = project_revision(&project);
    ok_cmd(
        start,
        serde_json::json!({
            "operation": action, "agent_project_id": id,
            "outcome": if desired_disabled {"disabled"} else {"enabled"},
            "changed": true, "revision": revision,
            "disabled": project.disabled, "path": project.path,
            "name": project.name, "description": project.description,
            "allow_patch": project.allow_patch
        }),
    )
}

fn matching_existing_project(
    projects_dir: &Path,
    id: &str,
    name: &str,
    path: &str,
    description: Option<&str>,
    allow_patch: bool,
) -> Result<Option<AgentProjectFile>, &'static str> {
    let config_path = projects_dir.join(format!("{id}.toml"));
    if !config_path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&config_path).map_err(|_| "operation_failed")?;
    let project = parse_agent_project_toml(&content).map_err(|_| "operation_failed")?;
    let matches = project.id == id
        && project.path == path
        && project.name.as_deref() == Some(name)
        && project.description.as_deref() == description
        && project.allow_patch == allow_patch
        && !project.disabled;
    if matches {
        Ok(Some(project))
    } else {
        Err("project_already_exists")
    }
}

fn validate_recovered_create_side_effects(
    path: &Path,
    template: &str,
    description: Option<&str>,
    git_init: bool,
) -> Result<(), &'static str> {
    if !path.is_dir() {
        return Err("project_already_exists");
    }
    if git_init && !path.join(".git").is_dir() {
        return Err("project_already_exists");
    }
    if template == "basic"
        && (!path.join("README.md").is_file() || !path.join(".gitignore").is_file())
    {
        return Err("project_already_exists");
    }
    if template == "empty" && description.is_some() && !path.join("README.md").is_file() {
        return Err("project_already_exists");
    }
    Ok(())
}

fn recovered_project_result(
    kind: &str,
    runtime_id: &str,
    client_id: &str,
    project: &AgentProjectFile,
    template: Option<&str>,
    git_init: bool,
) -> serde_json::Value {
    serde_json::json!({
        "id": runtime_id, "agent_project_id": project.id, "client_id": client_id,
        "name": project.name, "path": project.path, "description": project.description,
        "created_directory": false, "created_config": false, "overwritten": false,
        "allow_patch": project.allow_patch, "template": template,
        "git_initialized": git_init, "recovered": true, "changed": false,
        "operation": if kind == "create_project" { "create" } else { "register" },
        "outcome": if kind == "create_project" { "created" } else { "registered" },
        "revision": project_revision(project),
    })
}

/// Create and persist one Runner-managed temporary project. The directory name
/// and project id are generated here, never accepted from the server, and the
/// canonical result must be exactly one direct child of the configured root.
///
/// TODO: add an explicit retention policy plus a safe managed-project deletion
/// path that re-verifies this kind and root before removing anything.
fn handle_managed_temporary_project(
    policy: &AgentPolicy,
    projects_dir: &Path,
    temporary_projects_root: Option<&Path>,
    request: &ShellAgentShellRequest,
    json: &serde_json::Value,
    start: Instant,
) -> CommandResult {
    // This internal request accepts no caller-selected directory/id or
    // create-project behavior. Rejecting those fields makes the generated
    // direct-child invariant explicit even if a future caller bypasses the
    // public start_coding_task schema.
    if [
        "id",
        "path",
        "description",
        "allow_patch",
        "template",
        "git_init",
        "allow_existing_empty",
        "overwrite",
    ]
    .iter()
    .any(|field| json.get(*field).is_some())
    {
        return project_error_cmd(start, "invalid_request");
    }
    let name = match json.get("name") {
        None | Some(serde_json::Value::Null) => DEFAULT_MANAGED_TEMPORARY_PROJECT_NAME.to_string(),
        Some(serde_json::Value::String(value)) => {
            if validate_managed_temporary_project_name(value).is_err() {
                return project_error_cmd(start, "invalid_request");
            }
            value.trim().to_string()
        }
        Some(_) => return project_error_cmd(start, "invalid_request"),
    };
    let Some(temporary_projects_root) = temporary_projects_root else {
        return project_error_cmd(start, "temporary_projects_not_configured");
    };
    let canonical_root = match canonicalize_existing(temporary_projects_root) {
        Ok(root) if root.is_dir() => root,
        _ => return project_error_cmd(start, "temporary_projects_root_unavailable"),
    };
    if validate_project_path_policy(policy, &canonical_root).is_err() {
        return project_error_cmd(start, "temporary_projects_root_outside_allowed_roots");
    }

    for _ in 0..MANAGED_TEMPORARY_PROJECT_CREATE_ATTEMPTS {
        let id = format!(
            "{MANAGED_TEMPORARY_PROJECT_ID_PREFIX}-{}",
            uuid::Uuid::new_v4()
        );
        if projects_dir.join(format!("{id}.toml")).exists() {
            continue;
        }
        let requested_path = canonical_root.join(&id);
        match std::fs::create_dir(&requested_path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(_) => return project_error_cmd(start, "temporary_project_create_failed"),
        }
        let canonical_path = match canonicalize_existing(&requested_path) {
            Ok(path) if path.is_dir() && path.parent() == Some(canonical_root.as_path()) => path,
            _ => return project_error_cmd(start, "temporary_project_path_escape"),
        };
        let path = canonical_path.to_string_lossy().to_string();
        match run_git_bounded(&canonical_path, &["init"], Duration::from_secs(5), None) {
            Ok(output) if output.status.success() => {}
            Ok(_) | Err(_) => {
                let _ = std::fs::remove_dir_all(&canonical_path);
                return project_error_cmd(start, "temporary_project_git_init_failed");
            }
        }
        let description = None;
        let toml_content = build_project_toml_with_kind(
            &id,
            &name,
            &path,
            Some(MANAGED_TEMPORARY_PROJECT_KIND),
            &description,
            true,
        );
        let write_result = match write_project_toml_atomic(projects_dir, &id, &toml_content, false)
        {
            Ok(result) => result,
            Err(ProjectTomlWriteError::BeforeRename) => {
                // The directory is a newly created direct child of the managed
                // root and only contains the Git metadata initialized above.
                let _ = std::fs::remove_dir_all(&canonical_path);
                return project_error_cmd(start, "operation_failed");
            }
            Err(ProjectTomlWriteError::AfterRename) => {
                return project_error_cmd(start, "operation_indeterminate");
            }
        };
        let project = parse_agent_project_toml(&toml_content)
            .expect("generated managed temporary project TOML must parse");
        return ok_cmd(
            start,
            serde_json::json!({
                "id": format!("agent:{}:{}", request.client_id, id),
                "agent_project_id": id,
                "client_id": request.client_id,
                "name": name,
                "path": path,
                "description": serde_json::Value::Null,
                "kind": MANAGED_TEMPORARY_PROJECT_KIND,
                "source": MANAGED_TEMPORARY_PROJECT_KIND,
                "managed_temporary": true,
                "projects_config_path": write_result.config_path.to_string_lossy(),
                "created_directory": true,
                "created_config": write_result.created_config,
                "overwritten": false,
                "allow_patch": true,
                "template": "empty",
                "git_initialized": true,
                "revision": project_revision(&project),
                "operation": "create",
                "outcome": "created",
                "changed": true,
                "recovered": false,
            }),
        );
    }
    project_error_cmd(start, "temporary_project_name_collision")
}

/// Handle `register_project` / `create_project` agent requests. Parses the
/// JSON payload from `request.stdin`, validates fields and path against
/// policy, writes `projects_dir/<id>.toml` atomically (and for
/// `create_project` creates the directory / templates / optional git init),
/// and returns structured JSON in `CommandResult.stdout`.
#[cfg(test)]
pub(crate) fn handle_project_op(
    policy: &AgentPolicy,
    projects_dir: &Path,
    request: &ShellAgentShellRequest,
) -> CommandResult {
    handle_project_op_with_temporary_projects_root(policy, projects_dir, None, request)
}

pub(crate) fn handle_project_op_with_temporary_projects_root(
    policy: &AgentPolicy,
    projects_dir: &Path,
    temporary_projects_root: Option<&Path>,
    request: &ShellAgentShellRequest,
) -> CommandResult {
    let _registry_guard = match project_registry_write_lock().lock() {
        Ok(guard) => guard,
        Err(_) => return project_error_cmd(Instant::now(), "operation_failed"),
    };
    let start = Instant::now();
    let kind = request.kind.as_str();
    let payload = match request.stdin.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => {
            return CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(format!("{} request missing stdin payload", kind)),
            };
        }
    };
    let json: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            return CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(format!("failed to parse {} payload: {}", kind, e)),
            };
        }
    };
    if kind == "create_project"
        && json
            .get("managed_temporary_project")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    {
        return handle_managed_temporary_project(
            policy,
            projects_dir,
            temporary_projects_root,
            request,
            &json,
            start,
        );
    }
    let get_str = |key: &str| -> Result<String, String> {
        json.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("{} missing required field '{}'", kind, key))
    };
    let id = match get_str("id") {
        Ok(v) => v,
        Err(e) => return err_cmd(start, e),
    };
    let name = match get_str("name") {
        Ok(v) => v,
        Err(e) => return err_cmd(start, e),
    };
    let path = match get_str("path") {
        Ok(v) => v,
        Err(e) => return err_cmd(start, e),
    };
    let description = json
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let allow_patch = json
        .get("allow_patch")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let overwrite = json
        .get("overwrite")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Err(e) = validate_project_op_id(&id) {
        return err_cmd(start, e);
    }
    if let Err(e) = validate_project_op_name(&name) {
        return err_cmd(start, e);
    }
    if let Some(ref desc) = description {
        if let Err(e) = validate_project_op_description(desc) {
            return err_cmd(start, e);
        }
    }
    if path.is_empty() || path.contains('\0') || !path.starts_with('/') {
        return err_cmd(start, "path must be a non-empty absolute path".to_string());
    }

    let client_id = request.client_id.clone();
    let runtime_id = format!("agent:{}:{}", client_id, id);

    let toml_content = build_project_toml(&id, &name, &path, &description, allow_patch);
    let template = json
        .get("template")
        .and_then(|v| v.as_str())
        .unwrap_or("empty")
        .to_string();
    let git_init = json
        .get("git_init")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let allow_existing_empty = json
        .get("allow_existing_empty")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if kind == "create_project" && template != "empty" && template != "basic" {
        return project_error_cmd(start, "invalid_request");
    }

    if kind == "register_project" {
        // The directory must exist and be a directory.
        let path_buf = PathBuf::from(&path);
        let canonical = match path_buf.canonicalize() {
            Ok(c) => c,
            Err(e) => {
                return err_cmd(
                    start,
                    format!(
                        "path does not exist or cannot be canonicalized: {}: {}",
                        path, e
                    ),
                );
            }
        };
        if !canonical.is_dir() {
            return err_cmd(start, format!("path {} is not a directory", path));
        }
        if validate_project_path_policy(policy, &canonical).is_err() {
            return project_error_cmd(start, "path_outside_allowed_roots");
        }
        if !overwrite {
            match matching_existing_project(
                projects_dir,
                &id,
                &name,
                &path,
                description.as_deref(),
                allow_patch,
            ) {
                Ok(Some(project)) => {
                    return ok_cmd(
                        start,
                        recovered_project_result(
                            kind,
                            &runtime_id,
                            &client_id,
                            &project,
                            None,
                            false,
                        ),
                    )
                }
                Ok(None) => {}
                Err(code) => return project_error_cmd(start, code),
            }
        }
        let write_result =
            match write_project_toml_atomic(projects_dir, &id, &toml_content, overwrite) {
                Ok(p) => p,
                Err(ProjectTomlWriteError::BeforeRename) => {
                    return project_error_cmd(start, "operation_failed")
                }
                Err(ProjectTomlWriteError::AfterRename) => {
                    return project_error_cmd(start, "operation_indeterminate")
                }
            };
        let result = serde_json::json!({
            "id": runtime_id,
            "agent_project_id": id,
            "client_id": client_id,
            "name": name,
            "path": path,
            "description": description,
            "projects_config_path": write_result.config_path.to_string_lossy(),
            "created_config": write_result.created_config,
            "overwritten": write_result.overwritten,
            "allow_patch": allow_patch,
            "revision": project_revision(&parse_agent_project_toml(&toml_content).expect("generated project TOML must parse")),
            "operation": "register", "outcome": "registered", "changed": true, "recovered": false,
        });
        return ok_cmd(start, result);
    }

    // create_project
    let path_buf = PathBuf::from(&path);
    let mut created_directory = false;
    let mut created_paths = CreatedProjectPaths::default();

    // Determine the canonical parent for policy validation. If the path exists,
    // canonicalize it directly. If not, canonicalize the existing ancestor.
    let canonical_for_policy = if path_buf.exists() {
        match path_buf.canonicalize() {
            Ok(c) => c,
            Err(e) => {
                return err_cmd(
                    start,
                    format!("path cannot be canonicalized: {}: {}", path, e),
                );
            }
        }
    } else {
        // Find the nearest existing ancestor and canonicalize it.
        let mut ancestor = path_buf.clone();
        while !ancestor.exists() {
            if let Some(parent) = ancestor.parent() {
                ancestor = parent.to_path_buf();
            } else {
                break;
            }
        }
        match ancestor.canonicalize() {
            Ok(c) => c,
            Err(e) => {
                return err_cmd(
                    start,
                    format!(
                        "parent path cannot be canonicalized: {}: {}",
                        ancestor.display(),
                        e
                    ),
                );
            }
        }
    };
    if validate_project_path_policy(policy, &canonical_for_policy).is_err() {
        return project_error_cmd(start, "path_outside_allowed_roots");
    }
    if !overwrite {
        match matching_existing_project(
            projects_dir,
            &id,
            &name,
            &path,
            description.as_deref(),
            allow_patch,
        ) {
            Ok(Some(project)) => {
                if let Err(code) = validate_recovered_create_side_effects(
                    &path_buf,
                    &template,
                    description.as_deref(),
                    git_init,
                ) {
                    return project_error_cmd(start, code);
                }
                return ok_cmd(
                    start,
                    recovered_project_result(
                        kind,
                        &runtime_id,
                        &client_id,
                        &project,
                        Some(&template),
                        git_init,
                    ),
                );
            }
            Ok(None) => {}
            Err(code) => return project_error_cmd(start, code),
        }
    }

    // Handle existing vs new directory.
    if path_buf.exists() {
        let meta = match std::fs::metadata(&path_buf) {
            Ok(m) => m,
            Err(e) => return err_cmd(start, format!("failed to stat path {}: {}", path, e)),
        };
        if !meta.is_dir() {
            return err_cmd(
                start,
                format!("path {} exists but is not a directory", path),
            );
        }
        // Check if the directory is empty.
        let is_empty = match std::fs::read_dir(&path_buf) {
            Ok(mut it) => it.next().is_none(),
            Err(e) => {
                return err_cmd(start, format!("failed to read directory {}: {}", path, e));
            }
        };
        if !is_empty {
            return project_error_cmd(start, "path_not_empty");
        }
        if !allow_existing_empty {
            return project_error_cmd(start, "path_not_empty");
        }
    } else {
        // Create the directory.
        if let Err(e) = std::fs::create_dir_all(&path_buf) {
            return err_cmd(start, format!("failed to create directory {}: {}", path, e));
        }
        created_directory = true;
        created_paths.mark_project_dir_created(path_buf.clone());
    }

    // Apply template.
    if template == "basic" {
        let readme = if let Some(ref desc) = description {
            format!("# {}\n\n{}\n", name, desc)
        } else {
            format!("# {}\n", name)
        };
        let readme_path = path_buf.join("README.md");
        if let Err(e) = write_created_file(&readme_path, readme.as_bytes(), &mut created_paths) {
            created_paths.cleanup();
            return err_cmd(start, format!("failed to write README.md: {}", e));
        }
        let gitignore = "target/\nnode_modules/\n.env\n*.log\n";
        let gitignore_path = path_buf.join(".gitignore");
        if let Err(e) =
            write_created_file(&gitignore_path, gitignore.as_bytes(), &mut created_paths)
        {
            created_paths.cleanup();
            return err_cmd(start, format!("failed to write .gitignore: {}", e));
        }
    } else if template == "empty" {
        // For empty template, optionally create README.md if description is provided.
        if let Some(ref desc) = description {
            let readme = format!("# {}\n\n{}\n", name, desc);
            let readme_path = path_buf.join("README.md");
            if let Err(e) = write_created_file(&readme_path, readme.as_bytes(), &mut created_paths)
            {
                created_paths.cleanup();
                return err_cmd(start, format!("failed to write README.md: {}", e));
            }
        }
    }

    // git init.
    let mut git_initialized = false;
    if git_init {
        match run_git_bounded(&path_buf, &["init"], Duration::from_secs(5), None) {
            Ok(output) if output.status.success() => {
                git_initialized = true;
                created_paths.track(path_buf.join(".git"));
            }
            Ok(output) => {
                created_paths.cleanup();
                let stderr = String::from_utf8_lossy(&output.stderr);
                let suffix = if output.stderr_capped {
                    " [stderr truncated]"
                } else {
                    ""
                };
                return err_cmd(
                    start,
                    format!("git init failed: {}{}", stderr.trim(), suffix),
                );
            }
            Err(e) => {
                created_paths.cleanup();
                return err_cmd(start, format!("git init failed (is git installed?): {}", e));
            }
        }
    }

    // Write project TOML.
    let write_result = match write_project_toml_atomic(projects_dir, &id, &toml_content, overwrite)
    {
        Ok(p) => p,
        Err(ProjectTomlWriteError::BeforeRename) => {
            created_paths.cleanup();
            return project_error_cmd(start, "operation_failed");
        }
        Err(ProjectTomlWriteError::AfterRename) => {
            return project_error_cmd(start, "operation_indeterminate");
        }
    };
    let result = serde_json::json!({
        "id": runtime_id,
        "agent_project_id": id,
        "client_id": client_id,
        "name": name,
        "path": path,
        "description": description,
        "projects_config_path": write_result.config_path.to_string_lossy(),
        "created_directory": created_directory,
        "created_config": write_result.created_config,
        "overwritten": write_result.overwritten,
        "allow_patch": allow_patch,
        "template": template,
        "revision": project_revision(&parse_agent_project_toml(&toml_content).expect("generated project TOML must parse")),
        "git_initialized": git_initialized,
        "operation": "create", "outcome": "created", "changed": true, "recovered": false,
    });
    ok_cmd(start, result)
}

#[cfg(test)]
mod durability_tests {
    use super::*;

    #[test]
    fn registry_parent_sync_failures_are_not_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing").join("demo.toml");
        let error = sync_parent_dir(&missing).unwrap_err();
        assert!(error.contains("sync project registry directory"));
    }

    #[test]
    fn registry_loader_ignores_temp_and_unregister_tombstones() {
        let tmp = tempfile::tempdir().unwrap();
        let projects_dir = tmp.path().join("projects.d");
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&projects_dir).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        let content = build_project_toml("demo", "Demo", source.to_str().unwrap(), &None, true);
        std::fs::write(projects_dir.join("demo.toml"), &content).unwrap();
        std::fs::write(projects_dir.join(".demo.random.toml.tmp"), &content).unwrap();
        std::fs::write(
            projects_dir.join(".demo.random.toml.unregistering"),
            &content,
        )
        .unwrap();
        let projects = load_agent_project_summaries_from_dir(&projects_dir);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, "demo");
    }

    #[cfg(unix)]
    #[test]
    fn project_summary_reports_retargeted_symlinks_as_distinct_canonical_roots() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        let link = tmp.path().join("current");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        symlink(&first, &link).unwrap();
        let project = AgentProjectFile {
            id: "demo".to_string(),
            path: link.to_string_lossy().to_string(),
            shell_profile: None,
            allow_patch: true,
            name: None,
            kind: None,
            description: None,
            disabled: false,
            hooks: HashMap::new(),
        };

        let first_summary = agent_project_summary(&project, 1, false);
        assert_eq!(
            Path::new(&first_summary.path),
            first.canonicalize().unwrap()
        );
        std::fs::remove_file(&link).unwrap();
        symlink(&second, &link).unwrap();
        let second_summary = agent_project_summary(&project, 2, false);
        assert_eq!(
            Path::new(&second_summary.path),
            second.canonicalize().unwrap()
        );
        assert_ne!(first_summary.path, second_summary.path);
    }
}

#[cfg(all(test, unix))]
mod shutdown_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    fn process_exists(pid: i32) -> bool {
        // A Git descendant that ignored SIGTERM is SIGKILL'd by the private
        // process-group cleanup; once its parent (the Git leader) exits, it
        // lingers as a zombie until reaped. `kill(pid, 0)` still succeeds for a
        // zombie because the PID entry persists, so it cannot tell a reaped
        // leader from an unreaped zombie. Treat a zombie (state `Z`) as gone
        // on Linux; on other Unixes fall back to the kill probe, which is the
        // best liveness signal available without `/proc`.
        #[cfg(target_os = "linux")]
        {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                return false;
            };
            // /proc/<pid>/stat is `pid (comm) state ...`; `comm` may contain
            // spaces or parens, so split on the last `") "`.
            return stat
                .rsplit_once(") ")
                .and_then(|(_, rest)| rest.chars().next())
                .is_some_and(|state| state != 'Z');
        }
        #[cfg(not(target_os = "linux"))]
        {
            // SAFETY: signal 0 only probes a test child pid read from our fixture.
            (unsafe { libc::kill(pid, 0) }) == 0
                || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        }
    }

    #[test]
    fn project_git_scan_shutdown_kills_hanging_process_group_and_returns_bounded() {
        let root = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(root.path())
            .status()
            .unwrap();
        assert!(status.success());

        let pid_path = root.path().join("fsmonitor.pid");
        let hook = root.path().join("hanging-fsmonitor");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\ntrap '' TERM\nprintf '%s' \"$$\" > '{}'\nwhile :; do sleep 1; done\n",
                pid_path.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        let status = Command::new("git")
            .args(["config", "core.fsmonitor", hook.to_string_lossy().as_ref()])
            .current_dir(root.path())
            .status()
            .unwrap();
        assert!(status.success());

        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let project = root.path().to_path_buf();
        let started = Instant::now();
        let worker = thread::spawn(move || {
            run_git_bounded(
                &project,
                &["status", "--short"],
                Duration::from_secs(5),
                Some(worker_shutdown.as_ref()),
            )
        });
        let ready_deadline = Instant::now() + Duration::from_secs(1);
        while !pid_path.exists() && Instant::now() < ready_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let hook_started = pid_path.exists();
        shutdown.store(true, Ordering::SeqCst);
        let result = worker.join().unwrap();
        assert!(hook_started, "Git did not start the hanging fixture hook");
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "project Git scan ignored the shutdown flag"
        );

        let hook_pid = std::fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let gone_deadline = Instant::now() + Duration::from_secs(1);
        while process_exists(hook_pid) && Instant::now() < gone_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !process_exists(hook_pid),
            "hanging Git descendant survived process-group cleanup"
        );
    }
}
