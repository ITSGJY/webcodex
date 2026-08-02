use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use toml::{Table, Value as TomlValue};

use super::connections::{canonical_server_url, ensure_real_directory_tree};
use super::http::{fetch_runtime_status, post_json_authed, ApiCall};
use super::login::validate_client_id;
use super::profiles::{
    client_output_dir_for_profile, client_state_dir_for_profile, default_client_base_dir,
    default_client_state_base_dir, validate_client_profile,
};
use super::system::discover_internal_binary;

const CONNECT_MARKER_FILE: &str = "hosted-connect";
const KEY_DISCLOSED_FILE: &str = ".hosted-key-disclosed";
const RUNNER_STATE_FILE: &str = "runner.toml";
const RUNNER_LOG_FILE: &str = "runner.log";
const CONNECT_LOCK_FILE: &str = "connect.lock";
const DEFAULT_CONNECT_WAIT_MS: u64 = 15_000;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectOptions {
    pub(crate) server_url: String,
    pub(crate) key: Option<String>,
    pub(crate) key_file: Option<PathBuf>,
    pub(crate) project: PathBuf,
    pub(crate) profile: Option<String>,
    pub(crate) client_id: Option<String>,
    pub(crate) project_id: Option<String>,
    // Test seams are intentionally not exposed as command-line flags.
    pub(crate) config_base: Option<PathBuf>,
    pub(crate) state_base: Option<PathBuf>,
    pub(crate) runner_bin: Option<PathBuf>,
    pub(crate) wait_timeout_ms: u64,
}

#[derive(Debug, Clone)]
struct ResolvedKey {
    value: String,
    generated: bool,
    recovered_profile: Option<String>,
    warn_short: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ExistingAgentConfig {
    server_url: String,
    token: String,
    client_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ProjectFile {
    id: String,
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shell_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default = "default_true")]
    allow_patch: bool,
    #[serde(default)]
    disabled: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    hooks: BTreeMap<String, Vec<String>>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RunnerState {
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
enum RunnerStart {
    Started,
    Reused,
}

struct ProfileLock {
    file: File,
}

impl ProfileLock {
    fn acquire(state_dir: &Path) -> Result<Self, String> {
        let path = state_dir.join(CONNECT_LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|error| format!("failed to open profile lock {}: {error}", path.display()))?;
        file.try_lock_exclusive().map_err(|_| {
            format!(
                "another WebCodex command is updating this profile; retry after it finishes ({})",
                path.display()
            )
        })?;
        Ok(Self { file })
    }
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn sha256_hex(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

fn generate_shared_key() -> String {
    // Three independently generated UUID v4 values provide about 366 random
    // bits after their fixed version/variant bits, comfortably above 256 bits.
    let random = (0..3)
        .map(|_| uuid::Uuid::new_v4().simple().to_string())
        .collect::<String>();
    format!("wck_{random}")
}

fn normalize_shared_key(value: &str) -> Result<String, String> {
    let key = value.trim();
    if key.is_empty() {
        return Err("shared key cannot be empty".to_string());
    }
    if key.starts_with("wc_") {
        return Err(
            "wc_* values are managed WebCodex credentials, not hosted shared keys; use a different random value for `webcodex connect`, or use `webcodex login` for the managed flow"
                .to_string(),
        );
    }
    Ok(key.to_string())
}

fn read_key_file(path: &Path) -> Result<String, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("failed to inspect key file {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("key file {} is not a regular file", path.display()));
    }
    if metadata.len() > 16 * 1024 {
        return Err(format!("key file {} is unexpectedly large", path.display()));
    }
    let value = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read key file {}: {error}", path.display()))?;
    normalize_shared_key(&value)
}

fn server_host_label(server_url: &str) -> String {
    let parsed = url::Url::parse(server_url).expect("canonical server URL must parse");
    let raw = parsed.host_str().unwrap_or("server");
    let mut label = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while label.contains("--") {
        label = label.replace("--", "-");
    }
    label = label.trim_matches('-').to_string();
    if label.is_empty() {
        label = "server".to_string();
    }
    label.chars().take(55).collect()
}

fn derived_profile(server_url: &str, key: &str) -> String {
    let key_hash = sha256_hex(key.as_bytes());
    let identity = sha256_hex(format!("{server_url}\0{key_hash}").as_bytes());
    format!("{}-{}", server_host_label(server_url), &identity[..12])
}

fn generated_client_id(server_url: &str) -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let host = server_host_label(server_url);
    let budget = 80usize.saturating_sub(17);
    format!(
        "{}-{}",
        host.chars().take(budget).collect::<String>(),
        &suffix[..16]
    )
}

fn sanitize_project_id(value: &str) -> String {
    let mut output = String::new();
    let mut previous_separator = false;
    for character in value.trim().chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
            previous_separator = false;
        } else if !previous_separator && !output.is_empty() {
            output.push('-');
            previous_separator = true;
        }
        if output.len() == 64 {
            break;
        }
    }
    output.truncate(output.trim_end_matches('-').len());
    if output.is_empty() {
        "project".to_string()
    } else {
        output
    }
}

fn validate_project_id(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("project id cannot be empty".to_string());
    }
    if value.len() > 64 {
        return Err("project id must be at most 64 characters".to_string());
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("project id may only contain ASCII letters, digits, '-', and '_'".to_string());
    }
    Ok(value.to_string())
}

fn validate_existing_regular_file(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
    if metadata.is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{} is not a regular file; refusing to read or replace it",
            path.display()
        ));
    }
    Ok(())
}

fn read_existing_agent_config(path: &Path) -> Result<Option<ExistingAgentConfig>, String> {
    if !path.exists() {
        return Ok(None);
    }
    validate_existing_regular_file(path)?;
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read agent config {}: {error}", path.display()))?;
    toml::from_str(&content)
        .map(Some)
        .map_err(|error| format!("failed to parse agent config {}: {error}", path.display()))
}

fn read_project_files(projects_dir: &Path) -> Result<Vec<(PathBuf, ProjectFile)>, String> {
    let entries = match std::fs::read_dir(projects_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "failed to read project directory {}: {error}",
                projects_dir.display()
            ))
        }
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("toml"))
        .collect::<Vec<_>>();
    paths.sort();
    let mut projects = Vec::new();
    for path in paths {
        validate_existing_regular_file(&path)?;
        let content = std::fs::read_to_string(&path)
            .map_err(|error| format!("failed to read project file {}: {error}", path.display()))?;
        let project: ProjectFile = toml::from_str(&content)
            .map_err(|error| format!("failed to parse project file {}: {error}", path.display()))?;
        validate_project_id(&project.id).map_err(|error| {
            format!(
                "invalid project id in project file {}: {error}",
                path.display()
            )
        })?;
        projects.push((path, project));
    }
    Ok(projects)
}

fn stored_project_matches(project: &ProjectFile, canonical_project: &Path) -> bool {
    Path::new(&project.path)
        .canonicalize()
        .is_ok_and(|path| path == canonical_project)
}

fn recover_key_for_project(
    config_base: &Path,
    canonical_server: &str,
    canonical_project: &Path,
    explicit_profile: Option<&str>,
) -> Result<Option<(String, String, bool)>, String> {
    let profiles = config_base.join("clients");
    let mut candidates = Vec::new();
    let profile_names = if let Some(profile) = explicit_profile {
        vec![profile.to_string()]
    } else {
        match std::fs::read_dir(&profiles) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
                .filter(|name| validate_client_profile(name).is_ok())
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(format!(
                    "failed to inspect existing profiles {}: {error}",
                    profiles.display()
                ))
            }
        }
    };
    for profile in profile_names {
        let profile_dir = client_output_dir_for_profile(config_base, &profile);
        let config_path = profile_dir.join("agent.toml");
        let Some(config) = read_existing_agent_config(&config_path)? else {
            continue;
        };
        let Ok(stored_server) = canonical_server_url(&config.server_url) else {
            continue;
        };
        if stored_server.url != canonical_server {
            continue;
        }
        let Ok(key) = normalize_shared_key(&config.token) else {
            continue;
        };
        let project_match = read_project_files(&profile_dir.join("projects.d"))?
            .iter()
            .any(|(_, project)| stored_project_matches(project, canonical_project));
        if project_match || explicit_profile.is_some() {
            let key_needs_display =
                key.starts_with("wck_") && !profile_dir.join(KEY_DISCLOSED_FILE).is_file();
            candidates.push((key, profile, key_needs_display));
        }
    }
    if candidates.len() > 1 {
        return Err(
            "more than one hosted profile matches this Server and project; rerun with --profile or --key"
                .to_string(),
        );
    }
    Ok(candidates.pop())
}

fn resolve_key(
    opts: &ConnectOptions,
    config_base: &Path,
    canonical_server: &str,
    canonical_project: &Path,
) -> Result<ResolvedKey, String> {
    if opts.key.is_some() && opts.key_file.is_some() {
        return Err("--key and --key-file are mutually exclusive".to_string());
    }
    if let Some(value) = &opts.key {
        let value = normalize_shared_key(value)?;
        return Ok(ResolvedKey {
            warn_short: value.len() < 16,
            value,
            generated: false,
            recovered_profile: None,
        });
    }
    if let Some(path) = &opts.key_file {
        let value = read_key_file(path)?;
        return Ok(ResolvedKey {
            warn_short: value.len() < 16,
            value,
            generated: false,
            recovered_profile: None,
        });
    }
    if let Some((value, profile, key_needs_display)) = recover_key_for_project(
        config_base,
        canonical_server,
        canonical_project,
        opts.profile.as_deref(),
    )? {
        return Ok(ResolvedKey {
            value,
            generated: key_needs_display,
            recovered_profile: Some(profile),
            warn_short: false,
        });
    }
    Ok(ResolvedKey {
        value: generate_shared_key(),
        generated: true,
        recovered_profile: None,
        warn_short: false,
    })
}

fn ensure_private_directory(path: &Path) -> Result<PathBuf, String> {
    let path = ensure_real_directory_tree(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("failed to protect {}: {error}", path.display()))?;
    }
    Ok(path)
}

fn atomic_write(path: &Path, content: &[u8], secret: bool) -> Result<bool, String> {
    if path.exists() {
        validate_existing_regular_file(path)?;
        let existing = std::fs::read(path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        if existing == content {
            if secret {
                protect_secret_file(path)?;
            }
            return Ok(false);
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{} has no valid file name", path.display()))?;
    let temporary = parent.join(format!(".{filename}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(if secret { 0o600 } else { 0o600 });
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| format!("failed to create {}: {error}", temporary.display()))?;
        file.write_all(content)
            .map_err(|error| format!("failed to write {}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| format!("failed to sync {}: {error}", temporary.display()))?;
        std::fs::rename(&temporary, path).map_err(|error| {
            format!(
                "failed to atomically replace {} with {}: {error}",
                path.display(),
                temporary.display()
            )
        })?;
        if secret {
            protect_secret_file(path)?;
        }
        Ok(true)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn protect_secret_file(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("failed to protect {}: {error}", path.display()))?;
    }
    Ok(())
}

fn render_project_file(project: &ProjectFile) -> Result<String, String> {
    toml::to_string(project).map_err(|error| format!("failed to render project config: {error}"))
}

fn resolve_project(
    projects_dir: &Path,
    canonical_project: &Path,
    explicit_id: Option<&str>,
) -> Result<(PathBuf, ProjectFile, bool), String> {
    let existing = read_project_files(projects_dir)?;
    if let Some((path, project)) = existing
        .iter()
        .find(|(_, project)| stored_project_matches(project, canonical_project))
    {
        if explicit_id.is_some_and(|id| id.trim() != project.id) {
            return Err(format!(
                "project {} is already registered as {}; refusing to create a duplicate",
                canonical_project.display(),
                project.id
            ));
        }
        return Ok((path.clone(), project.clone(), true));
    }

    let basename = canonical_project
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("project");
    let explicit = explicit_id.map(validate_project_id).transpose()?;
    let mut id = explicit
        .clone()
        .unwrap_or_else(|| sanitize_project_id(basename));
    if let Some((_, collision)) = existing.iter().find(|(_, project)| project.id == id) {
        if explicit.is_some() {
            return Err(format!(
                "project id {} is already registered for a different path; choose another --project-id",
                collision.id
            ));
        }
        let path_hash = sha256_hex(canonical_project.to_string_lossy().as_bytes());
        let suffix = &path_hash[..8];
        let budget = 64usize.saturating_sub(suffix.len() + 1);
        id = format!(
            "{}-{suffix}",
            id.chars()
                .take(budget)
                .collect::<String>()
                .trim_end_matches('-')
        );
        if existing.iter().any(|(_, project)| project.id == id) {
            return Err(format!(
                "derived project id {id} is already registered for a different path; use --project-id"
            ));
        }
    }
    let project_path = projects_dir.join(format!("{id}.toml"));
    Ok((
        project_path,
        ProjectFile {
            id,
            path: canonical_project.to_string_lossy().to_string(),
            shell_profile: None,
            name: Some(basename.to_string()),
            kind: None,
            description: None,
            allow_patch: true,
            disabled: false,
            hooks: BTreeMap::new(),
        },
        false,
    ))
}

fn read_agent_document(path: &Path) -> Result<Table, String> {
    if !path.exists() {
        return Ok(Table::new());
    }
    validate_existing_regular_file(path)?;
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read agent config {}: {error}", path.display()))?;
    let document: TomlValue = toml::from_str(&content)
        .map_err(|error| format!("failed to parse agent config {}: {error}", path.display()))?;
    document
        .as_table()
        .cloned()
        .ok_or_else(|| format!("agent config {} is not a TOML table", path.display()))
}

fn render_agent_document(
    path: &Path,
    server_url: &str,
    key: &str,
    client_id: &str,
    projects_dir: &Path,
    canonical_project: &Path,
) -> Result<String, String> {
    let mut root = read_agent_document(path)?;
    root.insert(
        "server_url".to_string(),
        TomlValue::String(server_url.to_string()),
    );
    root.insert("token".to_string(), TomlValue::String(key.to_string()));
    root.insert(
        "client_id".to_string(),
        TomlValue::String(client_id.to_string()),
    );
    root.insert(
        "display_name".to_string(),
        TomlValue::String(client_id.to_string()),
    );
    root.remove("owner");
    root.insert(
        "projects_dir".to_string(),
        TomlValue::String(projects_dir.to_string_lossy().to_string()),
    );
    root.insert(
        "transport".to_string(),
        TomlValue::String("websocket".to_string()),
    );
    root.entry("poll_interval_ms".to_string())
        .or_insert(TomlValue::Integer(1000));

    let policy = root
        .entry("policy".to_string())
        .or_insert_with(|| TomlValue::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| format!("agent config {} has a non-table policy", path.display()))?;
    policy.insert("allow_raw_shell".to_string(), TomlValue::Boolean(true));
    policy.insert("allow_cwd_anywhere".to_string(), TomlValue::Boolean(false));
    let mut roots = policy
        .get("allowed_roots")
        .and_then(TomlValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(TomlValue::as_str)
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    roots.insert(canonical_project.to_string_lossy().to_string());
    policy.insert(
        "allowed_roots".to_string(),
        TomlValue::Array(roots.into_iter().map(TomlValue::String).collect()),
    );
    toml::to_string(&root).map_err(|error| format!("failed to render agent config: {error}"))
}

fn validate_existing_profile(
    config: Option<&ExistingAgentConfig>,
    canonical_server: &str,
    key: &str,
) -> Result<(), String> {
    let Some(config) = config else {
        return Ok(());
    };
    let stored_server = canonical_server_url(&config.server_url)
        .map_err(|_| "existing profile has an invalid server URL".to_string())?;
    if stored_server.url != canonical_server {
        return Err(
            "selected profile belongs to a different Server; choose another --profile".to_string(),
        );
    }
    if config.token.trim() != key {
        return Err(
            "selected profile belongs to a different shared key; choose another --profile"
                .to_string(),
        );
    }
    Ok(())
}

async fn preflight_shared_key(server_url: &str, key: &str) -> Result<(), String> {
    post_json_authed(ApiCall {
        server_url,
        token: key,
        path: "/api/projects/list",
        body: json!({}),
    })
    .await
    .map(|_| ())
    .map_err(|error| {
        format!(
            "Server did not accept hosted shared-key access: {error}. Confirm shared-key mode is enabled and use a non-wc_ key"
        )
    })
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

fn load_runner_state(state_dir: &Path) -> Result<Option<RunnerState>, String> {
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

fn process_matches(state: &RunnerState) -> bool {
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

fn stop_runner_unlocked(state_dir: &Path) -> Result<bool, String> {
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

fn ensure_runner_unlocked(
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

fn runtime_client_online(output: &JsonValue, client_id: &str) -> bool {
    output
        .pointer("/agents/clients")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .any(|client| {
            client.get("client_id").and_then(JsonValue::as_str) == Some(client_id)
                && (client.get("connected").and_then(JsonValue::as_bool) == Some(true)
                    || client.get("status").and_then(JsonValue::as_str) == Some("online"))
        })
}

fn project_visible(output: &JsonValue, runtime_project_id: &str, client_id: &str) -> bool {
    output
        .pointer("/output/projects")
        .or_else(|| output.get("projects"))
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .any(|project| {
            project.get("id").and_then(JsonValue::as_str) == Some(runtime_project_id)
                && project.get("client_id").and_then(JsonValue::as_str) == Some(client_id)
                && project
                    .get("connected")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(true)
        })
}

async fn wait_for_connection(
    server_url: &str,
    key: &str,
    client_id: &str,
    runtime_project_id: &str,
    state_dir: &Path,
    timeout_ms: u64,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    let mut last_error = None;
    loop {
        let summary = local_runner_state_summary(state_dir)?;
        if !summary.running {
            return Err("Runner exited before it registered with the Server".to_string());
        }
        let runtime = fetch_runtime_status(server_url, Some(key)).await;
        let projects = post_json_authed(ApiCall {
            server_url,
            token: key,
            path: "/api/projects/list",
            body: json!({}),
        })
        .await;
        match (runtime, projects) {
            (Ok(runtime), Ok(projects))
                if runtime
                    .output
                    .as_ref()
                    .is_some_and(|output| runtime_client_online(output, client_id))
                    && project_visible(&projects, runtime_project_id, client_id) =>
            {
                return Ok(())
            }
            (Ok(runtime), Ok(_)) if !runtime.reachable => {
                last_error = runtime.error;
            }
            (Err(error), _) | (_, Err(error)) => last_error = Some(error),
            _ => {}
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(format!(
        "timed out waiting for Runner and project visibility{}",
        last_error
            .map(|error| format!(": {error}"))
            .unwrap_or_default()
    ))
}

pub(crate) async fn run_connect(opts: ConnectOptions) -> Result<String, String> {
    let canonical_server = canonical_server_url(&opts.server_url)?;
    let canonical_project = opts.project.canonicalize().map_err(|error| {
        format!(
            "project path {} does not exist or cannot be resolved: {error}",
            opts.project.display()
        )
    })?;
    if !canonical_project.is_dir() {
        return Err(format!(
            "project path {} is not a directory",
            canonical_project.display()
        ));
    }
    let explicit_profile = opts
        .profile
        .as_deref()
        .map(validate_client_profile)
        .transpose()?;
    let config_base = opts
        .config_base
        .clone()
        .unwrap_or_else(default_client_base_dir);
    let state_base = opts
        .state_base
        .clone()
        .unwrap_or_else(default_client_state_base_dir);
    let resolved_key = resolve_key(
        &opts,
        &config_base,
        &canonical_server.url,
        &canonical_project,
    )?;
    let profile = explicit_profile
        .or(resolved_key.recovered_profile.clone())
        .unwrap_or_else(|| derived_profile(&canonical_server.url, &resolved_key.value));
    let profile = validate_client_profile(&profile)?;
    let config_base = ensure_real_directory_tree(&config_base)?;
    let state_base = ensure_real_directory_tree(&state_base)?;
    let profile_dir =
        ensure_private_directory(&client_output_dir_for_profile(&config_base, &profile))?;
    let state_dir = ensure_private_directory(&client_state_dir_for_profile(&state_base, &profile))?;
    let _lock = ProfileLock::acquire(&state_dir)?;

    let config_path = profile_dir.join("agent.toml");
    let projects_dir = ensure_private_directory(&profile_dir.join("projects.d"))?;
    let log_path = local_runner_log_path(&state_dir);
    let existing_config = read_existing_agent_config(&config_path)?;
    validate_existing_profile(
        existing_config.as_ref(),
        &canonical_server.url,
        &resolved_key.value,
    )?;
    let existing_summary = local_runner_state_summary(&state_dir)?;
    let client_id = match (&opts.client_id, existing_config.as_ref()) {
        (Some(requested), Some(existing)) => {
            let requested = validate_client_id(requested)?;
            if requested != existing.client_id && existing_summary.running {
                return Err(
                    "--client-id differs from the active profile; stop that Runner before changing its identity"
                        .to_string(),
                );
            }
            requested
        }
        (Some(requested), None) => validate_client_id(requested)?,
        (None, Some(existing)) => validate_client_id(&existing.client_id)?,
        (None, None) => generated_client_id(&canonical_server.url),
    };
    let (project_path, project, already_registered) = resolve_project(
        &projects_dir,
        &canonical_project,
        opts.project_id.as_deref(),
    )?;
    let runtime_project_id = format!("agent:{client_id}:{}", project.id);
    let runner_bin = opts
        .runner_bin
        .clone()
        .or_else(|| discover_internal_binary("webcodex-runner"))
        .ok_or_else(|| {
            "webcodex-runner was not found beside webcodex or in an absolute PATH entry".to_string()
        })?;

    // Fail before replacing a healthy profile when the destination cannot
    // authenticate this direct shared key at all.
    preflight_shared_key(&canonical_server.url, &resolved_key.value).await?;

    let project_changed = if already_registered {
        false
    } else {
        let project_content = render_project_file(&project)?;
        atomic_write(&project_path, project_content.as_bytes(), false)?
    };
    let agent_content = render_agent_document(
        &config_path,
        &canonical_server.url,
        &resolved_key.value,
        &client_id,
        &projects_dir,
        &canonical_project,
    )?;
    atomic_write(&config_path, agent_content.as_bytes(), true)?;
    atomic_write(
        &local_runner_profile_marker(&state_dir),
        format!("profile = {profile:?}\n").as_bytes(),
        false,
    )?;

    if project_changed
        && load_runner_state(&state_dir)?
            .as_ref()
            .is_some_and(process_matches)
    {
        stop_runner_unlocked(&state_dir)?;
    }
    let start = ensure_runner_unlocked(&runner_bin, &config_path, &state_dir).map_err(|error| {
        format!(
            "{error}. Runner logs: {}",
            local_runner_log_path(&state_dir).display()
        )
    })?;
    if let Err(error) = wait_for_connection(
        &canonical_server.url,
        &resolved_key.value,
        &client_id,
        &runtime_project_id,
        &state_dir,
        if opts.wait_timeout_ms == 0 {
            DEFAULT_CONNECT_WAIT_MS
        } else {
            opts.wait_timeout_ms
        },
    )
    .await
    {
        if start == RunnerStart::Started {
            let _ = stop_runner_unlocked(&state_dir);
        }
        return Err(format!("{error}. Runner logs: {}", log_path.display()));
    }
    if resolved_key.generated {
        atomic_write(
            &profile_dir.join(KEY_DISCLOSED_FILE),
            b"disclosed = true\n",
            false,
        )?;
    }

    let mut output = String::new();
    output.push_str("Connected to WebCodex\n\n");
    output.push_str(&format!("Server:       {}\n", canonical_server.url));
    output.push_str(&format!("MCP URL:      {}/mcp\n", canonical_server.url));
    output.push_str(&format!("Profile:      {profile}\n"));
    output.push_str(&format!("Client:       {client_id}\n"));
    output.push_str(&format!("Project:      {runtime_project_id}\n"));
    output.push_str("Runner:       running\n");
    output.push_str(&format!("Config:       {}\n", config_path.display()));
    output.push_str(&format!("Logs:         {}\n", log_path.display()));
    if resolved_key.warn_short {
        output.push_str(
            "\nWarning: the supplied shared key is short; use a long random value when possible.\n",
        );
    }
    if resolved_key.generated {
        output.push_str(&format!("\nMCP key: {}\n", resolved_key.value));
        output.push_str(
            "Copy this key now. It will not be printed in full by status commands.\n\
Use the same key in your MCP client.\n",
        );
    }
    output.push_str(&format!(
        "\nMCP URL: {}/mcp\nAuthentication: Bearer token\nToken: the same key used by this command\n",
        canonical_server.url
    ));
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_key_is_strong_and_not_managed() {
        let key = generate_shared_key();
        assert!(key.starts_with("wck_"));
        assert!(!key.starts_with("wc_"));
        assert!(key.len() >= 4 + 96);
    }

    #[test]
    fn explicit_shared_key_validation_trims_and_rejects_managed_values() {
        assert_eq!(
            normalize_shared_key("  shared-key  ").unwrap(),
            "shared-key"
        );
        assert!(normalize_shared_key("  ").unwrap_err().contains("empty"));
        for managed in ["wc_pat_example", "wc_agent_example", "wc_acct_example"] {
            let error = normalize_shared_key(managed).unwrap_err();
            assert!(error.contains("managed WebCodex credentials"));
            assert!(!error.contains(managed));
        }
    }

    #[test]
    fn profile_is_stable_and_separates_keys_and_origins() {
        let first = derived_profile("https://example.test", "alpha");
        assert_eq!(first, derived_profile("https://example.test", "alpha"));
        assert_ne!(first, derived_profile("https://example.test", "beta"));
        assert_ne!(first, derived_profile("http://example.test", "alpha"));
        validate_client_profile(&first).unwrap();
    }

    #[test]
    fn omitted_key_is_generated_once_then_recovered_from_the_matching_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let config_base = tmp.path().join("config");
        let options = ConnectOptions {
            server_url: "https://example.test".to_string(),
            key: None,
            key_file: None,
            project: project.clone(),
            profile: None,
            client_id: None,
            project_id: None,
            config_base: Some(config_base.clone()),
            state_base: None,
            runner_bin: None,
            wait_timeout_ms: 100,
        };
        let first = resolve_key(&options, &config_base, "https://example.test", &project).unwrap();
        assert!(first.generated);
        let profile = derived_profile("https://example.test", &first.value);
        let profile_dir = config_base.join("clients").join(&profile);
        std::fs::create_dir_all(profile_dir.join("projects.d")).unwrap();
        std::fs::write(
            profile_dir.join("agent.toml"),
            format!(
                "server_url = \"https://example.test\"\ntoken = {:?}\nclient_id = \"client\"\n",
                first.value
            ),
        )
        .unwrap();
        std::fs::write(
            profile_dir.join("projects.d/project.toml"),
            format!("id = \"project\"\npath = {:?}\n", project.to_string_lossy()),
        )
        .unwrap();
        let recovered =
            resolve_key(&options, &config_base, "https://example.test", &project).unwrap();
        assert!(recovered.generated);
        assert_eq!(recovered.value, first.value);
        assert_eq!(
            recovered.recovered_profile.as_deref(),
            Some(profile.as_str())
        );
        std::fs::write(profile_dir.join(KEY_DISCLOSED_FILE), "disclosed = true\n").unwrap();
        let disclosed =
            resolve_key(&options, &config_base, "https://example.test", &project).unwrap();
        assert!(!disclosed.generated);
    }

    #[test]
    fn project_id_sanitization_is_runner_compatible() {
        assert_eq!(
            sanitize_project_id("Hello, 世界 / repo.git"),
            "hello-repo-git"
        );
        assert_eq!(sanitize_project_id("..."), "project");
        validate_project_id(&sanitize_project_id(&"a".repeat(100))).unwrap();
    }

    #[test]
    fn project_collision_gets_stable_suffix_and_explicit_collision_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects.d");
        std::fs::create_dir(&projects).unwrap();
        let one = tmp.path().join("one/demo");
        let two = tmp.path().join("two/demo");
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();
        let (_, first, _) = resolve_project(&projects, &one.canonicalize().unwrap(), None).unwrap();
        atomic_write(
            &projects.join(format!("{}.toml", first.id)),
            render_project_file(&first).unwrap().as_bytes(),
            false,
        )
        .unwrap();
        let (_, second, _) =
            resolve_project(&projects, &two.canonicalize().unwrap(), None).unwrap();
        assert!(second.id.starts_with("demo-"));
        assert_eq!(
            second.id,
            resolve_project(&projects, &two.canonicalize().unwrap(), None)
                .unwrap()
                .1
                .id
        );
        let error =
            resolve_project(&projects, &two.canonicalize().unwrap(), Some("demo")).unwrap_err();
        assert!(error.contains("different path"));
    }

    #[cfg(unix)]
    #[test]
    fn atomic_config_updates_preserve_secret_permissions_and_merge_roots() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("agent.toml");
        let project_one = tmp.path().join("one");
        let project_two = tmp.path().join("two");
        let projects = tmp.path().join("projects.d");
        std::fs::create_dir(&project_one).unwrap();
        std::fs::create_dir(&project_two).unwrap();
        std::fs::create_dir(&projects).unwrap();
        let first = render_agent_document(
            &config,
            "https://example.test",
            "shared",
            "client",
            &projects,
            &project_one,
        )
        .unwrap();
        assert!(atomic_write(&config, first.as_bytes(), true).unwrap());
        let second = render_agent_document(
            &config,
            "https://example.test",
            "shared",
            "client",
            &projects,
            &project_two,
        )
        .unwrap();
        assert!(atomic_write(&config, second.as_bytes(), true).unwrap());
        let parsed: TomlValue = toml::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        let roots = parsed["policy"]["allowed_roots"].as_array().unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(
            std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!tmp.path().join("one/agent.toml").exists());
        assert!(!atomic_write(&config, second.as_bytes(), true).unwrap());
    }

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
