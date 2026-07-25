//! Experimental, allowlisted external tool backends for agent file operations.
//!
//! The first provider is a deliberately small stdio MCP client for
//! `claude mcp serve`. Native execution remains the default.

use super::config::{ClaudeCodeMcpConfig, ToolProviderStrategy, ToolProvidersConfig};
use super::files::sha256_hex_bytes;
use super::output::CommandResult;
use super::patches::validate_line_edit_agent_path;
use super::shell::cwd_allowed;
use super::AgentPolicy;
use crate::shell_protocol::{
    ClaudeCodeProviderStatus, ShellAgentShellRequest, ToolProvidersStatus,
    EXTERNAL_SEARCH_REQUEST_PREFIX,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_MCP_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_MCP_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_PENDING_REQUESTS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ProviderCapability {
    SearchProjectText,
    EditFile,
}

impl ProviderCapability {
    fn name(self) -> &'static str {
        match self {
            Self::SearchProjectText => "search_project_text",
            Self::EditFile => "edit_file",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteState {
    NotStarted,
    Uncertain,
}

#[derive(Debug, Clone)]
struct ProviderError {
    code: &'static str,
    write_state: WriteState,
}

impl ProviderError {
    fn new(code: &'static str) -> Self {
        Self {
            code,
            write_state: WriteState::NotStarted,
        }
    }

    fn with_state(mut self, write_state: WriteState) -> Self {
        self.write_state = write_state;
        self
    }
}

struct ToolExecutionContext<'a> {
    project_root: &'a Path,
    target: PathBuf,
    relative_path: &'a str,
    max_output_bytes: usize,
    timeout_secs: u64,
}

pub(crate) enum ExternalRoute {
    Native,
    Handled(CommandResult),
}

pub(crate) struct ExternalToolRouter {
    strategy: ToolProviderStrategy,
    claude: ClaudeCodeMcpProvider,
}

static EXTERNAL_TOOLS: OnceLock<ExternalToolRouter> = OnceLock::new();

pub(crate) fn configure_external_tools(
    config: &ToolProvidersConfig,
) -> &'static ExternalToolRouter {
    EXTERNAL_TOOLS.get_or_init(|| ExternalToolRouter::new(config))
}

pub(crate) fn external_tools() -> &'static ExternalToolRouter {
    EXTERNAL_TOOLS.get_or_init(|| ExternalToolRouter::new(&ToolProvidersConfig::default()))
}

impl ExternalToolRouter {
    pub(crate) fn new(config: &ToolProvidersConfig) -> Self {
        Self {
            strategy: config.strategy,
            claude: ClaudeCodeMcpProvider::new(config.claude_code.clone()),
        }
    }

    pub(crate) fn shutdown(&self) {
        self.claude.shutdown();
    }

    pub(crate) fn status(&self) -> ToolProvidersStatus {
        ToolProvidersStatus {
            strategy: match self.strategy {
                ToolProviderStrategy::Native => "native",
                ToolProviderStrategy::ClaudeCode => "claude_code",
                ToolProviderStrategy::ClaudeCodeThenNative => "claude_code_then_native",
            }
            .to_string(),
            claude_code: self.claude.status(),
        }
    }

    pub(crate) fn route(
        &self,
        policy: &AgentPolicy,
        request: &ShellAgentShellRequest,
    ) -> ExternalRoute {
        if self.strategy == ToolProviderStrategy::Native {
            return ExternalRoute::Native;
        }
        let capability = match request.kind.as_str() {
            "run_shell"
                if request.command.lines().next() == Some(EXTERNAL_SEARCH_REQUEST_PREFIX) =>
            {
                ProviderCapability::SearchProjectText
            }
            "file_replace_in_file" => ProviderCapability::EditFile,
            _ => return ExternalRoute::Native,
        };
        let started = Instant::now();
        let raw = if capability == ProviderCapability::SearchProjectText {
            request.stdin.as_deref()
        } else {
            request.content.as_deref()
        };
        let payload = match raw
            .ok_or_else(request_error)
            .and_then(|raw| serde_json::from_str(raw).map_err(|_| request_error()))
        {
            Ok(payload) => payload,
            Err(error) => return self.failure_or_native(capability, error, started),
        };
        let checked = validate_context(policy, request, capability, &payload);
        let (root, target, relative) = match checked {
            Ok(checked) => checked,
            Err(error) => {
                self.claude.record_error(&error);
                return ExternalRoute::Handled(provider_error_result(capability, error, started));
            }
        };
        let context = ToolExecutionContext {
            project_root: &root,
            target,
            relative_path: &relative,
            max_output_bytes: request
                .max_bytes
                .unwrap_or(MAX_MCP_OUTPUT_BYTES)
                .min(policy.max_output_bytes)
                .min(MAX_MCP_OUTPUT_BYTES),
            timeout_secs: request.timeout_secs.max(1).min(policy.max_timeout_secs),
        };
        match self.claude.call(capability, payload, context) {
            Ok(output) => ExternalRoute::Handled(command_result(
                output
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| output.to_string()),
                started,
            )),
            Err(error) => self.failure_or_native(capability, error, started),
        }
    }

    fn failure_or_native(
        &self,
        capability: ProviderCapability,
        error: ProviderError,
        started: Instant,
    ) -> ExternalRoute {
        self.claude.record_error(&error);
        if self.strategy == ToolProviderStrategy::ClaudeCodeThenNative
            && (capability != ProviderCapability::EditFile
                || error.write_state == WriteState::NotStarted)
        {
            ExternalRoute::Native
        } else {
            ExternalRoute::Handled(provider_error_result(capability, error, started))
        }
    }
}

fn provider_error_result(
    capability: ProviderCapability,
    error: ProviderError,
    started: Instant,
) -> CommandResult {
    let (write_state, changed) = match error.write_state {
        WriteState::NotStarted => ("not_started", Value::Bool(false)),
        WriteState::Uncertain => ("uncertain", Value::Null),
    };
    let output = json!({
        "format": "webcodex.external_provider_error.v1",
        "provider": "claude_code",
        "capability": capability.name(),
        "code": error.code,
        "message": error.code,
        "write_state": write_state,
        "changed": changed,
        "error": error.code,
    });
    command_result(output.to_string(), started)
}

fn command_result(stdout: String, started: Instant) -> CommandResult {
    CommandResult {
        exit_code: Some(0),
        stdout: Some(stdout),
        stderr: Some(String::new()),
        duration_ms: Some(started.elapsed().as_millis() as u64),
        error: None,
    }
}

fn validate_context(
    policy: &AgentPolicy,
    request: &ShellAgentShellRequest,
    capability: ProviderCapability,
    payload: &Value,
) -> Result<(PathBuf, PathBuf, String), ProviderError> {
    let root = request.cwd.as_deref().ok_or_else(path_error)?;
    let root = Path::new(root).canonicalize().map_err(|_| path_error())?;
    cwd_allowed(policy, &root).map_err(|_| path_error())?;
    let relative = if capability == ProviderCapability::SearchProjectText {
        payload.get("path").and_then(Value::as_str).unwrap_or(".")
    } else {
        request.path.as_deref().unwrap_or(".")
    };
    if capability == ProviderCapability::EditFile {
        validate_line_edit_agent_path(relative).map_err(|_| path_error())?;
    }
    let raw = Path::new(relative);
    if raw.is_absolute()
        || raw
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(path_error());
    }
    let target = root.join(raw).canonicalize().map_err(|_| path_error())?;
    if !target.starts_with(&root) {
        return Err(path_error());
    }
    Ok((root, target, relative.replace('\\', "/")))
}

fn path_error() -> ProviderError {
    ProviderError::new("provider_path_rejected")
}

struct ClaudeCodeMcpProvider {
    config: ClaudeCodeMcpConfig,
    projects: Mutex<HashMap<PathBuf, Arc<ProjectMcpClient>>>,
    version: Mutex<Option<String>>,
    last_error_code: Mutex<Option<&'static str>>,
}

impl ClaudeCodeMcpProvider {
    fn new(config: ClaudeCodeMcpConfig) -> Self {
        Self {
            config,
            projects: Mutex::new(HashMap::new()),
            version: Mutex::new(None),
            last_error_code: Mutex::new(None),
        }
    }

    fn shutdown(&self) {
        for (_, client) in self.projects.lock().unwrap().drain() {
            client.connection.shutdown();
        }
    }

    fn record_error(&self, error: &ProviderError) {
        *self.last_error_code.lock().unwrap() = Some(error.code);
    }

    fn status(&self) -> ClaudeCodeProviderStatus {
        let projects = self.projects.lock().unwrap();
        let client = projects.values().next();
        let version = self.version.lock().unwrap().clone();
        let last_error_code = self.last_error_code.lock().unwrap().map(str::to_string);
        let running = projects.values().any(|client| client.connection.is_alive());
        let mapping_status = |capability| {
            client
                .map(|client| client.mapping_status(capability, &self.config))
                .unwrap_or("unmapped")
                .to_string()
        };
        let capabilities = BTreeMap::from([
            (
                "search_project_text".to_string(),
                mapping_status(ProviderCapability::SearchProjectText),
            ),
            (
                "edit_file".to_string(),
                mapping_status(ProviderCapability::EditFile),
            ),
        ]);
        ClaudeCodeProviderStatus {
            enabled: self.config.enabled,
            version: version.clone(),
            available: running,
            process_state: if running {
                "running"
            } else if version.is_some() || last_error_code.is_some() {
                "stopped"
            } else {
                "not_started"
            }
            .to_string(),
            discovered_tool_names: client
                .into_iter()
                .flat_map(|client| client.tools.keys())
                .map(|name| sanitize_name(name))
                .take(64)
                .collect(),
            capabilities,
            last_error_code,
        }
    }

    fn project_client(
        &self,
        root: &Path,
        deadline: Instant,
    ) -> Result<Arc<ProjectMcpClient>, ProviderError> {
        if !self.config.enabled {
            let error = ProviderError::new("claude_code_unavailable");
            self.record_error(&error);
            return Err(error);
        }
        let mut projects = self.projects.lock().unwrap();
        if let Some(client) = projects.get(root) {
            if client.connection.is_alive() {
                return Ok(Arc::clone(client));
            }
            client.connection.shutdown();
            projects.remove(root);
        }
        let client = match ProjectMcpClient::start(root, &self.config, deadline) {
            Ok(client) => Arc::new(client),
            Err(error) => {
                self.record_error(&error);
                return Err(error);
            }
        };
        *self.version.lock().unwrap() = client.version.clone();
        projects.insert(root.to_path_buf(), Arc::clone(&client));
        Ok(client)
    }

    fn call(
        &self,
        capability: ProviderCapability,
        request: Value,
        context: ToolExecutionContext<'_>,
    ) -> Result<Value, ProviderError> {
        let budget = self.config.timeout_secs.min(context.timeout_secs);
        let deadline = Instant::now() + Duration::from_secs(budget);
        let client = self.project_client(context.project_root, deadline)?;
        let result = client.call(capability, request, &context, &self.config, deadline);
        if !client.connection.is_alive() {
            self.projects.lock().unwrap().remove(context.project_root);
        }
        if let Err(error) = &result {
            self.record_error(error);
        }
        result
    }
}

struct ProjectMcpClient {
    connection: Arc<McpConnection>,
    tools: BTreeMap<String, BTreeSet<String>>,
    version: Option<String>,
}

impl ProjectMcpClient {
    fn start(
        root: &Path,
        config: &ClaudeCodeMcpConfig,
        deadline: Instant,
    ) -> Result<Self, ProviderError> {
        let connection = McpConnection::spawn(root, config)?;
        let timeout = || {
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(10))
        };
        let initialized = connection.request(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "webcodex-agent", "version": env!("CARGO_PKG_VERSION")},
            }),
            timeout(),
            WriteState::NotStarted,
        )?;
        let version = initialized
            .pointer("/serverInfo/version")
            .and_then(Value::as_str)
            .map(sanitize_name);
        connection.write_json(
            &json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        )?;
        let listed =
            connection.request("tools/list", json!({}), timeout(), WriteState::NotStarted)?;
        let tools = listed
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(protocol_error)?
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name")?.as_str()?.to_string();
                let fields = tool
                    .pointer("/inputSchema/properties")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flat_map(|properties| properties.keys().cloned())
                    .collect();
                Some((name, fields))
            })
            .collect();
        Ok(Self {
            connection,
            tools,
            version,
        })
    }

    fn tool_for<'a>(
        &self,
        capability: ProviderCapability,
        config: &'a ClaudeCodeMcpConfig,
    ) -> Result<&'a str, ProviderError> {
        let configured = config
            .mapping
            .get(capability.name())
            .map(String::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(capability_error)?;
        if self.mapping_status(capability, config) == "available" {
            Ok(configured)
        } else {
            Err(capability_error())
        }
    }

    fn mapping_status(
        &self,
        capability: ProviderCapability,
        config: &ClaudeCodeMcpConfig,
    ) -> &'static str {
        let fields = config
            .mapping
            .get(capability.name())
            .filter(|name| !name.trim().is_empty())
            .and_then(|name| self.tools.get(name));
        match fields {
            None => "unmapped",
            Some(fields)
                if required_fields(capability)
                    .iter()
                    .all(|field| fields.contains(*field)) =>
            {
                "available"
            }
            Some(_) => "schema_mismatch",
        }
    }

    fn call(
        &self,
        capability: ProviderCapability,
        request: Value,
        context: &ToolExecutionContext<'_>,
        config: &ClaudeCodeMcpConfig,
        deadline: Instant,
    ) -> Result<Value, ProviderError> {
        let tool = self.tool_for(capability, config)?;
        let (arguments, expected_after) = build_arguments(capability, &request, context)?;
        let failure_state = if capability == ProviderCapability::EditFile {
            WriteState::Uncertain
        } else {
            WriteState::NotStarted
        };
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(ProviderError::new("mcp_request_timeout"));
        }
        let result = self.connection.request(
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
            timeout,
            failure_state,
        )?;
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(ProviderError::new("claude_tool_failed").with_state(failure_state));
        }
        match capability {
            ProviderCapability::SearchProjectText => normalize_search_result(&result, context),
            ProviderCapability::EditFile => normalize_edit_result(expected_after.unwrap(), context),
        }
    }
}

fn required_fields(capability: ProviderCapability) -> &'static [&'static str] {
    const GREP: &[&str] = &[
        "pattern",
        "path",
        "output_mode",
        "head_limit",
        "-n",
        "-B",
        "-A",
    ];
    const EDIT: &[&str] = &["file_path", "old_string", "new_string"];
    match capability {
        ProviderCapability::SearchProjectText => GREP,
        ProviderCapability::EditFile => EDIT,
    }
}

fn build_arguments(
    capability: ProviderCapability,
    request: &Value,
    context: &ToolExecutionContext<'_>,
) -> Result<(Value, Option<(String, String)>), ProviderError> {
    let target = context.target.to_string_lossy();
    match capability {
        ProviderCapability::SearchProjectText => {
            if ["include_globs", "exclude_globs"].iter().any(|field| {
                request
                    .get(field)
                    .and_then(Value::as_array)
                    .is_some_and(|values| !values.is_empty())
            }) {
                return Err(capability_error());
            }
            let output_mode = if request["result_mode"] == "matches" {
                "content"
            } else {
                request["result_mode"].as_str().ok_or_else(request_error)?
            };
            let mut args = json!({
                "pattern": request["pattern"],
                "path": target,
                "output_mode": output_mode,
                "head_limit": request["limit"],
            });
            if request["result_mode"] == "matches" {
                args["-n"] = json!(true);
                args["-B"] = request["context_before"].clone();
                args["-A"] = request["context_after"].clone();
            }
            Ok((args, None))
        }
        ProviderCapability::EditFile => {
            let old = request
                .get("old")
                .and_then(Value::as_str)
                .ok_or_else(request_error)?;
            let new = request
                .get("new")
                .and_then(Value::as_str)
                .ok_or_else(request_error)?;
            let expected = request
                .get("expected_replacements")
                .and_then(Value::as_i64)
                .unwrap_or(1);
            let allow_multiple = request
                .get("allow_multiple")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if expected != 1 || allow_multiple {
                return Err(capability_error());
            }
            let before = std::fs::read_to_string(&context.target).map_err(|_| request_error())?;
            if before.matches(old).count() != 1 {
                return Err(request_error());
            }
            let after = before.replacen(old, new, 1);
            Ok((
                json!({
                    "file_path": target,
                    "old_string": old,
                    "new_string": new,
                }),
                Some((before, after)),
            ))
        }
    }
}

fn normalize_search_result(
    result: &Value,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, ProviderError> {
    let raw = tool_text(result, context.max_output_bytes)?;
    let root = context.project_root.to_string_lossy();
    let root_prefix = format!("{}/", root.trim_end_matches('/'));
    let mut lines = Vec::new();
    lines.push(
        json!({"webcodex_search":{"backend":"claude_code","feature_unavailable":false}})
            .to_string(),
    );
    for line in raw.lines() {
        let normalized = line.strip_prefix(&root_prefix).unwrap_or(line);
        let path = normalized
            .split_once(':')
            .map_or(normalized, |(path, _)| path);
        if Path::new(path)
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
        {
            continue;
        }
        lines.push(normalized.to_string());
    }
    Ok(Value::String(lines.join("\n")))
}

fn normalize_edit_result(
    (before, expected_after): (String, String),
    context: &ToolExecutionContext<'_>,
) -> Result<Value, ProviderError> {
    let after = std::fs::read_to_string(&context.target).map_err(|_| {
        ProviderError::new("edit_result_uncertain").with_state(WriteState::Uncertain)
    })?;
    if after != expected_after {
        return Err(ProviderError::new("edit_result_uncertain").with_state(WriteState::Uncertain));
    }
    Ok(json!({
        "changed": before != after,
        "path": context.relative_path,
        "replacements": 1,
        "before_sha256": sha256_hex_bytes(before.as_bytes()),
        "after_sha256": sha256_hex_bytes(after.as_bytes()),
        "bytes_written": after.len(),
    }))
}

fn tool_text(result: &Value, maximum: usize) -> Result<String, ProviderError> {
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if text.len() > maximum {
        return Err(ProviderError::new("provider_response_too_large"));
    }
    Ok(text)
}

fn capability_error() -> ProviderError {
    ProviderError::new("provider_capability_unavailable")
}

fn request_error() -> ProviderError {
    ProviderError::new("provider_invalid_request")
}

fn protocol_error() -> ProviderError {
    ProviderError::new("mcp_protocol_error")
}

type PendingSender = mpsc::Sender<Result<Value, ProviderError>>;

struct McpConnection {
    child: Mutex<Child>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, PendingSender>>>,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    shutdown_started: AtomicBool,
}

impl Drop for McpConnection {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl McpConnection {
    fn spawn(root: &Path, config: &ClaudeCodeMcpConfig) -> Result<Arc<Self>, ProviderError> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        apply_safe_environment(&mut command);
        let mut child = command
            .spawn()
            .map_err(|_| ProviderError::new("claude_code_spawn_failed"))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().ok_or_else(protocol_error)?));
        let stdout = child.stdout.take().ok_or_else(protocol_error)?;
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let connection = Arc::new(Self {
            child: Mutex::new(child),
            stdin: Arc::clone(&stdin),
            pending: Arc::clone(&pending),
            next_id: AtomicU64::new(1),
            alive: Arc::clone(&alive),
            shutdown_started: AtomicBool::new(false),
        });
        spawn_stdout_reader(stdout, stdin, pending, alive);
        Ok(connection)
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        failure_state: WriteState,
    ) -> Result<Value, ProviderError> {
        if !self.is_alive() {
            return Err(protocol_error().with_state(failure_state));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.len() >= MAX_PENDING_REQUESTS {
                return Err(ProviderError::new("mcp_pending_limit"));
            }
            pending.insert(id, tx);
        }
        let message = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        if let Err(error) = self.write_json(&message) {
            self.pending.lock().unwrap().remove(&id);
            return Err(error.with_state(failure_state));
        }
        match rx.recv_timeout(timeout) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(error.with_state(failure_state)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending.lock().unwrap().remove(&id);
                self.shutdown();
                Err(ProviderError::new("mcp_request_timeout").with_state(failure_state))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(ProviderError::new("mcp_connection_closed").with_state(failure_state))
            }
        }
    }

    fn write_json(&self, value: &Value) -> Result<(), ProviderError> {
        write_json(&self.stdin, value)
    }

    fn shutdown(&self) {
        if self.shutdown_started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.alive.store(false, Ordering::SeqCst);
        let mut child = self.child.lock().unwrap();
        #[cfg(unix)]
        if child.id() != 0 {
            // SAFETY: this is the private process group created at spawn.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        let _ = child.kill();
        let _ = child.wait();
        drop(child);
        fail_pending(&self.pending, ProviderError::new("mcp_connection_closed"));
    }
}

fn write_json(stdin: &Mutex<ChildStdin>, value: &Value) -> Result<(), ProviderError> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| protocol_error())?;
    if bytes.len() > MAX_MCP_MESSAGE_BYTES {
        return Err(protocol_error());
    }
    bytes.push(b'\n');
    let mut writer = stdin.lock().unwrap();
    writer.write_all(&bytes).map_err(|_| protocol_error())?;
    writer.flush().map_err(|_| protocol_error())
}

fn spawn_stdout_reader(
    stdout: impl Read + Send + 'static,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, PendingSender>>>,
    alive: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let bytes = match read_bounded_line(&mut reader) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => break,
                Err(error) => {
                    fail_pending(&pending, error);
                    break;
                }
            };
            let value: Value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(_) => {
                    fail_pending(&pending, ProviderError::new("mcp_invalid_json"));
                    break;
                }
            };
            let method = value.get("method");
            let id = value.get("id");
            if method.is_some() && id.is_none() {
                continue;
            }
            if method.is_some() {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id.cloned().unwrap_or(Value::Null),
                    "error": {"code": -32601, "message": "Method not found"},
                });
                if let Err(error) = write_json(&stdin, &response) {
                    fail_pending(&pending, error);
                    break;
                }
                continue;
            }
            let Some(id) = id.and_then(Value::as_u64) else {
                continue;
            };
            let Some(sender) = pending.lock().unwrap().remove(&id) else {
                continue;
            };
            let response = if value.get("error").is_some() {
                Err(ProviderError::new("mcp_rpc_error"))
            } else {
                value.get("result").cloned().ok_or_else(protocol_error)
            };
            let _ = sender.send(response);
        }
        alive.store(false, Ordering::SeqCst);
        fail_pending(&pending, ProviderError::new("mcp_connection_closed"));
    });
}

fn read_bounded_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, ProviderError> {
    let mut line = Vec::new();
    reader
        .take((MAX_MCP_MESSAGE_BYTES + 2) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|_| protocol_error())?;
    if line.is_empty() {
        return Ok(None);
    }
    let terminated = line.last() == Some(&b'\n');
    if line.len() > MAX_MCP_MESSAGE_BYTES + usize::from(terminated) {
        return Err(ProviderError::new("mcp_message_too_large"));
    }
    if terminated {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    Ok(Some(line))
}

fn fail_pending(pending: &Mutex<HashMap<u64, PendingSender>>, error: ProviderError) {
    for (_, sender) in pending.lock().unwrap().drain() {
        let _ = sender.send(Err(error.clone()));
    }
}

fn apply_safe_environment(command: &mut Command) {
    command.env_clear();
    for key in "PATH HOME LANG LC_ALL TMPDIR XDG_CONFIG_HOME XDG_DATA_HOME XDG_CACHE_HOME CLAUDE_CONFIG_DIR"
        .split_whitespace()
    {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

fn sanitize_name(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(120)
        .collect()
}

#[cfg(test)]
#[path = "external_tools_tests.rs"]
mod tests;
