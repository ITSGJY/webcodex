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
    ClaudeCodeProviderStatus, ProviderCallSummary, ShellAgentShellRequest, ToolProvidersStatus,
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
/// Experimental raw Claude harness bounds (branch-local; not production API).
const MAX_EXPERIMENTAL_TOOLS: usize = 64;
const MAX_EXPERIMENTAL_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_EXPERIMENTAL_RESULT_BYTES: usize = 256 * 1024;
const MAX_EXPERIMENTAL_DESCRIPTION_CHARS: usize = 4_096;
const EXPERIMENTAL_KIND_LIST: &str = "claude_list_tools";
const EXPERIMENTAL_KIND_DESCRIBE: &str = "claude_describe_tool";
const EXPERIMENTAL_KIND_CALL: &str = "claude_tool_call";

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
    NotSubmitted,
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
            write_state: WriteState::NotSubmitted,
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
    NativeFallback(NativeFallback),
    Handled(CommandResult),
}

pub(crate) struct NativeFallback {
    capability: ProviderCapability,
    started: Instant,
}

pub(crate) struct ExternalToolRouter {
    strategy: ToolProviderStrategy,
    claude: ClaudeCodeMcpProvider,
    sent_status_revision: AtomicU64,
    claimed_status_revision: AtomicU64,
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
            sent_status_revision: AtomicU64::new(0),
            claimed_status_revision: AtomicU64::new(0),
        }
    }

    pub(crate) fn shutdown(&self) {
        self.claude.shutdown();
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> ToolProvidersStatus {
        self.status_with_revision().0
    }

    fn status_with_revision(&self) -> (ToolProvidersStatus, u64) {
        let (claude_code, revision) = self.claude.status_with_revision();
        (
            ToolProvidersStatus {
                strategy: self.strategy_name().to_string(),
                claude_code,
            },
            revision,
        )
    }

    fn strategy_name(&self) -> &'static str {
        match self.strategy {
            ToolProviderStrategy::Native => "native",
            ToolProviderStrategy::ClaudeCode => "claude_code",
            ToolProviderStrategy::ClaudeCodeThenNative => "claude_code_then_native",
        }
    }

    /// Claim one changed status revision for an existing transport message.
    /// Snapshotting completes before the caller performs any network I/O.
    pub(crate) fn claim_status_update(&self) -> Option<(ToolProvidersStatus, u64)> {
        if self.claimed_status_revision.load(Ordering::SeqCst) != 0 {
            return None;
        }
        let (status, revision) = self.status_with_revision();
        if revision <= self.sent_status_revision.load(Ordering::SeqCst) {
            return None;
        }
        self.claimed_status_revision
            .compare_exchange(0, revision, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| (status, revision))
    }

    pub(crate) fn mark_status_reported(&self, revision: u64) {
        self.sent_status_revision
            .fetch_max(revision, Ordering::SeqCst);
        let _ = self.claimed_status_revision.compare_exchange(
            revision,
            0,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    pub(crate) fn release_status_update(&self, revision: u64) {
        let _ = self.claimed_status_revision.compare_exchange(
            revision,
            0,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    pub(crate) fn registration_status(&self) -> (ToolProvidersStatus, u64) {
        self.status_with_revision()
    }

    pub(crate) fn route(
        &self,
        policy: &AgentPolicy,
        request: &ShellAgentShellRequest,
    ) -> ExternalRoute {
        // Fixed experimental harness surface — independent of production strategy.
        if is_experimental_claude_kind(&request.kind) {
            return ExternalRoute::Handled(self.handle_experimental(policy, request));
        }
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
                self.claude.record_call(
                    call_summary(
                        capability,
                        "claude_code",
                        false,
                        false,
                        error_write_state(capability, error.write_state),
                        started,
                        Some(error.code),
                    ),
                    false,
                );
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
            Ok(output) => {
                self.claude.record_call(
                    call_summary(
                        capability,
                        "claude_code",
                        false,
                        true,
                        (capability == ProviderCapability::EditFile).then_some("confirmed"),
                        started,
                        None,
                    ),
                    true,
                );
                ExternalRoute::Handled(command_result(
                    output
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| output.to_string()),
                    started,
                ))
            }
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
                || error.write_state == WriteState::NotSubmitted)
        {
            ExternalRoute::NativeFallback(NativeFallback {
                capability,
                started,
            })
        } else {
            self.claude.record_call(
                call_summary(
                    capability,
                    "claude_code",
                    false,
                    false,
                    error_write_state(capability, error.write_state),
                    started,
                    Some(error.code),
                ),
                false,
            );
            ExternalRoute::Handled(provider_error_result(capability, error, started))
        }
    }

    pub(crate) fn complete_native_fallback(
        &self,
        fallback: NativeFallback,
        result: &CommandResult,
    ) {
        let succeeded = native_result_succeeded(fallback.capability, result);
        let write_state =
            (fallback.capability == ProviderCapability::EditFile).then_some(if succeeded {
                "confirmed"
            } else {
                "not_submitted"
            });
        self.claude.record_call(
            call_summary(
                fallback.capability,
                "native",
                true,
                succeeded,
                write_state,
                fallback.started,
                (!succeeded).then_some("native_tool_failed"),
            ),
            false,
        );
    }

    fn handle_experimental(
        &self,
        policy: &AgentPolicy,
        request: &ShellAgentShellRequest,
    ) -> CommandResult {
        let started = Instant::now();
        match self.claude.experimental_dispatch(policy, request) {
            Ok(value) => command_result(value.to_string(), started),
            Err(error) => {
                self.claude.record_error(&error);
                command_result(
                    json!({
                        "experimental": true,
                        "error": experimental_error_code(error.code),
                        "code": experimental_error_code(error.code),
                        "message": experimental_error_code(error.code),
                    })
                    .to_string(),
                    started,
                )
            }
        }
    }
}

fn is_experimental_claude_kind(kind: &str) -> bool {
    matches!(
        kind,
        EXPERIMENTAL_KIND_LIST | EXPERIMENTAL_KIND_DESCRIBE | EXPERIMENTAL_KIND_CALL
    )
}

fn experimental_error_code(code: &str) -> &str {
    match code {
        "claude_tool_not_found"
        | "claude_schema_unavailable"
        | "claude_arguments_invalid"
        | "claude_mcp_timeout"
        | "claude_mcp_process_exited"
        | "claude_tool_error"
        | "claude_result_too_large"
        | "claude_code_unavailable"
        | "provider_path_rejected"
        | "provider_invalid_request" => code,
        "mcp_request_timeout" => "claude_mcp_timeout",
        "mcp_connection_closed"
        | "claude_code_spawn_failed"
        | "mcp_protocol_error"
        | "mcp_invalid_json"
        | "mcp_message_too_large"
        | "mcp_rpc_error"
        | "mcp_pending_limit" => "claude_mcp_process_exited",
        "claude_tool_failed" => "claude_tool_error",
        "provider_response_too_large" => "claude_result_too_large",
        other => other,
    }
}

fn call_summary(
    capability: ProviderCapability,
    selected_provider: &str,
    fallback_used: bool,
    succeeded: bool,
    write_state: Option<&str>,
    started: Instant,
    error_code: Option<&str>,
) -> ProviderCallSummary {
    ProviderCallSummary {
        capability: capability.name().to_string(),
        selected_provider: selected_provider.to_string(),
        fallback_used,
        result: if succeeded { "success" } else { "failure" }.to_string(),
        write_state: write_state.map(str::to_string),
        duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        error_code: error_code.map(str::to_string),
    }
}

fn error_write_state(
    capability: ProviderCapability,
    write_state: WriteState,
) -> Option<&'static str> {
    (capability == ProviderCapability::EditFile).then_some(match write_state {
        WriteState::NotSubmitted => "not_submitted",
        WriteState::Uncertain => "uncertain",
    })
}

fn native_result_succeeded(capability: ProviderCapability, result: &CommandResult) -> bool {
    if result.error.is_some() {
        return false;
    }
    match capability {
        ProviderCapability::SearchProjectText => matches!(result.exit_code, Some(0 | 1)),
        ProviderCapability::EditFile => {
            result.exit_code == Some(0)
                && result
                    .stdout
                    .as_deref()
                    .and_then(|stdout| serde_json::from_str::<Value>(stdout).ok())
                    .is_some_and(|output| output.get("error").map_or(true, Value::is_null))
        }
    }
}

fn provider_error_result(
    capability: ProviderCapability,
    error: ProviderError,
    started: Instant,
) -> CommandResult {
    let (write_state, changed) = match error.write_state {
        WriteState::NotSubmitted => ("not_submitted", Value::Bool(false)),
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

fn unmapped_capabilities() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("edit_file".to_string(), "unmapped".to_string()),
        ("search_project_text".to_string(), "unmapped".to_string()),
    ])
}

struct ProviderState {
    status: Mutex<ClaudeCodeProviderStatus>,
    revision: AtomicU64,
}

impl ProviderState {
    fn new(enabled: bool) -> Self {
        Self {
            status: Mutex::new(ClaudeCodeProviderStatus {
                enabled,
                version: None,
                available: false,
                process_state: "not_started".to_string(),
                discovered_tool_names: Vec::new(),
                capabilities: unmapped_capabilities(),
                last_error_code: None,
                last_call: None,
            }),
            // Revision one represents the initialized configuration snapshot.
            revision: AtomicU64::new(1),
        }
    }

    fn update(&self, update: impl FnOnce(&mut ClaudeCodeProviderStatus)) {
        let mut status = self.status.lock().unwrap();
        let previous = status.clone();
        update(&mut status);
        if *status != previous {
            self.revision.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn snapshot_with_revision(&self) -> (ClaudeCodeProviderStatus, u64) {
        let status = self.status.lock().unwrap();
        (status.clone(), self.revision.load(Ordering::SeqCst))
    }

    fn stopped(&self, error_code: Option<&str>) {
        self.update(|status| {
            status.available = false;
            status.process_state = "stopped".to_string();
            if let Some(error_code) = error_code {
                status.last_error_code = Some(error_code.to_string());
            }
        });
    }
}

struct ClaudeCodeMcpProvider {
    config: ClaudeCodeMcpConfig,
    projects: Mutex<HashMap<PathBuf, Arc<ProjectMcpClient>>>,
    state: Arc<ProviderState>,
}

impl ClaudeCodeMcpProvider {
    fn new(config: ClaudeCodeMcpConfig) -> Self {
        let state = Arc::new(ProviderState::new(config.enabled));
        Self {
            config,
            projects: Mutex::new(HashMap::new()),
            state,
        }
    }

    fn shutdown(&self) {
        let clients = self
            .projects
            .lock()
            .unwrap()
            .drain()
            .map(|(_, client)| client)
            .collect::<Vec<_>>();
        for client in clients {
            client.connection.shutdown();
        }
        self.state.stopped(None);
    }

    fn record_error(&self, error: &ProviderError) {
        self.state.update(|status| {
            status.last_error_code = Some(error.code.to_string());
        });
    }

    fn record_call(&self, summary: ProviderCallSummary, clear_error: bool) {
        self.state.update(|status| {
            status.last_call = Some(summary);
            if clear_error && status.process_state == "running" {
                status.last_error_code = None;
            }
        });
    }

    #[cfg(test)]
    fn status(&self) -> ClaudeCodeProviderStatus {
        self.status_with_revision().0
    }

    fn status_with_revision(&self) -> (ClaudeCodeProviderStatus, u64) {
        if self
            .projects
            .try_lock()
            .ok()
            .is_some_and(|projects| projects.values().any(|client| client.connection.is_alive()))
        {
            self.state.update(|status| {
                if status.process_state == "stopped" {
                    status.available = true;
                    status.process_state = "running".to_string();
                }
            });
        }
        self.state.snapshot_with_revision()
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
        self.state.update(|status| {
            status.available = false;
            status.process_state = "starting".to_string();
            status.version = None;
            status.discovered_tool_names.clear();
            status.capabilities = unmapped_capabilities();
            status.last_error_code = None;
        });
        let client =
            match ProjectMcpClient::start(root, &self.config, deadline, Arc::clone(&self.state)) {
                Ok(client) => Arc::new(client),
                Err(error) => {
                    self.state.stopped(Some(error.code));
                    self.record_error(&error);
                    return Err(error);
                }
            };
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

    /// Experimental: list/describe/call raw Claude MCP tools for one project root.
    fn experimental_dispatch(
        &self,
        policy: &AgentPolicy,
        request: &ShellAgentShellRequest,
    ) -> Result<Value, ProviderError> {
        let root = request.cwd.as_deref().ok_or_else(path_error)?;
        let root = Path::new(root).canonicalize().map_err(|_| path_error())?;
        cwd_allowed(policy, &root).map_err(|_| path_error())?;
        let timeout_secs = request
            .timeout_secs
            .max(1)
            .min(policy.max_timeout_secs)
            .min(self.config.timeout_secs.max(1));
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let process_reused = self
            .projects
            .lock()
            .unwrap()
            .get(&root)
            .is_some_and(|client| client.connection.is_alive());
        let client = self.project_client(&root, deadline)?;
        let payload = request
            .content
            .as_deref()
            .filter(|raw| !raw.trim().is_empty())
            .map(|raw| serde_json::from_str::<Value>(raw).map_err(|_| request_error()))
            .transpose()?
            .unwrap_or_else(|| json!({}));
        let started = Instant::now();
        let outcome = match request.kind.as_str() {
            EXPERIMENTAL_KIND_LIST => Ok(client.experimental_list_tools(process_reused)),
            EXPERIMENTAL_KIND_DESCRIBE => {
                let tool_name = payload
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .ok_or_else(request_error)?;
                client.experimental_describe_tool(tool_name, process_reused)
            }
            EXPERIMENTAL_KIND_CALL => {
                let tool_name = payload
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .ok_or_else(request_error)?;
                let arguments = payload
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                client.experimental_tool_call(tool_name, arguments, process_reused, deadline)
            }
            _ => Err(request_error()),
        };
        if !client.connection.is_alive() {
            self.projects.lock().unwrap().remove(&root);
        }
        match &outcome {
            Ok(_) => self.record_call(
                ProviderCallSummary {
                    capability: request.kind.clone(),
                    selected_provider: "claude_code".to_string(),
                    fallback_used: false,
                    result: "success".to_string(),
                    write_state: None,
                    duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                    error_code: None,
                },
                true,
            ),
            Err(error) => {
                self.record_error(error);
                self.record_call(
                    ProviderCallSummary {
                        capability: request.kind.clone(),
                        selected_provider: "claude_code".to_string(),
                        fallback_used: false,
                        result: "failure".to_string(),
                        write_state: None,
                        duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                        error_code: Some(experimental_error_code(error.code).to_string()),
                    },
                    false,
                );
            }
        }
        outcome
    }
}

#[derive(Clone)]
struct DiscoveredTool {
    fields: BTreeSet<String>,
    description: String,
    input_schema: Value,
    schema_hash: String,
}

struct ProjectMcpClient {
    connection: Arc<McpConnection>,
    tools: BTreeMap<String, DiscoveredTool>,
    version: Option<String>,
}

impl ProjectMcpClient {
    fn start(
        root: &Path,
        config: &ClaudeCodeMcpConfig,
        deadline: Instant,
        state: Arc<ProviderState>,
    ) -> Result<Self, ProviderError> {
        let connection = McpConnection::spawn(root, config, Arc::clone(&state))?;
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
            WriteState::NotSubmitted,
        )?;
        let version = initialized
            .pointer("/serverInfo/version")
            .and_then(Value::as_str)
            .and_then(sanitize_version);
        state.update(|status| {
            status.version = version.clone();
            status.process_state = "discovering".to_string();
            status.last_error_code = None;
        });
        connection.write_json(
            &json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        )?;
        let listed =
            connection.request("tools/list", json!({}), timeout(), WriteState::NotSubmitted)?;
        let mut tools = BTreeMap::new();
        for tool in listed
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(protocol_error)?
        {
            let Some(name) = tool.get("name").and_then(Value::as_str).map(str::to_string) else {
                continue;
            };
            if sanitize_tool_name(&name).is_none() || tools.contains_key(&name) {
                continue;
            }
            let input_schema = tool
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            let schema_bytes = serde_json::to_vec(&input_schema).unwrap_or_default();
            if schema_bytes.len() > MAX_EXPERIMENTAL_SCHEMA_BYTES {
                continue;
            }
            let fields = input_schema
                .pointer("/properties")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|properties| properties.keys().cloned())
                .collect();
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(MAX_EXPERIMENTAL_DESCRIPTION_CHARS)
                .collect::<String>();
            let schema_hash = schema_hash_hex(&input_schema);
            tools.insert(
                name,
                DiscoveredTool {
                    fields,
                    description,
                    input_schema,
                    schema_hash,
                },
            );
            if tools.len() >= MAX_EXPERIMENTAL_TOOLS {
                break;
            }
        }
        let client = Self {
            connection,
            tools,
            version,
        };
        let mut discovered_tool_names = client
            .tools
            .keys()
            .filter_map(|name| sanitize_tool_name(name))
            .collect::<Vec<_>>();
        discovered_tool_names.sort();
        discovered_tool_names.dedup();
        discovered_tool_names.truncate(MAX_EXPERIMENTAL_TOOLS);
        state.update(|status| {
            status.discovered_tool_names = discovered_tool_names;
            status.process_state = "mapping".to_string();
        });
        let capabilities = BTreeMap::from([
            (
                "edit_file".to_string(),
                client
                    .mapping_status(ProviderCapability::EditFile, config)
                    .to_string(),
            ),
            (
                "search_project_text".to_string(),
                client
                    .mapping_status(ProviderCapability::SearchProjectText, config)
                    .to_string(),
            ),
        ]);
        state.update(|status| {
            status.capabilities = capabilities;
            status.available = true;
            status.process_state = "running".to_string();
            status.last_error_code = None;
        });
        Ok(client)
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
        let tool = config
            .mapping
            .get(capability.name())
            .filter(|name| !name.trim().is_empty())
            .and_then(|name| self.tools.get(name));
        match tool {
            None => "unmapped",
            Some(tool)
                if required_fields(capability)
                    .iter()
                    .all(|field| tool.fields.contains(*field)) =>
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
            WriteState::NotSubmitted
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

    fn experimental_list_tools(&self, process_reused: bool) -> Value {
        let mut tools = self
            .tools
            .iter()
            .filter_map(|(name, tool)| {
                let name = sanitize_tool_name(name)?;
                Some(json!({
                    "name": name,
                    "schema_hash": tool.schema_hash,
                }))
            })
            .collect::<Vec<_>>();
        tools.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
        let truncated = tools.len() > MAX_EXPERIMENTAL_TOOLS;
        tools.truncate(MAX_EXPERIMENTAL_TOOLS);
        json!({
            "experimental": true,
            "claude_version": self.version,
            "process_reused": process_reused,
            "tools": tools,
            "truncated": truncated,
        })
    }

    fn experimental_describe_tool(
        &self,
        tool_name: &str,
        process_reused: bool,
    ) -> Result<Value, ProviderError> {
        let tool = self
            .tools
            .get(tool_name)
            .ok_or_else(|| ProviderError::new("claude_tool_not_found"))?;
        let mut description = tool.description.clone();
        let mut input_schema = tool.input_schema.clone();
        let mut schema_bytes = serde_json::to_vec(&input_schema).map_err(|_| protocol_error())?;
        let mut truncated = false;
        if schema_bytes.len() > MAX_EXPERIMENTAL_SCHEMA_BYTES {
            input_schema = json!({
                "type": "object",
                "truncated": true,
                "note": "schema exceeded experimental describe bound",
            });
            schema_bytes = serde_json::to_vec(&input_schema).unwrap_or_default();
            truncated = true;
        }
        if description.chars().count() > MAX_EXPERIMENTAL_DESCRIPTION_CHARS {
            description = description
                .chars()
                .take(MAX_EXPERIMENTAL_DESCRIPTION_CHARS)
                .collect();
            truncated = true;
        }
        let _ = schema_bytes;
        Ok(json!({
            "experimental": true,
            "tool_name": tool_name,
            "claude_version": self.version,
            "schema_hash": tool.schema_hash,
            "description": description,
            "input_schema": input_schema,
            "process_reused": process_reused,
            "truncated": truncated,
        }))
    }

    fn experimental_tool_call(
        &self,
        tool_name: &str,
        arguments: Value,
        process_reused: bool,
        deadline: Instant,
    ) -> Result<Value, ProviderError> {
        let tool = self
            .tools
            .get(tool_name)
            .ok_or_else(|| ProviderError::new("claude_tool_not_found"))?;
        if tool.input_schema.is_null() {
            return Err(ProviderError::new("claude_schema_unavailable"));
        }
        validate_against_schema(&tool.input_schema, &arguments)
            .map_err(|_| ProviderError::new("claude_arguments_invalid"))?;
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(ProviderError::new("mcp_request_timeout"));
        }
        let started = Instant::now();
        let result = self.connection.request(
            "tools/call",
            json!({"name": tool_name, "arguments": arguments}),
            timeout,
            WriteState::NotSubmitted,
        )?;
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut result_value = result;
        let mut result_truncated = false;
        let encoded = serde_json::to_vec(&result_value).map_err(|_| protocol_error())?;
        if encoded.len() > MAX_EXPERIMENTAL_RESULT_BYTES {
            result_truncated = true;
            result_value = json!({
                "truncated": true,
                "note": "claude tool result exceeded experimental bound",
                "original_bytes": encoded.len(),
                "isError": is_error,
            });
            if encoded.len() > MAX_EXPERIMENTAL_RESULT_BYTES * 2 {
                return Err(ProviderError::new("claude_result_too_large"));
            }
        }
        Ok(json!({
            "experimental": true,
            "tool_name": tool_name,
            "claude_version": self.version,
            "schema_hash": tool.schema_hash,
            "duration_ms": started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            "process_reused": process_reused,
            "is_error": is_error,
            "result": result_value,
            "result_truncated": result_truncated,
        }))
    }
}

fn schema_hash_hex(schema: &Value) -> String {
    let canonical = canonicalize_json(schema);
    sha256_hex_bytes(canonical.as_bytes())
}

fn canonicalize_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            let parts = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(&key).unwrap_or_else(|_| "\"\"".into()),
                        canonicalize_json(&map[&key])
                    )
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(items) => {
            let parts = items.iter().map(canonicalize_json).collect::<Vec<_>>();
            format!("[{}]", parts.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".into()),
    }
}

/// Minimal JSON Schema subset for Claude harness tools (not a full engine).
fn validate_against_schema(schema: &Value, value: &Value) -> Result<(), ()> {
    validate_schema_node(schema, value)
}

fn validate_schema_node(schema: &Value, value: &Value) -> Result<(), ()> {
    if let Some(enum_values) = schema.get("enum").and_then(Value::as_array) {
        if !enum_values.iter().any(|item| item == value) {
            return Err(());
        }
    }
    let type_name = schema.get("type").and_then(Value::as_str);
    match type_name {
        Some("object") => {
            let object = value.as_object().ok_or(())?;
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for key in required {
                    let key = key.as_str().ok_or(())?;
                    if !object.contains_key(key) {
                        return Err(());
                    }
                }
            }
            let properties = schema
                .get("properties")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let additional = schema
                .get("additionalProperties")
                .cloned()
                .unwrap_or(Value::Bool(true));
            for (key, item) in object {
                if let Some(property_schema) = properties.get(key) {
                    validate_schema_node(property_schema, item)?;
                } else {
                    match &additional {
                        Value::Bool(false) => return Err(()),
                        Value::Bool(true) | Value::Null => {}
                        other => validate_schema_node(other, item)?,
                    }
                }
            }
            Ok(())
        }
        Some("array") => {
            let items = value.as_array().ok_or(())?;
            if let Some(item_schema) = schema.get("items") {
                for item in items {
                    validate_schema_node(item_schema, item)?;
                }
            }
            Ok(())
        }
        Some("string") => {
            if value.is_string() {
                Ok(())
            } else {
                Err(())
            }
        }
        Some("integer") => {
            if value.as_i64().is_some() || value.as_u64().is_some() {
                Ok(())
            } else {
                Err(())
            }
        }
        Some("number") => {
            if value.as_f64().is_some() {
                Ok(())
            } else {
                Err(())
            }
        }
        Some("boolean") => {
            if value.is_boolean() {
                Ok(())
            } else {
                Err(())
            }
        }
        Some("null") => {
            if value.is_null() {
                Ok(())
            } else {
                Err(())
            }
        }
        _ => {
            if let Some(one_of) = schema.get("oneOf").and_then(Value::as_array) {
                if one_of
                    .iter()
                    .any(|branch| validate_schema_node(branch, value).is_ok())
                {
                    return Ok(());
                }
                return Err(());
            }
            if let Some(any_of) = schema.get("anyOf").and_then(Value::as_array) {
                if any_of
                    .iter()
                    .any(|branch| validate_schema_node(branch, value).is_ok())
                {
                    return Ok(());
                }
                return Err(());
            }
            Ok(())
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
    shutdown_started: Arc<AtomicBool>,
    state: Arc<ProviderState>,
}

impl Drop for McpConnection {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl McpConnection {
    fn spawn(
        root: &Path,
        config: &ClaudeCodeMcpConfig,
        state: Arc<ProviderState>,
    ) -> Result<Arc<Self>, ProviderError> {
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
        let shutdown_started = Arc::new(AtomicBool::new(false));
        state.update(|status| {
            status.process_state = "initializing".to_string();
        });
        let connection = Arc::new(Self {
            child: Mutex::new(child),
            stdin: Arc::clone(&stdin),
            pending: Arc::clone(&pending),
            next_id: AtomicU64::new(1),
            alive: Arc::clone(&alive),
            shutdown_started: Arc::clone(&shutdown_started),
            state: Arc::clone(&state),
        });
        spawn_stdout_reader(stdout, stdin, pending, alive, shutdown_started, state);
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
            return Err(protocol_error());
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
        self.state.stopped(None);
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
    shutdown_started: Arc<AtomicBool>,
    state: Arc<ProviderState>,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut terminal_error = ProviderError::new("mcp_connection_closed");
        loop {
            let bytes = match read_bounded_line(&mut reader) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => break,
                Err(error) => {
                    terminal_error = error;
                    break;
                }
            };
            let value: Value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(_) => {
                    terminal_error = ProviderError::new("mcp_invalid_json");
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
                    terminal_error = error;
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
        if !shutdown_started.load(Ordering::SeqCst) {
            state.stopped(Some(terminal_error.code));
        }
        fail_pending(&pending, terminal_error);
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

fn sanitize_tool_name(value: &str) -> Option<String> {
    if value.is_empty()
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-.:".contains(character))
    {
        return None;
    }
    Some(value.chars().take(120).collect())
}

#[cfg(test)]
fn sanitize_name(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(120)
        .collect()
}

fn sanitize_version(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().any(|character| {
            !(character.is_ascii_alphanumeric()
                || matches!(character, ' ' | '.' | '_' | '-' | '+' | '(' | ')'))
        })
    {
        return None;
    }
    Some(value.chars().take(80).collect())
}

#[cfg(test)]
#[path = "external_tools_tests.rs"]
mod tests;
