use reqwest::blocking::Client;
use std::collections::{HashMap, VecDeque};
use std::error::Error as StdError;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tracing_subscriber::EnvFilter;
use webcodex_runner::shutdown::{lock_unpoison, ActivityTracker};

#[cfg(test)]
#[path = "webcodex_runner/job_manager_tests.rs"]
mod job_manager_tests;
mod webcodex_runner;

use webcodex_agent_config as agent_init;
use webcodex_core::{
    apply_edits_shared, artifact_policy, build_info, lsp_bridge, shell_protocol, validation_bridge,
};
use webcodex_sandbox as command_sandbox;
use webcodex_workspace::{project_overview, workspace_checkpoint};

use shell_protocol::{
    validation_infrastructure_failure_code, AgentPolicySummary, ShellAgentJobUpdateRequest,
    ShellAgentPollPayload, ShellAgentPollRequest, ShellAgentPollResponse, ShellAgentProjectSummary,
    ShellAgentShellRequest, ShellClientCapabilities, ShellClientRegisterRequest,
    ShellClientRegisterResponse, ShellJobContext, ShellJobInventory, ShellJobLogSnapshot,
    ShellJobSnapshot, ShellJobStreamSnapshot, ShellJobValidationProgress, ShellJobValidationStep,
    ShellProfileSummaryEntry, ShellProfilesSummary, AGENT_PROTOCOL_VERSION_POLLING_V1,
    JOB_INVENTORY_MAX_ACTIVE_JOBS, JOB_INVENTORY_MAX_SERIALIZED_BYTES,
    JOB_INVENTORY_MAX_TERMINAL_JOBS, JOB_SNAPSHOT_STREAM_MAX_BYTES, JOB_TERMINAL_RETENTION_SECS,
    VALIDATION_STEP_SPAWN_FAILED_CODE, VALIDATION_STEP_WAIT_FAILED_CODE,
    VALIDATION_TOOL_UNAVAILABLE_CODE,
};

#[cfg(test)]
use agent_init::{TRANSPORT_AUTO, TRANSPORT_POLLING, TRANSPORT_QUIC, TRANSPORT_WEBSOCKET};
#[cfg(test)]
use shell_protocol::{
    AgentEnvelope, AGENT_PROTOCOL_VERSION_QUIC_V1, AGENT_PROTOCOL_VERSION_WEBSOCKET_V1,
};
#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::net::SocketAddr;
use webcodex_runner::contains_any;
#[cfg(test)]
use webcodex_runner::QuicClientConfig;
#[cfg(test)]
use webcodex_runner::{
    agent_project_summary, auto_transport_plan, build_ws_request, default_quic_alpn,
    default_quic_connect_timeout_secs, default_quic_keepalive_interval_secs,
    default_websocket_connect_timeout_secs, effective_transport, handle_project_op,
    load_agent_project_summaries_from_dir, max_concurrent_jobs, non_empty_token,
    parse_agent_project_toml, quic_client_bind_addr_for, resolve_quic_config,
    resolve_quic_server_addrs, run_shell, run_shell_with_profiles,
    run_shell_with_profiles_in_sandbox, server_url_to_ws, sha256_hex_bytes,
    validate_project_path_policy, websocket_session, AgentRuntimeState, ShellProfileConfig,
    CLIENT_PROFILE_ERROR, DEFAULT_MAX_CONCURRENT_JOBS, WS_OUTGOING_CAPACITY,
};
use webcodex_runner::{
    client_profile_agent_config, configured_prepared_shell_job_command,
    configured_shell_job_command, configured_validation_job_command, cwd_allowed,
    default_config_path, dispatch_request, err_cmd, handle_apply_text_edits_file_request,
    handle_artifact_file_request, handle_basic_file_request, handle_checkpoint_file_request,
    handle_line_edit_file_request, handle_replace_in_file_request,
    handle_write_project_file_request, hostname, is_artifact_request_kind,
    is_basic_file_request_kind, is_checkpoint_request_kind, is_line_edit_request_kind,
    is_project_op, load_config, ok_cmd, projects_dir, resolve_prepared_shell_profile,
    resolve_requested_path, run_agent, validate_client_profile, validate_line_edit_agent_path,
    AgentConfig, AgentPolicy, AgentProjectCache, AgentSink, CommandResult, HotAgentConfig,
    HttpSendConfig, PreparedShellProfile, PreparedShellProfileCache, ReloadableAgentConfig,
    ShellConfig, SubmitResultError,
};

const JOB_UPDATE_INTERVAL_MS: u64 = 250;
const AGENT_REGISTER_PATH: &str = "/api/shell/agent/register";
const AGENT_POLL_PATH: &str = "/api/shell/agent/poll";
/// Polling HTTP responses can carry the current largest 15 MiB request
/// payloads plus their JSON envelope, but must never be loaded without a
/// finite bound.
const AGENT_HTTP_RESPONSE_BODY_MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
struct JobManager {
    max_concurrent: usize,
    jobs: Arc<Mutex<HashMap<String, RunningJob>>>,
    queued: Arc<
        Mutex<
            VecDeque<(
                AgentSink,
                u64,
                AgentPolicy,
                ShellConfig,
                PathBuf,
                ShellAgentShellRequest,
            )>,
        >,
    >,
    prepared_profiles: PreparedShellProfileCache,
    lifecycle: Arc<Mutex<()>>,
    shutting_down: Arc<AtomicBool>,
    workers: ActivityTracker,
    current_sink: Arc<Mutex<Option<AgentSink>>>,
}

impl JobManager {
    fn new(max_concurrent: usize) -> Self {
        Self {
            max_concurrent: max_concurrent.max(1),
            jobs: Arc::new(Mutex::new(HashMap::new())),
            queued: Arc::new(Mutex::new(VecDeque::new())),
            prepared_profiles: PreparedShellProfileCache::default(),
            lifecycle: Arc::new(Mutex::new(())),
            shutting_down: Arc::new(AtomicBool::new(false)),
            workers: ActivityTracker::default(),
            current_sink: Arc::new(Mutex::new(None)),
        }
    }
}

#[derive(Debug, Clone)]
struct RunningJob {
    client_id: String,
    agent_instance_id: String,
    snapshot: ShellJobSnapshot,
    child: Option<Arc<Mutex<Child>>>,
    process_group_id: Option<u32>,
    stop_requested: Arc<AtomicBool>,
    slot_reserved: bool,
}

#[cfg(test)]
fn test_job_snapshot(job_id: &str) -> ShellJobSnapshot {
    ShellJobSnapshot {
        job_id: job_id.to_string(),
        request_id: format!("request-{job_id}"),
        status: "running".to_string(),
        update_seq: 1,
        created_at: chrono::Utc::now().timestamp(),
        started_at: Some(chrono::Utc::now().timestamp()),
        ended_at: None,
        exit_code: None,
        duration_ms: None,
        error: None,
        context: shell_protocol::ShellJobContext {
            runtime_project_id: None,
            workflow_session_id: None,
            project_cwd: None,
            cwd: None,
            purpose: Some("other".to_string()),
            shell: Some("configured".to_string()),
            command_preview: "test job".to_string(),
            validation_steps: Vec::new(),
        },
        stdout: ShellJobStreamSnapshot::default(),
        stderr: ShellJobStreamSnapshot::default(),
        validation_progress: None,
    }
}

#[cfg(test)]
fn test_job_context(cwd: &Path, validation_steps: Vec<String>) -> shell_protocol::ShellJobContext {
    shell_protocol::ShellJobContext {
        runtime_project_id: None,
        workflow_session_id: None,
        project_cwd: None,
        cwd: Some(cwd.to_string_lossy().into_owned()),
        purpose: Some("other".to_string()),
        shell: Some("configured".to_string()),
        command_preview: "test command".to_string(),
        validation_steps,
    }
}

#[derive(Debug)]
enum OutputChunk {
    Stdout(String),
    Stderr(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentCliAction {
    Run {
        config_path: PathBuf,
        once: bool,
    },
    Exit {
        code: i32,
        stdout: String,
        stderr: String,
    },
}

fn usage() -> &'static str {
    "Usage: webcodex-runner [--config PATH] [--once]\n\n\
     Options:\n\
       -h, --help                 Print help and exit\n\
       -V, --version              Print version and exit\n\
       -c, --config PATH          Agent config path for normal runtime\n\
       --profile NAME             Client config profile for default config path\n\
       --once                     Complete one successful poll, then exit (polling transport)\n\n\
     With --profile, the default config path is derived under\n\
     /etc/webcodex/clients/<profile> for root or\n\
     ~/.config/webcodex/clients/<profile> for non-root users. Explicit\n\
     --config overrides the profile-derived default.\n\n\
     Environment:\n\
       WEBCODEX_AGENT_CONFIG      default config path override\n\
     Example agent.toml:\n\
       server_url = \"https://v4.yyjeqhc.cn\"\n\
       token = \"...\"\n\
       client_id = \"xrh\"\n\
       display_name = \"XRH\"\n\
       owner = \"yyjeqhc\"\n\
       projects_dir = \"/root/.config/webcodex/projects.d\"\n\
       poll_interval_ms = 1000\n\
\n\
       [policy]\n\
       allow_raw_shell = true\n\
       allow_cwd_anywhere = true\n\
       max_timeout_secs = 3600\n\
       max_output_bytes = 262144\n"
}

fn parse_args() -> Result<AgentCliAction, String> {
    parse_agent_args(std::env::args().skip(1))
}

fn parse_agent_args<I, S>(args: I) -> Result<AgentCliAction, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_string())
        .collect();
    if args.len() == 1 {
        match args[0].as_str() {
            "--help" | "-h" => {
                return Ok(AgentCliAction::Exit {
                    code: 0,
                    stdout: usage().to_string(),
                    stderr: String::new(),
                });
            }
            "--version" | "-V" => {
                return Ok(AgentCliAction::Exit {
                    code: 0,
                    stdout: build_info::version_output("webcodex-runner"),
                    stderr: String::new(),
                });
            }
            _ => {}
        }
    }
    let mut config_path = std::env::var("WEBCODEX_AGENT_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_config_path());
    let mut config_explicit = false;
    let mut profile: Option<String> = None;
    let mut once = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                return Ok(AgentCliAction::Exit {
                    code: 0,
                    stdout: usage().to_string(),
                    stderr: String::new(),
                });
            }
            "--version" | "-V" => {
                return Ok(AgentCliAction::Exit {
                    code: 0,
                    stdout: build_info::version_output("webcodex-runner"),
                    stderr: String::new(),
                });
            }
            "--once" => once = true,
            "--config" | "-c" => {
                let Some(path) = args.next() else {
                    return Err("--config requires a path".to_string());
                };
                config_path = PathBuf::from(path);
                config_explicit = true;
            }
            "--profile" => {
                let Some(value) = args.next() else {
                    return Err("--profile requires a value".to_string());
                };
                profile = Some(value);
            }
            _ => return Err(format!("unknown argument: {}\n{}", arg, usage())),
        }
    }
    if let Some(profile) = profile
        .as_deref()
        .map(validate_client_profile)
        .transpose()?
    {
        if !config_explicit {
            config_path = client_profile_agent_config(&profile);
        }
    }
    Ok(AgentCliAction::Run { config_path, once })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentHttpErrorKind {
    ServerUnavailable,
    Auth,
    NotFound,
    /// A local URL/TLS configuration failure that retrying cannot repair.
    Config,
    /// 4xx (other than auth/endpoint kinds): the server understood the
    /// exchange and rejected this exact request. Resending the identical
    /// payload cannot succeed.
    ClientRejected,
    Status,
    RequestTimeout,
    Request,
    /// The response was incomplete or was recognizably produced by a
    /// temporary proxy/upstream failure.
    DecodeTransient,
    /// The response was complete enough to prove that it does not implement
    /// the expected server protocol.
    ProtocolDecode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentHttpError {
    kind: AgentHttpErrorKind,
    path: String,
    summary: String,
    /// Bounded structured server error, when the response contract supplied
    /// one. Recovery classifiers use this instead of parsing display strings.
    server_error: Option<String>,
}

impl AgentHttpError {
    fn status(path: &str, status: reqwest::StatusCode, body: &str) -> Self {
        let kind = match status.as_u16() {
            401 | 403 => AgentHttpErrorKind::Auth,
            404 => AgentHttpErrorKind::NotFound,
            // Explicitly retryable request-level statuses.
            408 | 429 => AgentHttpErrorKind::Status,
            code if (500..600).contains(&code) => AgentHttpErrorKind::ServerUnavailable,
            code if (400..500).contains(&code) => AgentHttpErrorKind::ClientRejected,
            _ if looks_like_proxy_html_error(body) => AgentHttpErrorKind::ServerUnavailable,
            _ => AgentHttpErrorKind::Status,
        };
        let server_error = structured_body_error(body);
        let mut summary = http_status_summary(status);
        if kind == AgentHttpErrorKind::ClientRejected {
            if let Some(detail) = server_error.as_deref() {
                summary = format!("{}: {}", summary, detail);
            }
        }
        Self {
            kind,
            path: bounded_endpoint_path(path),
            summary,
            server_error,
        }
    }

    fn request(path: &str, error: reqwest::Error) -> Self {
        let chain = error_chain_text(&error);
        let kind = if error.is_builder() || looks_like_fatal_tls_request(&chain) {
            AgentHttpErrorKind::Config
        } else if looks_like_server_down_request(&error, &chain) {
            AgentHttpErrorKind::ServerUnavailable
        } else if error.is_timeout() {
            AgentHttpErrorKind::RequestTimeout
        } else {
            AgentHttpErrorKind::Request
        };
        Self {
            kind,
            path: bounded_endpoint_path(path),
            summary: request_error_summary(error, &chain),
            server_error: None,
        }
    }

    fn decode_transient(path: &str, summary: String) -> Self {
        Self {
            kind: AgentHttpErrorKind::DecodeTransient,
            path: bounded_endpoint_path(path),
            summary,
            server_error: None,
        }
    }

    fn protocol_decode(path: &str, summary: String) -> Self {
        Self {
            kind: AgentHttpErrorKind::ProtocolDecode,
            path: bounded_endpoint_path(path),
            summary,
            server_error: None,
        }
    }
}

impl std::fmt::Display for AgentHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            AgentHttpErrorKind::ServerUnavailable => {
                write!(f, "server unavailable for {}: {}", self.path, self.summary)
            }
            AgentHttpErrorKind::Auth => write!(
                f,
                "authentication failed for {}: {}; check agent token/config",
                self.path, self.summary
            ),
            AgentHttpErrorKind::NotFound => write!(
                f,
                "endpoint missing or incompatible server for {}: {}",
                self.path, self.summary
            ),
            AgentHttpErrorKind::Config => {
                write!(
                    f,
                    "HTTP/TLS configuration failed for {}: {}",
                    self.path, self.summary
                )
            }
            AgentHttpErrorKind::ClientRejected => {
                write!(f, "server rejected {} request: {}", self.path, self.summary)
            }
            AgentHttpErrorKind::Status
            | AgentHttpErrorKind::RequestTimeout
            | AgentHttpErrorKind::Request => {
                write!(f, "{} request failed: {}", self.path, self.summary)
            }
            AgentHttpErrorKind::DecodeTransient => {
                write!(
                    f,
                    "transient response corruption for {}: {}",
                    self.path, self.summary
                )
            }
            AgentHttpErrorKind::ProtocolDecode => write!(
                f,
                "response from {} incompatible with server protocol: {}",
                self.path, self.summary
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegisterRecoveryAction {
    Retry,
    WaitForLease,
    Fatal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RegisterErrorKind {
    Transient,
    LeaseConflict,
    Auth,
    EndpointMissing,
    Rejected,
    Config,
    Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegisterError {
    kind: RegisterErrorKind,
    message: String,
}

impl RegisterError {
    fn from_http(error: AgentHttpError, client_id: &str) -> Self {
        let kind = match error.kind {
            AgentHttpErrorKind::ServerUnavailable
            | AgentHttpErrorKind::Status
            | AgentHttpErrorKind::RequestTimeout
            | AgentHttpErrorKind::Request
            | AgentHttpErrorKind::DecodeTransient => RegisterErrorKind::Transient,
            AgentHttpErrorKind::Auth => RegisterErrorKind::Auth,
            AgentHttpErrorKind::NotFound => RegisterErrorKind::EndpointMissing,
            AgentHttpErrorKind::Config => RegisterErrorKind::Config,
            AgentHttpErrorKind::ProtocolDecode => RegisterErrorKind::Protocol,
            AgentHttpErrorKind::ClientRejected
                if is_active_instance_lease_conflict(client_id, error.server_error.as_deref()) =>
            {
                RegisterErrorKind::LeaseConflict
            }
            AgentHttpErrorKind::ClientRejected => RegisterErrorKind::Rejected,
        };
        let message = if error.kind == AgentHttpErrorKind::ProtocolDecode {
            format!(
                "register response incompatible with server protocol: endpoint={} {}",
                error.path, error.summary
            )
        } else {
            error.to_string()
        };
        Self { kind, message }
    }

    fn from_response_error(client_id: &str, error: Option<String>) -> Self {
        let summary =
            bounded_single_line(error.as_deref().unwrap_or("register failed without error"));
        let kind = if is_active_instance_lease_conflict(client_id, Some(&summary)) {
            RegisterErrorKind::LeaseConflict
        } else if looks_like_auth_failure_message(&summary) {
            RegisterErrorKind::Auth
        } else {
            RegisterErrorKind::Rejected
        };
        Self {
            kind,
            message: format!("register rejected by server: {summary}"),
        }
    }

    fn recovery_action(&self) -> RegisterRecoveryAction {
        match self.kind {
            RegisterErrorKind::Transient => RegisterRecoveryAction::Retry,
            RegisterErrorKind::LeaseConflict => RegisterRecoveryAction::WaitForLease,
            RegisterErrorKind::Auth
            | RegisterErrorKind::EndpointMissing
            | RegisterErrorKind::Rejected
            | RegisterErrorKind::Config
            | RegisterErrorKind::Protocol => RegisterRecoveryAction::Fatal,
        }
    }

    fn into_message(self) -> String {
        self.message
    }
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollingRecoveryAction {
    RetryPoll,
    ReRegister,
    Fatal,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PollErrorKind {
    Transient,
    SessionLost,
    Auth,
    EndpointMissing,
    Rejected,
    Config,
    Protocol,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PollError {
    kind: PollErrorKind,
    message: String,
}

impl PollError {
    fn new(kind: PollErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn from_http(error: AgentHttpError, client_id: &str) -> Self {
        match error.kind {
            AgentHttpErrorKind::ServerUnavailable => Self::new(
                PollErrorKind::Transient,
                format!(
                    "server unavailable while polling {}: {}",
                    error.path, error.summary
                ),
            ),
            AgentHttpErrorKind::Auth => Self::new(
                PollErrorKind::Auth,
                format!(
                    "authentication failed while polling {}: {}; check agent token/config",
                    error.path, error.summary
                ),
            ),
            AgentHttpErrorKind::NotFound => Self::new(
                PollErrorKind::EndpointMissing,
                format!(
                    "poll endpoint missing or incompatible server while polling {}: {}",
                    error.path, error.summary
                ),
            ),
            AgentHttpErrorKind::Config => Self::new(
                PollErrorKind::Config,
                format!(
                    "HTTP/TLS configuration failed while polling {}: {}",
                    error.path, error.summary
                ),
            ),
            AgentHttpErrorKind::RequestTimeout => Self::new(
                PollErrorKind::Transient,
                format!(
                    "poll request timed out while polling {}: {}",
                    error.path, error.summary
                ),
            ),
            AgentHttpErrorKind::ClientRejected
                if is_unknown_polling_session(client_id, error.server_error.as_deref()) =>
            {
                Self::new(
                    PollErrorKind::SessionLost,
                    format!(
                        "polling session is not registered for client_id={}",
                        bounded_single_line(client_id)
                    ),
                )
            }
            AgentHttpErrorKind::ClientRejected => Self::new(
                PollErrorKind::Rejected,
                format!(
                    "server permanently rejected polling {}: {}",
                    error.path, error.summary
                ),
            ),
            AgentHttpErrorKind::Status | AgentHttpErrorKind::Request => Self::new(
                PollErrorKind::Transient,
                format!(
                    "poll request failed while polling {}: {}",
                    error.path, error.summary
                ),
            ),
            AgentHttpErrorKind::DecodeTransient => Self::new(
                PollErrorKind::Transient,
                format!(
                    "transient poll response corruption: endpoint={} {}",
                    error.path, error.summary
                ),
            ),
            AgentHttpErrorKind::ProtocolDecode => Self::new(
                PollErrorKind::Protocol,
                format!(
                    "poll response incompatible with server protocol: endpoint={} {}",
                    error.path, error.summary
                ),
            ),
        }
    }

    /// Classify a fatal result submission failure surfaced by
    /// `dispatch_request`. Permanent rejection and exhausted transient retries
    /// are resolved as payload-lifecycle outcomes inside the HTTP sink, so
    /// neither can trigger polling sleep/re-registration recovery here.
    fn from_submit(error: SubmitResultError) -> Self {
        match error {
            SubmitResultError::FatalAuth(message) => Self::new(PollErrorKind::Auth, message),
            SubmitResultError::FatalProtocol(message) => {
                Self::new(PollErrorKind::EndpointMissing, message)
            }
            SubmitResultError::FatalConfig(message) => Self::new(PollErrorKind::Config, message),
            SubmitResultError::TransportClosed(message) => {
                Self::new(PollErrorKind::Rejected, message)
            }
            SubmitResultError::Shutdown(message) => Self::new(PollErrorKind::Shutdown, message),
        }
    }

    fn from_response_error(client_id: &str, error: Option<String>) -> Self {
        let message = error.unwrap_or_else(|| "poll failed without error".to_string());
        let summary = bounded_single_line(&message);
        if looks_like_auth_failure_message(&summary) {
            Self::new(
                PollErrorKind::Auth,
                format!(
                    "authentication failed while polling {}: {}; check agent token/config",
                    AGENT_POLL_PATH, summary
                ),
            )
        } else if is_unknown_polling_session(client_id, Some(&summary)) {
            Self::new(
                PollErrorKind::SessionLost,
                format!(
                    "polling session is not registered for client_id={}",
                    bounded_single_line(client_id)
                ),
            )
        } else {
            Self::new(
                PollErrorKind::Rejected,
                format!("server permanently rejected polling response: {summary}"),
            )
        }
    }

    fn recovery_action(&self) -> PollingRecoveryAction {
        match self.kind {
            PollErrorKind::Transient => PollingRecoveryAction::RetryPoll,
            PollErrorKind::SessionLost => PollingRecoveryAction::ReRegister,
            PollErrorKind::Auth
            | PollErrorKind::EndpointMissing
            | PollErrorKind::Rejected
            | PollErrorKind::Config
            | PollErrorKind::Protocol => PollingRecoveryAction::Fatal,
            PollErrorKind::Shutdown => PollingRecoveryAction::Shutdown,
        }
    }

    #[cfg(test)]
    fn is_terminal(&self) -> bool {
        self.recovery_action() == PollingRecoveryAction::Fatal
    }

    #[cfg(test)]
    fn is_shutdown(&self) -> bool {
        self.kind == PollErrorKind::Shutdown
    }

    fn into_message(self) -> String {
        self.message
    }
}

impl std::fmt::Display for PollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn http_status_summary(status: reqwest::StatusCode) -> String {
    match status.canonical_reason() {
        Some(reason) => format!("HTTP {} {}", status.as_u16(), reason),
        None => format!("HTTP {}", status.as_u16()),
    }
}

fn looks_like_proxy_html_error(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("<html")
        && contains_any(
            &lower,
            &[
                "bad gateway",
                "service unavailable",
                "gateway timeout",
                "nginx",
                "upstream",
            ],
        )
}

fn looks_like_server_down_request(error: &reqwest::Error, chain: &str) -> bool {
    if error.is_connect() {
        return true;
    }
    let lower = chain.to_ascii_lowercase();
    contains_any(
        &lower,
        &[
            "connection refused",
            "connection reset",
            "connection aborted",
            "connection closed",
            "early eof",
            "unexpected eof",
            "incomplete message",
            "broken pipe",
        ],
    )
}

fn looks_like_fatal_tls_request(chain: &str) -> bool {
    let lower = chain.to_ascii_lowercase();
    contains_any(
        &lower,
        &[
            "certificate verify failed",
            "invalid peer certificate",
            "unknownissuer",
            "notvalidforname",
            "certificateunknown",
            "invalid certificate",
            "no application protocol",
            "alpn mismatch",
        ],
    )
}

fn looks_like_auth_failure_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    contains_any(
        &lower,
        &[
            "unauthorized",
            "forbidden",
            "invalid token",
            "bad token",
            "auth failed",
            "authentication",
        ],
    )
}

fn is_active_instance_lease_conflict(client_id: &str, error: Option<&str>) -> bool {
    let expected = format!(
        "agent client {} is already online with a different instance",
        client_id
    );
    error == Some(expected.as_str())
}

fn is_unknown_polling_session(client_id: &str, error: Option<&str>) -> bool {
    let expected = format!("unknown shell client: {}", client_id);
    error == Some(expected.as_str())
}

fn error_chain_text(error: &reqwest::Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = StdError::source(error);
    while let Some(err) = source {
        parts.push(err.to_string());
        source = err.source();
    }
    parts.join(": ")
}

fn request_error_summary(error: reqwest::Error, chain: &str) -> String {
    let lower = chain.to_ascii_lowercase();
    if lower.contains("connection refused") {
        "connection refused".to_string()
    } else if lower.contains("connection reset") {
        "connection reset".to_string()
    } else if lower.contains("connection aborted") {
        "connection aborted".to_string()
    } else if lower.contains("broken pipe") {
        "broken pipe".to_string()
    } else if contains_any(
        &lower,
        &[
            "connection closed",
            "early eof",
            "unexpected eof",
            "incomplete message",
        ],
    ) {
        "connection closed before response completed".to_string()
    } else if error.is_connect() {
        "connection failed".to_string()
    } else if error.is_timeout() {
        "request timed out".to_string()
    } else {
        bounded_single_line(&error.without_url().to_string())
    }
}

fn bounded_single_line(text: &str) -> String {
    const MAX_CHARS: usize = 160;
    let mut out = String::new();
    let mut last_space = false;
    for ch in text.chars() {
        let ch = if ch.is_whitespace() || ch.is_control() {
            ' '
        } else {
            ch
        };
        if ch == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(ch);
        if out.chars().count() >= MAX_CHARS {
            out.push_str("...");
            break;
        }
    }
    out.trim().to_string()
}

fn bounded_endpoint_path(path: &str) -> String {
    let without_query = path.split_once('?').map_or(path, |(path, _)| path);
    bounded_single_line(without_query)
}

/// Extract the structured `error` field from a JSON error response body, if
/// present. Non-JSON bodies (proxy HTML, truncated payloads) yield `None` so
/// raw response bytes never leak into diagnostics.
fn structured_body_error(body: &str) -> Option<String> {
    const MAX_PARSE_BYTES: usize = 64 * 1024;
    if body.len() > MAX_PARSE_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?.as_str()?;
    let error = bounded_single_line(error);
    if error.is_empty() {
        None
    } else {
        Some(error)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct BoundedResponseBody {
    bytes: Vec<u8>,
    exceeded_limit: bool,
}

fn read_bounded_response_body<R: Read>(
    reader: &mut R,
    content_length: Option<u64>,
    max_bytes: usize,
) -> std::io::Result<BoundedResponseBody> {
    let read_limit = (max_bytes as u64).saturating_add(1);
    let initial_capacity = content_length
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_default()
        .min(max_bytes.saturating_add(1));
    let mut bytes = Vec::with_capacity(initial_capacity);
    reader.take(read_limit).read_to_end(&mut bytes)?;
    let exceeded_limit = bytes.len() > max_bytes;
    if exceeded_limit {
        bytes.truncate(max_bytes);
    }
    Ok(BoundedResponseBody {
        bytes,
        exceeded_limit,
    })
}

fn bounded_response_content_type(
    value: Option<&reqwest::header::HeaderValue>,
    token: &str,
) -> String {
    match value {
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|value| {
                let media_type = value.split(';').next()?.trim();
                let lower = media_type.to_ascii_lowercase();
                let token = token.trim();
                if media_type.is_empty()
                    || lower.contains("authorization")
                    || lower.contains("bearer")
                    || (!token.is_empty() && media_type.contains(token))
                    || !media_type.chars().all(|ch| {
                        ch.is_ascii_alphanumeric()
                            || matches!(
                                ch,
                                '/' | '!' | '#' | '$' | '&' | '^' | '_' | '.' | '+' | '-'
                            )
                    })
                {
                    None
                } else {
                    Some(bounded_single_line(media_type))
                }
            })
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "<redacted-or-invalid>".to_string()),
        None => "<missing>".to_string(),
    }
}

fn response_decode_summary(
    status: reqwest::StatusCode,
    content_type: &str,
    detail: impl AsRef<str>,
) -> String {
    format!(
        "status={} content_type={} {}",
        http_status_summary(status),
        content_type,
        detail.as_ref()
    )
}

fn looks_like_transient_proxy_response(content_type: &str, body: &[u8]) -> bool {
    const MAX_INSPECT_BYTES: usize = 8 * 1024;
    let inspected = &body[..body.len().min(MAX_INSPECT_BYTES)];
    let text = String::from_utf8_lossy(inspected);
    let lower = text.to_ascii_lowercase();
    let has_temporary_gateway_marker = contains_any(
        &lower,
        &[
            "bad gateway",
            "service unavailable",
            "gateway timeout",
            "upstream connect error",
            "upstream connection error",
            "upstream unavailable",
            "proxy error",
            "temporarily unavailable",
        ],
    );
    let looks_html = content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/html"))
        || lower.contains("<html")
        || lower.contains("<!doctype html");
    if looks_html && has_temporary_gateway_marker {
        return true;
    }
    let plain = lower.trim();
    body.len() <= MAX_INSPECT_BYTES
        && (matches!(
            plain,
            "bad gateway"
                | "service unavailable"
                | "gateway timeout"
                | "upstream unavailable"
                | "temporarily unavailable"
        ) || plain.starts_with("upstream connect error")
            || plain.starts_with("upstream connection error"))
}

fn serde_json_category_name(error: &serde_json::Error) -> &'static str {
    match error.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "data",
        serde_json::error::Category::Eof => "eof",
    }
}

fn decode_json_response<R>(
    path: &str,
    status: reqwest::StatusCode,
    content_type: &str,
    body: BoundedResponseBody,
) -> Result<R, AgentHttpError>
where
    R: serde::de::DeserializeOwned,
{
    if body.bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(AgentHttpError::decode_transient(
            path,
            response_decode_summary(status, content_type, "empty response body"),
        ));
    }
    if looks_like_transient_proxy_response(content_type, &body.bytes) {
        return Err(AgentHttpError::decode_transient(
            path,
            response_decode_summary(
                status,
                content_type,
                "recognized temporary proxy/upstream response",
            ),
        ));
    }
    if body.exceeded_limit {
        return Err(AgentHttpError::protocol_decode(
            path,
            response_decode_summary(
                status,
                content_type,
                format!(
                    "response body exceeds limit_bytes={}",
                    AGENT_HTTP_RESPONSE_BODY_MAX_BYTES
                ),
            ),
        ));
    }
    serde_json::from_slice(&body.bytes).map_err(|error| {
        let detail = format!(
            "serde_category={} line={} column={}",
            serde_json_category_name(&error),
            error.line(),
            error.column()
        );
        let summary = response_decode_summary(status, content_type, detail);
        if error.is_eof() {
            AgentHttpError::decode_transient(path, summary)
        } else {
            AgentHttpError::protocol_decode(path, summary)
        }
    })
}

fn post_json<T, R>(
    client: &Client,
    cfg: &AgentConfig,
    path: &str,
    body: &T,
) -> Result<R, AgentHttpError>
where
    T: serde::Serialize + ?Sized,
    R: serde::de::DeserializeOwned,
{
    post_json_with_auth(client, &cfg.server_url, &cfg.token, path, body)
}

fn post_json_with_auth<T, R>(
    client: &Client,
    server_url: &str,
    token: &str,
    path: &str,
    body: &T,
) -> Result<R, AgentHttpError>
where
    T: serde::Serialize + ?Sized,
    R: serde::de::DeserializeOwned,
{
    let url = format!("{}{}", server_url.trim_end_matches('/'), path);
    let mut req = client.post(url);
    if !token.trim().is_empty() {
        req = req.bearer_auth(token.trim());
    }
    let resp = req
        .json(body)
        .send()
        .map_err(|e| AgentHttpError::request(path, e))?;
    let status = resp.status();
    let content_type =
        bounded_response_content_type(resp.headers().get(reqwest::header::CONTENT_TYPE), token);
    let content_length = resp.content_length();
    if content_length.is_some_and(|length| length > AGENT_HTTP_RESPONSE_BODY_MAX_BYTES as u64) {
        if !status.is_success() {
            return Err(AgentHttpError::status(path, status, ""));
        }
        return Err(AgentHttpError::protocol_decode(
            path,
            response_decode_summary(
                status,
                &content_type,
                format!(
                    "declared response body exceeds limit_bytes={}",
                    AGENT_HTTP_RESPONSE_BODY_MAX_BYTES
                ),
            ),
        ));
    }
    let mut resp = resp;
    let body = match read_bounded_response_body(
        &mut resp,
        content_length,
        AGENT_HTTP_RESPONSE_BODY_MAX_BYTES,
    ) {
        Ok(body) => body,
        Err(error) if status.is_success() => {
            return Err(AgentHttpError::decode_transient(
                path,
                response_decode_summary(
                    status,
                    &content_type,
                    format!("response body read interrupted io_kind={:?}", error.kind()),
                ),
            ));
        }
        Err(_) => return Err(AgentHttpError::status(path, status, "")),
    };
    if !status.is_success() {
        let text = String::from_utf8_lossy(&body.bytes);
        return Err(AgentHttpError::status(path, status, &text));
    }
    decode_json_response(path, status, &content_type, body)
}

fn agent_register_capabilities(cfg: &AgentConfig) -> ShellClientCapabilities {
    let mut capabilities = cfg.capabilities.clone().unwrap_or_default();
    capabilities.jobs = true;
    capabilities.file_read = true;
    capabilities.file_write = true;
    capabilities.async_jobs = true;
    capabilities.async_shell_jobs = true;
    capabilities.structured_validation_argv = true;
    capabilities.project_lifecycle = true;
    capabilities.job_state_reconciliation = true;
    // New agents always advertise read-only LSP navigation. Older agents omit
    // the field and deserialize as false on the server.
    capabilities.lsp_read_only_navigation = true;
    // Advertise only after a real child-process enforcement probe proves Linux
    // Landlock ABI v3 (including TRUNCATE) works on this host. Every request
    // still applies the policy again in pre_exec and fails closed on error.
    capabilities.sandbox_inspect_commands =
        crate::command_sandbox::inspect_sandbox_available().is_ok();
    capabilities
}

#[cfg(test)]
fn build_register_request(
    cfg: &AgentConfig,
    projects: Vec<ShellAgentProjectSummary>,
    protocol_version: &str,
    agent_instance_id: &str,
    prepared_cache_count: usize,
) -> ShellClientRegisterRequest {
    let runtime = ReloadableAgentConfig::new(cfg.clone(), PathBuf::new());
    build_register_request_with_provider_status(
        cfg,
        &runtime,
        projects,
        protocol_version,
        agent_instance_id,
        prepared_cache_count,
        ShellJobInventory {
            active_complete: true,
            jobs: Vec::new(),
        },
    )
    .0
}

fn build_register_request_with_provider_status(
    cfg: &AgentConfig,
    runtime: &ReloadableAgentConfig,
    projects: Vec<ShellAgentProjectSummary>,
    protocol_version: &str,
    agent_instance_id: &str,
    prepared_cache_count: usize,
    job_inventory: ShellJobInventory,
) -> (
    ShellClientRegisterRequest,
    Arc<webcodex_runner::external_tools::ExternalToolRouter>,
    u64,
) {
    let hot = runtime.snapshot();
    let capabilities = agent_register_capabilities(cfg);
    let (mut tool_providers, revision) = hot.external_tools.registration_status();
    tool_providers.config_reload = hot.reload_status();
    (
        ShellClientRegisterRequest {
            client_id: cfg.client_id.clone(),
            agent_instance_id: agent_instance_id.to_string(),
            display_name: cfg.display_name.clone(),
            owner: cfg.owner.clone(),
            hostname: cfg.hostname.clone().or_else(hostname),
            capabilities: Some(capabilities),
            projects: Some(projects),
            agent_protocol_version: Some(protocol_version.to_string()),
            policy: Some(register_policy_summary(
                &hot,
                prepared_cache_count,
                tool_providers,
            )),
            process_started_at: Some(process_started_at()),
            build: Some(runner_build_info()),
            job_inventory: Some(job_inventory),
        },
        Arc::clone(&hot.external_tools),
        revision,
    )
}

/// Unix timestamp when this runner process started. Captured on first call;
/// `run_agent` initializes it at startup so registration payloads report the
/// real process start, not the first register time after a reconnect.
fn process_started_at() -> i64 {
    static STARTED_AT: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *STARTED_AT.get_or_init(|| chrono::Utc::now().timestamp())
}

/// Non-secret runner build identity for mixed-version diagnostics.
fn runner_build_info() -> shell_protocol::AgentBuildInfo {
    let info = build_info::current();
    shell_protocol::AgentBuildInfo {
        version: Some(info.version.to_string()),
        git_commit: info.git_commit.map(str::to_string),
    }
}

/// Shell dialect derived from a program path basename. Only `sh` and `bash`
/// map to portable dialects; anything else is `custom` and callers that need
/// deterministic syntax must select an explicit `shell=sh|bash`.
fn shell_dialect_for_program(program: &str) -> &'static str {
    match std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
    {
        "sh" => "sh",
        "bash" => "bash",
        _ => "custom",
    }
}

/// Build the sanitized shell-profiles summary from the active shell config.
/// Exposes only safe metadata: profile names, whether each has an init_script
/// (boolean, never the body), env key counts (never values), the resolved
/// program, and arg counts. `prepared_cache_count` is the number of snapshots
/// prepared at call time (typically 0 right after start). Never includes env
/// values, init_script bodies, tokens, or the full env snapshot.
fn build_shell_profiles_summary(
    shell: &ShellConfig,
    prepared_cache_count: usize,
) -> ShellProfilesSummary {
    let profiles: Vec<ShellProfileSummaryEntry> = shell
        .profiles
        .iter()
        .map(|(name, profile)| {
            let program = profile
                .program
                .clone()
                .unwrap_or_else(|| shell.program.clone());
            let args = profile.args.clone().unwrap_or_else(|| shell.args.clone());
            let dialect = shell_dialect_for_program(&program);
            ShellProfileSummaryEntry {
                name: name.clone(),
                has_init_script: profile.init_script.is_some(),
                env_keys_count: profile.env.len(),
                program,
                args_count: args.len(),
                dialect: Some(dialect.to_string()),
            }
        })
        .collect();
    // Default execution path when the caller selects no explicit shell:
    // shell.default_profile if set, otherwise the plain shell program.
    // (A project-level shell_profile override is reported per project.)
    let default_program = shell
        .default_profile
        .as_deref()
        .and_then(|name| shell.profiles.get(name))
        .and_then(|profile| profile.program.clone())
        .unwrap_or_else(|| shell.program.clone());
    let default_dialect = shell_dialect_for_program(&default_program).to_string();
    // Explicit shell=sh|bash always resolves on the runner; configured custom
    // profiles add the custom dialect.
    let mut available: Vec<String> = vec!["sh".to_string(), "bash".to_string()];
    for entry in &profiles {
        if let Some(dialect) = entry.dialect.as_deref() {
            if !available.iter().any(|existing| existing == dialect) {
                available.push(dialect.to_string());
            }
        }
    }
    ShellProfilesSummary {
        default_profile: shell.default_profile.clone(),
        configured_count: shell.profiles.len(),
        prepared_cache_count,
        profiles,
        default_dialect: Some(default_dialect),
        available_dialects: Some(available),
    }
}

/// Build the sanitized agent policy summary sent at registration. Mirrors the
/// local `AgentPolicy` but only carries non-secret fields. The shell env
/// values and init_script path are intentionally NOT included. The sanitized
/// shell-profiles summary is attached so observability can show which profile
/// a project resolves to without exposing env values or init_script bodies.
fn register_policy_summary(
    cfg: &HotAgentConfig,
    prepared_cache_count: usize,
    tool_providers: shell_protocol::ToolProvidersStatus,
) -> AgentPolicySummary {
    AgentPolicySummary {
        allow_raw_shell: cfg.policy.allow_raw_shell,
        allow_cwd_anywhere: cfg.policy.allow_cwd_anywhere,
        allowed_roots: cfg.policy.allowed_roots.clone(),
        max_timeout_secs: cfg.policy.max_timeout_secs,
        max_output_bytes: cfg.policy.max_output_bytes,
        shell_profiles: Some(build_shell_profiles_summary(
            &cfg.shell,
            prepared_cache_count,
        )),
        tool_providers: Some(tool_providers),
    }
}

fn register(
    client: &Client,
    cfg: &AgentConfig,
    runtime: &ReloadableAgentConfig,
    project_cache: &mut AgentProjectCache,
    shutdown: Option<&AtomicBool>,
    agent_instance_id: &str,
    prepared_cache_count: usize,
    jobs: &JobManager,
) -> Result<(usize, ShellJobInventory), RegisterError> {
    let projects = project_cache.get_with_shutdown(cfg, shutdown);
    let projects_count = projects.iter().filter(|project| !project.disabled).count();
    let job_inventory = jobs.inventory();
    let (body, provider, provider_revision) = build_register_request_with_provider_status(
        cfg,
        runtime,
        projects,
        AGENT_PROTOCOL_VERSION_POLLING_V1,
        agent_instance_id,
        prepared_cache_count,
        job_inventory.clone(),
    );
    let response: ShellClientRegisterResponse = post_json(client, cfg, AGENT_REGISTER_PATH, &body)
        .map_err(|error| RegisterError::from_http(error, &cfg.client_id))?;
    if response.success {
        provider.mark_status_reported(provider_revision);
        Ok((projects_count, job_inventory))
    } else {
        Err(RegisterError::from_response_error(
            &cfg.client_id,
            response.error,
        ))
    }
}

fn is_file_request_kind(kind: &str) -> bool {
    is_basic_file_request_kind(kind)
        || is_line_edit_request_kind(kind)
        || is_artifact_request_kind(kind)
        || is_checkpoint_request_kind(kind)
}

fn handle_file_request(policy: &AgentPolicy, request: &ShellAgentShellRequest) -> CommandResult {
    let Some(path) = request.path.as_deref() else {
        return CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(0),
            error: Some("file request missing path".to_string()),
        };
    };
    let start = Instant::now();
    if is_line_edit_request_kind(&request.kind) {
        if let Err(e) = validate_line_edit_agent_path(path) {
            return CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(0),
                error: Some(e),
            };
        }
    }
    let resolved = match resolve_requested_path(policy, request.cwd.as_deref(), path) {
        Ok(path) => path,
        Err(e) => {
            return CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(0),
                error: Some(e),
            }
        }
    };
    match request.kind.as_str() {
        "file_replace_line_range"
        | "file_insert_at_line"
        | "file_delete_line_range"
        | "file_replace_exact_block"
        | "file_insert_before_pattern"
        | "file_insert_after_pattern" => handle_line_edit_file_request(request, &resolved, start),
        "file_replace_in_file" => handle_replace_in_file_request(request, &resolved, start),
        "file_write_project_file" => handle_write_project_file_request(request, &resolved, start),
        "file_apply_text_edits" => handle_apply_text_edits_file_request(policy, request, start),
        "file_save_project_artifact"
        | "file_read_project_artifact_metadata"
        | "file_read_project_artifact"
        | "file_artifact_upload_begin"
        | "file_artifact_upload_chunk"
        | "file_artifact_upload_finish"
        | "file_artifact_upload_abort" => handle_artifact_file_request(request, &resolved, start),
        "file_checkpoint_create" | "file_checkpoint_restore" => {
            handle_checkpoint_file_request(request, &resolved, start)
        }
        "file_read" | "file_write" | "file_list" | "file_project_overview" => {
            handle_basic_file_request(policy, request, &resolved, start)
        }
        _ => CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(format!("unknown file request kind: {}", request.kind)),
        },
    }
}

#[derive(Debug, Default)]
struct CreatedProjectPaths {
    project_dir_created: Option<PathBuf>,
    paths: Vec<PathBuf>,
}

impl CreatedProjectPaths {
    fn mark_project_dir_created(&mut self, path: PathBuf) {
        self.project_dir_created = Some(path);
    }

    fn track(&mut self, path: PathBuf) {
        self.paths.push(path);
    }

    fn cleanup(&self) {
        for path in self.paths.iter().rev() {
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(path);
            } else if path.exists() {
                let _ = std::fs::remove_file(path);
            }
        }
        if let Some(dir) = &self.project_dir_created {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

fn write_created_file(
    path: &Path,
    content: &[u8],
    created_paths: &mut CreatedProjectPaths,
) -> Result<(), std::io::Error> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    created_paths.track(path.to_path_buf());
    file.write_all(content)
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    tx: mpsc::SyncSender<OutputChunk>,
    stdout: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // A bounded channel plus fixed-size reads prevents a fast child (or
        // one enormous line) from retaining unbounded output in the runner
        // while a transport send is slow.
        let mut buf = [0_u8; 8 * 1024];
        let mut utf8_pending = Vec::with_capacity(buf.len() + 3);
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let text = take_utf8_output(&mut utf8_pending, true);
                    if !text.is_empty() {
                        let _ = if stdout {
                            tx.send(OutputChunk::Stdout(text))
                        } else {
                            tx.send(OutputChunk::Stderr(text))
                        };
                    }
                    break;
                }
                Ok(read) => {
                    utf8_pending.extend_from_slice(&buf[..read]);
                    let text = take_utf8_output(&mut utf8_pending, false);
                    if !text.is_empty() {
                        let _ = if stdout {
                            tx.send(OutputChunk::Stdout(text))
                        } else {
                            tx.send(OutputChunk::Stderr(text))
                        };
                    }
                }
                Err(_) => {
                    let text = take_utf8_output(&mut utf8_pending, true);
                    if !text.is_empty() {
                        let _ = if stdout {
                            tx.send(OutputChunk::Stdout(text))
                        } else {
                            tx.send(OutputChunk::Stderr(text))
                        };
                    }
                    break;
                }
            }
        }
    })
}

/// Drain every complete UTF-8 sequence from `pending`, retaining only a
/// trailing incomplete scalar between reads. Truly invalid bytes keep the
/// runner's historical lossy-decoding behavior, but a valid scalar split at
/// the fixed read boundary is never replaced or cut.
fn take_utf8_output(pending: &mut Vec<u8>, end_of_stream: bool) -> String {
    let mut output = String::new();
    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                output.push_str(text);
                pending.clear();
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    let valid = std::str::from_utf8(&pending[..valid_up_to])
                        .expect("valid_up_to always identifies valid UTF-8");
                    output.push_str(valid);
                    pending.drain(..valid_up_to);
                }
                if let Some(error_len) = error.error_len() {
                    pending.drain(..error_len);
                    output.push('\u{fffd}');
                    continue;
                }
                if end_of_stream {
                    output.push_str(&String::from_utf8_lossy(pending));
                    pending.clear();
                }
                break;
            }
        }
    }
    output
}

fn join_reader_threads_until(mut readers: Vec<std::thread::JoinHandle<()>>, deadline: Instant) {
    loop {
        let mut index = 0;
        while index < readers.len() {
            if readers[index].is_finished() {
                let reader = readers.swap_remove(index);
                let _ = reader.join();
            } else {
                index += 1;
            }
        }
        if readers.is_empty() {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            // Dropping a JoinHandle detaches it. The output channel is bounded,
            // so an abnormal pipe holder cannot retain unbounded runner memory
            // or block process shutdown.
            return;
        }
        std::thread::sleep(Duration::from_millis(10).min(remaining));
    }
}

fn wait_failure_error(validation: bool, error: &std::io::Error) -> String {
    if validation {
        VALIDATION_STEP_WAIT_FAILED_CODE.to_string()
    } else {
        format!("failed to wait job: {error}")
    }
}

fn validation_failed_step(status: &str, error: Option<&str>, step_name: &str) -> Option<String> {
    (status == "failed"
        && error
            .and_then(validation_infrastructure_failure_code)
            .is_none())
    .then(|| step_name.to_string())
}

fn validation_module_available(
    shell: &ShellConfig,
    profile: Option<&PreparedShellProfile>,
    cwd: &Path,
    step: &ShellJobValidationStep,
    inspect_scratch: Option<&crate::command_sandbox::InspectScratch>,
    shutdown: Option<&AtomicBool>,
) -> bool {
    if step.program != "python" {
        return true;
    }
    let Some(module) = step
        .args
        .windows(2)
        .find(|p| p[0] == "-m")
        .map(|p| p[1].as_str())
    else {
        return false;
    };
    const PROBE: &str =
        "import importlib.util,sys;sys.exit(0 if importlib.util.find_spec(sys.argv[1]) else 42)";
    let args = ["-I", "-c", PROBE, module].map(str::to_string);
    let Ok(mut command) = configured_validation_job_command(shell, profile, &step.program, &args)
    else {
        return false;
    };
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(scratch) = inspect_scratch {
        if crate::command_sandbox::sandbox_command_inspect(&mut command, scratch).is_err() {
            return false;
        }
    }
    let Ok(child) = command.spawn() else {
        return false;
    };
    let child = Arc::new(Mutex::new(child));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let wait_result = {
            let mut child = lock_unpoison(&child);
            child.try_wait()
        };
        match wait_result {
            Ok(Some(status)) => {
                let success = status.success();
                let _ = kill_child_group(&child);
                return success;
            }
            Ok(None) => {
                if shutdown.is_some_and(|flag| flag.load(Ordering::SeqCst))
                    || Instant::now() >= deadline
                {
                    let _ = kill_child_group(&child);
                    return false;
                }
            }
            Err(_) => {
                let _ = kill_child_group(&child);
                return false;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn classify_process_group_signal_error(
    pgid: u32,
    signal: i32,
    error: std::io::Error,
) -> Result<bool, String> {
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Err(format!(
            "permission denied signaling process group {pgid} with signal {signal}"
        )),
        _ => Err(format!(
            "failed to signal process group {pgid} with signal {signal}: {error}"
        )),
    }
}

#[cfg(unix)]
fn signal_process_group(pgid: u32, signal: i32) -> Result<bool, String> {
    if pgid == 0 {
        return Err("process-group id 0 is invalid".to_string());
    }
    let target = i32::try_from(pgid).map_err(|_| format!("process-group id {pgid} exceeds i32"))?;
    // SAFETY: callers only pass the private process-group id of a child that
    // this JobManager launched through `setsid`.
    if unsafe { libc::kill(-target, signal) } == 0 {
        Ok(true)
    } else {
        classify_process_group_signal_error(pgid, signal, std::io::Error::last_os_error())
    }
}

fn kill_child_group(child: &Arc<Mutex<Child>>) -> Result<(), String> {
    let pid = lock_unpoison(child).id();
    #[cfg(unix)]
    {
        if pid == 0 {
            return Err("job child has invalid process-group id 0".to_string());
        }
        if !signal_process_group(pid, libc::SIGTERM)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
        // Escalate the whole group, not only the leader; a descendant may
        // ignore SIGTERM or outlive the wrapper shell.
        if signal_process_group(pid, 0)? {
            let _ = signal_process_group(pid, libc::SIGKILL)?;
        }
    }
    #[cfg(not(unix))]
    lock_unpoison(child)
        .kill()
        .map_err(|error| error.to_string())?;
    if reap_job_child_until(child, Instant::now() + Duration::from_secs(1))? {
        Ok(())
    } else {
        Err("job child did not reap within the bounded stop deadline".to_string())
    }
}

fn try_reap_job_child(child: &Arc<Mutex<Child>>) -> Result<bool, String> {
    let mut child = match child.try_lock() {
        Ok(child) => child,
        Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
    };
    match child.try_wait() {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

fn reap_job_child_until(child: &Arc<Mutex<Child>>, deadline: Instant) -> Result<bool, String> {
    loop {
        if try_reap_job_child(child)? {
            return Ok(true);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10).min(remaining));
    }
}

#[derive(Clone)]
struct JobShutdownTarget {
    child: Arc<Mutex<Child>>,
    process_group_id: Option<u32>,
}

struct JobShutdownBatch {
    targets: Vec<JobShutdownTarget>,
    running: usize,
    failures: usize,
}

#[derive(Debug, Clone, Copy)]
struct JobShutdownOutcome {
    resources: usize,
    timed_out: usize,
    failures: usize,
}

fn shutdown_target_running(target: &mut JobShutdownTarget) -> bool {
    let child_running = !matches!(try_reap_job_child(&target.child), Ok(true));
    #[cfg(unix)]
    let group_running = match target.process_group_id {
        Some(process_group_id) => match signal_process_group(process_group_id, 0) {
            Ok(true) => true,
            Ok(false) => {
                target.process_group_id = None;
                false
            }
            Err(_) => true,
        },
        None => false,
    };
    #[cfg(not(unix))]
    let group_running = false;
    child_running || group_running
}

fn shutdown_target_child_running(target: &mut JobShutdownTarget) -> bool {
    !matches!(try_reap_job_child(&target.child), Ok(true))
}

#[derive(Debug, Default)]
struct RunnerJobDelta {
    status: String,
    stdout_chunk: Option<String>,
    stderr_chunk: Option<String>,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
    error: Option<String>,
    validation_progress: Option<ShellJobValidationProgress>,
    finished: bool,
}

fn runner_job_is_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "stopped" | "timeout" | "timed_out" | "lost" | "cancelled"
    )
}

fn runner_job_is_active(status: &str) -> bool {
    matches!(status, "agent_queued" | "running" | "stop_requested")
}

fn job_update_from_snapshot(
    client_id: &str,
    agent_instance_id: &str,
    snapshot: &ShellJobSnapshot,
) -> ShellAgentJobUpdateRequest {
    ShellAgentJobUpdateRequest {
        client_id: client_id.to_string(),
        agent_instance_id: agent_instance_id.to_string(),
        job_id: snapshot.job_id.clone(),
        request_id: Some(snapshot.request_id.clone()),
        update_seq: Some(snapshot.update_seq),
        status: snapshot.status.clone(),
        stdout_chunk: None,
        stderr_chunk: None,
        stdout_tail: None,
        stderr_tail: None,
        log_snapshot: Some(ShellJobLogSnapshot {
            stdout: snapshot.stdout.clone(),
            stderr: snapshot.stderr.clone(),
        }),
        exit_code: snapshot.exit_code,
        duration_ms: snapshot.duration_ms,
        error: snapshot.error.clone(),
        validation_progress: snapshot.validation_progress.clone(),
        finished: runner_job_is_terminal(&snapshot.status),
    }
}

fn runner_retained_line_count(value: &str) -> usize {
    value.lines().count()
}

fn append_runner_stream(stream: &mut ShellJobStreamSnapshot, chunk: Option<&str>) {
    let Some(chunk) = chunk else {
        return;
    };
    stream.tail.push_str(chunk);
    if stream.tail.len() > JOB_SNAPSHOT_STREAM_MAX_BYTES {
        let observed_next = stream
            .first_retained_line
            .saturating_add(runner_retained_line_count(&stream.tail));
        let minimum_start = stream.tail.len() - JOB_SNAPSHOT_STREAM_MAX_BYTES;
        if let Some(relative_newline) = stream.tail[minimum_start..].find('\n') {
            let drop_end = minimum_start + relative_newline + 1;
            let dropped_lines = stream.tail[..drop_end]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count();
            stream.tail.drain(..drop_end);
            stream.first_retained_line = stream.first_retained_line.saturating_add(dropped_lines);
        } else {
            let mut start = minimum_start;
            while start < stream.tail.len() && !stream.tail.is_char_boundary(start) {
                start += 1;
            }
            let dropped_lines = stream.tail[..start]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count();
            stream.tail.drain(..start);
            stream.first_retained_line = stream.first_retained_line.saturating_add(dropped_lines);
        }
        if stream.tail.is_empty() {
            // The last retained partial line was dropped too. Preserve the
            // absolute next cursor by advancing the empty range to the
            // observed end rather than resetting it backwards.
            stream.first_retained_line = observed_next;
        }
        stream.truncated = true;
    }
    stream.next_line = stream
        .first_retained_line
        .saturating_add(runner_retained_line_count(&stream.tail));
}

fn trim_runner_stream_to(stream: &mut ShellJobStreamSnapshot, max_bytes: usize) {
    if stream.tail.len() <= max_bytes {
        return;
    }
    let observed_next = stream
        .first_retained_line
        .saturating_add(runner_retained_line_count(&stream.tail));
    let minimum_start = stream.tail.len().saturating_sub(max_bytes);
    if let Some(relative_newline) = stream.tail[minimum_start..].find('\n') {
        let drop_end = minimum_start + relative_newline + 1;
        let dropped_lines = stream.tail[..drop_end]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        stream.tail.drain(..drop_end);
        stream.first_retained_line = stream.first_retained_line.saturating_add(dropped_lines);
    } else {
        let mut start = minimum_start;
        while start < stream.tail.len() && !stream.tail.is_char_boundary(start) {
            start += 1;
        }
        let dropped_lines = stream.tail[..start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        stream.tail.drain(..start);
        stream.first_retained_line = stream.first_retained_line.saturating_add(dropped_lines);
    }
    if stream.tail.is_empty() {
        stream.first_retained_line = observed_next;
    }
    stream.truncated = true;
    stream.next_line = stream
        .first_retained_line
        .saturating_add(runner_retained_line_count(&stream.tail));
}

fn bounded_runner_error(error: Option<String>) -> Option<String> {
    error.map(|error| error.chars().take(4_096).collect())
}

fn validate_runner_job_context(
    context: &ShellJobContext,
    request: &ShellAgentShellRequest,
    client_id: &str,
) -> Result<(), String> {
    const MAX_CONTEXT_FIELD_CHARS: usize = 1_024;
    const MAX_COMMAND_PREVIEW_CHARS: usize = 121;
    let bounded =
        |value: &str, max_chars: usize| !value.contains('\0') && value.chars().count() <= max_chars;
    if !bounded(&context.command_preview, MAX_COMMAND_PREVIEW_CHARS)
        || context.command_preview.contains(['\r', '\n'])
    {
        return Err("job recovery context command_preview is invalid or oversized".to_string());
    }
    for (name, value) in [
        ("project_cwd", context.project_cwd.as_deref()),
        ("cwd", context.cwd.as_deref()),
        ("purpose", context.purpose.as_deref()),
        ("shell", context.shell.as_deref()),
    ] {
        if value.is_some_and(|value| !bounded(value, MAX_CONTEXT_FIELD_CHARS)) {
            return Err(format!(
                "job recovery context {name} is invalid or oversized"
            ));
        }
    }
    if context.cwd != request.cwd {
        return Err("job recovery context cwd does not match the execution request".to_string());
    }
    if context.purpose.as_deref().is_some_and(|purpose| {
        !matches!(
            purpose,
            "validation"
                | "test"
                | "build"
                | "format"
                | "release"
                | "diagnostic"
                | "operation"
                | "other"
        )
    }) {
        return Err("job recovery context purpose is invalid".to_string());
    }
    if context
        .shell
        .as_deref()
        .is_some_and(|shell| !matches!(shell, "sh" | "bash" | "configured" | "custom"))
    {
        return Err("job recovery context shell is invalid".to_string());
    }
    let validation_context = request.kind == "start_validation_job";
    if (validation_context && !(1..=3).contains(&context.validation_steps.len()))
        || (!validation_context && !context.validation_steps.is_empty())
        || context
            .validation_steps
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != context.validation_steps.len()
        || context
            .validation_steps
            .iter()
            .any(|step| !matches!(step.as_str(), "format" | "check" | "test"))
    {
        return Err("job recovery context validation_steps are invalid".to_string());
    }
    if let Some(project_id) = context.runtime_project_id.as_deref() {
        let prefix = format!("agent:{client_id}:");
        if !bounded(project_id, MAX_CONTEXT_FIELD_CHARS)
            || project_id
                .strip_prefix(&prefix)
                .is_none_or(|suffix| suffix.is_empty())
        {
            return Err(
                "job recovery context runtime_project_id does not match the runner".to_string(),
            );
        }
    }
    if let Some(session_id) = context.workflow_session_id.as_deref() {
        if context.runtime_project_id.is_none()
            || session_id.len() > 128
            || !session_id.starts_with("wc_sess_")
            || !session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err("job recovery context workflow_session_id is invalid".to_string());
        }
    }
    Ok(())
}

impl JobManager {
    fn install_sink(&self, sink: AgentSink) {
        *lock_unpoison(&self.current_sink) = Some(sink);
    }

    fn current_sink(&self) -> Option<AgentSink> {
        lock_unpoison(&self.current_sink).clone()
    }

    fn send_recorded_update(&self, update: ShellAgentJobUpdateRequest) {
        let Some(sink) = self.current_sink() else {
            return;
        };
        let _ = sink.send_job_update(&update);
    }

    fn record_update(
        &self,
        job_id: &str,
        mut delta: RunnerJobDelta,
    ) -> Option<ShellAgentJobUpdateRequest> {
        let update = {
            let mut jobs = lock_unpoison(&self.jobs);
            let job = jobs.get_mut(job_id)?;
            if runner_job_is_terminal(&job.snapshot.status) {
                // The first locally observed terminal outcome is immutable.
                // In particular, a racing stop request or late output poll
                // must not revive a handle-free retained record.
                return None;
            }
            let now = chrono::Utc::now().timestamp();
            append_runner_stream(&mut job.snapshot.stdout, delta.stdout_chunk.as_deref());
            append_runner_stream(&mut job.snapshot.stderr, delta.stderr_chunk.as_deref());
            job.snapshot.update_seq = job.snapshot.update_seq.saturating_add(1);
            if !delta.status.trim().is_empty() {
                let incoming_status = delta.status.trim();
                let would_regress_stop = job.snapshot.status == "stop_requested"
                    && matches!(incoming_status, "agent_queued" | "running");
                let would_regress_running =
                    job.snapshot.status == "running" && incoming_status == "agent_queued";
                if !would_regress_stop && !would_regress_running {
                    job.snapshot.status = incoming_status.to_string();
                }
            }
            if job.snapshot.started_at.is_none()
                && matches!(
                    job.snapshot.status.as_str(),
                    "running"
                        | "completed"
                        | "failed"
                        | "stopped"
                        | "timeout"
                        | "timed_out"
                        | "cancelled"
                )
            {
                job.snapshot.started_at = Some(now);
            }
            if delta.validation_progress.is_some() {
                job.snapshot.validation_progress = delta.validation_progress.clone();
            }
            if runner_job_is_terminal(&job.snapshot.status) || delta.finished {
                job.snapshot.ended_at.get_or_insert(now);
                job.snapshot.exit_code = delta.exit_code;
                job.snapshot.duration_ms = delta.duration_ms;
                job.snapshot.error = bounded_runner_error(delta.error.take());
                job.child = None;
                job.process_group_id = None;
                job.slot_reserved = false;
            } else if delta.error.is_some() {
                job.snapshot.error = bounded_runner_error(delta.error.take());
            }
            // Each sequenced update carries the current authoritative bounded
            // tails. If transport calls complete out of order, a higher
            // sequence still contains every retained byte visible to the
            // lower one, so ignoring stale updates cannot lose or duplicate
            // output.
            job_update_from_snapshot(&job.client_id, &job.agent_instance_id, &job.snapshot)
        };
        self.prune_terminal_records();
        Some(update)
    }

    fn update_and_send(&self, job_id: &str, delta: RunnerJobDelta) {
        if let Some(update) = self.record_update(job_id, delta) {
            self.send_recorded_update(update);
        }
    }

    fn replay_snapshots_since(&self, registered: &ShellJobInventory) {
        let Some(sink) = self.current_sink() else {
            return;
        };
        let registered_sequences = registered
            .jobs
            .iter()
            .map(|snapshot| (snapshot.job_id.as_str(), snapshot.update_seq))
            .collect::<std::collections::HashMap<_, _>>();
        for snapshot in self.inventory().jobs.into_iter().filter(|snapshot| {
            registered_sequences
                .get(snapshot.job_id.as_str())
                .is_none_or(|sequence| snapshot.update_seq > *sequence)
        }) {
            let update =
                job_update_from_snapshot(sink.client_id(), sink.agent_instance_id(), &snapshot);
            let _ = sink.send_job_update(&update);
        }
    }

    fn resend_snapshot(&self, job_id: &str) {
        let update = lock_unpoison(&self.jobs).get(job_id).map(|job| {
            job_update_from_snapshot(&job.client_id, &job.agent_instance_id, &job.snapshot)
        });
        if let Some(update) = update {
            self.send_recorded_update(update);
        }
    }

    fn fail_job(
        &self,
        request: &ShellAgentShellRequest,
        error: String,
        validation_progress: Option<ShellJobValidationProgress>,
    ) {
        let Some(job_id) = request.job_id.as_deref() else {
            return;
        };
        self.update_and_send(
            job_id,
            RunnerJobDelta {
                status: "failed".to_string(),
                duration_ms: Some(0),
                error: Some(error),
                validation_progress,
                finished: true,
                ..Default::default()
            },
        );
        self.start_available_queued();
    }

    fn prune_terminal_records(&self) {
        let now = chrono::Utc::now().timestamp();
        let mut jobs = lock_unpoison(&self.jobs);
        let expired = jobs
            .iter()
            .filter_map(|(job_id, job)| {
                (runner_job_is_terminal(&job.snapshot.status)
                    && job.snapshot.ended_at.is_some_and(|ended| {
                        now.saturating_sub(ended) >= JOB_TERMINAL_RETENTION_SECS
                    }))
                .then(|| job_id.clone())
            })
            .collect::<Vec<_>>();
        for job_id in expired {
            jobs.remove(&job_id);
        }
        let mut terminal = jobs
            .iter()
            .filter(|(_, job)| runner_job_is_terminal(&job.snapshot.status))
            .map(|(job_id, job)| {
                (
                    job_id.clone(),
                    job.snapshot.ended_at.unwrap_or(job.snapshot.created_at),
                )
            })
            .collect::<Vec<_>>();
        terminal.sort_by_key(|(_, ended_at)| *ended_at);
        let excess = terminal
            .len()
            .saturating_sub(JOB_INVENTORY_MAX_TERMINAL_JOBS);
        for (job_id, _) in terminal.into_iter().take(excess) {
            jobs.remove(&job_id);
        }
    }

    fn inventory(&self) -> ShellJobInventory {
        self.prune_terminal_records();
        let jobs = lock_unpoison(&self.jobs);
        let mut active = jobs
            .values()
            .filter(|job| runner_job_is_active(&job.snapshot.status))
            .map(|job| job.snapshot.clone())
            .collect::<Vec<_>>();
        let mut terminal = jobs
            .values()
            .filter(|job| runner_job_is_terminal(&job.snapshot.status))
            .map(|job| job.snapshot.clone())
            .collect::<Vec<_>>();
        drop(jobs);
        active.sort_by_key(|snapshot| snapshot.created_at);
        terminal.sort_by(|left, right| {
            right
                .ended_at
                .unwrap_or(right.created_at)
                .cmp(&left.ended_at.unwrap_or(left.created_at))
        });
        terminal.truncate(JOB_INVENTORY_MAX_TERMINAL_JOBS);
        let mut inventory = ShellJobInventory {
            active_complete: true,
            jobs: active,
        };

        // Active records are never omitted. Only when active records alone
        // exceed the frame budget do their authoritative tails shrink.
        let mut tail_limit = JOB_SNAPSHOT_STREAM_MAX_BYTES;
        while serde_json::to_vec(&inventory)
            .map(|bytes| bytes.len() > JOB_INVENTORY_MAX_SERIALIZED_BYTES)
            .unwrap_or(true)
            && tail_limit > 0
        {
            tail_limit /= 2;
            for snapshot in &mut inventory.jobs {
                trim_runner_stream_to(&mut snapshot.stdout, tail_limit);
                trim_runner_stream_to(&mut snapshot.stderr, tail_limit);
            }
        }

        // Add newest terminal history only while it fits. Serializing each
        // record once avoids repeatedly encoding a multi-megabyte inventory
        // while preserving the newest-first eviction rule.
        let mut serialized_len = serde_json::to_vec(&inventory)
            .map(|bytes| bytes.len())
            .unwrap_or(JOB_INVENTORY_MAX_SERIALIZED_BYTES.saturating_add(1));
        for snapshot in terminal {
            let Ok(encoded) = serde_json::to_vec(&snapshot) else {
                continue;
            };
            let separator = usize::from(!inventory.jobs.is_empty());
            let added = encoded.len().saturating_add(separator);
            if serialized_len.saturating_add(added) > JOB_INVENTORY_MAX_SERIALIZED_BYTES {
                break;
            }
            serialized_len = serialized_len.saturating_add(added);
            inventory.jobs.push(snapshot);
        }
        inventory
    }

    fn has_work(&self) -> bool {
        lock_unpoison(&self.jobs)
            .values()
            .any(|job| runner_job_is_active(&job.snapshot.status))
            || !lock_unpoison(&self.queued).is_empty()
    }

    fn stop_accepting_work(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    fn cancel_queued_for_shutdown(&self) -> usize {
        let _lifecycle = lock_unpoison(&self.lifecycle);
        self.shutting_down.store(true, Ordering::SeqCst);
        let mut queued = lock_unpoison(&self.queued);
        let cancelled = queued.len();
        queued.clear();
        cancelled
    }

    fn signal_all_for_shutdown(&self) -> JobShutdownBatch {
        let running = {
            let jobs = lock_unpoison(&self.jobs);
            jobs.iter()
                .filter(|(_, job)| runner_job_is_active(&job.snapshot.status))
                .map(|(_, job)| {
                    (
                        job.child.clone(),
                        job.process_group_id,
                        Arc::clone(&job.stop_requested),
                    )
                })
                .collect::<Vec<_>>()
        };
        let running_count = running.len();
        let mut targets = Vec::with_capacity(running.len());
        let mut failures = 0;
        for (child, process_group_id, stop_requested) in running {
            stop_requested.store(true, Ordering::SeqCst);
            let Some(child) = child else {
                continue;
            };
            #[cfg(unix)]
            if let Some(process_group_id) = process_group_id {
                if signal_process_group(process_group_id, libc::SIGTERM).is_err() {
                    failures += 1;
                }
            }
            #[cfg(not(unix))]
            if lock_unpoison(&child).kill().is_err() {
                failures += 1;
            }
            targets.push(JobShutdownTarget {
                child,
                process_group_id,
            });
        }
        JobShutdownBatch {
            running: running_count,
            targets,
            failures,
        }
    }

    fn drain_shutdown(&self, mut batch: JobShutdownBatch, deadline: Instant) -> JobShutdownOutcome {
        const TERM_GRACE: Duration = Duration::from_millis(500);
        let resources = batch.targets.len();
        let grace_deadline = deadline.min(Instant::now() + TERM_GRACE);
        while Instant::now() < grace_deadline {
            if batch
                .targets
                .iter_mut()
                .all(|target| !shutdown_target_running(target))
            {
                break;
            }
            let remaining = grace_deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(Duration::from_millis(10).min(remaining));
        }

        for target in &mut batch.targets {
            #[cfg(unix)]
            if let Some(process_group_id) = target.process_group_id {
                if signal_process_group(process_group_id, libc::SIGKILL).is_err() {
                    batch.failures += 1;
                }
            }
            #[cfg(not(unix))]
            if shutdown_target_child_running(target) && lock_unpoison(&target.child).kill().is_err()
            {
                batch.failures += 1;
            }
        }

        while Instant::now() < deadline {
            if batch
                .targets
                .iter_mut()
                .all(|target| !shutdown_target_child_running(target))
            {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(Duration::from_millis(10).min(remaining));
        }
        let mut timed_out = 0;
        for target in &mut batch.targets {
            timed_out += usize::from(shutdown_target_child_running(target));
        }
        JobShutdownOutcome {
            resources,
            timed_out,
            failures: batch.failures,
        }
    }

    #[cfg(test)]
    fn stop_all(&self) {
        self.stop_accepting_work();
        self.cancel_queued_for_shutdown();
        let batch = self.signal_all_for_shutdown();
        let outcome = self.drain_shutdown(batch, Instant::now() + Duration::from_secs(2));
        if outcome.timed_out > 0 || outcome.failures > 0 {
            eprintln!(
                "webcodex-runner shutdown job cleanup incomplete resources={} timed_out={} failures={}",
                outcome.resources, outcome.timed_out, outcome.failures
            );
        }
    }

    fn wait_for_workers(&self, deadline: Instant) -> bool {
        self.workers.wait_until(deadline)
    }

    fn worker_count(&self) -> usize {
        self.workers.active()
    }

    fn shutdown_rejection(&self, request: &ShellAgentShellRequest) {
        self.fail_job(request, "runner is shutting down".to_string(), None);
    }

    fn enqueue(
        &self,
        sink: AgentSink,
        generation: u64,
        policy: AgentPolicy,
        shell: ShellConfig,
        projects_dir: PathBuf,
        request: ShellAgentShellRequest,
    ) {
        let Some(job_id) = request.job_id.clone() else {
            return;
        };
        let Some(context) = request.job_context.clone() else {
            let _ = sink.send_job_update(&ShellAgentJobUpdateRequest {
                client_id: sink.client_id().to_string(),
                agent_instance_id: sink.agent_instance_id().to_string(),
                job_id,
                request_id: Some(request.request_id),
                update_seq: Some(1),
                status: "failed".to_string(),
                stdout_chunk: None,
                stderr_chunk: None,
                stdout_tail: None,
                stderr_tail: None,
                log_snapshot: None,
                exit_code: None,
                duration_ms: Some(0),
                error: Some("job start request is missing recovery context".to_string()),
                validation_progress: None,
                finished: true,
            });
            return;
        };
        if let Err(error) = validate_runner_job_context(&context, &request, sink.client_id()) {
            let _ = sink.send_job_update(&ShellAgentJobUpdateRequest {
                client_id: sink.client_id().to_string(),
                agent_instance_id: sink.agent_instance_id().to_string(),
                job_id,
                request_id: Some(request.request_id),
                update_seq: Some(1),
                status: "failed".to_string(),
                stdout_chunk: None,
                stderr_chunk: None,
                stdout_tail: None,
                stderr_tail: None,
                log_snapshot: None,
                exit_code: None,
                duration_ms: Some(0),
                error: Some(error),
                validation_progress: None,
                finished: true,
            });
            return;
        }
        self.install_sink(sink.clone());
        let client_id = sink.client_id().to_string();
        let agent_instance_id = sink.agent_instance_id().to_string();
        let (queue_locally, immediate_failure) = {
            let _lifecycle = lock_unpoison(&self.lifecycle);
            let shutting_down = self.shutting_down.load(Ordering::SeqCst);
            let mut jobs = lock_unpoison(&self.jobs);
            if jobs.contains_key(&job_id) {
                return;
            }
            let active_count = jobs
                .values()
                .filter(|job| runner_job_is_active(&job.snapshot.status))
                .count();
            let reserved = jobs
                .values()
                .filter(|job| {
                    job.client_id == client_id
                        && job.slot_reserved
                        && runner_job_is_active(&job.snapshot.status)
                })
                .count();
            let inventory_full = active_count >= JOB_INVENTORY_MAX_ACTIVE_JOBS;
            let immediate_failure = if inventory_full {
                Some(format!(
                    "runner active job inventory limit reached ({})",
                    JOB_INVENTORY_MAX_ACTIVE_JOBS
                ))
            } else if shutting_down {
                Some("runner is shutting down".to_string())
            } else {
                None
            };
            let queue_locally = immediate_failure.is_none() && reserved >= self.max_concurrent;
            let slot_reserved = immediate_failure.is_none() && !queue_locally;
            let now = chrono::Utc::now().timestamp();
            let terminal = immediate_failure.is_some();
            jobs.insert(
                job_id.clone(),
                RunningJob {
                    client_id: client_id.clone(),
                    agent_instance_id,
                    snapshot: ShellJobSnapshot {
                        job_id: job_id.clone(),
                        request_id: request.request_id.clone(),
                        status: if terminal {
                            "failed".to_string()
                        } else {
                            "agent_queued".to_string()
                        },
                        update_seq: u64::from(terminal),
                        created_at: request.created_at,
                        started_at: None,
                        ended_at: terminal.then_some(now),
                        exit_code: None,
                        duration_ms: terminal.then_some(0),
                        error: immediate_failure.clone(),
                        context,
                        stdout: ShellJobStreamSnapshot::default(),
                        stderr: ShellJobStreamSnapshot::default(),
                        validation_progress: None,
                    },
                    child: None,
                    process_group_id: None,
                    stop_requested: Arc::new(AtomicBool::new(false)),
                    slot_reserved,
                },
            );
            drop(jobs);
            if queue_locally {
                lock_unpoison(&self.queued).push_back((
                    sink.clone(),
                    generation,
                    policy.clone(),
                    shell.clone(),
                    projects_dir.clone(),
                    request.clone(),
                ));
            }
            (queue_locally, immediate_failure)
        };
        if let Some(error) = immediate_failure {
            debug_assert!(!error.is_empty());
            self.resend_snapshot(&job_id);
            self.prune_terminal_records();
            return;
        }
        self.update_and_send(
            &job_id,
            RunnerJobDelta {
                status: "agent_queued".to_string(),
                ..Default::default()
            },
        );
        if queue_locally {
            return;
        }
        self.start_now(sink, generation, policy, shell, projects_dir, request);
    }

    fn start_now(
        &self,
        sink: AgentSink,
        generation: u64,
        policy: AgentPolicy,
        shell: ShellConfig,
        projects_dir: PathBuf,
        request: ShellAgentShellRequest,
    ) {
        if self.shutting_down.load(Ordering::SeqCst) {
            self.shutdown_rejection(&request);
            return;
        }
        self.start_shell_job(sink, generation, policy, shell, projects_dir, request);
    }

    fn start_available_queued(&self) {
        loop {
            if self.shutting_down.load(Ordering::SeqCst) {
                lock_unpoison(&self.queued).clear();
                return;
            }
            let next = {
                let _lifecycle = lock_unpoison(&self.lifecycle);
                if self.shutting_down.load(Ordering::SeqCst) {
                    lock_unpoison(&self.queued).clear();
                    return;
                }
                let mut jobs = lock_unpoison(&self.jobs);
                let mut queued = lock_unpoison(&self.queued);
                let mut selected = None;
                for (idx, (_, _, _policy, _shell, _projects_dir, request)) in
                    queued.iter().enumerate()
                {
                    let reserved = jobs
                        .values()
                        .filter(|job| {
                            job.client_id == request.client_id
                                && job.slot_reserved
                                && runner_job_is_active(&job.snapshot.status)
                        })
                        .count();
                    if reserved < self.max_concurrent {
                        selected = Some(idx);
                        break;
                    }
                }
                if let Some(idx) = selected {
                    if let Some(job_id) = queued[idx].5.job_id.as_deref() {
                        if let Some(job) = jobs.get_mut(job_id) {
                            job.slot_reserved = true;
                        }
                    }
                    queued.remove(idx)
                } else {
                    None
                }
            };
            let Some((sink, generation, policy, shell, projects_dir, request)) = next else {
                return;
            };
            self.start_now(sink, generation, policy, shell, projects_dir, request);
        }
    }

    fn start_shell_job(
        &self,
        _sink: AgentSink,
        generation: u64,
        policy: AgentPolicy,
        shell: ShellConfig,
        projects_dir: PathBuf,
        request: ShellAgentShellRequest,
    ) {
        let Some(job_id) = request.job_id.clone() else {
            return;
        };
        if !policy.allow_raw_shell {
            self.fail_job(
                &request,
                "raw shell is disabled by local agent policy".to_string(),
                None,
            );
            return;
        }
        let cwd_path = request
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
        if let Err(e) = cwd_allowed(&policy, &cwd_path) {
            self.fail_job(&request, e, None);
            return;
        }
        let validation = request.kind == "start_validation_job";
        let steps = if validation {
            match serde_json::from_str::<Vec<ShellJobValidationStep>>(&request.command) {
                Ok(steps)
                    if (1..=3).contains(&steps.len())
                        && steps.iter().all(ShellJobValidationStep::is_canonical)
                        && steps.iter().enumerate().all(|(index, step)| {
                            !steps[..index]
                                .iter()
                                .any(|earlier| earlier.name == step.name)
                        }) =>
                {
                    steps
                }
                _ => {
                    self.fail_job(
                        &request,
                        "invalid structured validation plan".to_string(),
                        None,
                    );
                    return;
                }
            }
        } else {
            Vec::new()
        };
        if validation
            && request.job_context.as_ref().is_none_or(|context| {
                context.validation_steps
                    != steps
                        .iter()
                        .map(|step| step.name.clone())
                        .collect::<Vec<_>>()
            })
        {
            self.fail_job(
                &request,
                "structured validation plan does not match recovery context".to_string(),
                Some(ShellJobValidationProgress {
                    completed: 0,
                    current_step: None,
                    failed_step: None,
                }),
            );
            return;
        }
        let sandbox_mode = request.sandbox.clone();
        let inspect_scratch = match sandbox_mode.as_deref() {
            None => None,
            Some(crate::command_sandbox::INSPECT_SANDBOX_MODE) => {
                match crate::command_sandbox::InspectScratch::create() {
                    Ok(scratch) => Some(scratch),
                    Err(error) => {
                        self.fail_job(
                            &request,
                            format!("inspect sandbox unavailable: {error}"),
                            None,
                        );
                        return;
                    }
                }
            }
            Some(other) => {
                self.fail_job(&request, format!("unknown sandbox mode '{other}'"), None);
                return;
            }
        };
        // Profile preparation runs an init script. Inspect execution bypasses
        // that unsandboxed preparation and uses the base shell environment;
        // the actual command (and global init script, if any) is sandboxed.
        let prepared_profile = match inspect_scratch.as_ref() {
            Some(_) => None,
            None => match resolve_prepared_shell_profile(
                generation,
                &shell,
                &projects_dir,
                &cwd_path,
                request.cwd.is_some(),
                &self.prepared_profiles,
                Some(self.shutting_down.as_ref()),
            ) {
                Ok(profile) => profile,
                Err(e) => {
                    self.fail_job(&request, e, None);
                    return;
                }
            },
        };
        if validation
            && steps.iter().any(|step| {
                !validation_module_available(
                    &shell,
                    prepared_profile.as_deref(),
                    &cwd_path,
                    step,
                    inspect_scratch.as_ref(),
                    Some(self.shutting_down.as_ref()),
                )
            })
        {
            self.fail_job(
                &request,
                VALIDATION_TOOL_UNAVAILABLE_CODE.to_string(),
                Some(ShellJobValidationProgress {
                    completed: 0,
                    current_step: None,
                    failed_step: None,
                }),
            );
            return;
        }
        let step_count = if validation { steps.len() } else { 1 };
        let mut commands = VecDeque::with_capacity(step_count);
        for index in 0..step_count {
            let configured = if validation {
                configured_validation_job_command(
                    &shell,
                    prepared_profile.as_deref(),
                    &steps[index].program,
                    &steps[index].args,
                )
            } else {
                match prepared_profile.as_deref() {
                    Some(profile) => {
                        configured_prepared_shell_job_command(profile, &request.command)
                    }
                    None => configured_shell_job_command(&shell, &request.command),
                }
            };
            let mut command = match configured {
                Ok(command) => command,
                Err(error) => {
                    self.fail_job(&request, error, None);
                    return;
                }
            };
            if validation {
                command.envs(
                    steps[index]
                        .env
                        .iter()
                        .map(|(key, value)| (key.as_str(), value.as_str())),
                );
            }
            if let Some(scratch) = inspect_scratch.as_ref() {
                if let Err(error) =
                    crate::command_sandbox::sandbox_command_inspect(&mut command, scratch)
                {
                    self.fail_job(
                        &request,
                        format!("inspect sandbox unavailable: {error}"),
                        None,
                    );
                    return;
                }
            }
            command
                .current_dir(&cwd_path)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            commands.push_back(command);
        }
        let stop_requested = {
            let _lifecycle = lock_unpoison(&self.lifecycle);
            if self.shutting_down.load(Ordering::SeqCst) {
                None
            } else {
                let mut jobs = lock_unpoison(&self.jobs);
                let Some(job) = jobs.get_mut(&job_id) else {
                    return;
                };
                job.slot_reserved = true;
                Some(Arc::clone(&job.stop_requested))
            }
        };
        let Some(stop_requested) = stop_requested else {
            self.shutdown_rejection(&request);
            return;
        };
        let start = Instant::now();
        let spawn = commands
            .pop_front()
            .expect("validated non-empty plan")
            .spawn();
        let mut child = match spawn {
            Ok(c) => c,
            Err(e) => {
                if validation {
                    self.fail_job(
                        &request,
                        VALIDATION_STEP_SPAWN_FAILED_CODE.to_string(),
                        Some(ShellJobValidationProgress {
                            completed: 0,
                            current_step: None,
                            failed_step: None,
                        }),
                    );
                } else {
                    let error = prepared_profile
                        .as_ref()
                        .map(|profile_name| {
                            format!(
                                "failed to spawn shell profile '{}': {}",
                                profile_name.profile_name, e
                            )
                        })
                        .unwrap_or_else(|| format!("failed to spawn command: {}", e));
                    self.fail_job(&request, error, None);
                }
                return;
            }
        };
        let process_group_id = child.id();
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let mut child = Arc::new(Mutex::new(child));
        let reject_for_shutdown = {
            let _lifecycle = lock_unpoison(&self.lifecycle);
            if self.shutting_down.load(Ordering::SeqCst) || stop_requested.load(Ordering::SeqCst) {
                true
            } else if let Some(job) = lock_unpoison(&self.jobs).get_mut(&job_id) {
                job.child = Some(child.clone());
                job.process_group_id = Some(process_group_id);
                false
            } else {
                true
            }
        };
        if reject_for_shutdown {
            let _ = kill_child_group(&child);
            self.shutdown_rejection(&request);
            return;
        }
        self.update_and_send(
            &job_id,
            RunnerJobDelta {
                status: "running".to_string(),
                validation_progress: validation.then(|| ShellJobValidationProgress {
                    completed: 0,
                    current_step: Some(steps[0].name.clone()),
                    failed_step: None,
                }),
                ..Default::default()
            },
        );
        let jobs = self.jobs.clone();
        let lifecycle = Arc::clone(&self.lifecycle);
        let shutting_down = Arc::clone(&self.shutting_down);
        let manager = self.clone();
        let inspect_scratch_guard = inspect_scratch;
        let worker_guard = self.workers.enter();
        std::thread::spawn(move || {
            let _worker_guard = worker_guard;
            // Keep the private writable directory alive for every process in
            // the job, then clean it when the terminal update has been sent.
            let _inspect_scratch_guard = inspect_scratch_guard;
            let timeout_secs = request.timeout_secs.min(policy.max_timeout_secs).max(1);
            let mut step_index = 0;
            let (final_status, out, err, final_progress) = loop {
                const OUTPUT_CHANNEL_CAPACITY: usize = 64;
                let (tx, rx) = mpsc::sync_channel::<OutputChunk>(OUTPUT_CHANNEL_CAPACITY);
                let mut readers = Vec::new();
                if let Some(stdout) = stdout {
                    readers.push(spawn_reader(stdout, tx.clone(), true));
                }
                if let Some(stderr) = stderr {
                    readers.push(spawn_reader(stderr, tx.clone(), false));
                }
                drop(tx);
                let step_status = loop {
                    let mut out = String::new();
                    let mut err = String::new();
                    while let Ok(chunk) = rx.try_recv() {
                        match chunk {
                            OutputChunk::Stdout(text) => out.push_str(&text),
                            OutputChunk::Stderr(text) => err.push_str(&text),
                        }
                    }
                    if !out.is_empty() || !err.is_empty() {
                        manager.update_and_send(
                            &job_id,
                            RunnerJobDelta {
                                status: "running".to_string(),
                                stdout_chunk: (!out.is_empty()).then_some(out),
                                stderr_chunk: (!err.is_empty()).then_some(err),
                                validation_progress: validation.then(|| {
                                    ShellJobValidationProgress {
                                        completed: step_index,
                                        current_step: Some(steps[step_index].name.clone()),
                                        failed_step: None,
                                    }
                                }),
                                ..Default::default()
                            },
                        );
                    }
                    let wait_result = {
                        let mut child = lock_unpoison(&child);
                        child.try_wait()
                    };
                    match wait_result {
                        Ok(Some(status)) => {
                            let stopped = stop_requested.load(Ordering::SeqCst);
                            break (
                                if stopped {
                                    "stopped"
                                } else if status.success() {
                                    "completed"
                                } else {
                                    "failed"
                                }
                                .to_string(),
                                Some(status.code().unwrap_or(-1)),
                                if stopped {
                                    Some("job stopped by request".to_string())
                                } else {
                                    None
                                },
                            );
                        }
                        Ok(None) => {
                            if stop_requested.load(Ordering::SeqCst) {
                                let _ = kill_child_group(&child);
                                break (
                                    "stopped".to_string(),
                                    Some(-1),
                                    Some("job stopped by request".to_string()),
                                );
                            }
                            if start.elapsed() >= Duration::from_secs(timeout_secs) {
                                stop_requested.store(true, Ordering::SeqCst);
                                let _ = kill_child_group(&child);
                                break (
                                    "timeout".to_string(),
                                    Some(-1),
                                    Some(format!("job timed out after {} seconds", timeout_secs)),
                                );
                            }
                        }
                        Err(e) => {
                            // The host lost track of a process it started.
                            // For a validation job that must arrive as a
                            // machine-readable infrastructure code: the step
                            // did not fail, its outcome is simply unknown,
                            // and saying "check failed" would blame the
                            // project for the executor's problem.
                            eprintln!("webcodex-runner failed to wait job {job_id}: {e}");
                            break (
                                "failed".to_string(),
                                None,
                                Some(wait_failure_error(validation, &e)),
                            );
                        }
                    }
                    std::thread::sleep(Duration::from_millis(JOB_UPDATE_INTERVAL_MS));
                };
                // A direct child can exit while a background descendant keeps
                // stdout/stderr open. Terminate the private group before the
                // bounded reader join so cleanup cannot wait forever on EOF.
                let _ = kill_child_group(&child);
                join_reader_threads_until(readers, Instant::now() + Duration::from_secs(1));
                let mut out = String::new();
                let mut err = String::new();
                while let Ok(chunk) = rx.try_recv() {
                    match chunk {
                        OutputChunk::Stdout(text) => out.push_str(&text),
                        OutputChunk::Stderr(text) => err.push_str(&text),
                    }
                }
                if step_status.0 == "completed" && step_index + 1 < step_count {
                    step_index += 1;
                    if stop_requested.load(Ordering::SeqCst) {
                        break (
                            (
                                "stopped".to_string(),
                                Some(-1),
                                Some("job stopped by request".to_string()),
                            ),
                            out,
                            err,
                            validation.then(|| ShellJobValidationProgress {
                                completed: step_index,
                                current_step: None,
                                failed_step: None,
                            }),
                        );
                    }
                    {
                        let _lifecycle_guard = lock_unpoison(&lifecycle);
                        if shutting_down.load(Ordering::SeqCst)
                            || stop_requested.load(Ordering::SeqCst)
                        {
                            break (
                                (
                                    "stopped".to_string(),
                                    Some(-1),
                                    Some("job stopped by request".to_string()),
                                ),
                                out,
                                err,
                                validation.then(|| ShellJobValidationProgress {
                                    completed: step_index,
                                    current_step: None,
                                    failed_step: None,
                                }),
                            );
                        }
                    }
                    let spawn = commands
                        .pop_front()
                        .expect("one command per validation step")
                        .spawn();
                    let mut next = match spawn {
                        Ok(child) => child,
                        Err(_error) => {
                            break (
                                (
                                    "failed".to_string(),
                                    None,
                                    Some(VALIDATION_STEP_SPAWN_FAILED_CODE.to_string()),
                                ),
                                out,
                                err,
                                validation.then(|| ShellJobValidationProgress {
                                    completed: step_index,
                                    current_step: None,
                                    failed_step: None,
                                }),
                            )
                        }
                    };
                    let next_stdout = next.stdout.take();
                    let next_stderr = next.stderr.take();
                    let process_group_id = next.id();
                    let next = Arc::new(Mutex::new(next));
                    let reject_for_shutdown = {
                        let _lifecycle_guard = lock_unpoison(&lifecycle);
                        if shutting_down.load(Ordering::SeqCst)
                            || stop_requested.load(Ordering::SeqCst)
                        {
                            true
                        } else if let Some(job) = lock_unpoison(&jobs).get_mut(&job_id) {
                            job.child = Some(Arc::clone(&next));
                            job.process_group_id = Some(process_group_id);
                            false
                        } else {
                            true
                        }
                    };
                    if reject_for_shutdown {
                        let _ = kill_child_group(&next);
                        break (
                            (
                                "stopped".to_string(),
                                Some(-1),
                                Some("job stopped by request".to_string()),
                            ),
                            out,
                            err,
                            validation.then(|| ShellJobValidationProgress {
                                completed: step_index,
                                current_step: None,
                                failed_step: None,
                            }),
                        );
                    }
                    child = next;
                    manager.update_and_send(
                        &job_id,
                        RunnerJobDelta {
                            status: "running".to_string(),
                            stdout_chunk: (!out.is_empty()).then_some(out),
                            stderr_chunk: (!err.is_empty()).then_some(err),
                            validation_progress: validation.then(|| ShellJobValidationProgress {
                                completed: step_index,
                                current_step: Some(steps[step_index].name.clone()),
                                failed_step: None,
                            }),
                            ..Default::default()
                        },
                    );
                    stdout = next_stdout;
                    stderr = next_stderr;
                    continue;
                }
                let progress = validation.then(|| ShellJobValidationProgress {
                    completed: if step_status.0 == "completed" {
                        steps.len()
                    } else {
                        step_index
                    },
                    current_step: None,
                    // An infrastructure code names no failed step: the
                    // connector reads `failed_step` as "this check rejected
                    // the work", which is exactly what did not happen.
                    failed_step: validation_failed_step(
                        &step_status.0,
                        step_status.2.as_deref(),
                        &steps[step_index].name,
                    ),
                });
                break (step_status, out, err, progress);
            };
            manager.update_and_send(
                &job_id,
                RunnerJobDelta {
                    status: final_status.0,
                    stdout_chunk: (!out.is_empty()).then_some(out),
                    stderr_chunk: (!err.is_empty()).then_some(err),
                    exit_code: final_status.1,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: final_status.2,
                    validation_progress: final_progress,
                    finished: true,
                    ..Default::default()
                },
            );
            manager.start_available_queued();
        });
    }

    fn stop(&self, job_id: &str) -> Result<(), String> {
        let queued_job = {
            let _lifecycle = lock_unpoison(&self.lifecycle);
            let mut queued = lock_unpoison(&self.queued);
            if let Some(pos) = queued
                .iter()
                .position(|(_, _, _, _, _, request)| request.job_id.as_deref() == Some(job_id))
            {
                queued.remove(pos)
            } else {
                None
            }
        };
        if let Some((_sink, _generation, _policy, _shell, _projects_dir, _request)) = queued_job {
            self.update_and_send(
                job_id,
                RunnerJobDelta {
                    status: "stopped".to_string(),
                    stderr_chunk: Some("job stopped before start".to_string()),
                    exit_code: Some(-1),
                    duration_ms: Some(0),
                    error: Some("job stopped before start".to_string()),
                    finished: true,
                    ..Default::default()
                },
            );
            self.start_available_queued();
            return Ok(());
        }
        let (child, stop_requested) = {
            let jobs = lock_unpoison(&self.jobs);
            let Some(job) = jobs.get(job_id) else {
                return Err(format!("unknown local job: {}", job_id));
            };
            if runner_job_is_terminal(&job.snapshot.status) {
                drop(jobs);
                // A stop can race a terminal update that failed in transport.
                // Replay the retained terminal snapshot with its original
                // sequence so the server converges instead of remaining
                // `stop_requested`.
                self.resend_snapshot(job_id);
                return Ok(());
            }
            (job.child.clone(), job.stop_requested.clone())
        };
        stop_requested.store(true, Ordering::SeqCst);
        self.update_and_send(
            job_id,
            RunnerJobDelta {
                status: "stop_requested".to_string(),
                error: Some("stop requested".to_string()),
                ..Default::default()
            },
        );
        if let Some(child) = child {
            kill_child_group(&child).map_err(|e| format!("failed to kill job {}: {}", job_id, e))
        } else {
            Ok(())
        }
    }
}
fn handle_one_poll(
    client: &Client,
    cfg: &AgentConfig,
    runtime: &Arc<ReloadableAgentConfig>,
    jobs: &JobManager,
    project_cache: &mut AgentProjectCache,
    agent_instance_id: &str,
    lsp: &webcodex_runner::LspSupervisor,
    shutdown: &Arc<AtomicBool>,
    dispatches: &ActivityTracker,
) -> Result<bool, PollError> {
    let metadata_config = runtime.snapshot();
    let provider_update =
        metadata_config
            .external_tools
            .claim_status_update()
            .map(|(mut status, revision)| {
                status.config_reload = metadata_config.reload_status();
                (
                    status,
                    Arc::clone(&metadata_config.external_tools),
                    revision,
                )
            });
    let poll = ShellAgentPollPayload {
        request: ShellAgentPollRequest {
            client_id: cfg.client_id.clone(),
            agent_instance_id: agent_instance_id.to_string(),
            projects: Some(project_cache.get_with_shutdown(cfg, Some(shutdown.as_ref()))),
        },
        tool_providers: provider_update
            .as_ref()
            .map(|(status, _, _)| status.clone()),
    };
    let response: ShellAgentPollResponse = match post_json(client, cfg, AGENT_POLL_PATH, &poll) {
        Ok(response) => response,
        Err(error) => {
            if let Some((_, provider, revision)) = provider_update {
                provider.release_status_update(revision);
            }
            return Err(PollError::from_http(error, &cfg.client_id));
        }
    };
    if !response.success {
        if let Some((_, provider, revision)) = provider_update {
            provider.release_status_update(revision);
        }
        return Err(PollError::from_response_error(
            &cfg.client_id,
            response.error,
        ));
    }
    if let Some((_, provider, revision)) = provider_update {
        provider.mark_status_reported(revision);
    }
    let sink = AgentSink::Http(HttpSendConfig {
        client: client.clone(),
        server_url: cfg.server_url.clone(),
        token: cfg.token.clone(),
        client_id: cfg.client_id.clone(),
        agent_instance_id: agent_instance_id.to_string(),
        shutdown: Arc::clone(shutdown),
    });
    jobs.install_sink(sink.clone());
    let Some(request) = response.request else {
        return Ok(false);
    };
    let project_op = is_project_op(&request.kind);
    let hot = runtime.snapshot();
    let runtime = Arc::clone(runtime);
    let jobs = jobs.clone();
    let projects_dir = projects_dir(cfg);
    let lsp = lsp.clone();
    let dispatch_guard = dispatches.enter();
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _dispatch_guard = dispatch_guard;
        let result = dispatch_request(&sink, &hot, &runtime, &jobs, &projects_dir, &lsp, request);
        let _ = result_tx.send(result);
    });
    let result = loop {
        match result_rx.recv_timeout(Duration::from_millis(25)) {
            Ok(result) => break result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if shutdown.load(Ordering::SeqCst) {
                    return Err(PollError::from_submit(SubmitResultError::Shutdown(
                        "process shutdown".to_string(),
                    )));
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(PollError::from_submit(SubmitResultError::TransportClosed(
                    "polling dispatch worker closed".to_string(),
                )));
            }
        }
    };
    if project_op && result.is_ok() {
        project_cache.invalidate();
    }
    result.map_err(PollError::from_submit)
}

fn main() {
    // Pin the process start timestamp before any transport work so register
    // payloads report real process identity even after reconnect loops.
    let _ = process_started_at();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();

    let action = match parse_args() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(2);
        }
    };
    let (config_path, once) = match action {
        AgentCliAction::Run { config_path, once } => (config_path, once),
        AgentCliAction::Exit {
            code,
            stdout,
            stderr,
        } => {
            if !stdout.is_empty() {
                print!("{}", stdout);
            }
            if !stderr.is_empty() {
                eprint!("{}", stderr);
            }
            std::process::exit(code);
        }
    };
    let cfg = match load_config(&config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(2);
        }
    };
    if cfg.token.trim().is_empty() {
        eprintln!(
            "webcodex-runner warning: agent token is empty; connecting without Authorization; the server must be started with --open"
        );
    }
    if let Err(e) = run_agent(cfg, config_path, once) {
        eprintln!("webcodex-runner failed: {}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
