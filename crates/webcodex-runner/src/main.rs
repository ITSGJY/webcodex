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
    ShellClientRegisterResponse, ShellJobValidationProgress, ShellJobValidationStep,
    ShellProfileSummaryEntry, ShellProfilesSummary, AGENT_PROTOCOL_VERSION_POLLING_V1,
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
        }
    }
}

#[derive(Debug, Clone)]
struct RunningJob {
    client_id: String,
    child: Option<Arc<Mutex<Child>>>,
    process_group_id: Option<u32>,
    stop_requested: Arc<AtomicBool>,
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

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
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
) -> Result<usize, RegisterError> {
    let projects = project_cache.get_with_shutdown(cfg, shutdown);
    let projects_count = projects.iter().filter(|project| !project.disabled).count();
    let (body, provider, provider_revision) = build_register_request_with_provider_status(
        cfg,
        runtime,
        projects,
        AGENT_PROTOCOL_VERSION_POLLING_V1,
        agent_instance_id,
        prepared_cache_count,
    );
    let response: ShellClientRegisterResponse = post_json(client, cfg, AGENT_REGISTER_PATH, &body)
        .map_err(|error| RegisterError::from_http(error, &cfg.client_id))?;
    if response.success {
        provider.mark_status_reported(provider_revision);
        Ok(projects_count)
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
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => {
                    let text = String::from_utf8_lossy(&buf[..read]).to_string();
                    let _ = if stdout {
                        tx.send(OutputChunk::Stdout(text))
                    } else {
                        tx.send(OutputChunk::Stderr(text))
                    };
                }
                Err(_) => break,
            }
        }
    })
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

/// Report a job-start failure over the active transport. Used by
/// `JobManager::start_shell_job` when spawn/cwd/policy checks fail before the
/// job can run.
fn send_start_failure(sink: &AgentSink, request: ShellAgentShellRequest, error: String) {
    send_job_start_failure(sink, request, error, None);
}

fn send_validation_executor_failure(
    sink: &AgentSink,
    request: ShellAgentShellRequest,
    completed: usize,
    code: &str,
) {
    send_job_start_failure(
        sink,
        request,
        code.to_string(),
        Some(ShellJobValidationProgress {
            completed,
            current_step: None,
            failed_step: None,
        }),
    );
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

fn send_job_start_failure(
    sink: &AgentSink,
    request: ShellAgentShellRequest,
    error: String,
    validation_progress: Option<ShellJobValidationProgress>,
) {
    if let Some(job_id) = request.job_id {
        let _ = sink.send_job_update(&ShellAgentJobUpdateRequest {
            client_id: sink.client_id().to_string(),
            agent_instance_id: sink.agent_instance_id().to_string(),
            job_id,
            request_id: Some(request.request_id),
            status: "failed".to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            exit_code: None,
            duration_ms: Some(0),
            error: Some(error),
            validation_progress,
            finished: true,
        });
    }
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

impl JobManager {
    fn has_work(&self) -> bool {
        !lock_unpoison(&self.jobs).is_empty() || !lock_unpoison(&self.queued).is_empty()
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
            if !shutdown_target_running(target) {
                continue;
            }
            #[cfg(unix)]
            if let Some(process_group_id) = target.process_group_id {
                if signal_process_group(process_group_id, libc::SIGKILL).is_err() {
                    batch.failures += 1;
                }
            }
            #[cfg(not(unix))]
            if lock_unpoison(&target.child).kill().is_err() {
                batch.failures += 1;
            }
        }

        while Instant::now() < deadline {
            if batch
                .targets
                .iter_mut()
                .all(|target| !shutdown_target_running(target))
            {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(Duration::from_millis(10).min(remaining));
        }
        let mut timed_out = 0;
        for target in &mut batch.targets {
            timed_out += usize::from(shutdown_target_running(target));
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

    fn active_job_count(&self, client_id: &str) -> usize {
        lock_unpoison(&self.jobs)
            .values()
            .filter(|job| job.client_id == client_id)
            .count()
    }

    fn shutdown_rejection(&self, sink: &AgentSink, request: ShellAgentShellRequest) {
        send_job_start_failure(sink, request, "runner is shutting down".to_string(), None);
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
        if self.shutting_down.load(Ordering::SeqCst) {
            self.shutdown_rejection(&sink, request);
            return;
        }
        let client_id = sink.client_id().to_string();
        let active = self.active_job_count(&client_id);
        if active >= self.max_concurrent {
            let queued_update = ShellAgentJobUpdateRequest {
                client_id: client_id.clone(),
                agent_instance_id: sink.agent_instance_id().to_string(),
                job_id: job_id.clone(),
                request_id: Some(request.request_id.clone()),
                status: "agent_queued".to_string(),
                stdout_chunk: None,
                stderr_chunk: None,
                stdout_tail: None,
                stderr_tail: None,
                exit_code: None,
                duration_ms: None,
                error: None,
                validation_progress: None,
                finished: false,
            };
            let lifecycle = lock_unpoison(&self.lifecycle);
            if self.shutting_down.load(Ordering::SeqCst) {
                drop(lifecycle);
                self.shutdown_rejection(&sink, request);
                return;
            }
            let update_sink = sink.clone();
            lock_unpoison(&self.queued).push_back((
                sink,
                generation,
                policy,
                shell,
                projects_dir,
                request,
            ));
            drop(lifecycle);
            let _ = update_sink.send_job_update(&queued_update);
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
            self.shutdown_rejection(&sink, request);
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
                let jobs = lock_unpoison(&self.jobs);
                let mut queued = lock_unpoison(&self.queued);
                let mut selected = None;
                for (idx, (_, _, _policy, _shell, _projects_dir, request)) in
                    queued.iter().enumerate()
                {
                    let active = jobs
                        .values()
                        .filter(|job| job.client_id == request.client_id)
                        .count();
                    if active < self.max_concurrent {
                        selected = Some(idx);
                        break;
                    }
                }
                selected.and_then(|idx| queued.remove(idx))
            };
            let Some((sink, generation, policy, shell, projects_dir, request)) = next else {
                return;
            };
            self.start_now(sink, generation, policy, shell, projects_dir, request);
        }
    }

    fn start_shell_job(
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
        if !policy.allow_raw_shell {
            send_start_failure(
                &sink,
                request,
                "raw shell is disabled by local agent policy".to_string(),
            );
            return;
        }
        let cwd_path = request
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
        if let Err(e) = cwd_allowed(&policy, &cwd_path) {
            send_start_failure(&sink, request, e);
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
                    send_start_failure(
                        &sink,
                        request,
                        "invalid structured validation plan".to_string(),
                    );
                    return;
                }
            }
        } else {
            Vec::new()
        };
        let sandbox_mode = request.sandbox.clone();
        let inspect_scratch = match sandbox_mode.as_deref() {
            None => None,
            Some(crate::command_sandbox::INSPECT_SANDBOX_MODE) => {
                match crate::command_sandbox::InspectScratch::create() {
                    Ok(scratch) => Some(scratch),
                    Err(error) => {
                        send_start_failure(
                            &sink,
                            request,
                            format!("inspect sandbox unavailable: {error}"),
                        );
                        return;
                    }
                }
            }
            Some(other) => {
                send_start_failure(&sink, request, format!("unknown sandbox mode '{other}'"));
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
                    send_start_failure(&sink, request, e);
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
            send_validation_executor_failure(&sink, request, 0, VALIDATION_TOOL_UNAVAILABLE_CODE);
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
                    send_start_failure(&sink, request, error);
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
                    send_start_failure(
                        &sink,
                        request,
                        format!("inspect sandbox unavailable: {error}"),
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
        let stop_requested = Arc::new(AtomicBool::new(false));
        let client_id = sink.client_id().to_string();
        {
            let _lifecycle = lock_unpoison(&self.lifecycle);
            if self.shutting_down.load(Ordering::SeqCst) {
                self.shutdown_rejection(&sink, request);
                return;
            }
            // Reserve the job before spawning outside the lifecycle mutex.
            // Shutdown can then set this stop flag even while Command::spawn
            // is waiting for a child-side pre-exec hook.
            lock_unpoison(&self.jobs).insert(
                job_id.clone(),
                RunningJob {
                    client_id: client_id.clone(),
                    child: None,
                    process_group_id: None,
                    stop_requested: Arc::clone(&stop_requested),
                },
            );
        }
        let start = Instant::now();
        let spawn = commands
            .pop_front()
            .expect("validated non-empty plan")
            .spawn();
        let mut child = match spawn {
            Ok(c) => c,
            Err(e) => {
                lock_unpoison(&self.jobs).remove(&job_id);
                if validation {
                    send_validation_executor_failure(
                        &sink,
                        request,
                        0,
                        VALIDATION_STEP_SPAWN_FAILED_CODE,
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
                    send_start_failure(&sink, request, error);
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
                lock_unpoison(&self.jobs).remove(&job_id);
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
            self.shutdown_rejection(&sink, request);
            return;
        }
        let _ = sink.send_job_update(&ShellAgentJobUpdateRequest {
            client_id: client_id.clone(),
            agent_instance_id: sink.agent_instance_id().to_string(),
            job_id: job_id.clone(),
            request_id: Some(request.request_id.clone()),
            status: "running".to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            exit_code: None,
            duration_ms: None,
            error: None,
            validation_progress: validation.then(|| ShellJobValidationProgress {
                completed: 0,
                current_step: Some(steps[0].name.clone()),
                failed_step: None,
            }),
            finished: false,
        });
        let jobs = self.jobs.clone();
        let queued = self.queued.clone();
        let prepared_profiles = self.prepared_profiles.clone();
        let lifecycle = Arc::clone(&self.lifecycle);
        let shutting_down = Arc::clone(&self.shutting_down);
        let workers = self.workers.clone();
        let max_concurrent = self.max_concurrent;
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
                        let _ = sink.send_job_update(&ShellAgentJobUpdateRequest {
                            client_id: sink.client_id().to_string(),
                            agent_instance_id: sink.agent_instance_id().to_string(),
                            job_id: job_id.clone(),
                            request_id: Some(request.request_id.clone()),
                            status: "running".to_string(),
                            stdout_chunk: (!out.is_empty()).then_some(out),
                            stderr_chunk: (!err.is_empty()).then_some(err),
                            stdout_tail: None,
                            stderr_tail: None,
                            exit_code: None,
                            duration_ms: None,
                            error: None,
                            validation_progress: validation.then(|| ShellJobValidationProgress {
                                completed: step_index,
                                current_step: Some(steps[step_index].name.clone()),
                                failed_step: None,
                            }),
                            finished: false,
                        });
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
                    let _ = sink.send_job_update(&ShellAgentJobUpdateRequest {
                        client_id: sink.client_id().to_string(),
                        agent_instance_id: sink.agent_instance_id().to_string(),
                        job_id: job_id.clone(),
                        request_id: Some(request.request_id.clone()),
                        status: "running".to_string(),
                        stdout_chunk: (!out.is_empty()).then_some(out),
                        stderr_chunk: (!err.is_empty()).then_some(err),
                        stdout_tail: None,
                        stderr_tail: None,
                        exit_code: None,
                        duration_ms: None,
                        error: None,
                        validation_progress: validation.then(|| ShellJobValidationProgress {
                            completed: step_index,
                            current_step: Some(steps[step_index].name.clone()),
                            failed_step: None,
                        }),
                        finished: false,
                    });
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
            let _ = sink.send_job_update(&ShellAgentJobUpdateRequest {
                client_id: sink.client_id().to_string(),
                agent_instance_id: sink.agent_instance_id().to_string(),
                job_id: job_id.clone(),
                request_id: Some(request.request_id),
                status: final_status.0,
                stdout_chunk: (!out.is_empty()).then_some(out),
                stderr_chunk: (!err.is_empty()).then_some(err),
                stdout_tail: None,
                stderr_tail: None,
                exit_code: final_status.1,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: final_status.2,
                validation_progress: final_progress,
                finished: true,
            });
            lock_unpoison(&jobs).remove(&job_id);
            let manager = JobManager {
                max_concurrent,
                jobs: jobs.clone(),
                queued: queued.clone(),
                prepared_profiles,
                lifecycle,
                shutting_down,
                workers,
            };
            manager.start_available_queued();
        });
    }

    fn stop(&self, job_id: &str) -> Result<(), String> {
        let queued_job = {
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
        if let Some((sink, _generation, _policy, _shell, _projects_dir, request)) = queued_job {
            let request_id = request.request_id.clone();
            let job_id = request.job_id.clone().unwrap_or_default();
            let _ = sink.send_job_update(&ShellAgentJobUpdateRequest {
                client_id: sink.client_id().to_string(),
                agent_instance_id: sink.agent_instance_id().to_string(),
                job_id,
                request_id: Some(request_id),
                status: "stopped".to_string(),
                stdout_chunk: None,
                stderr_chunk: None,
                stdout_tail: None,
                stderr_tail: Some("job stopped before start".to_string()),
                exit_code: Some(-1),
                duration_ms: Some(0),
                error: Some("job stopped before start".to_string()),
                validation_progress: None,
                finished: true,
            });
            return Ok(());
        }
        let (child, stop_requested) = {
            let jobs = lock_unpoison(&self.jobs);
            let Some(job) = jobs.get(job_id) else {
                return Err(format!("unknown local job: {}", job_id));
            };
            (job.child.clone(), job.stop_requested.clone())
        };
        stop_requested.store(true, Ordering::SeqCst);
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
    let Some(request) = response.request else {
        return Ok(false);
    };
    let project_op = is_project_op(&request.kind);
    let sink = AgentSink::Http(HttpSendConfig {
        client: client.clone(),
        server_url: cfg.server_url.clone(),
        token: cfg.token.clone(),
        client_id: cfg.client_id.clone(),
        agent_instance_id: agent_instance_id.to_string(),
        shutdown: Arc::clone(shutdown),
    });
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
mod tests {
    use super::*;
    use crate::webcodex_runner::handle_project_lifecycle_op;
    static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Policy for tests that exercise shell/profile behavior inside a temp dir
    /// rather than the filesystem boundary itself. `AgentPolicy::default()` is
    /// now fail-closed (empty `allowed_roots` reaches nothing), so these tests
    /// opt out of the boundary explicitly instead of leaning on a permissive
    /// production default.
    fn unrestricted_test_policy() -> AgentPolicy {
        AgentPolicy {
            allow_cwd_anywhere: true,
            ..AgentPolicy::default()
        }
    }

    fn test_config(projects_dir: PathBuf) -> AgentConfig {
        AgentConfig {
            server_url: "http://127.0.0.1:8000".to_string(),
            token: "test-token".to_string(),
            client_id: "oe".to_string(),
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            projects_dir: Some(projects_dir),
            poll_interval_ms: 1000,
            capabilities: None,
            max_concurrent_jobs: None,
            policy: unrestricted_test_policy(),
            shell: ShellConfig::default(),
            transport: None,
            websocket_connect_timeout_secs: default_websocket_connect_timeout_secs(),
            quic: None,
            tool_providers: Default::default(),
        }
    }

    fn runtime_config(cfg: &AgentConfig) -> Arc<ReloadableAgentConfig> {
        Arc::new(ReloadableAgentConfig::new(cfg.clone(), PathBuf::new()))
    }

    #[test]
    fn bounded_response_body_reader_stops_after_limit_plus_one() {
        let mut reader = std::io::Cursor::new(vec![b'x'; 66]);
        let body = read_bounded_response_body(&mut reader, None, 64).unwrap();
        assert!(body.exceeded_limit);
        assert_eq!(body.bytes.len(), 64);
        assert_eq!(
            reader.position(),
            65,
            "the bounded reader must not consume the unbounded remainder"
        );
    }

    #[test]
    fn response_decode_distinguishes_empty_eof_and_complete_syntax_errors() {
        for bytes in [b"".as_slice(), br#"{"success":true,"request":"#.as_slice()] {
            let error = decode_json_response::<ShellAgentPollResponse>(
                AGENT_POLL_PATH,
                reqwest::StatusCode::OK,
                "application/json",
                BoundedResponseBody {
                    bytes: bytes.to_vec(),
                    exceeded_limit: false,
                },
            )
            .unwrap_err();
            assert_eq!(error.kind, AgentHttpErrorKind::DecodeTransient);
        }

        let error = decode_json_response::<ShellAgentPollResponse>(
            AGENT_POLL_PATH,
            reqwest::StatusCode::OK,
            "application/json",
            BoundedResponseBody {
                bytes: b"{not-json".to_vec(),
                exceeded_limit: false,
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, AgentHttpErrorKind::ProtocolDecode);
        assert!(error.summary.contains("serde_category=syntax"));
        assert!(!error.to_string().contains("{not-json"));
    }

    #[test]
    fn protocol_decode_diagnostics_omit_queries_credentials_and_response_values() {
        let content_type = reqwest::header::HeaderValue::from_static(
            "application/json; authorization=Bearer SECRET-TOKEN",
        );
        let content_type = bounded_response_content_type(Some(&content_type), "SECRET-TOKEN");
        let error = decode_json_response::<ShellAgentPollResponse>(
            "/api/shell/agent/poll?token=SECRET-TOKEN",
            reqwest::StatusCode::OK,
            &content_type,
            BoundedResponseBody {
                bytes: br#"{"success":"SECRET-TOKEN","request":null,"error":null}"#.to_vec(),
                exceeded_limit: false,
            },
        )
        .unwrap_err();
        let message = error.to_string();
        assert_eq!(error.kind, AgentHttpErrorKind::ProtocolDecode);
        assert!(
            message.contains("content_type=application/json"),
            "{message}"
        );
        assert!(!message.contains('?'), "{message}");
        assert!(!message.contains("SECRET-TOKEN"), "{message}");
        assert!(!message.contains("authorization"), "{message}");
        assert!(!message.contains('\n'), "{message}");
    }

    #[test]
    fn result_400_is_classified_permanent_with_bounded_structured_reason() {
        let error = AgentHttpError::status(
            "/api/shell/agent/result",
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"success":false,"error":"unknown or expired shell request: req-1"}"#,
        );
        assert_eq!(error.kind, AgentHttpErrorKind::ClientRejected);
        let message = error.to_string();
        assert!(
            message.contains("server rejected /api/shell/agent/result request"),
            "{message}"
        );
        assert!(message.contains("HTTP 400 Bad Request"), "{message}");
        assert!(
            message.contains("unknown or expired shell request: req-1"),
            "{message}"
        );
    }

    #[test]
    fn result_4xx_html_bodies_stay_permanent_and_never_leak_markup() {
        let bad_request = AgentHttpError::status(
            "/api/shell/agent/result",
            reqwest::StatusCode::BAD_REQUEST,
            "<html>\n<body><h1>400 Bad Request</h1><center>nginx</center></body>\n</html>",
        );
        assert_eq!(bad_request.kind, AgentHttpErrorKind::ClientRejected);
        assert!(!bad_request.to_string().contains("<html"), "{bad_request}");

        let too_large = AgentHttpError::status(
            "/api/shell/agent/result",
            reqwest::StatusCode::PAYLOAD_TOO_LARGE,
            "<html><center>nginx</center><center>413 Request Entity Too Large</center></html>",
        );
        assert_eq!(too_large.kind, AgentHttpErrorKind::ClientRejected);
        assert!(!too_large.to_string().contains("nginx"), "{too_large}");
    }

    #[test]
    fn result_400_structured_reason_is_bounded_for_large_json_bodies() {
        let huge = format!(r#"{{"success":false,"error":"{}"}}"#, "x".repeat(10_000));
        let error = AgentHttpError::status(
            "/api/shell/agent/result",
            reqwest::StatusCode::BAD_REQUEST,
            &huge,
        );
        assert_eq!(error.kind, AgentHttpErrorKind::ClientRejected);
        let message = error.to_string();
        assert!(
            message.chars().count() < 300,
            "unbounded message: {} chars",
            message.chars().count()
        );
    }

    #[test]
    fn http_status_classification_keeps_retryable_auth_and_gateway_kinds() {
        let cases = [
            (reqwest::StatusCode::UNAUTHORIZED, AgentHttpErrorKind::Auth),
            (reqwest::StatusCode::FORBIDDEN, AgentHttpErrorKind::Auth),
            (reqwest::StatusCode::NOT_FOUND, AgentHttpErrorKind::NotFound),
            (
                reqwest::StatusCode::REQUEST_TIMEOUT,
                AgentHttpErrorKind::Status,
            ),
            (
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                AgentHttpErrorKind::Status,
            ),
            (
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                AgentHttpErrorKind::ServerUnavailable,
            ),
            (
                reqwest::StatusCode::BAD_GATEWAY,
                AgentHttpErrorKind::ServerUnavailable,
            ),
            (
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                AgentHttpErrorKind::ServerUnavailable,
            ),
            (
                reqwest::StatusCode::GATEWAY_TIMEOUT,
                AgentHttpErrorKind::ServerUnavailable,
            ),
        ];
        for (status, expected) in cases {
            let error = AgentHttpError::status("/api/shell/agent/result", status, "{}");
            assert_eq!(error.kind, expected, "status {status}");
        }
    }

    #[test]
    fn register_recovery_classification_is_strict_about_lease_conflicts() {
        let lease = AgentHttpError::status(
            AGENT_REGISTER_PATH,
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"success":false,"error":"agent client oe is already online with a different instance"}"#,
        );
        let lease = RegisterError::from_http(lease, "oe");
        assert_eq!(
            lease.recovery_action(),
            RegisterRecoveryAction::WaitForLease
        );

        for body in [
            r#"{"success":false,"error":"agent client identity is unavailable"}"#,
            r#"{"success":false,"error":"agent token owner is 'alice'; cannot register owner 'bob'"}"#,
            r#"{"success":false,"error":"agent client oe is already online"}"#,
        ] {
            let rejected =
                AgentHttpError::status(AGENT_REGISTER_PATH, reqwest::StatusCode::BAD_REQUEST, body);
            let rejected = RegisterError::from_http(rejected, "oe");
            assert_eq!(
                rejected.recovery_action(),
                RegisterRecoveryAction::Fatal,
                "{body}"
            );
        }
    }

    #[test]
    fn poll_recovery_actions_separate_transport_session_and_fatal_errors() {
        let transient = PollError::from_http(
            AgentHttpError::status(
                AGENT_POLL_PATH,
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                "{}",
            ),
            "oe",
        );
        assert_eq!(
            transient.recovery_action(),
            PollingRecoveryAction::RetryPoll
        );

        let missing_session = PollError::from_http(
            AgentHttpError::status(
                AGENT_POLL_PATH,
                reqwest::StatusCode::BAD_REQUEST,
                r#"{"success":false,"error":"unknown shell client: oe"}"#,
            ),
            "oe",
        );
        assert_eq!(
            missing_session.recovery_action(),
            PollingRecoveryAction::ReRegister
        );

        let ordinary_400 = PollError::from_http(
            AgentHttpError::status(
                AGENT_POLL_PATH,
                reqwest::StatusCode::BAD_REQUEST,
                r#"{"success":false,"error":"invalid poll payload"}"#,
            ),
            "oe",
        );
        assert_eq!(ordinary_400.recovery_action(), PollingRecoveryAction::Fatal);
    }

    #[test]
    fn tls_configuration_markers_are_fatal_but_dns_and_eof_are_not() {
        assert!(looks_like_fatal_tls_request(
            "error: invalid peer certificate: UnknownIssuer"
        ));
        assert!(looks_like_fatal_tls_request(
            "tls error: no application protocol; ALPN mismatch"
        ));
        assert!(!looks_like_fatal_tls_request(
            "dns error: temporary failure in name resolution"
        ));
        assert!(!looks_like_fatal_tls_request(
            "connection closed: unexpected EOF"
        ));
    }

    #[test]
    fn submit_fatal_error_classes_map_to_terminal_poll_contract() {
        assert!(PollError::from_submit(SubmitResultError::FatalAuth("auth".into())).is_terminal());
        assert!(
            PollError::from_submit(SubmitResultError::FatalProtocol("missing".into()))
                .is_terminal()
        );
        assert!(PollError::from_submit(SubmitResultError::FatalConfig("tls".into())).is_terminal());
        assert!(
            PollError::from_submit(SubmitResultError::TransportClosed("closed".into()))
                .is_terminal()
        );
        let shutdown =
            PollError::from_submit(SubmitResultError::Shutdown("process shutdown".into()));
        assert!(!shutdown.is_terminal());
        assert!(shutdown.is_shutdown());
    }

    fn reload_toml(
        client_id: &str,
        max_jobs: Option<usize>,
        max_timeout: u64,
        max_output: usize,
        shell_program: &str,
        strategy: &str,
        claude_enabled: bool,
        claude_command: &str,
    ) -> String {
        let max_jobs = max_jobs
            .map(|value| format!("max_concurrent_jobs = {value}\n"))
            .unwrap_or_default();
        format!(
            r#"server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "{client_id}"
owner = "alice"
poll_interval_ms = 1000
{max_jobs}
policy.allow_raw_shell = true
policy.allow_cwd_anywhere = true
policy.allowed_roots = ["/"]
policy.max_timeout_secs = {max_timeout}
policy.max_output_bytes = {max_output}
shell.program = "{shell_program}"
shell.args = ["-c"]
tool_providers.strategy = "{strategy}"
tool_providers.claude_code.enabled = {claude_enabled}
tool_providers.claude_code.command = "{claude_command}"
tool_providers.claude_code.args = ["mcp", "serve"]
tool_providers.claude_code.timeout_secs = 30
"#
        )
    }

    fn reload_fixture() -> (tempfile::TempDir, PathBuf, ReloadableAgentConfig) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent.toml");
        std::fs::write(
            &path,
            reload_toml("oe", None, 60, 1024, "sh", "native", false, "claude"),
        )
        .unwrap();
        let runtime = ReloadableAgentConfig::new(load_config(&path).unwrap(), path.clone());
        (tmp, path, runtime)
    }

    #[test]
    fn reload_field_classification_is_exhaustive_and_allowlisted() {
        let startup = test_config(PathBuf::from("projects-a"));
        let mut hot_only = startup.clone();
        hot_only.policy.max_timeout_secs += 1;
        hot_only.shell.program = "bash".to_string();
        hot_only.tool_providers.strategy =
            webcodex_runner::config::ToolProviderStrategy::ClaudeCodeThenNative;
        assert!(webcodex_runner::config::restart_required_fields(&startup, &hot_only).is_empty());

        let mut changed = hot_only;
        changed.server_url.push_str("/other");
        changed.token.push('2');
        changed.client_id.push('2');
        changed.display_name = Some("changed".to_string());
        changed.owner = Some("changed".to_string());
        changed.hostname = Some("changed".to_string());
        changed.projects_dir = Some(PathBuf::from("projects-b"));
        changed.poll_interval_ms += 1;
        changed.capabilities = Some(ShellClientCapabilities::default());
        changed.max_concurrent_jobs = Some(4);
        changed.transport = Some(TRANSPORT_QUIC.to_string());
        changed.websocket_connect_timeout_secs += 1;
        changed.quic = Some(quic_client_config());
        assert_eq!(
            webcodex_runner::config::restart_required_fields(&startup, &changed).join(" "),
            "capabilities client_id display_name hostname max_concurrent_jobs owner poll_interval_ms projects_dir quic server_url token transport websocket_connect_timeout_secs"
        );
    }

    #[test]
    fn valid_reload_switches_one_complete_generation_and_preserves_old_snapshot() {
        let (_tmp, path, runtime) = reload_fixture();
        let old = runtime.snapshot();

        std::fs::write(
            &path,
            reload_toml(
                "oe",
                None,
                120,
                2048,
                "bash",
                "claude_code_then_native",
                false,
                "claude",
            ),
        )
        .unwrap();
        let status = runtime.reload();
        let new = runtime.snapshot();

        assert_eq!(status.last_reload_result, "success");
        assert_eq!(status.generation, 2);
        assert!(!status.restart_required);
        assert_eq!(
            (
                old.generation,
                old.policy.max_timeout_secs,
                old.shell.program.as_str()
            ),
            (1, 60, "sh")
        );
        assert_eq!(old.external_tools.status().strategy, "native");
        assert_eq!(
            (
                new.policy.max_timeout_secs,
                new.policy.max_output_bytes,
                new.shell.program.as_str()
            ),
            (120, 2048, "bash")
        );
        assert_eq!(
            new.external_tools.status().strategy,
            "claude_code_then_native"
        );
    }

    #[test]
    fn failed_reload_keeps_generation_and_can_recover() {
        let (_tmp, path, runtime) = reload_fixture();
        let old = runtime.snapshot();

        std::fs::remove_file(&path).unwrap();
        let status = runtime.reload();
        assert_eq!(status.generation, 1);
        assert_eq!(
            status.last_reload_error_code.as_deref(),
            Some("config_read_failed")
        );

        for (candidate, code) in [
            ("{ invalid toml".to_string(), "config_parse_failed"),
            (
                reload_toml("oe", None, 60, 1024, "", "native", false, "claude"),
                "config_validation_failed",
            ),
            (
                reload_toml("oe", None, 60, 1024, "sh", "native", true, ""),
                "provider_config_invalid",
            ),
        ] {
            std::fs::write(&path, candidate).unwrap();
            let status = runtime.reload();
            assert_eq!(status.generation, 1);
            assert_eq!(status.last_reload_result, "failure");
            assert_eq!(status.last_reload_error_code.as_deref(), Some(code));
        }
        assert_eq!(old.policy.max_timeout_secs, 60);
        let serialized = serde_json::to_string(&runtime.snapshot().reload_status()).unwrap();
        assert!(!serialized.contains(path.to_string_lossy().as_ref()));
        assert!(!serialized.contains("test-token"));

        std::fs::write(
            &path,
            reload_toml("oe", None, 90, 1024, "sh", "native", false, "claude"),
        )
        .unwrap();
        assert_eq!(runtime.reload().generation, 2);
        assert_eq!(runtime.snapshot().policy.max_timeout_secs, 90);
    }

    #[test]
    fn mixed_reload_applies_hot_fields_and_reports_static_restart_fields() {
        let (_tmp, path, runtime) = reload_fixture();
        std::fs::write(
            &path,
            reload_toml(
                "oe-new",
                Some(8),
                180,
                4096,
                "bash",
                "native",
                false,
                "claude",
            ),
        )
        .unwrap();

        let status = runtime.reload();
        let active = runtime.snapshot();
        assert_eq!(status.last_reload_result, "partial");
        assert!(status.restart_required);
        assert_eq!(
            status.restart_required_fields,
            ["client_id", "max_concurrent_jobs"]
        );
        assert_eq!(
            (
                active.policy.max_timeout_secs,
                active.policy.max_output_bytes,
                active.shell.program.as_str()
            ),
            (180, 4096, "bash")
        );
    }

    fn quic_client_config() -> QuicClientConfig {
        QuicClientConfig {
            server_addr: "v4.example.test:8443".to_string(),
            server_name: "v4.example.test".to_string(),
            alpn: default_quic_alpn(),
            connect_timeout_secs: default_quic_connect_timeout_secs(),
            keepalive_interval_secs: default_quic_keepalive_interval_secs(),
        }
    }

    #[test]
    fn agent_config_defaults_transport_to_websocket_without_quic_section() {
        // No transport field and no [quic] section: default stays websocket.
        let toml = r#"
server_url = "http://127.0.0.1:8000"
token = "t"
client_id = "oe"
"#;
        let cfg: AgentConfig = toml::from_str(toml).unwrap();
        assert!(cfg.transport.is_none());
        assert!(cfg.quic.is_none());
        assert_eq!(effective_transport(&cfg), TRANSPORT_WEBSOCKET);
        assert_eq!(
            cfg.websocket_connect_timeout_secs,
            default_websocket_connect_timeout_secs()
        );
        assert_eq!(
            auto_transport_plan(&cfg),
            vec![TRANSPORT_WEBSOCKET, TRANSPORT_POLLING]
        );
    }

    #[test]
    fn agent_config_rejects_zero_websocket_connect_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent.toml");
        std::fs::write(
            &path,
            r#"
server_url = "http://127.0.0.1:8000"
token = "t"
client_id = "oe"
websocket_connect_timeout_secs = 0
"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err();
        assert!(
            err.contains("websocket_connect_timeout_secs must be > 0"),
            "{err}"
        );
    }

    #[test]
    fn agent_config_accepts_transport_quic_with_quic_section() {
        let toml = r#"
server_url = "http://127.0.0.1:8000"
token = "t"
client_id = "oe"
transport = "quic"

[quic]
server_addr = "v4.example.test:8443"
server_name = "v4.example.test"
"#;
        let cfg: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.transport.as_deref(), Some("quic"));
        let quic = cfg.quic.expect("quic section");
        assert_eq!(quic.server_addr, "v4.example.test:8443");
        assert_eq!(quic.server_name, "v4.example.test");
        // Defaults applied.
        assert_eq!(quic.alpn, "webcodex-runner/1");
        assert_eq!(quic.connect_timeout_secs, 10);
        assert_eq!(quic.keepalive_interval_secs, 20);
    }

    #[test]
    fn agent_config_accepts_transport_auto() {
        let toml = r#"
server_url = "http://127.0.0.1:8000"
token = "t"
client_id = "oe"
transport = "auto"
"#;
        let cfg: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.transport.as_deref(), Some(TRANSPORT_AUTO));
        assert_eq!(effective_transport(&cfg), TRANSPORT_AUTO);
        assert_eq!(
            auto_transport_plan(&cfg),
            vec![TRANSPORT_WEBSOCKET, TRANSPORT_POLLING]
        );
    }

    #[test]
    fn auto_transport_plan_tries_quic_then_websocket_then_polling() {
        let mut cfg = test_config(PathBuf::from("/tmp/x"));
        cfg.transport = Some(TRANSPORT_AUTO.to_string());
        cfg.quic = Some(quic_client_config());
        assert_eq!(
            auto_transport_plan(&cfg),
            vec![TRANSPORT_QUIC, TRANSPORT_WEBSOCKET, TRANSPORT_POLLING]
        );
    }

    #[test]
    fn strict_quic_transport_still_requires_quic_section() {
        let mut cfg = test_config(PathBuf::from("/tmp/x"));
        cfg.transport = Some(TRANSPORT_QUIC.to_string());
        let err = resolve_quic_config(&cfg).unwrap_err();
        assert!(err.contains("transport=quic requires a [quic] section"));
        assert_eq!(effective_transport(&cfg), TRANSPORT_QUIC);
    }

    #[test]
    fn resolve_quic_config_errors_when_section_missing() {
        let mut cfg = test_config(PathBuf::from("/tmp/x"));
        cfg.transport = Some("quic".to_string());
        let err = resolve_quic_config(&cfg).unwrap_err();
        assert!(err.contains("[quic]"), "err was: {err}");
    }

    #[test]
    fn resolve_quic_config_errors_when_server_addr_or_name_missing() {
        let mut cfg = test_config(PathBuf::from("/tmp/x"));
        cfg.transport = Some("quic".to_string());

        // Missing server_addr.
        cfg.quic = Some(QuicClientConfig {
            server_addr: "  ".to_string(),
            server_name: "v4.example.test".to_string(),
            alpn: default_quic_alpn(),
            connect_timeout_secs: 10,
            keepalive_interval_secs: 20,
        });
        let err = resolve_quic_config(&cfg).unwrap_err();
        assert!(err.contains("server_addr"), "err was: {err}");

        // Missing server_name.
        cfg.quic = Some(QuicClientConfig {
            server_addr: "v4.example.test:8443".to_string(),
            server_name: String::new(),
            alpn: default_quic_alpn(),
            connect_timeout_secs: 10,
            keepalive_interval_secs: 20,
        });
        let err = resolve_quic_config(&cfg).unwrap_err();
        assert!(err.contains("server_name"), "err was: {err}");
    }

    #[test]
    fn resolve_quic_config_accepts_valid_section() {
        let mut cfg = test_config(PathBuf::from("/tmp/x"));
        cfg.transport = Some("quic".to_string());
        cfg.quic = Some(quic_client_config());
        let resolved = resolve_quic_config(&cfg).unwrap();
        assert_eq!(resolved.server_addr, "v4.example.test:8443");
        assert_eq!(resolved.server_name, "v4.example.test");
    }

    #[test]
    fn resolve_quic_server_addrs_accepts_hostname_port() {
        let addrs = resolve_quic_server_addrs("localhost:8443").unwrap();
        assert!(addrs.iter().any(|addr| addr.port() == 8443));
    }

    #[test]
    fn resolve_quic_server_addrs_rejects_missing_port() {
        let err = resolve_quic_server_addrs("localhost").unwrap_err();
        assert!(err.contains("failed to resolve"), "err was: {err}");
    }

    #[test]
    fn quic_client_bind_addr_matches_remote_address_family() {
        let v4: SocketAddr = "127.0.0.1:8443".parse().unwrap();
        let v6: SocketAddr = "[::1]:8443".parse().unwrap();
        assert!(quic_client_bind_addr_for(v4).is_ipv4());
        assert!(quic_client_bind_addr_for(v6).is_ipv6());
    }

    #[test]
    fn agent_cli_help_and_version_exit_before_runtime() {
        match parse_agent_args(["--help"]).unwrap() {
            AgentCliAction::Exit {
                code,
                stdout,
                stderr,
            } => {
                assert_eq!(code, 0);
                assert!(stdout.contains("Usage: webcodex-runner"));
                assert!(!stdout.contains("webcodex-runner init"));
                assert!(stderr.is_empty());
            }
            other => panic!("expected help exit, got {other:?}"),
        }
        match parse_agent_args(["--version"]).unwrap() {
            AgentCliAction::Exit {
                code,
                stdout,
                stderr,
            } => {
                assert_eq!(code, 0);
                assert!(stdout.starts_with(&format!(
                    "webcodex-runner {} (commit ",
                    env!("CARGO_PKG_VERSION")
                )));
                assert!(stdout.trim_end().ends_with(')'));
                assert_ne!(
                    stdout,
                    format!("webcodex-runner {}\n", env!("CARGO_PKG_VERSION"))
                );
                assert!(stderr.is_empty());
            }
            other => panic!("expected version exit, got {other:?}"),
        }
    }

    #[test]
    fn agent_cli_has_no_init_alias() {
        let error = parse_agent_args(["init"]).unwrap_err();
        assert!(error.contains("unknown argument: init"));
    }

    #[test]
    fn agent_version_output_includes_build_metadata() {
        match parse_agent_args(["-V"]).unwrap() {
            AgentCliAction::Exit {
                code,
                stdout,
                stderr,
            } => {
                assert_eq!(code, 0);
                assert!(stdout.contains("commit "));
                assert!(stdout.starts_with("webcodex-runner "));
                assert!(stderr.is_empty());
            }
            other => panic!("expected version exit, got {other:?}"),
        }
    }

    #[test]
    fn agent_cli_legacy_runtime_args_are_preserved() {
        let action = parse_agent_args(["--config", "/tmp/agent.toml", "--once"]).unwrap();
        assert_eq!(
            action,
            AgentCliAction::Run {
                config_path: PathBuf::from("/tmp/agent.toml"),
                once: true,
            }
        );
    }

    #[test]
    fn agent_cli_profile_derives_default_config_path() {
        let action = parse_agent_args(["--profile", "special"]).unwrap();
        assert_eq!(
            action,
            AgentCliAction::Run {
                config_path: client_profile_agent_config("special"),
                once: false,
            }
        );
    }

    #[test]
    fn agent_cli_explicit_config_overrides_profile() {
        let action =
            parse_agent_args(["--profile", "special", "--config", "/tmp/agent.toml"]).unwrap();
        assert_eq!(
            action,
            AgentCliAction::Run {
                config_path: PathBuf::from("/tmp/agent.toml"),
                once: false,
            }
        );
    }

    #[test]
    fn agent_cli_rejects_unsafe_profile() {
        let err = parse_agent_args(["--profile", "../x"]).unwrap_err();
        assert_eq!(err, CLIENT_PROFILE_ERROR);
    }

    #[test]
    fn empty_tokens_config_parser_accepts_empty_and_whitespace_token() {
        for token in ["", "   "] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("agent.toml");
            std::fs::write(
                &path,
                format!(
                    "server_url = \"http://127.0.0.1:8000\"\ntoken = \"{}\"\nclient_id = \"open-agent\"\n[policy]\nallow_cwd_anywhere = true\n",
                    token
                ),
            )
            .unwrap();

            let cfg = load_config(&path).unwrap();
            assert_eq!(cfg.token, token);
            assert_eq!(non_empty_token(&cfg.token), None);
        }
    }

    #[test]
    fn agent_config_without_shell_section_parses() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent.toml");
        std::fs::write(
            &path,
            r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"

[policy]
allow_cwd_anywhere = true
"#,
        )
        .unwrap();

        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.shell, ShellConfig::default());
    }

    #[test]
    fn agent_config_shell_profiles_parse() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent.toml");
        std::fs::write(
            &path,
            r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"

[policy]
allow_cwd_anywhere = true

[shell]
default_profile = "rust"

[shell.profiles.rust]
description = "Rust development tools"
program = "sh"
args = ["-c"]
init_script = '''
export RUST_BACKTRACE=1
'''

[shell.profiles.rust.env]
PATH = "/root/.cargo/bin:/usr/bin:/bin"
CARGO_HOME = "/root/.cargo"
RUSTUP_HOME = "/root/.rustup"

[shell.profiles.py-venv]
description = "Project-local Python virtual environment"
program = "bash"
args = ["-lc"]
init_script = '''
source .venv/bin/activate
'''
"#,
        )
        .unwrap();

        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.shell.default_profile.as_deref(), Some("rust"));
        let rust = cfg.shell.profiles.get("rust").unwrap();
        assert_eq!(rust.description.as_deref(), Some("Rust development tools"));
        assert_eq!(rust.program.as_deref(), Some("sh"));
        assert_eq!(rust.args.as_ref().unwrap(), &vec!["-c".to_string()]);
        assert_eq!(
            rust.env.get("CARGO_HOME").map(String::as_str),
            Some("/root/.cargo")
        );
        assert!(rust
            .init_script
            .as_deref()
            .unwrap()
            .contains("RUST_BACKTRACE=1"));
        assert!(cfg.shell.profiles.contains_key("py-venv"));
    }

    #[test]
    fn agent_config_shell_default_profile_must_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent.toml");
        std::fs::write(
            &path,
            r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"

[policy]
allow_cwd_anywhere = true

[shell]
default_profile = "missing"

[shell.profiles.rust]
program = "sh"
"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err();
        assert!(err.contains("default_profile"), "{err}");
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn agent_config_shell_profile_name_must_be_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent.toml");
        std::fs::write(
            &path,
            r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"

[policy]
allow_cwd_anywhere = true

[shell.profiles."bad/name"]
program = "sh"
"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err();
        assert!(err.contains("shell profile name"), "{err}");
        assert!(err.contains("slash"), "{err}");
    }

    #[test]
    fn agent_config_shell_profile_type_errors_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent.toml");
        std::fs::write(
            &path,
            r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"

[policy]
allow_cwd_anywhere = true

[shell.profiles.rust]
args = "-c"
"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err();
        assert!(err.contains("failed to parse config"), "{err}");
        assert!(err.contains("args"), "{err}");
    }

    #[test]
    fn agent_config_shell_profile_env_type_errors_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent.toml");
        std::fs::write(
            &path,
            r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"

[policy]
allow_cwd_anywhere = true

[shell.profiles.rust.env]
PATH = ["/root/.cargo/bin"]
"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err();
        assert!(err.contains("failed to parse config"), "{err}");
        assert!(err.contains("env"), "{err}");
    }

    #[test]
    fn agent_config_shell_errors_do_not_include_init_script_body() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent.toml");
        let secret = "DO_NOT_LEAK_THIS_INLINE_SCRIPT_BODY";
        std::fs::write(
            &path,
            format!(
                r#"
server_url = "http://127.0.0.1:8000"
token = "test-token"
client_id = "agent-1"

[policy]
allow_cwd_anywhere = true

[shell]
default_profile = "missing"

[shell.profiles.rust]
init_script = '''
export SECRET={}
'''
"#,
                secret
            ),
        )
        .unwrap();

        let err = load_config(&path).unwrap_err();
        assert!(err.contains("default_profile"), "{err}");
        assert!(!err.contains(secret), "{err}");
    }

    #[test]
    fn agent_project_toml_parse_sorts_hook_names() {
        let project = parse_agent_project_toml(
            r#"
id = "webcodex"
path = "/root/git/webcodex"
kind = "rust"
shell_profile = "rust"

[hooks]
precommit = ["cargo test"]
doctor = ["git status --short"]
"#,
        )
        .unwrap();
        let summary = agent_project_summary(&project, 123456, false);
        assert_eq!(summary.id, "webcodex");
        assert_eq!(summary.name.as_deref(), Some("webcodex"));
        assert_eq!(summary.path, "/root/git/webcodex");
        assert_eq!(summary.kind.as_deref(), Some("rust"));
        assert_eq!(summary.hooks, vec!["doctor", "precommit"]);
        assert_eq!(summary.updated_at, 123456);
        assert_eq!(summary.git_branch, None);
        assert_eq!(project.shell_profile.as_deref(), Some("rust"));
    }

    #[test]
    fn agent_project_toml_rejects_invalid_id() {
        let err = parse_agent_project_toml(
            r#"
id = "bad id"
path = "/tmp/webcodex"
"#,
        )
        .unwrap_err();
        assert!(err.contains("ASCII letters"));
    }

    #[test]
    fn agent_project_toml_hints_when_server_projects_format_is_used() {
        let err = parse_agent_project_toml(
            r#"
[projects.smoke]
path = "/root/webcodex-smoke"
"#,
        )
        .unwrap_err();
        assert!(err.contains("missing field"), "{err}");
        assert!(err.contains("server projects.toml"), "{err}");
        assert!(
            err.contains("Agent projects.d files must use top-level fields"),
            "{err}"
        );
        assert!(err.contains("id = \"smoke\""), "{err}");
        assert!(err.contains("path = \"/path/to/repo\""), "{err}");
    }

    #[test]
    fn agent_project_toml_rejects_invalid_shell_profile() {
        let err = parse_agent_project_toml(
            r#"
id = "demo"
path = "/tmp/webcodex"
shell_profile = "../rust"
"#,
        )
        .unwrap_err();
        assert!(err.contains("project.shell_profile"), "{err}");
    }

    #[test]
    fn missing_projects_dir_returns_empty_list() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing-projects.d");
        let projects = load_agent_project_summaries_from_dir(&missing);
        assert!(projects.is_empty());
    }

    #[test]
    fn max_concurrent_jobs_defaults_to_two_and_clamps_to_one() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_config(tmp.path().join("config/projects.d"));
        assert_eq!(max_concurrent_jobs(&cfg), DEFAULT_MAX_CONCURRENT_JOBS);

        cfg.max_concurrent_jobs = Some(0);
        assert_eq!(max_concurrent_jobs(&cfg), 1);

        cfg.max_concurrent_jobs = Some(4);
        assert_eq!(max_concurrent_jobs(&cfg), 4);
    }

    #[test]
    fn shell_config_default_preserves_sh_c_behavior() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let cwd = tmp.path().to_string_lossy().to_string();
        let result = run_shell(
            &cfg.policy,
            &ShellConfig::default(),
            Some(&cwd),
            "printf default-ok",
            None,
            10,
            None,
        );
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.as_deref(), Some("default-ok"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inspect_shell_real_smoke_reads_checks_and_blocks_project_writes() {
        if crate::command_sandbox::inspect_sandbox_available().is_err() {
            // The fail-closed unavailable path has a dedicated sandbox test.
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let projects_dir = tmp.path().join("projects.d");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::create_dir_all(&projects_dir).unwrap();
        let manifest =
            "[package]\nname = \"inspect-runner-smoke\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let lockfile = "# This file is automatically @generated by Cargo.\n\
                        # It is not intended for manual editing.\n\
                        version = 3\n\n\
                        [[package]]\n\
                        name = \"inspect-runner-smoke\"\n\
                        version = \"0.1.0\"\n";
        std::fs::write(project.join("Cargo.toml"), manifest).unwrap();
        std::fs::write(project.join("Cargo.lock"), lockfile).unwrap();
        std::fs::write(project.join("src/lib.rs"), "pub fn answer() -> u8 { 42 }\n").unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&project)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "inspect@example.invalid"]);
        git(&["config", "user.name", "Inspect Smoke"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "seed"]);

        let run_inspect = |command: &str| {
            run_shell_with_profiles_in_sandbox(
                1,
                &unrestricted_test_policy(),
                &ShellConfig::default(),
                &projects_dir,
                &PreparedShellProfileCache::default(),
                Some(project.to_string_lossy().as_ref()),
                command,
                None,
                60,
                None,
                Some(crate::command_sandbox::INSPECT_SANDBOX_MODE),
            )
        };

        let inspection = run_inspect(
            "rg 'inspect-runner-smoke' Cargo.toml \
             && git status --short \
             && cargo check --offline \
             && printf scratch-ok > \"$TMPDIR/proof\" \
             && test \"$(cat \"$TMPDIR/proof\")\" = scratch-ok",
        );
        assert_eq!(inspection.exit_code, Some(0), "{inspection:?}");
        assert!(!project.join("target").exists());

        for command in [
            "printf created > created.txt",
            "printf changed > Cargo.toml",
            "truncate -s 0 Cargo.toml",
            "rm Cargo.toml",
            "mv Cargo.toml renamed.toml",
            "sh -c 'printf child > child.txt'",
        ] {
            let denied = run_inspect(command);
            assert_ne!(denied.exit_code, Some(0), "{command}: {denied:?}");
        }
        assert_eq!(
            std::fs::read_to_string(project.join("Cargo.toml")).unwrap(),
            manifest
        );
        assert!(!project.join("created.txt").exists());
        assert!(!project.join("child.txt").exists());
        assert!(!project.join("renamed.toml").exists());

        let normal = run_shell(
            &unrestricted_test_policy(),
            &ShellConfig::default(),
            Some(project.to_string_lossy().as_ref()),
            "printf normal-ok > normal.txt",
            None,
            10,
            None,
        );
        assert_eq!(normal.exit_code, Some(0), "{normal:?}");
        assert_eq!(
            std::fs::read_to_string(project.join("normal.txt")).unwrap(),
            "normal-ok"
        );
    }

    #[test]
    fn shell_config_path_prepend_discovers_fake_executable() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        let exe = bin_dir.join("webcodex-fake-tool");
        std::fs::write(&exe, "#!/bin/sh\nprintf fake-tool-ok\n").unwrap();
        let mut perms = std::fs::metadata(&exe).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&exe, perms).unwrap();
        let shell = ShellConfig {
            path_prepend: vec![bin_dir],
            ..ShellConfig::default()
        };
        let cwd = tmp.path().to_string_lossy().to_string();
        let result = run_shell(
            &cfg.policy,
            &shell,
            Some(&cwd),
            "webcodex-fake-tool",
            None,
            10,
            None,
        );
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.as_deref(), Some("fake-tool-ok"));
    }

    #[test]
    fn shell_config_env_values_are_available() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let shell = ShellConfig {
            env: HashMap::from([("WEBCODEX_TEST_VALUE".to_string(), "env-ok".to_string())]),
            ..ShellConfig::default()
        };
        let cwd = tmp.path().to_string_lossy().to_string();
        let result = run_shell(
            &cfg.policy,
            &shell,
            Some(&cwd),
            "printf %s \"$WEBCODEX_TEST_VALUE\"",
            None,
            10,
            None,
        );
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.as_deref(), Some("env-ok"));
    }

    #[test]
    fn shell_config_init_script_is_sourced() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let init = tmp.path().join("init.sh");
        std::fs::write(&init, "export WEBCODEX_INIT_TEST=init-ok\n").unwrap();
        let shell = ShellConfig {
            init_script: Some(init),
            ..ShellConfig::default()
        };
        let cwd = tmp.path().to_string_lossy().to_string();
        let result = run_shell(
            &cfg.policy,
            &shell,
            Some(&cwd),
            "printf %s \"$WEBCODEX_INIT_TEST\"",
            None,
            10,
            None,
        );
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.as_deref(), Some("init-ok"));
    }

    #[test]
    fn shell_config_bash_like_args_are_respected_when_available() {
        if !Path::new("/bin/bash").exists() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let shell = ShellConfig {
            program: "/bin/bash".to_string(),
            args: vec!["-lc".to_string()],
            ..ShellConfig::default()
        };
        let cwd = tmp.path().to_string_lossy().to_string();
        let result = run_shell(
            &cfg.policy,
            &shell,
            Some(&cwd),
            "[[ 1 -eq 1 ]] && printf bash-ok",
            None,
            10,
            None,
        );
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.as_deref(), Some("bash-ok"));
    }

    fn shell_with_profiles(
        default_profile: Option<&str>,
        profiles: Vec<(&str, ShellProfileConfig)>,
    ) -> ShellConfig {
        ShellConfig {
            default_profile: default_profile.map(str::to_string),
            profiles: profiles
                .into_iter()
                .map(|(name, profile)| (name.to_string(), profile))
                .collect(),
            ..ShellConfig::default()
        }
    }

    fn profile_env(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn write_agent_project(
        projects_dir: &Path,
        id: &str,
        path: &Path,
        shell_profile: Option<&str>,
    ) {
        std::fs::create_dir_all(projects_dir).unwrap();
        let shell_profile = shell_profile
            .map(|profile| format!("shell_profile = {:?}\n", profile))
            .unwrap_or_default();
        std::fs::write(
            projects_dir.join(format!("{}.toml", id)),
            format!(
                "id = {:?}\npath = {:?}\nname = {:?}\n{}",
                id,
                path.to_string_lossy(),
                id,
                shell_profile
            ),
        )
        .unwrap();
    }

    fn run_profile_shell(
        policy: &AgentPolicy,
        shell: &ShellConfig,
        projects_dir: &Path,
        cache: &PreparedShellProfileCache,
        cwd: &Path,
        command: &str,
    ) -> CommandResult {
        let cwd = cwd.to_string_lossy().to_string();
        run_shell_with_profiles(
            1,
            policy,
            shell,
            projects_dir,
            cache,
            Some(&cwd),
            command,
            None,
            10,
            None,
        )
    }

    #[test]
    fn prepared_profile_env_is_available_to_run_shell() {
        let tmp = tempfile::tempdir().unwrap();
        let shell = shell_with_profiles(
            Some("test"),
            vec![(
                "test",
                ShellProfileConfig {
                    env: profile_env(&[("WEBCODEX_TEST_PROFILE", "from_env")]),
                    ..ShellProfileConfig::default()
                },
            )],
        );
        let result = run_profile_shell(
            &unrestricted_test_policy(),
            &shell,
            tmp.path(),
            &PreparedShellProfileCache::default(),
            tmp.path(),
            "printf %s \"$WEBCODEX_TEST_PROFILE\"",
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(result.stdout.as_deref(), Some("from_env"));
    }

    #[test]
    fn prepared_profile_init_script_export_is_available_to_run_shell() {
        let tmp = tempfile::tempdir().unwrap();
        let shell = shell_with_profiles(
            Some("test"),
            vec![(
                "test",
                ShellProfileConfig {
                    program: Some("/bin/sh".to_string()),
                    args: Some(vec!["-c".to_string()]),
                    init_script: Some("export WEBCODEX_TEST_PROFILE=from_snapshot".to_string()),
                    ..ShellProfileConfig::default()
                },
            )],
        );
        let result = run_profile_shell(
            &unrestricted_test_policy(),
            &shell,
            tmp.path(),
            &PreparedShellProfileCache::default(),
            tmp.path(),
            "printf %s \"$WEBCODEX_TEST_PROFILE\"",
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(result.stdout.as_deref(), Some("from_snapshot"));
    }

    #[test]
    fn prepared_profile_init_script_is_project_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("project");
        let projects_dir = tmp.path().join("projects.d");
        std::fs::create_dir_all(project_dir.join(".venv/bin")).unwrap();
        std::fs::write(
            project_dir.join(".venv/bin/activate"),
            "export WEBCODEX_PROJECT_VENV=project_local\n",
        )
        .unwrap();
        write_agent_project(&projects_dir, "demo", &project_dir, Some("py-venv"));
        let shell = shell_with_profiles(
            None,
            vec![(
                "py-venv",
                ShellProfileConfig {
                    program: Some("/bin/sh".to_string()),
                    args: Some(vec!["-c".to_string()]),
                    init_script: Some(". .venv/bin/activate".to_string()),
                    ..ShellProfileConfig::default()
                },
            )],
        );
        let result = run_profile_shell(
            &unrestricted_test_policy(),
            &shell,
            &projects_dir,
            &PreparedShellProfileCache::default(),
            &project_dir,
            "printf %s \"$WEBCODEX_PROJECT_VENV\"",
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(result.stdout.as_deref(), Some("project_local"));
    }

    #[test]
    fn project_shell_profile_overrides_default_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("project");
        let projects_dir = tmp.path().join("projects.d");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_agent_project(&projects_dir, "demo", &project_dir, Some("project"));
        let shell = shell_with_profiles(
            Some("default"),
            vec![
                (
                    "default",
                    ShellProfileConfig {
                        env: profile_env(&[("WEBCODEX_TEST_PROFILE", "default")]),
                        ..ShellProfileConfig::default()
                    },
                ),
                (
                    "project",
                    ShellProfileConfig {
                        env: profile_env(&[("WEBCODEX_TEST_PROFILE", "project")]),
                        ..ShellProfileConfig::default()
                    },
                ),
            ],
        );
        let result = run_profile_shell(
            &unrestricted_test_policy(),
            &shell,
            &projects_dir,
            &PreparedShellProfileCache::default(),
            &project_dir,
            "printf %s \"$WEBCODEX_TEST_PROFILE\"",
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(result.stdout.as_deref(), Some("project"));
    }

    fn shell_job_request(cwd: &Path, command: &str) -> ShellAgentShellRequest {
        ShellAgentShellRequest {
            request_id: "req-job".to_string(),
            client_id: "ws-client".to_string(),
            kind: "start_job".to_string(),
            job_id: Some("job-profile".to_string()),
            cwd: Some(cwd.to_string_lossy().to_string()),
            path: None,
            content: None,
            max_bytes: None,
            old_text: None,
            pattern: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            line: None,
            create_dirs: false,
            command: command.to_string(),
            stdin: None,
            timeout_secs: 10,
            requested_by: "tester".to_string(),
            created_at: 0,
            validation: None,
            lsp: None,
            sandbox: None,
        }
    }

    fn wait_for_job_stdout(rx: &mut tokio::sync::mpsc::Receiver<AgentEnvelope>) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stdout = String::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(AgentEnvelope::JobUpdate { payload }) => {
                    if let Some(chunk) = payload.stdout_chunk {
                        stdout.push_str(&chunk);
                    }
                    if payload.finished {
                        return stdout;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        panic!("timed out waiting for job completion; stdout so far: {stdout:?}");
    }

    fn line_edit_request(
        cwd: &Path,
        kind: &str,
        path: &str,
        content: Option<&str>,
        start_line: Option<usize>,
        end_line: Option<usize>,
        line: Option<usize>,
        expected_sha256: Option<String>,
        expected_prefix: Option<&str>,
    ) -> ShellAgentShellRequest {
        ShellAgentShellRequest {
            request_id: format!("req-{kind}"),
            client_id: "agent-1".to_string(),
            kind: kind.to_string(),
            job_id: None,
            cwd: Some(cwd.to_string_lossy().to_string()),
            path: Some(path.to_string()),
            content: content.map(str::to_string),
            max_bytes: None,
            old_text: None,
            pattern: None,
            expected_sha256,
            expected_prefix: expected_prefix.map(str::to_string),
            start_line,
            end_line,
            line,
            create_dirs: false,
            command: String::new(),
            stdin: None,
            timeout_secs: 30,
            requested_by: "tester".to_string(),
            created_at: 0,
            validation: None,
            lsp: None,
            sandbox: None,
        }
    }

    fn anchor_edit_request(
        cwd: &Path,
        kind: &str,
        path: &str,
        old_text: Option<&str>,
        pattern: Option<&str>,
        content: Option<&str>,
        expected_sha256: Option<String>,
    ) -> ShellAgentShellRequest {
        ShellAgentShellRequest {
            request_id: format!("req-{kind}"),
            client_id: "agent-1".to_string(),
            kind: kind.to_string(),
            job_id: None,
            cwd: Some(cwd.to_string_lossy().to_string()),
            path: Some(path.to_string()),
            content: content.map(str::to_string),
            max_bytes: None,
            old_text: old_text.map(str::to_string),
            pattern: pattern.map(str::to_string),
            expected_sha256,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            line: None,
            create_dirs: false,
            command: String::new(),
            stdin: None,
            timeout_secs: 30,
            requested_by: "tester".to_string(),
            created_at: 0,
            validation: None,
            lsp: None,
            sandbox: None,
        }
    }

    fn file_read_request(
        cwd: &Path,
        path: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
        max_bytes: Option<usize>,
    ) -> ShellAgentShellRequest {
        ShellAgentShellRequest {
            request_id: "req-file-read".to_string(),
            client_id: "agent-1".to_string(),
            kind: "file_read".to_string(),
            job_id: None,
            cwd: Some(cwd.to_string_lossy().to_string()),
            path: Some(path.to_string()),
            content: None,
            max_bytes,
            old_text: None,
            pattern: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line,
            end_line,
            line: None,
            create_dirs: false,
            command: String::new(),
            stdin: None,
            timeout_secs: 30,
            requested_by: "tester".to_string(),
            created_at: 0,
            validation: None,
            lsp: None,
            sandbox: None,
        }
    }

    fn line_edit_json(result: CommandResult) -> serde_json::Value {
        assert_eq!(result.exit_code, Some(0), "unexpected result: {:?}", result);
        assert!(
            result.error.is_none(),
            "unexpected error: {:?}",
            result.error
        );
        serde_json::from_str(result.stdout.as_deref().expect("stdout json")).unwrap()
    }

    fn file_read_json(result: CommandResult) -> serde_json::Value {
        assert_eq!(result.exit_code, Some(0), "unexpected result: {:?}", result);
        assert!(
            result.error.is_none(),
            "unexpected error: {:?}",
            result.error
        );
        serde_json::from_str(result.stdout.as_deref().expect("stdout json")).unwrap()
    }

    #[test]
    fn agent_file_read_without_range_preserves_plain_text_output() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::write(tmp.path().join("small.txt"), "one\ntwo\n").unwrap();

        let out = handle_file_request(
            &policy,
            &file_read_request(tmp.path(), "small.txt", None, None, Some(1024)),
        );

        assert_eq!(out.exit_code, Some(0), "unexpected result: {out:?}");
        assert_eq!(out.stdout.as_deref(), Some("one\ntwo\n"));
    }

    #[cfg(unix)]
    #[test]
    fn agent_file_read_rejects_symlink_escape_even_when_policy_allows_target() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "outside-secret").unwrap();
        std::os::unix::fs::symlink(&secret, project.path().join("leak.txt")).unwrap();

        let mut policy = project_policy(project.path());
        policy.allowed_roots.push(outside.path().to_path_buf());
        let out = handle_file_request(
            &policy,
            &file_read_request(project.path(), "leak.txt", None, None, Some(1024)),
        );

        assert_eq!(out.exit_code, None);
        assert_eq!(
            out.error.as_deref(),
            Some("file_read path escapes project root")
        );
        assert!(!out.stdout.unwrap_or_default().contains("outside-secret"));
    }

    #[test]
    fn agent_file_read_range_reads_large_file_subset_under_max_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let mut content = String::new();
        for n in 1..=500 {
            content.push_str(&format!("line-{n:04}\n"));
        }
        let expected_sha256 = sha256_hex_bytes(content.as_bytes());
        std::fs::write(tmp.path().join("large.txt"), content).unwrap();

        let out = file_read_json(handle_file_request(
            &policy,
            &file_read_request(tmp.path(), "large.txt", Some(250), Some(252), Some(128)),
        ));

        assert_eq!(out["format"], "webcodex.file_read_range.v1");
        assert_eq!(out["content"], "line-0250\nline-0251\nline-0252");
        assert_eq!(out["total_lines"], 500);
        assert_eq!(out["start_line"], 250);
        assert_eq!(out["limit"], 3);
        assert_eq!(out["sha256"], expected_sha256);
    }

    #[test]
    fn agent_file_read_range_beyond_total_lines_returns_empty_content() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::write(tmp.path().join("short.txt"), "one\ntwo\nthree\n").unwrap();

        let out = file_read_json(handle_file_request(
            &policy,
            &file_read_request(tmp.path(), "short.txt", Some(10), Some(12), Some(128)),
        ));

        assert_eq!(out["format"], "webcodex.file_read_range.v1");
        assert_eq!(out["content"], "");
        assert_eq!(out["total_lines"], 3);
        assert_eq!(out["start_line"], 10);
        assert_eq!(out["limit"], 3);
    }

    #[test]
    fn agent_file_read_range_preserves_empty_selected_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::write(tmp.path().join("blank.txt"), "\nsecond\nthird\n").unwrap();

        let out = file_read_json(handle_file_request(
            &policy,
            &file_read_request(tmp.path(), "blank.txt", Some(1), Some(2), Some(128)),
        ));

        assert_eq!(out["format"], "webcodex.file_read_range.v1");
        assert_eq!(out["content"], "\nsecond");
        assert_eq!(out["total_lines"], 3);
        assert_eq!(out["start_line"], 1);
        assert_eq!(out["limit"], 2);
    }

    #[test]
    fn agent_file_read_range_output_obeys_max_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::write(tmp.path().join("limited.txt"), "alpha\nbeta\n").unwrap();

        let out = handle_file_request(
            &policy,
            &file_read_request(tmp.path(), "limited.txt", Some(1), Some(1), Some(4)),
        );

        assert!(out.exit_code.is_none(), "unexpected success: {out:?}");
        assert!(out.error.expect("error").contains("exceeds max_bytes"));
    }

    #[test]
    fn replace_exact_block_replaces_single_block() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("anchor.txt");
        std::fs::write(&file, "alpha\nold block\nomega\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &anchor_edit_request(
                tmp.path(),
                "file_replace_exact_block",
                "anchor.txt",
                Some("old block\n"),
                None,
                Some("new block\n"),
                None,
            ),
        ));
        assert_eq!(out["changed"], true);
        assert_eq!(out["matches_replaced"], 1);
        assert_eq!(out["bytes_before"], "alpha\nold block\nomega\n".len());
        assert_eq!(out["bytes_after"], "alpha\nnew block\nomega\n".len());
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nnew block\nomega\n"
        );
    }

    #[test]
    fn replace_exact_block_accepts_matching_whole_file_sha256_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("anchor.txt");
        let original = "alpha\nold block\nomega\n";
        std::fs::write(&file, original).unwrap();
        let whole_file_sha256 = sha256_hex_bytes(original.as_bytes());

        let out = line_edit_json(handle_file_request(
            &policy,
            &anchor_edit_request(
                tmp.path(),
                "file_replace_exact_block",
                "anchor.txt",
                Some("old block\n"),
                None,
                Some("new block\n"),
                Some(whole_file_sha256),
            ),
        ));
        assert_eq!(out["changed"], true);
        assert_eq!(out["matches_replaced"], 1);
        assert_ne!(
            out.get("error").and_then(|v| v.as_str()),
            Some("expected_old_sha256 mismatch")
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nnew block\nomega\n"
        );
    }

    #[test]
    fn replace_exact_block_rejects_mismatched_whole_file_sha256_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("anchor.txt");
        let original = "alpha\nold block\nomega\n";
        std::fs::write(&file, original).unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &anchor_edit_request(
                tmp.path(),
                "file_replace_exact_block",
                "anchor.txt",
                Some("old block\n"),
                None,
                Some("new block\n"),
                Some(
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
                ),
            ),
        ));
        assert_eq!(out["changed"], false);
        let err = out["error"].as_str().unwrap();
        assert!(err.contains("expected_old_sha256 mismatch"));
        assert!(err.contains("No files were modified"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    }

    #[test]
    fn replace_exact_block_rejects_missing_old_text_without_write() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("anchor.txt");
        std::fs::write(&file, "alpha\nomega\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &anchor_edit_request(
                tmp.path(),
                "file_replace_exact_block",
                "anchor.txt",
                Some("missing"),
                None,
                Some("new"),
                None,
            ),
        ));
        let err = out["error"].as_str().unwrap();
        assert!(err.contains("Rejected before write"));
        assert!(err.contains("No files were modified"));
        assert!(err.contains("Retry guidance"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\nomega\n");
    }

    #[test]
    fn replace_exact_block_rejects_multiple_matches_without_write() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("anchor.txt");
        std::fs::write(&file, "dup\ndup\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &anchor_edit_request(
                tmp.path(),
                "file_replace_exact_block",
                "anchor.txt",
                Some("dup"),
                None,
                Some("x"),
                None,
            ),
        ));
        let err = out["error"].as_str().unwrap();
        assert!(err.contains("Rejected before write"));
        assert!(err.contains("expected exactly one match"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "dup\ndup\n");
    }

    #[test]
    fn replace_exact_block_rejects_empty_old_text() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::write(tmp.path().join("anchor.txt"), "alpha\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &anchor_edit_request(
                tmp.path(),
                "file_replace_exact_block",
                "anchor.txt",
                Some(""),
                None,
                Some("x"),
                None,
            ),
        ));
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("old_text must be non-empty"));
    }

    #[test]
    fn replace_exact_block_rejects_non_utf8_file() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("binary.bin");
        std::fs::write(&file, [0xff, 0xfe, 0xfd]).unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &anchor_edit_request(
                tmp.path(),
                "file_replace_exact_block",
                "binary.bin",
                Some("old"),
                None,
                Some("new"),
                None,
            ),
        ));
        assert!(out["error"].as_str().unwrap().contains("not valid UTF-8"));
        assert_eq!(std::fs::read(&file).unwrap(), vec![0xff, 0xfe, 0xfd]);
    }

    #[test]
    fn insert_before_pattern_inserts_before_single_literal_match() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("anchor.txt");
        std::fs::write(&file, "alpha\nomega\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &anchor_edit_request(
                tmp.path(),
                "file_insert_before_pattern",
                "anchor.txt",
                None,
                Some("omega"),
                Some("before\n"),
                None,
            ),
        ));
        assert_eq!(out["pattern_matches"], 1);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nbefore\nomega\n"
        );
    }

    #[test]
    fn insert_after_pattern_inserts_after_single_literal_match() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("anchor.txt");
        std::fs::write(&file, "alpha\nomega\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &anchor_edit_request(
                tmp.path(),
                "file_insert_after_pattern",
                "anchor.txt",
                None,
                Some("alpha"),
                Some("-after"),
                None,
            ),
        ));
        assert_eq!(out["pattern_matches"], 1);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha-after\nomega\n"
        );
    }

    #[test]
    fn insert_pattern_rejects_missing_pattern_without_write() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("anchor.txt");
        std::fs::write(&file, "alpha\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &anchor_edit_request(
                tmp.path(),
                "file_insert_before_pattern",
                "anchor.txt",
                None,
                Some("missing"),
                Some("x"),
                None,
            ),
        ));
        let err = out["error"].as_str().unwrap();
        assert!(err.contains("Rejected before write"));
        assert!(err.contains("No files were modified"));
        assert!(err.contains("Retry guidance"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");
    }

    #[test]
    fn insert_pattern_rejects_multiple_matches_without_write() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("anchor.txt");
        std::fs::write(&file, "x-x-x").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &anchor_edit_request(
                tmp.path(),
                "file_insert_after_pattern",
                "anchor.txt",
                None,
                Some("x"),
                Some("!"),
                None,
            ),
        ));
        let err = out["error"].as_str().unwrap();
        assert!(err.contains("expected exactly one match"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x-x-x");
    }

    #[test]
    fn insert_pattern_rejects_empty_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::write(tmp.path().join("anchor.txt"), "alpha\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &anchor_edit_request(
                tmp.path(),
                "file_insert_before_pattern",
                "anchor.txt",
                None,
                Some(""),
                Some("x"),
                None,
            ),
        ));
        assert!(out["error"].as_str().unwrap().contains("literal pattern"));
    }

    #[test]
    fn insert_pattern_rejects_empty_text() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::write(tmp.path().join("anchor.txt"), "alpha\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &anchor_edit_request(
                tmp.path(),
                "file_insert_after_pattern",
                "anchor.txt",
                None,
                Some("alpha"),
                Some(""),
                None,
            ),
        ));
        let err = out["error"].as_str().unwrap();
        assert!(err.contains("inserted text must not be empty"));
        assert!(err.contains("Retry guidance"));
    }

    fn apply_text_edits_request(
        cwd: &Path,
        path: &str,
        mut payload: serde_json::Value,
    ) -> ShellAgentShellRequest {
        if payload.get("changes").is_none() {
            let expected_sha256 = payload
                .get("expected_file_sha256")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    sha256_hex_bytes(&std::fs::read(cwd.join(path)).unwrap_or_default())
                });
            payload = serde_json::json!({
                "dry_run": payload.get("dry_run").cloned().unwrap_or(serde_json::Value::Bool(false)),
                "changes": [{
                    "kind": "edit",
                    "path": path,
                    "expected_sha256": expected_sha256,
                    "edits": payload.get("edits").cloned().unwrap_or_else(|| serde_json::json!([]))
                }]
            });
        }
        ShellAgentShellRequest {
            request_id: "req-apply-text-edits".to_string(),
            client_id: "agent-1".to_string(),
            kind: "file_apply_text_edits".to_string(),
            job_id: None,
            cwd: Some(cwd.to_string_lossy().to_string()),
            path: Some(path.to_string()),
            content: Some(payload.to_string()),
            max_bytes: None,
            old_text: None,
            pattern: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            line: None,
            create_dirs: false,
            command: String::new(),
            stdin: None,
            timeout_secs: 30,
            requested_by: "tester".to_string(),
            created_at: 0,
            validation: None,
            lsp: None,
            sandbox: None,
        }
    }

    fn json_file_op_request(
        cwd: &Path,
        kind: &str,
        path: &str,
        payload: serde_json::Value,
    ) -> ShellAgentShellRequest {
        ShellAgentShellRequest {
            request_id: format!("req-{kind}"),
            client_id: "agent-1".to_string(),
            kind: kind.to_string(),
            job_id: None,
            cwd: Some(cwd.to_string_lossy().to_string()),
            path: Some(path.to_string()),
            content: Some(payload.to_string()),
            max_bytes: None,
            old_text: None,
            pattern: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            line: None,
            create_dirs: false,
            command: String::new(),
            stdin: None,
            timeout_secs: 30,
            requested_by: "tester".to_string(),
            created_at: 0,
            validation: None,
            lsp: None,
            sandbox: None,
        }
    }

    fn fake_zip_eocd_with_entries(entries: u16) -> Vec<u8> {
        let mut bytes = b"PK\x05\x06".to_vec();
        bytes.extend_from_slice(&[0, 0]); // disk number
        bytes.extend_from_slice(&[0, 0]); // central directory disk
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]); // central directory size
        bytes.extend_from_slice(&[0, 0, 0, 0]); // central directory offset
        bytes.extend_from_slice(&[0, 0]); // comment length
        bytes
    }

    fn artifact_upload_temp_paths(
        root: &Path,
        artifact_path: &str,
        upload_id: &str,
    ) -> (PathBuf, PathBuf) {
        let target = root.join(artifact_path);
        let parent = target.parent().expect("artifact path parent");
        (
            parent.join(format!(".wc-upload-{upload_id}.part")),
            parent.join(format!(".wc-upload-{upload_id}.json")),
        )
    }

    fn directory_contains_name_prefix(dir: &Path, prefix: &str) -> bool {
        if !dir.exists() {
            return false;
        }
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .any(|name| name.starts_with(prefix))
    }

    fn assert_upload_temp_files_exist(root: &Path, artifact_path: &str, upload_id: &str) {
        let (part, sidecar) = artifact_upload_temp_paths(root, artifact_path, upload_id);
        assert!(
            part.exists(),
            "missing upload part file: {}",
            part.display()
        );
        assert!(
            sidecar.exists(),
            "missing upload sidecar file: {}",
            sidecar.display()
        );
        let parent = part.parent().unwrap();
        assert!(
            !directory_contains_name_prefix(parent, ".pd-upload-"),
            "legacy .pd upload temp files must not be created in {}",
            parent.display()
        );
    }

    fn assert_no_upload_temp_files(root: &Path, artifact_path: &str) {
        let target = root.join(artifact_path);
        let Some(parent) = target.parent() else {
            return;
        };
        assert!(
            !directory_contains_name_prefix(parent, ".wc-upload-"),
            "upload temp files remained in {}",
            parent.display()
        );
        assert!(
            !directory_contains_name_prefix(parent, ".pd-upload-"),
            "legacy .pd upload temp files remained in {}",
            parent.display()
        );
    }

    #[test]
    fn file_save_project_artifact_writes_binary_and_blocks_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let path = "artifacts/imports/tiny.png";
        let content_base64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            [0x89, b'P', b'N', b'G'],
        );
        let payload = serde_json::json!({
            "path": path,
            "content_base64": content_base64,
            "mime_type": "image/png",
            "overwrite": false,
            "max_bytes": 1024,
        });

        let out = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_save_project_artifact",
                path,
                payload.clone(),
            ),
        ));

        assert_eq!(out["path"], path);
        assert_eq!(out["bytes_written"], 4);
        assert_eq!(out["mime_type"], "image/png");
        assert_eq!(out["sha256"].as_str().unwrap().len(), 64);
        assert_eq!(
            std::fs::read(tmp.path().join(path)).unwrap(),
            vec![0x89, b'P', b'N', b'G']
        );
        let parent = tmp.path().join("artifacts/imports");
        assert!(
            !directory_contains_name_prefix(&parent, ".wc-artifact-"),
            "atomic artifact temp file should not remain"
        );
        assert!(
            !directory_contains_name_prefix(&parent, ".pd-artifact-"),
            "legacy .pd artifact temp file should not remain"
        );

        let out2 = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(tmp.path(), "file_save_project_artifact", path, payload),
        ));
        assert!(out2["error"]
            .as_str()
            .unwrap()
            .contains("overwrite is false"));
    }

    #[test]
    fn file_read_project_artifact_metadata_counts_zip_without_extracting() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let zip_path = tmp.path().join("sample.zip");
        std::fs::write(&zip_path, fake_zip_eocd_with_entries(2)).unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_read_project_artifact_metadata",
                "sample.zip",
                serde_json::json!({"path": "sample.zip", "max_bytes": 1024}),
            ),
        ));

        assert_eq!(out["mime_type"], "application/zip");
        assert_eq!(out["archive_entries_count"], 2);
        assert!(
            out["modified_at"].as_u64().unwrap() > 0,
            "modified_at should be unix timestamp seconds"
        );
        assert!(!tmp.path().join("a.txt").exists());
        assert!(!tmp.path().join("b.txt").exists());
    }

    #[test]
    fn file_read_project_artifact_reads_binary_chunks() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let bytes = [0, 159, 146, 150, b'a', b'b', b'c', b'd'];
        std::fs::write(tmp.path().join("data.bin"), bytes).unwrap();

        let first = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_read_project_artifact",
                "data.bin",
                serde_json::json!({"path": "data.bin", "offset": 0, "length": 4, "max_file_bytes": 1024}),
            ),
        ));
        assert_eq!(first["file_bytes"], bytes.len());
        assert_eq!(first["offset"], 0);
        assert_eq!(first["bytes_returned"], 4);
        assert_eq!(first["next_offset"], 4);
        assert_eq!(first["truncated"], true);
        assert_eq!(first["eof"], false);
        assert_eq!(
            first["content_base64"],
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes[..4])
        );

        let second = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_read_project_artifact",
                "data.bin",
                serde_json::json!({"path": "data.bin", "offset": 4, "length": 20, "max_file_bytes": 1024}),
            ),
        ));
        assert_eq!(second["sha256"], first["sha256"]);
        assert_eq!(second["offset"], 4);
        assert_eq!(second["bytes_returned"], bytes.len() - 4);
        assert_eq!(second["next_offset"], bytes.len());
        assert_eq!(second["truncated"], false);
        assert_eq!(second["eof"], true);
        assert_eq!(
            second["content_base64"],
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes[4..])
        );

        let at_eof = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_read_project_artifact",
                "data.bin",
                serde_json::json!({"path": "data.bin", "offset": bytes.len(), "length": 4, "max_file_bytes": 1024}),
            ),
        ));
        assert_eq!(at_eof["bytes_returned"], 0);
        assert_eq!(at_eof["next_offset"], bytes.len());
        assert_eq!(at_eof["truncated"], false);
        assert_eq!(at_eof["eof"], true);
    }

    #[test]
    fn file_artifact_upload_chunks_finish_and_abort() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let path = "artifacts/imports/upload.bin";
        let bytes = b"abcdefgh";
        let expected_sha256 = sha256_hex_bytes(bytes);

        let begin = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_begin",
                path,
                serde_json::json!({
                    "path": path,
                    "expected_bytes": bytes.len(),
                    "expected_sha256": expected_sha256,
                    "mime_type": null,
                    "overwrite": false,
                    "max_bytes": 1024,
                }),
            ),
        ));
        let upload_id = begin["upload_id"].as_str().unwrap().to_string();
        assert!(upload_id.starts_with("wc_upload_"));
        assert_eq!(begin["received_bytes"], 0);
        assert!(!tmp.path().join(path).exists());
        assert_upload_temp_files_exist(tmp.path(), path, &upload_id);

        let first = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes[..4]);
        let out = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                path,
                serde_json::json!({
                    "path": path,
                    "upload_id": upload_id.clone(),
                    "offset": 0,
                    "content_base64": first,
                    "max_chunk_bytes": 4,
                }),
            ),
        ));
        assert_eq!(out["received_bytes"], 4);
        assert_eq!(out["next_offset"], 4);
        assert!(!tmp.path().join(path).exists());
        assert_upload_temp_files_exist(tmp.path(), path, &upload_id);

        let second =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes[4..]);
        let out = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                path,
                serde_json::json!({
                    "path": path,
                    "upload_id": upload_id.clone(),
                    "offset": 4,
                    "content_base64": second,
                    "max_chunk_bytes": 4,
                }),
            ),
        ));
        assert_eq!(out["received_bytes"], bytes.len());

        let finish = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_finish",
                path,
                serde_json::json!({
                    "path": path,
                    "upload_id": upload_id.clone(),
                }),
            ),
        ));
        assert_eq!(finish["committed"], true);
        assert_eq!(finish["bytes"], bytes.len());
        assert_eq!(finish["sha256"], sha256_hex_bytes(bytes));
        assert_eq!(std::fs::read(tmp.path().join(path)).unwrap(), bytes);
        assert_no_upload_temp_files(tmp.path(), path);

        let abort_path = "artifacts/imports/abort.bin";
        let begin_abort = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_begin",
                abort_path,
                serde_json::json!({
                    "path": abort_path,
                    "expected_bytes": null,
                    "expected_sha256": null,
                    "mime_type": null,
                    "overwrite": false,
                    "max_bytes": 1024,
                }),
            ),
        ));
        let abort_upload_id = begin_abort["upload_id"].as_str().unwrap();
        assert_upload_temp_files_exist(tmp.path(), abort_path, abort_upload_id);
        let abort = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_abort",
                abort_path,
                serde_json::json!({
                    "path": abort_path,
                    "upload_id": abort_upload_id,
                }),
            ),
        ));
        assert_eq!(abort["aborted"], true);
        assert!(!tmp.path().join(abort_path).exists());
        assert_no_upload_temp_files(tmp.path(), abort_path);
    }

    #[test]
    fn file_artifact_upload_begin_rejects_validation_and_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());

        let sensitive = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_begin",
                ".env",
                serde_json::json!({
                    "path": ".env",
                    "expected_bytes": 1,
                    "expected_sha256": null,
                    "mime_type": "text/plain",
                    "overwrite": false,
                    "max_bytes": 1024,
                }),
            ),
        ));
        assert!(sensitive["error"]
            .as_str()
            .unwrap()
            .contains("sensitive artifact path"));

        let bad_hash_path = "artifacts/imports/bad-hash.txt";
        let bad_hash = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_begin",
                bad_hash_path,
                serde_json::json!({
                    "path": bad_hash_path,
                    "expected_bytes": 1,
                    "expected_sha256": "not-a-sha",
                    "mime_type": "text/plain",
                    "overwrite": false,
                    "max_bytes": 1024,
                }),
            ),
        ));
        assert!(bad_hash["error"]
            .as_str()
            .unwrap()
            .contains("expected_sha256 must be"));

        let too_large_path = "artifacts/imports/too-large.txt";
        let too_large = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_begin",
                too_large_path,
                serde_json::json!({
                    "path": too_large_path,
                    "expected_bytes": 5,
                    "expected_sha256": null,
                    "mime_type": "text/plain",
                    "overwrite": false,
                    "max_bytes": 4,
                }),
            ),
        ));
        assert_eq!(too_large["error"], "expected_bytes exceeds max_bytes");

        let unsafe_octet_path = "artifacts/imports/raw.bin";
        let unsafe_octet = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_begin",
                unsafe_octet_path,
                serde_json::json!({
                    "path": unsafe_octet_path,
                    "expected_bytes": 1,
                    "expected_sha256": null,
                    "mime_type": "application/octet-stream",
                    "overwrite": false,
                    "max_bytes": 1024,
                }),
            ),
        ));
        let unsafe_octet_error = unsafe_octet["error"].as_str().unwrap();
        assert!(unsafe_octet_error.contains(".artifact"));
        assert!(unsafe_octet_error.contains(".txt"));
        assert!(unsafe_octet_error.contains("artifacts/smoke/<name>.artifact"));
        assert_eq!(unsafe_octet["failure_kind"], "policy_rejected");

        let existing_path = "artifacts/imports/existing.txt";
        std::fs::create_dir_all(tmp.path().join("artifacts/imports")).unwrap();
        std::fs::write(tmp.path().join(existing_path), b"old").unwrap();
        let existing = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_begin",
                existing_path,
                serde_json::json!({
                    "path": existing_path,
                    "expected_bytes": 3,
                    "expected_sha256": null,
                    "mime_type": "text/plain",
                    "overwrite": false,
                    "max_bytes": 1024,
                }),
            ),
        ));
        assert_eq!(existing["error"], "file exists and overwrite is false");
        assert_eq!(
            std::fs::read(tmp.path().join(existing_path)).unwrap(),
            b"old"
        );

        #[cfg(unix)]
        {
            let symlink_path = "artifacts/imports/link.txt";
            let victim = tmp.path().join("victim.txt");
            std::fs::write(&victim, b"victim").unwrap();
            std::os::unix::fs::symlink(&victim, tmp.path().join(symlink_path)).unwrap();
            let symlink = line_edit_json(handle_file_request(
                &policy,
                &json_file_op_request(
                    tmp.path(),
                    "file_artifact_upload_begin",
                    symlink_path,
                    serde_json::json!({
                        "path": symlink_path,
                        "expected_bytes": 3,
                        "expected_sha256": null,
                        "mime_type": "text/plain",
                        "overwrite": true,
                        "max_bytes": 1024,
                    }),
                ),
            ));
            assert_eq!(
                symlink["error"],
                "refusing to overwrite symlink artifact path"
            );
            assert_eq!(std::fs::read(&victim).unwrap(), b"victim");
        }
    }

    #[test]
    fn file_artifact_upload_chunk_rejects_validation_and_keeps_final_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let path = "artifacts/imports/chunk.bin";
        let begin = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_begin",
                path,
                serde_json::json!({
                    "path": path,
                    "expected_bytes": null,
                    "expected_sha256": null,
                    "mime_type": null,
                    "overwrite": false,
                    "max_bytes": 1024 * 1024,
                }),
            ),
        ));
        let upload_id = begin["upload_id"].as_str().unwrap().to_string();

        let invalid_id = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                path,
                serde_json::json!({
                    "path": path,
                    "upload_id": "bad",
                    "offset": 0,
                    "content_base64": "YQ==",
                    "max_chunk_bytes": 64 * 1024,
                }),
            ),
        ));
        assert!(invalid_id["error"]
            .as_str()
            .unwrap()
            .contains("upload_id must start"));

        let invalid_base64 = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                path,
                serde_json::json!({
                    "path": path,
                    "upload_id": upload_id.clone(),
                    "offset": 0,
                    "content_base64": "not valid base64!",
                    "max_chunk_bytes": 64 * 1024,
                }),
            ),
        ));
        assert!(invalid_base64["error"]
            .as_str()
            .unwrap()
            .contains("invalid base64"));

        let empty = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                path,
                serde_json::json!({
                    "path": path,
                    "upload_id": upload_id.clone(),
                    "offset": 0,
                    "content_base64": "",
                    "max_chunk_bytes": 64 * 1024,
                }),
            ),
        ));
        assert!(empty["error"]
            .as_str()
            .unwrap()
            .contains("decoded chunk must contain at least 1 byte"));

        let too_large = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            vec![b'x'; 64 * 1024 + 1],
        );
        let too_large = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                path,
                serde_json::json!({
                    "path": path,
                    "upload_id": upload_id.clone(),
                    "offset": 0,
                    "content_base64": too_large,
                    "max_chunk_bytes": 64 * 1024,
                }),
            ),
        ));
        assert_eq!(too_large["error"], "decoded chunk too large");

        let first = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                path,
                serde_json::json!({
                    "path": path,
                    "upload_id": upload_id.clone(),
                    "offset": 0,
                    "content_base64": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"abc"),
                    "max_chunk_bytes": 64 * 1024,
                }),
            ),
        ));
        assert_eq!(first["received_bytes"], 3);
        assert!(!tmp.path().join(path).exists());

        let wrong_offset = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                path,
                serde_json::json!({
                    "path": path,
                    "upload_id": upload_id.clone(),
                    "offset": 0,
                    "content_base64": "ZA==",
                    "max_chunk_bytes": 64 * 1024,
                }),
            ),
        ));
        assert_eq!(
            wrong_offset["error"],
            "offset does not match received_bytes"
        );
        assert_eq!(wrong_offset["received_bytes"], 3);
        assert_eq!(wrong_offset["next_offset"], 3);

        let other_path = "artifacts/imports/other.bin";
        let mismatch = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                other_path,
                serde_json::json!({
                    "path": other_path,
                    "upload_id": upload_id.clone(),
                    "offset": 3,
                    "content_base64": "ZA==",
                    "max_chunk_bytes": 64 * 1024,
                }),
            ),
        ));
        assert_eq!(
            mismatch["error"],
            "upload_id does not belong to requested path"
        );
        assert!(!tmp.path().join(path).exists());
        assert_upload_temp_files_exist(tmp.path(), path, &upload_id);
    }

    #[test]
    fn file_artifact_upload_finish_validation_failures_keep_retry_state() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let path = "artifacts/imports/retry.bin";

        let begin = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_begin",
                path,
                serde_json::json!({
                    "path": path,
                    "expected_bytes": 4,
                    "expected_sha256": null,
                    "mime_type": null,
                    "overwrite": false,
                    "max_bytes": 1024,
                }),
            ),
        ));
        let upload_id = begin["upload_id"].as_str().unwrap().to_string();
        let first = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"abc");
        let chunk = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                path,
                serde_json::json!({
                    "path": path,
                    "upload_id": upload_id.clone(),
                    "offset": 0,
                    "content_base64": first,
                    "max_chunk_bytes": 64 * 1024,
                }),
            ),
        ));
        assert_eq!(chunk["received_bytes"], 3);

        let failed_finish = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_finish",
                path,
                serde_json::json!({"path": path, "upload_id": upload_id.clone()}),
            ),
        ));
        assert_eq!(
            failed_finish["error"],
            "uploaded byte count does not match expected_bytes"
        );
        assert_eq!(failed_finish["committed"], false);
        assert!(!tmp.path().join(path).exists());
        assert_upload_temp_files_exist(tmp.path(), path, &upload_id);

        let retry_chunk = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                path,
                serde_json::json!({
                    "path": path,
                    "upload_id": upload_id.clone(),
                    "offset": 3,
                    "content_base64": "ZA==",
                    "max_chunk_bytes": 64 * 1024,
                }),
            ),
        ));
        assert_eq!(retry_chunk["received_bytes"], 4);
        let finish = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_finish",
                path,
                serde_json::json!({"path": path, "upload_id": upload_id.clone()}),
            ),
        ));
        assert_eq!(finish["committed"], true);
        assert_eq!(std::fs::read(tmp.path().join(path)).unwrap(), b"abcd");
        assert_no_upload_temp_files(tmp.path(), path);

        let sha_path = "artifacts/imports/bad-sha.bin";
        let bad_sha = "0000000000000000000000000000000000000000000000000000000000000000";
        let begin_sha = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_begin",
                sha_path,
                serde_json::json!({
                    "path": sha_path,
                    "expected_bytes": null,
                    "expected_sha256": bad_sha,
                    "mime_type": null,
                    "overwrite": false,
                    "max_bytes": 1024,
                }),
            ),
        ));
        let sha_upload_id = begin_sha["upload_id"].as_str().unwrap().to_string();
        let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"abcd");
        let _ = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                sha_path,
                serde_json::json!({
                    "path": sha_path,
                    "upload_id": sha_upload_id.clone(),
                    "offset": 0,
                    "content_base64": data,
                    "max_chunk_bytes": 64 * 1024,
                }),
            ),
        ));
        let sha_failed = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_finish",
                sha_path,
                serde_json::json!({"path": sha_path, "upload_id": sha_upload_id.clone()}),
            ),
        ));
        assert_eq!(
            sha_failed["error"],
            "uploaded sha256 does not match expected_sha256"
        );
        assert_eq!(sha_failed["committed"], false);
        assert!(!tmp.path().join(sha_path).exists());
        assert_upload_temp_files_exist(tmp.path(), sha_path, &sha_upload_id);
    }

    #[test]
    fn file_artifact_upload_finish_refuses_late_target_when_overwrite_false() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let path = "artifacts/imports/race.bin";
        let begin = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_begin",
                path,
                serde_json::json!({
                    "path": path,
                    "expected_bytes": null,
                    "expected_sha256": null,
                    "mime_type": null,
                    "overwrite": false,
                    "max_bytes": 1024,
                }),
            ),
        ));
        let upload_id = begin["upload_id"].as_str().unwrap().to_string();
        let chunk = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"new");
        let _ = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                path,
                serde_json::json!({
                    "path": path,
                    "upload_id": upload_id.clone(),
                    "offset": 0,
                    "content_base64": chunk,
                    "max_chunk_bytes": 64 * 1024,
                }),
            ),
        ));
        std::fs::write(tmp.path().join(path), b"old").unwrap();
        let finish = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_finish",
                path,
                serde_json::json!({"path": path, "upload_id": upload_id.clone()}),
            ),
        ));
        assert_eq!(finish["error"], "file exists and overwrite is false");
        assert_eq!(std::fs::read(tmp.path().join(path)).unwrap(), b"old");
        assert_upload_temp_files_exist(tmp.path(), path, &upload_id);
    }

    #[cfg(unix)]
    #[test]
    fn file_artifact_upload_finish_refuses_late_symlink_even_with_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let path = "artifacts/imports/symlink-race.bin";
        let begin = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_begin",
                path,
                serde_json::json!({
                    "path": path,
                    "expected_bytes": null,
                    "expected_sha256": null,
                    "mime_type": null,
                    "overwrite": true,
                    "max_bytes": 1024,
                }),
            ),
        ));
        let upload_id = begin["upload_id"].as_str().unwrap().to_string();
        let chunk = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"new");
        let _ = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                path,
                serde_json::json!({
                    "path": path,
                    "upload_id": upload_id.clone(),
                    "offset": 0,
                    "content_base64": chunk,
                    "max_chunk_bytes": 64 * 1024,
                }),
            ),
        ));
        let victim = tmp.path().join("victim-race.bin");
        std::fs::write(&victim, b"victim").unwrap();
        std::os::unix::fs::symlink(&victim, tmp.path().join(path)).unwrap();
        let finish = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_finish",
                path,
                serde_json::json!({"path": path, "upload_id": upload_id.clone()}),
            ),
        ));
        assert_eq!(
            finish["error"],
            "refusing to overwrite symlink artifact path"
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"victim");
        assert_upload_temp_files_exist(tmp.path(), path, &upload_id);
    }

    #[test]
    fn file_artifact_upload_abort_rejects_wrong_ids_and_cleans_only_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let path = "artifacts/imports/abort-target.bin";
        std::fs::create_dir_all(tmp.path().join("artifacts/imports")).unwrap();
        std::fs::write(tmp.path().join(path), b"final").unwrap();
        let begin = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_begin",
                path,
                serde_json::json!({
                    "path": path,
                    "expected_bytes": null,
                    "expected_sha256": null,
                    "mime_type": null,
                    "overwrite": true,
                    "max_bytes": 1024,
                }),
            ),
        ));
        let upload_id = begin["upload_id"].as_str().unwrap().to_string();
        let chunk = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"temp");
        let _ = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_chunk",
                path,
                serde_json::json!({
                    "path": path,
                    "upload_id": upload_id.clone(),
                    "offset": 0,
                    "content_base64": chunk,
                    "max_chunk_bytes": 64 * 1024,
                }),
            ),
        ));

        let missing = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_abort",
                path,
                serde_json::json!({"path": path, "upload_id": "wc_upload_missing"}),
            ),
        ));
        assert!(missing["error"]
            .as_str()
            .unwrap()
            .contains("upload not found"));
        assert_eq!(std::fs::read(tmp.path().join(path)).unwrap(), b"final");
        assert_upload_temp_files_exist(tmp.path(), path, &upload_id);

        let other_path = "artifacts/imports/abort-other.bin";
        let mismatch = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_abort",
                other_path,
                serde_json::json!({"path": other_path, "upload_id": upload_id.clone()}),
            ),
        ));
        assert_eq!(
            mismatch["error"],
            "upload_id does not belong to requested path"
        );
        assert_upload_temp_files_exist(tmp.path(), path, &upload_id);

        let abort = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_artifact_upload_abort",
                path,
                serde_json::json!({"path": path, "upload_id": upload_id.clone()}),
            ),
        ));
        assert_eq!(abort["aborted"], true);
        assert_eq!(abort["received_bytes"], 4);
        assert_eq!(std::fs::read(tmp.path().join(path)).unwrap(), b"final");
        assert_no_upload_temp_files(tmp.path(), path);
    }

    #[cfg(unix)]
    #[test]
    fn file_project_artifact_ops_reject_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("outside.bin");
        std::fs::write(&outside, b"outside-secret-content").unwrap();
        std::os::unix::fs::symlink(&outside, root.path().join("leak.bin")).unwrap();
        let mut policy = project_policy(root.path());
        policy.allowed_roots.push(outside_dir.path().to_path_buf());

        let read = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                root.path(),
                "file_read_project_artifact",
                "leak.bin",
                serde_json::json!({"path":"leak.bin","offset":0,"length":8,"max_file_bytes":1024}),
            ),
        ));
        assert_eq!(read["error"], "artifact path escapes project root");
        assert!(!read.to_string().contains("outside-secret-content"));

        let metadata = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                root.path(),
                "file_read_project_artifact_metadata",
                "leak.bin",
                serde_json::json!({"path":"leak.bin","max_bytes":1024}),
            ),
        ));
        assert_eq!(metadata["error"], "artifact path escapes project root");
        assert!(!metadata.to_string().contains("outside-secret-content"));

        let save = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                root.path(),
                "file_save_project_artifact",
                "leak.bin",
                serde_json::json!({
                    "path":"leak.bin",
                    "content_base64":"bmV3",
                    "mime_type":"text/plain",
                    "overwrite":true,
                    "max_bytes":1024
                }),
            ),
        ));
        assert_eq!(save["error"], "refusing to overwrite symlink artifact path");
        assert_eq!(
            std::fs::read(&outside).expect("outside file remains readable"),
            b"outside-secret-content"
        );
        assert!(!save.to_string().contains("outside-secret-content"));
    }

    #[test]
    fn file_replace_in_file_replaces_multiple_when_expected_count_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("target.txt");
        std::fs::write(&file, "a a a").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_replace_in_file",
                "target.txt",
                serde_json::json!({
                    "path": "target.txt",
                    "old": "a",
                    "new": "b",
                    "expected_replacements": 3,
                    "allow_multiple": true,
                }),
            ),
        ));

        assert_eq!(out["changed"], true);
        assert_eq!(out["replacements"], 3);
        assert_eq!(out["before_sha256"].as_str().unwrap().len(), 64);
        assert_eq!(out["after_sha256"].as_str().unwrap().len(), 64);
        assert_eq!(out["bytes_written"], "b b b".len());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "b b b");
    }

    #[test]
    fn file_replace_in_file_rejects_missing_and_ambiguous_without_write() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let missing_file = tmp.path().join("missing.txt");
        let dup_file = tmp.path().join("dup.txt");
        std::fs::write(&missing_file, "hello world").unwrap();
        std::fs::write(&dup_file, "a a a").unwrap();

        let missing = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_replace_in_file",
                "missing.txt",
                serde_json::json!({
                    "old": "absent",
                    "new": "x",
                    "expected_replacements": 1,
                    "allow_multiple": false,
                }),
            ),
        ));
        assert_eq!(missing["changed"], false);
        assert!(missing["error"].as_str().unwrap().contains("not found"));
        assert_eq!(
            std::fs::read_to_string(&missing_file).unwrap(),
            "hello world"
        );

        let ambiguous = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_replace_in_file",
                "dup.txt",
                serde_json::json!({
                    "old": "a",
                    "new": "b",
                    "expected_replacements": 1,
                    "allow_multiple": false,
                }),
            ),
        ));
        assert_eq!(ambiguous["changed"], false);
        assert!(ambiguous["error"].as_str().unwrap().contains("multiple"));
        assert_eq!(std::fs::read_to_string(&dup_file).unwrap(), "a a a");
    }

    #[test]
    fn file_replace_in_file_rejects_count_mismatch_and_non_utf8_without_write() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let count_file = tmp.path().join("count.txt");
        let bin_file = tmp.path().join("bin.dat");
        std::fs::write(&count_file, "a a a").unwrap();
        std::fs::write(&bin_file, [0xFF, 0xFE, 0xFD]).unwrap();

        let count = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_replace_in_file",
                "count.txt",
                serde_json::json!({
                    "old": "a",
                    "new": "b",
                    "expected_replacements": 2,
                    "allow_multiple": true,
                }),
            ),
        ));
        assert_eq!(count["changed"], false);
        assert_eq!(count["occurrences"], 3);
        assert!(count["error"].as_str().unwrap().contains("mismatch"));
        assert_eq!(std::fs::read_to_string(&count_file).unwrap(), "a a a");

        let non_utf8 = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_replace_in_file",
                "bin.dat",
                serde_json::json!({
                    "old": "x",
                    "new": "y",
                    "expected_replacements": 1,
                    "allow_multiple": false,
                }),
            ),
        ));
        assert_eq!(non_utf8["changed"], false);
        assert!(non_utf8["error"].as_str().unwrap().contains("UTF-8"));
    }

    #[test]
    fn file_replace_in_file_rejects_string_allow_multiple() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("target.txt");
        std::fs::write(&file, "a a a").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_replace_in_file",
                "target.txt",
                serde_json::json!({
                    "old": "a",
                    "new": "b",
                    "expected_replacements": 3,
                    "allow_multiple": "false",
                }),
            ),
        ));

        assert_eq!(out["changed"], false);
        assert_eq!(out["error"], "allow_multiple must be a boolean");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a a a");
    }

    #[test]
    fn file_write_project_file_creates_parent_dirs_and_reports_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("nested/new.txt");

        let out = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_write_project_file",
                "nested/new.txt",
                serde_json::json!({
                    "path": "nested/new.txt",
                    "content": "line1\nline2\n",
                    "overwrite": false,
                    "expected_sha256": null,
                    "expected_content_prefix": null,
                }),
            ),
        ));

        assert_eq!(out["created"], true);
        assert_eq!(out["overwritten"], false);
        assert_eq!(out["bytes_written"], 12);
        assert_eq!(out["sha256"].as_str().unwrap().len(), 64);
        assert!(out["warning"].is_null());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "line1\nline2\n");
    }

    #[test]
    fn file_write_project_file_rejects_existing_without_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("target.txt");
        std::fs::write(&file, "original").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_write_project_file",
                "target.txt",
                serde_json::json!({
                    "content": "new",
                    "overwrite": false,
                    "expected_sha256": null,
                    "expected_content_prefix": null,
                }),
            ),
        ));

        assert_eq!(out["created"], false);
        assert_eq!(out["overwritten"], false);
        assert!(out["error"].as_str().unwrap().contains("overwrite"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
    }

    #[test]
    fn file_write_project_file_rejects_string_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("target.txt");
        std::fs::write(&file, "original").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_write_project_file",
                "target.txt",
                serde_json::json!({
                    "content": "new",
                    "overwrite": "false",
                    "expected_sha256": null,
                    "expected_content_prefix": null,
                }),
            ),
        ));

        assert_eq!(out["created"], false);
        assert_eq!(out["error"], "overwrite must be a boolean");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
    }

    #[test]
    fn file_write_project_file_enforces_sha_and_prefix_guards() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("target.txt");
        std::fs::write(&file, "original").unwrap();
        let original_sha = sha256_hex_bytes("original".as_bytes());

        let sha_ok = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_write_project_file",
                "target.txt",
                serde_json::json!({
                    "content": "v1 replaced",
                    "overwrite": true,
                    "expected_sha256": original_sha,
                    "expected_content_prefix": null,
                }),
            ),
        ));
        assert_eq!(sha_ok["overwritten"], true);
        assert!(sha_ok["warning"].is_null());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1 replaced");

        let prefix_ok = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_write_project_file",
                "target.txt",
                serde_json::json!({
                    "content": "v1 final",
                    "overwrite": true,
                    "expected_sha256": null,
                    "expected_content_prefix": "v1 ",
                }),
            ),
        ));
        assert_eq!(prefix_ok["overwritten"], true);
        assert!(prefix_ok["warning"].is_null());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1 final");

        let sha_bad = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_write_project_file",
                "target.txt",
                serde_json::json!({
                    "content": "bad",
                    "overwrite": true,
                    "expected_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                    "expected_content_prefix": null,
                }),
            ),
        ));
        assert_eq!(sha_bad["created"], false);
        assert!(sha_bad["error"].as_str().unwrap().contains("sha256"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1 final");
    }

    #[test]
    fn file_write_project_file_warns_on_unguarded_overwrite_and_rejects_bad_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("target.txt");
        std::fs::write(&file, "v2 content").unwrap();

        let prefix_bad = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_write_project_file",
                "target.txt",
                serde_json::json!({
                    "content": "bad",
                    "overwrite": true,
                    "expected_sha256": null,
                    "expected_content_prefix": "v1 ",
                }),
            ),
        ));
        assert_eq!(prefix_bad["created"], false);
        assert!(prefix_bad["error"].as_str().unwrap().contains("prefix"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2 content");

        let unguarded = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_write_project_file",
                "target.txt",
                serde_json::json!({
                    "content": "unguarded",
                    "overwrite": true,
                    "expected_sha256": null,
                    "expected_content_prefix": null,
                }),
            ),
        ));
        assert_eq!(unguarded["overwritten"], true);
        assert!(unguarded["warning"]
            .as_str()
            .unwrap()
            .contains("expected_sha256"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "unguarded");

        let nul = line_edit_json(handle_file_request(
            &policy,
            &json_file_op_request(
                tmp.path(),
                "file_write_project_file",
                "new.txt",
                serde_json::json!({
                    "content": "a\u{0000}b",
                    "overwrite": false,
                    "expected_sha256": null,
                    "expected_content_prefix": null,
                }),
            ),
        ));
        assert_eq!(nul["created"], false);
        assert!(nul["error"].as_str().unwrap().contains("NUL"));
        assert!(!tmp.path().join("new.txt").exists());
    }

    #[test]
    fn file_apply_text_edits_applies_multi_file_transaction() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::write(tmp.path().join("a.txt"), "alpha\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "beta\n").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "gamma\n").unwrap();
        let hash = |path: &str| sha256_hex_bytes(&std::fs::read(tmp.path().join(path)).unwrap());

        let out = line_edit_json(handle_file_request(
            &policy,
            &apply_text_edits_request(
                tmp.path(),
                "a.txt",
                serde_json::json!({
                    "changes": [
                        {
                            "kind": "edit",
                            "path": "a.txt",
                            "expected_sha256": hash("a.txt"),
                            "edits": [{"kind": "replace_exact", "old_text": "alpha", "new_text": "ALPHA"}]
                        },
                        {"kind": "create", "path": "nested/new.txt", "content": "new\n"},
                        {"kind": "delete", "path": "b.txt", "expected_sha256": hash("b.txt")},
                        {"kind": "rename", "path": "c.txt", "to_path": "moved/c.txt", "expected_sha256": hash("c.txt")}
                    ]
                }),
            ),
        ));

        assert_eq!(out["changed"], true);
        assert_eq!(out["applied_count"], 4);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "ALPHA\n"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("nested/new.txt")).unwrap(),
            "new\n"
        );
        assert!(!tmp.path().join("b.txt").exists());
        assert!(!tmp.path().join("c.txt").exists());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("moved/c.txt")).unwrap(),
            "gamma\n"
        );
        assert_eq!(out["files"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn file_apply_text_edits_hash_conflict_keeps_every_file_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::write(tmp.path().join("a.txt"), "alpha\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "beta\n").unwrap();
        let a_hash = sha256_hex_bytes(&std::fs::read(tmp.path().join("a.txt")).unwrap());

        let out = line_edit_json(handle_file_request(
            &policy,
            &apply_text_edits_request(
                tmp.path(),
                "a.txt",
                serde_json::json!({
                    "changes": [
                        {
                            "kind": "edit",
                            "path": "a.txt",
                            "expected_sha256": a_hash,
                            "edits": [{"kind": "replace_exact", "old_text": "alpha", "new_text": "ALPHA"}]
                        },
                        {
                            "kind": "delete",
                            "path": "b.txt",
                            "expected_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        }
                    ]
                }),
            ),
        ));

        assert_eq!(out["error_kind"], "sha256_conflict");
        assert_eq!(out["change_index"], 1);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "alpha\n"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("b.txt")).unwrap(),
            "beta\n"
        );
    }

    #[test]
    fn file_apply_text_edits_rejects_resolved_path_aliases() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/a.txt"), "alpha\n").unwrap();
        let hash = sha256_hex_bytes(&std::fs::read(tmp.path().join("src/a.txt")).unwrap());

        let out = line_edit_json(handle_file_request(
            &policy,
            &apply_text_edits_request(
                tmp.path(),
                "src/a.txt",
                serde_json::json!({
                    "changes": [
                        {
                            "kind": "edit",
                            "path": "src/a.txt",
                            "expected_sha256": hash,
                            "edits": [{"kind": "replace_exact", "old_text": "alpha", "new_text": "ALPHA"}]
                        },
                        {
                            "kind": "delete",
                            "path": "src//a.txt",
                            "expected_sha256": hash
                        }
                    ]
                }),
            ),
        ));

        assert_eq!(out["error_kind"], "path_overlap");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src/a.txt")).unwrap(),
            "alpha\n"
        );
    }

    #[test]
    fn file_apply_text_edits_replace_exact_writes_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("target.txt");
        std::fs::write(&file, "old\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &apply_text_edits_request(
                tmp.path(),
                "target.txt",
                serde_json::json!({
                    "edits": [
                        {"kind": "replace_exact", "old_text": "old", "new_text": "new"}
                    ]
                }),
            ),
        ));
        assert_eq!(out["changed"], true);
        assert_eq!(out["would_change"], true);
        assert_eq!(out["changed_paths"][0], "target.txt");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");
    }

    #[test]
    fn file_apply_text_edits_dry_run_does_not_write() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("target.txt");
        std::fs::write(&file, "old\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &apply_text_edits_request(
                tmp.path(),
                "target.txt",
                serde_json::json!({
                    "dry_run": true,
                    "edits": [
                        {"kind": "replace_exact", "old_text": "old", "new_text": "new"}
                    ]
                }),
            ),
        ));
        assert_eq!(out["dry_run"], true);
        assert_eq!(out["changed"], false);
        assert_eq!(out["would_change"], true);
        assert_eq!(out["changed_paths"][0], "target.txt");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");
    }

    #[test]
    fn file_apply_text_edits_rejects_missing_match_without_write() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("target.txt");
        std::fs::write(&file, "alpha\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &apply_text_edits_request(
                tmp.path(),
                "target.txt",
                serde_json::json!({
                    "edits": [
                        {"kind": "replace_exact", "old_text": "missing", "new_text": "x"}
                    ]
                }),
            ),
        ));
        let msg = out["error"].as_str().unwrap();
        assert!(msg.contains("match text was not found"));
        assert!(msg.contains("No files were modified"));
        assert_eq!(out["changed"], false);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");
    }

    #[test]
    fn file_apply_text_edits_rejects_ambiguous_match_without_write() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("target.txt");
        std::fs::write(&file, "dup-dup\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &apply_text_edits_request(
                tmp.path(),
                "target.txt",
                serde_json::json!({
                    "edits": [
                        {"kind": "replace_exact", "old_text": "dup", "new_text": "x"}
                    ]
                }),
            ),
        ));
        let msg = out["error"].as_str().unwrap();
        assert!(msg.contains("matched 2 times"));
        assert!(msg.contains("No files were modified"));
        assert_eq!(out["changed"], false);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "dup-dup\n");
    }

    #[test]
    fn file_apply_text_edits_expected_file_sha256_mismatch_without_write() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("target.txt");
        std::fs::write(&file, "alpha\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &apply_text_edits_request(
                tmp.path(),
                "target.txt",
                serde_json::json!({
                    "expected_file_sha256": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdead",
                    "edits": [
                        {"kind": "replace_exact", "old_text": "alpha", "new_text": "beta"}
                    ]
                }),
            ),
        ));
        let err = out["error"].as_str().unwrap();
        assert!(err.contains("expected_sha256 does not match"));
        assert!(err.contains("No files were modified"));
        assert_eq!(out["changed"], false);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");
    }

    #[test]
    fn file_apply_text_edits_insert_before_after_and_delete_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("target.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &apply_text_edits_request(
                tmp.path(),
                "target.txt",
                serde_json::json!({
                    "edits": [
                        {"kind": "insert_after", "anchor_text": "alpha\n", "new_text": "ALPHA-AFTER\n"},
                        {"kind": "delete_exact", "old_text": "beta\n"},
                        {"kind": "insert_before", "anchor_text": "gamma\n", "new_text": "GAMMA-BEFORE\n"}
                    ]
                }),
            ),
        ));
        assert_eq!(out["changed"], true);
        assert_eq!(out["applied_count"], 1);
        assert_eq!(out["files"][0]["edits"].as_array().unwrap().len(), 3);
        assert_eq!(out["changed_paths"][0], "target.txt");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nALPHA-AFTER\nGAMMA-BEFORE\ngamma\n"
        );
    }

    #[test]
    fn file_apply_text_edits_rejects_overlapping_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("target.txt");
        std::fs::write(&file, "abcdef\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &apply_text_edits_request(
                tmp.path(),
                "target.txt",
                serde_json::json!({
                    "edits": [
                        {"kind": "replace_exact", "old_text": "abc", "new_text": "ABC"},
                        {"kind": "replace_exact", "old_text": "cde", "new_text": "CDE"}
                    ]
                }),
            ),
        ));
        let err = out["error"].as_str().unwrap();
        assert!(err.contains("edits overlap"));
        assert!(err.contains("No files were modified"));
        assert_eq!(out["changed"], false);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "abcdef\n");
    }

    #[test]
    fn agent_native_line_edit_replace_insert_delete_happy_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        let file = tmp.path().join("src/example.rs");
        std::fs::write(&file, "one\ntwo\nthree\nfour\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &line_edit_request(
                tmp.path(),
                "file_replace_line_range",
                "src/example.rs",
                Some("TWO\nTHREE"),
                Some(2),
                Some(3),
                None,
                None,
                None,
            ),
        ));
        assert_eq!(out["changed"], true);
        assert_eq!(out["start_line"], 2);
        assert_eq!(out["end_line"], 3);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "one\nTWO\nTHREE\nfour\n"
        );

        let out = line_edit_json(handle_file_request(
            &policy,
            &line_edit_request(
                tmp.path(),
                "file_insert_at_line",
                "src/example.rs",
                Some("middle"),
                None,
                None,
                Some(2),
                None,
                None,
            ),
        ));
        assert_eq!(out["changed"], true);
        assert_eq!(out["line"], 2);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "one\nmiddle\nTWO\nTHREE\nfour\n"
        );

        let out = line_edit_json(handle_file_request(
            &policy,
            &line_edit_request(
                tmp.path(),
                "file_delete_line_range",
                "src/example.rs",
                None,
                Some(2),
                Some(3),
                None,
                None,
                None,
            ),
        ));
        assert_eq!(out["changed"], true);
        assert_eq!(out["old_line_count"], 2);
        assert_eq!(out["new_line_count"], 0);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "one\nTHREE\nfour\n"
        );
    }

    #[test]
    fn agent_native_line_edit_guards_reject_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        let file = tmp.path().join("example.rs");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &line_edit_request(
                tmp.path(),
                "file_replace_line_range",
                "example.rs",
                Some("TWO"),
                Some(2),
                Some(2),
                None,
                Some(
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
                ),
                None,
            ),
        ));
        assert_eq!(out["changed"], false);
        assert_eq!(out["error"], "expected_old_sha256 mismatch");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\nthree\n");

        let anchor = sha256_hex_bytes("two\n".as_bytes());
        let out = line_edit_json(handle_file_request(
            &policy,
            &line_edit_request(
                tmp.path(),
                "file_insert_at_line",
                "example.rs",
                Some("middle"),
                None,
                None,
                Some(2),
                Some(anchor),
                Some("three"),
            ),
        ));
        assert_eq!(out["changed"], false);
        assert_eq!(out["error"], "expected_anchor_prefix mismatch");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\nthree\n");
    }

    #[test]
    fn agent_native_line_edit_rejects_ranges_utf8_sensitive_and_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::write(tmp.path().join("example.rs"), "one\ntwo\n").unwrap();

        let out = line_edit_json(handle_file_request(
            &policy,
            &line_edit_request(
                tmp.path(),
                "file_delete_line_range",
                "example.rs",
                None,
                Some(2),
                Some(3),
                None,
                None,
                None,
            ),
        ));
        assert_eq!(out["changed"], false);
        assert_eq!(out["error"], "invalid line range");

        let out = line_edit_json(handle_file_request(
            &policy,
            &line_edit_request(
                tmp.path(),
                "file_insert_at_line",
                "example.rs",
                Some("three"),
                None,
                None,
                Some(3),
                None,
                None,
            ),
        ));
        assert_eq!(out["changed"], true);
        assert_eq!(out["old_line_count"], 0);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("example.rs")).unwrap(),
            "one\ntwo\nthree\n"
        );

        std::fs::write(tmp.path().join("bad.bin"), [0xff, 0xfe]).unwrap();
        let out = line_edit_json(handle_file_request(
            &policy,
            &line_edit_request(
                tmp.path(),
                "file_replace_line_range",
                "bad.bin",
                Some("ok"),
                Some(1),
                Some(1),
                None,
                None,
                None,
            ),
        ));
        assert_eq!(out["changed"], false);
        assert_eq!(out["error"], "file is not valid UTF-8");

        let sensitive = handle_file_request(
            &policy,
            &line_edit_request(
                tmp.path(),
                "file_replace_line_range",
                ".env",
                Some("SECRET=2"),
                Some(1),
                Some(1),
                None,
                None,
                None,
            ),
        );
        assert!(sensitive
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("sensitive"));

        let escaped = handle_file_request(
            &policy,
            &line_edit_request(
                tmp.path(),
                "file_replace_line_range",
                "../outside.txt",
                Some("x"),
                Some(1),
                Some(1),
                None,
                None,
                None,
            ),
        );
        assert!(escaped
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("escape"));
    }

    #[test]
    fn prepared_profile_run_shell_and_run_job_see_same_env() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("project");
        let projects_dir = tmp.path().join("projects.d");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_agent_project(&projects_dir, "demo", &project_dir, Some("test"));
        let shell = shell_with_profiles(
            None,
            vec![(
                "test",
                ShellProfileConfig {
                    env: profile_env(&[("WEBCODEX_TEST_PROFILE", "same")]),
                    ..ShellProfileConfig::default()
                },
            )],
        );
        let jobs = JobManager::new(1);
        let shell_result = run_profile_shell(
            &unrestricted_test_policy(),
            &shell,
            &projects_dir,
            &jobs.prepared_profiles,
            &project_dir,
            "printf %s \"$WEBCODEX_TEST_PROFILE\"",
        );
        assert_eq!(shell_result.stdout.as_deref(), Some("same"));

        let (sink, mut rx) = ws_sink("ws-client");
        let lsp = webcodex_runner::LspSupervisor::default();
        let mut cfg = test_config(projects_dir.clone());
        cfg.shell = shell.clone();
        let hot = runtime_config(&cfg);
        dispatch_request(
            &sink,
            &hot.snapshot(),
            &hot,
            &jobs,
            &projects_dir,
            &lsp,
            shell_job_request(&project_dir, "printf %s \"$WEBCODEX_TEST_PROFILE\""),
        )
        .unwrap();
        assert_eq!(wait_for_job_stdout(&mut rx), "same");
    }

    #[test]
    fn prepared_profile_init_script_runs_once_per_project_profile_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let counter = tmp.path().join("prepare-count");
        let init_script = format!(
            "count=$(cat {:?} 2>/dev/null || echo 0)\ncount=$((count + 1))\nprintf '%s\\n' \"$count\" > {:?}\nexport WEBCODEX_TEST_PROFILE=counted",
            counter.to_string_lossy(),
            counter.to_string_lossy()
        );
        let shell = shell_with_profiles(
            Some("test"),
            vec![(
                "test",
                ShellProfileConfig {
                    program: Some("sh".to_string()),
                    args: Some(vec!["-c".to_string()]),
                    init_script: Some(init_script),
                    ..ShellProfileConfig::default()
                },
            )],
        );
        let cache = PreparedShellProfileCache::default();
        for _ in 0..2 {
            let result = run_profile_shell(
                &unrestricted_test_policy(),
                &shell,
                tmp.path(),
                &cache,
                tmp.path(),
                "printf %s \"$WEBCODEX_TEST_PROFILE\"",
            );
            assert_eq!(result.exit_code, Some(0), "{result:?}");
            assert_eq!(result.stdout.as_deref(), Some("counted"));
        }
        assert_eq!(std::fs::read_to_string(&counter).unwrap().trim(), "1");
        let cwd = tmp.path().to_string_lossy().to_string();
        let result = run_shell_with_profiles(
            2,
            &unrestricted_test_policy(),
            &shell,
            tmp.path(),
            &cache,
            Some(&cwd),
            "printf %s \"$WEBCODEX_TEST_PROFILE\"",
            None,
            10,
            None,
        );
        assert_eq!(result.stdout.as_deref(), Some("counted"));
        assert_eq!(std::fs::read_to_string(&counter).unwrap().trim(), "2");

        // A late request that still holds generation 1 may prepare its own
        // snapshot, but it must not evict the already-cached active generation.
        let stale = run_shell_with_profiles(
            1,
            &unrestricted_test_policy(),
            &shell,
            tmp.path(),
            &cache,
            Some(&cwd),
            "printf %s \"$WEBCODEX_TEST_PROFILE\"",
            None,
            10,
            None,
        );
        assert_eq!(stale.stdout.as_deref(), Some("counted"));
        assert_eq!(std::fs::read_to_string(&counter).unwrap().trim(), "3");

        let current = run_shell_with_profiles(
            2,
            &unrestricted_test_policy(),
            &shell,
            tmp.path(),
            &cache,
            Some(&cwd),
            "printf %s \"$WEBCODEX_TEST_PROFILE\"",
            None,
            10,
            None,
        );
        assert_eq!(current.stdout.as_deref(), Some("counted"));
        assert_eq!(std::fs::read_to_string(counter).unwrap().trim(), "3");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn prepared_profile_init_script_stdout_noise_does_not_break_env_capture() {
        let tmp = tempfile::tempdir().unwrap();
        let shell = shell_with_profiles(
            Some("test"),
            vec![(
                "test",
                ShellProfileConfig {
                    program: Some("sh".to_string()),
                    args: Some(vec!["-c".to_string()]),
                    init_script: Some(
                        "echo noise before env\nexport WEBCODEX_TEST_PROFILE=ok".to_string(),
                    ),
                    ..ShellProfileConfig::default()
                },
            )],
        );
        let result = run_profile_shell(
            &unrestricted_test_policy(),
            &shell,
            tmp.path(),
            &PreparedShellProfileCache::default(),
            tmp.path(),
            "printf %s \"$WEBCODEX_TEST_PROFILE\"",
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(result.stdout.as_deref(), Some("ok"));
    }

    #[cfg(unix)]
    #[test]
    fn prepared_profile_prepare_reaps_background_pipe_holder() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("prepare-background-pipe-holder.pid");
        let init_script = format!(
            "sleep 60 & background_pid=$!; printf '%s' \"$background_pid\" > {}; export WEBCODEX_TEST_PROFILE=ready",
            shell_quote_path(&pid_file)
        );
        let shell = shell_with_profiles(
            Some("test"),
            vec![(
                "test",
                ShellProfileConfig {
                    program: Some("/bin/sh".to_string()),
                    args: Some(vec!["-c".to_string()]),
                    init_script: Some(init_script),
                    ..ShellProfileConfig::default()
                },
            )],
        );
        let policy = unrestricted_test_policy();
        let cache = PreparedShellProfileCache::default();
        let projects_dir = tmp.path().to_path_buf();
        let cwd = tmp.path().to_string_lossy().to_string();
        let worker_shell = shell.clone();
        let worker_policy = policy.clone();
        let worker_cache = cache.clone();
        let worker_projects_dir = projects_dir.clone();
        let worker_cwd = cwd.clone();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = run_shell_with_profiles(
                1,
                &worker_policy,
                &worker_shell,
                &worker_projects_dir,
                &worker_cache,
                Some(&worker_cwd),
                "printf %s \"$WEBCODEX_TEST_PROFILE\"",
                None,
                10,
                None,
            );
            let _ = result_tx.send(result);
        });

        let received = result_rx.recv_timeout(Duration::from_secs(5));
        if received.is_err() {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|contents| contents.trim().parse::<u32>().ok())
            {
                // SAFETY: the PID was written by this test's background
                // command. This failure-path cleanup targets only that PID.
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
        worker.join().expect("prepared profile worker panicked");
        let result = received.unwrap_or_else(|error| {
            panic!("prepared profile prepare did not return within its bound: {error}")
        });

        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(result.stdout.as_deref(), Some("ready"), "{result:?}");
        assert_eq!(cache.len(), 1, "prepared profile cache was not established");
        assert_descendant_reaped(&pid_file);
    }

    #[test]
    fn prepared_profile_errors_do_not_leak_init_script_body() {
        let tmp = tempfile::tempdir().unwrap();
        let secret = "DO_NOT_LEAK_THIS_INLINE_SCRIPT_BODY";
        let shell = shell_with_profiles(
            Some("test"),
            vec![(
                "test",
                ShellProfileConfig {
                    program: Some("sh".to_string()),
                    args: Some(vec!["-c".to_string()]),
                    init_script: Some(format!("export SECRET={secret}\nfalse")),
                    ..ShellProfileConfig::default()
                },
            )],
        );
        let result = run_profile_shell(
            &unrestricted_test_policy(),
            &shell,
            tmp.path(),
            &PreparedShellProfileCache::default(),
            tmp.path(),
            "true",
        );
        let err = result.error.expect("prepare should fail");
        assert!(err.contains("failed to prepare shell profile"), "{err}");
        assert!(!err.contains(secret), "{err}");
    }

    #[test]
    fn prepared_profile_filters_webcodex_token_env() {
        let _guard = TEST_ENV_LOCK.lock().unwrap();
        let saved = std::env::var_os("WEBCODEX_TOKEN");
        std::env::set_var("WEBCODEX_TOKEN", "secret-token");
        let tmp = tempfile::tempdir().unwrap();
        let shell =
            shell_with_profiles(Some("test"), vec![("test", ShellProfileConfig::default())]);
        let result = run_profile_shell(
            &unrestricted_test_policy(),
            &shell,
            tmp.path(),
            &PreparedShellProfileCache::default(),
            tmp.path(),
            "if [ -z \"${WEBCODEX_TOKEN+x}\" ]; then printf absent; else printf present; fi",
        );
        match saved {
            Some(value) => std::env::set_var("WEBCODEX_TOKEN", value),
            None => std::env::remove_var("WEBCODEX_TOKEN"),
        }
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(result.stdout.as_deref(), Some("absent"));
    }

    #[test]
    fn prepared_profile_missing_marker_is_reported_without_script_body() {
        let tmp = tempfile::tempdir().unwrap();
        let secret = "DO_NOT_LEAK_THIS_INLINE_SCRIPT_BODY";
        let shell = shell_with_profiles(
            Some("test"),
            vec![(
                "test",
                ShellProfileConfig {
                    program: Some("sh".to_string()),
                    args: Some(vec!["-c".to_string()]),
                    init_script: Some(format!("export SECRET={secret}\nexec >/dev/null")),
                    ..ShellProfileConfig::default()
                },
            )],
        );
        let result = run_profile_shell(
            &unrestricted_test_policy(),
            &shell,
            tmp.path(),
            &PreparedShellProfileCache::default(),
            tmp.path(),
            "true",
        );
        let err = result.error.expect("prepare should fail");
        assert!(err.contains("env marker not found"), "{err}");
        assert!(!err.contains(secret), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn prepared_profile_env_payload_parse_failure_is_reported() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let fake_env = bin.join("env");
        std::fs::write(&fake_env, "#!/bin/sh\nprintf 'bad\\000'\n").unwrap();
        let mut perms = std::fs::metadata(&fake_env).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_env, perms).unwrap();
        let shell = shell_with_profiles(
            Some("test"),
            vec![(
                "test",
                ShellProfileConfig {
                    program: Some("/bin/sh".to_string()),
                    args: Some(vec!["-c".to_string()]),
                    env: profile_env(&[("PATH", bin.to_string_lossy().as_ref())]),
                    init_script: Some("export WEBCODEX_TEST_PROFILE=ok".to_string()),
                    ..ShellProfileConfig::default()
                },
            )],
        );
        let result = run_profile_shell(
            &unrestricted_test_policy(),
            &shell,
            tmp.path(),
            &PreparedShellProfileCache::default(),
            tmp.path(),
            "true",
        );
        let err = result.error.expect("prepare should fail");
        assert!(err.contains("entry missing '='"), "{err}");
    }

    #[test]
    fn prepared_profile_program_spawn_failure_mentions_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let shell = shell_with_profiles(
            Some("test"),
            vec![(
                "test",
                ShellProfileConfig {
                    program: Some("/definitely/missing/webcodex-shell".to_string()),
                    args: Some(vec!["-c".to_string()]),
                    ..ShellProfileConfig::default()
                },
            )],
        );
        let result = run_profile_shell(
            &unrestricted_test_policy(),
            &shell,
            tmp.path(),
            &PreparedShellProfileCache::default(),
            tmp.path(),
            "true",
        );
        let err = result.error.expect("spawn should fail");
        assert!(
            err.contains("failed to spawn shell profile 'test'"),
            "{err}"
        );
    }

    #[test]
    fn project_shell_profile_missing_profile_returns_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("project");
        let projects_dir = tmp.path().join("projects.d");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_agent_project(&projects_dir, "demo", &project_dir, Some("missing"));
        let result = run_profile_shell(
            &unrestricted_test_policy(),
            &ShellConfig::default(),
            &projects_dir,
            &PreparedShellProfileCache::default(),
            &project_dir,
            "true",
        );
        let err = result.error.expect("profile should be missing");
        assert!(
            err.contains("project 'demo' shell_profile 'missing'"),
            "{err}"
        );
    }

    #[test]
    fn shell_job_success_and_failure_results_are_structured() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let cwd = tmp.path().to_string_lossy().to_string();

        let success = run_shell(
            &cfg.policy,
            &cfg.shell,
            Some(&cwd),
            "printf hello; printf warn >&2",
            None,
            10,
            None,
        );
        assert_eq!(success.exit_code, Some(0));
        assert_eq!(success.stdout.as_deref(), Some("hello"));
        assert_eq!(success.stderr.as_deref(), Some("warn"));
        assert!(success.error.is_none());

        let failure = run_shell(
            &cfg.policy,
            &cfg.shell,
            Some(&cwd),
            "exit 7",
            None,
            10,
            None,
        );
        assert_eq!(failure.exit_code, Some(7));
        assert!(failure.error.is_none());
    }

    #[test]
    fn shell_job_writes_stdin_to_child() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let cwd = tmp.path().to_string_lossy().to_string();

        let result = run_shell(
            &cfg.policy,
            &cfg.shell,
            Some(&cwd),
            "cat",
            Some("stdin payload\n"),
            10,
            None,
        );
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.as_deref(), Some("stdin payload\n"));
        assert!(result.error.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn shell_job_preserves_result_when_child_closes_stdin_early() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let cwd = tmp.path().to_string_lossy().to_string();
        // Larger than a pipe buffer, so write_all observes the closed reader
        // instead of winning the race by buffering the whole payload.
        let input = "unused payload\n".repeat(128 * 1024);

        let result = run_shell(
            &cfg.policy,
            &cfg.shell,
            Some(&cwd),
            "exec 0<&-; printf capability-unavailable; exit 23",
            Some(&input),
            10,
            None,
        );

        assert_eq!(result.exit_code, Some(23), "{result:?}");
        assert_eq!(result.stdout.as_deref(), Some("capability-unavailable"));
        assert!(result.error.is_none(), "{result:?}");
    }

    #[cfg(unix)]
    #[test]
    fn shell_job_rejects_cwd_symlink_escape() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), project.path().join("outside")).unwrap();
        let policy = AgentPolicy {
            allow_cwd_anywhere: false,
            allowed_roots: vec![project.path().to_path_buf()],
            ..AgentPolicy::default()
        };

        let result = run_shell(
            &policy,
            &ShellConfig::default(),
            Some(project.path().join("outside").to_string_lossy().as_ref()),
            "pwd",
            None,
            10,
            None,
        );

        assert_eq!(result.exit_code, None);
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("outside allowed_roots")));
    }

    #[test]
    fn shell_job_timeout_returns_timeout_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let cwd = tmp.path().to_string_lossy().to_string();

        let result = run_shell(
            &cfg.policy,
            &cfg.shell,
            Some(&cwd),
            "sleep 2",
            None,
            1,
            None,
        );
        assert_eq!(result.exit_code, Some(-1));
        assert_eq!(result.error.as_deref(), Some("command timed out"));
        assert!(result
            .stderr
            .as_deref()
            .unwrap_or_default()
            .contains("command timed out after 1 seconds"));
    }

    #[cfg(unix)]
    fn shell_quote_path(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    #[cfg(unix)]
    fn long_lived_descendant_command(pid_file: &Path) -> String {
        format!(
            "sleep 60 & descendant=$!; printf '%s' \"$descendant\" > {}; wait",
            shell_quote_path(pid_file)
        )
    }

    #[cfg(unix)]
    fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        predicate()
    }

    #[cfg(unix)]
    fn descendant_is_gone(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        // SAFETY: signal 0 only probes the PID written by this test command;
        // it does not deliver a signal to the process.
        let missing = unsafe { libc::kill(pid as i32, 0) == -1 };
        missing && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    #[cfg(unix)]
    struct DescendantCleanup {
        pid: u32,
    }

    #[cfg(unix)]
    impl DescendantCleanup {
        fn disarm(&mut self) {
            self.pid = 0;
        }
    }

    #[cfg(unix)]
    impl Drop for DescendantCleanup {
        fn drop(&mut self) {
            if self.pid != 0 {
                // SAFETY: the PID was created by this test. This is a
                // best-effort failure-path cleanup and never targets a group.
                unsafe {
                    libc::kill(self.pid as i32, libc::SIGKILL);
                }
            }
        }
    }

    #[cfg(unix)]
    fn assert_descendant_reaped(pid_file: &Path) {
        assert!(
            wait_until(Duration::from_secs(2), || pid_file.exists()),
            "descendant pid file was not created: {}",
            pid_file.display()
        );
        let pid = std::fs::read_to_string(pid_file)
            .expect("read descendant pid file")
            .trim()
            .parse::<u32>()
            .expect("parse descendant pid");
        let mut cleanup = DescendantCleanup { pid };
        assert!(
            wait_until(Duration::from_secs(5), || descendant_is_gone(pid)),
            "descendant {pid} survived synchronous shell cancellation"
        );
        cleanup.disarm();
    }

    #[cfg(unix)]
    #[test]
    fn shell_job_timeout_reaps_descendant_process_group() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let cwd = tmp.path().to_string_lossy().to_string();
        let pid_file = tmp.path().join("timeout-descendant.pid");

        let result = run_shell(
            &cfg.policy,
            &cfg.shell,
            Some(&cwd),
            &long_lived_descendant_command(&pid_file),
            None,
            1,
            None,
        );

        assert_eq!(result.exit_code, Some(-1), "{result:?}");
        assert_eq!(
            result.error.as_deref(),
            Some("command timed out"),
            "{result:?}"
        );
        assert!(
            result
                .stderr
                .as_deref()
                .unwrap_or_default()
                .contains("command timed out after 1 seconds"),
            "{result:?}"
        );
        assert_descendant_reaped(&pid_file);
    }

    #[cfg(unix)]
    #[test]
    fn shell_job_timeout_profile_reaps_descendant_process_group() {
        let tmp = tempfile::tempdir().unwrap();
        let shell =
            shell_with_profiles(Some("test"), vec![("test", ShellProfileConfig::default())]);
        let policy = unrestricted_test_policy();
        let cache = PreparedShellProfileCache::default();
        let cwd = tmp.path().to_string_lossy().to_string();
        let pid_file = tmp.path().join("profile-timeout-descendant.pid");

        // Exercise the production request path directly rather than the
        // test-only `run_shell` wrapper.
        let result = run_shell_with_profiles(
            1,
            &policy,
            &shell,
            tmp.path(),
            &cache,
            Some(&cwd),
            &long_lived_descendant_command(&pid_file),
            None,
            1,
            None,
        );

        assert_eq!(cache.len(), 1, "prepared profile path was not used");
        assert_eq!(result.exit_code, Some(-1), "{result:?}");
        assert_eq!(
            result.error.as_deref(),
            Some("command timed out"),
            "{result:?}"
        );
        assert_descendant_reaped(&pid_file);
    }

    #[test]
    fn shell_job_stop_flag_is_best_effort() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let cwd = tmp.path().to_string_lossy().to_string();
        let stop_requested = AtomicBool::new(true);

        let result = run_shell(
            &cfg.policy,
            &cfg.shell,
            Some(&cwd),
            "sleep 2",
            None,
            10,
            Some(&stop_requested),
        );
        assert_eq!(result.exit_code, Some(-1));
        assert_eq!(result.error.as_deref(), Some("job stopped"));
        assert!(result
            .stderr
            .as_deref()
            .unwrap_or_default()
            .contains("job stopped by request"));
    }

    #[cfg(unix)]
    #[test]
    fn shell_job_stop_reaps_descendant_process_group() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let cwd = tmp.path().to_string_lossy().to_string();
        let pid_file = tmp.path().join("stop-descendant.pid");
        let stop_requested = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop_requested);
        let stop_pid_file = pid_file.clone();
        let stopper = std::thread::spawn(move || {
            let created = wait_until(Duration::from_secs(2), || stop_pid_file.exists());
            stop_flag.store(true, Ordering::SeqCst);
            created
        });

        let result = run_shell(
            &cfg.policy,
            &cfg.shell,
            Some(&cwd),
            &long_lived_descendant_command(&pid_file),
            None,
            10,
            Some(stop_requested.as_ref()),
        );

        assert!(stopper.join().expect("stopper thread panicked"));
        assert_eq!(result.exit_code, Some(-1), "{result:?}");
        assert_eq!(result.error.as_deref(), Some("job stopped"), "{result:?}");
        assert!(
            result
                .stderr
                .as_deref()
                .unwrap_or_default()
                .contains("job stopped by request"),
            "{result:?}"
        );
        assert_descendant_reaped(&pid_file);
    }

    #[test]
    fn shell_job_stdout_stderr_are_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_config(tmp.path().join("config/projects.d"));
        cfg.policy.max_output_bytes = 8;
        let cwd = tmp.path().to_string_lossy().to_string();

        let result = run_shell(
            &cfg.policy,
            &cfg.shell,
            Some(&cwd),
            "printf 0123456789; printf abcdefghij >&2",
            None,
            10,
            None,
        );
        assert_eq!(result.exit_code, Some(0));
        let stdout = result.stdout.unwrap();
        let stderr = result.stderr.unwrap();
        assert!(stdout.contains("[output truncated to last 8 bytes]"));
        assert!(stdout.ends_with("23456789"));
        assert!(stderr.contains("[output truncated to last 8 bytes]"));
        assert!(stderr.ends_with("cdefghij"));
    }

    #[test]
    fn register_request_announces_correct_protocol_version() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_config(tmp.path().join("config/projects.d"));
        // A stale or hand-edited config cannot force capability advertisement:
        // registration replaces it with the result of the real host probe.
        cfg.capabilities = Some(ShellClientCapabilities {
            sandbox_inspect_commands: true,
            project_lifecycle: false,
            ..Default::default()
        });
        for (version, expected_str) in [
            (AGENT_PROTOCOL_VERSION_POLLING_V1, "polling-v1"),
            (AGENT_PROTOCOL_VERSION_WEBSOCKET_V1, "websocket-v1"),
            (AGENT_PROTOCOL_VERSION_QUIC_V1, "quic-v1"),
        ] {
            let body = build_register_request(&cfg, Vec::new(), version, "inst-1", 0);
            assert_eq!(body.agent_instance_id, "inst-1");
            assert_eq!(
                body.agent_protocol_version.as_deref(),
                Some(version),
                "version mismatch for {expected_str}"
            );
            assert_eq!(body.agent_protocol_version.as_deref(), Some(expected_str));
        }
        // Also verify capabilities are advertised (check once for polling).
        let body = build_register_request(
            &cfg,
            Vec::new(),
            AGENT_PROTOCOL_VERSION_POLLING_V1,
            "inst-1",
            0,
        );
        let caps = body.capabilities.expect("agent registers capabilities");
        assert!(caps.shell);
        assert!(caps.file_read);
        assert!(caps.file_write);
        assert!(caps.async_jobs);
        assert!(caps.async_shell_jobs);
        assert!(caps.structured_validation_argv);
        assert!(caps.lsp_read_only_navigation);
        assert_eq!(
            caps.sandbox_inspect_commands,
            crate::command_sandbox::inspect_sandbox_available().is_ok()
        );
    }

    #[test]
    fn register_request_carries_sanitized_shell_profiles_summary() {
        // A config with one profile carrying a secret env value and a secret
        // init_script body. The sanitized summary must report the profile name,
        // has_init_script=true, and env_keys_count, but MUST NOT include the env
        // value or the init_script body.
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_config(tmp.path().join("config/projects.d"));
        let secret_env = "DO_NOT_LEAK_THIS_ENV_VALUE";
        let secret_script = "DO_NOT_LEAK_THIS_INIT_SCRIPT_BODY";
        cfg.shell = shell_with_profiles(
            Some("rust"),
            vec![(
                "rust",
                ShellProfileConfig {
                    program: Some("sh".to_string()),
                    args: Some(vec!["-c".to_string()]),
                    env: profile_env(&[("SECRET_KEY", secret_env)]),
                    init_script: Some(secret_script.to_string()),
                    ..ShellProfileConfig::default()
                },
            )],
        );
        let body = build_register_request(
            &cfg,
            Vec::new(),
            AGENT_PROTOCOL_VERSION_POLLING_V1,
            "inst-1",
            0,
        );
        let policy = body.policy.expect("agent registers a policy");
        let summary = policy
            .shell_profiles
            .as_ref()
            .expect("sanitized shell profiles summary is present");
        assert_eq!(summary.default_profile.as_deref(), Some("rust"));
        assert_eq!(summary.configured_count, 1);
        assert_eq!(summary.profiles.len(), 1);
        let entry = &summary.profiles[0];
        assert_eq!(entry.name, "rust");
        assert!(entry.has_init_script);
        assert_eq!(entry.env_keys_count, 1);
        assert_eq!(entry.program, "sh");
        assert_eq!(entry.args_count, 1);
        // Sanitization: the rendered summary never carries env values or the
        // init_script body.
        let rendered = serde_json::to_string(summary).unwrap();
        assert!(!rendered.contains(secret_env), "{rendered}");
        assert!(!rendered.contains(secret_script), "{rendered}");
    }

    // ------------------------------------------------------------------------
    // WebSocket transport helpers + shared dispatch over a WebSocket sink
    // ------------------------------------------------------------------------

    #[test]
    fn server_url_to_ws_converts_http_https_and_rejects_bare() {
        assert_eq!(
            server_url_to_ws("http://127.0.0.1:8080", "/api/agents/ws").unwrap(),
            "ws://127.0.0.1:8080/api/agents/ws"
        );
        assert_eq!(
            server_url_to_ws("https://example.com/", "/api/agents/ws").unwrap(),
            "wss://example.com/api/agents/ws"
        );
        // Already a ws(s) URL passes through.
        assert_eq!(
            server_url_to_ws("wss://example.com", "/api/agents/ws").unwrap(),
            "wss://example.com/api/agents/ws"
        );
        assert!(server_url_to_ws("ftp://x", "/api/agents/ws").is_err());
    }

    #[test]
    fn generated_agent_instance_id_is_non_empty_uuid_like() {
        // `run_agent` generates the instance id the same way; verify the
        // format here without driving the full agent loop.
        let id = uuid::Uuid::new_v4().to_string();
        assert!(!id.is_empty());
        // Canonical UUID v4 is 36 chars: 8-4-4-4-12 hex groups.
        assert_eq!(id.len(), 36);
        assert_eq!(id.chars().filter(|c| *c == '-').count(), 4);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        // The register builder carries it through unchanged.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let body =
            build_register_request(&cfg, Vec::new(), AGENT_PROTOCOL_VERSION_POLLING_V1, &id, 0);
        assert_eq!(body.agent_instance_id, id);
        assert!(!body.agent_instance_id.is_empty());
    }

    fn ws_sink(client_id: &str) -> (AgentSink, tokio::sync::mpsc::Receiver<AgentEnvelope>) {
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentEnvelope>(WS_OUTGOING_CAPACITY);
        (
            AgentSink::WebSocket {
                tx,
                client_id: client_id.to_string(),
                agent_instance_id: "ws-inst".to_string(),
            },
            rx,
        )
    }

    fn quic_sink(client_id: &str) -> (AgentSink, tokio::sync::mpsc::Receiver<AgentEnvelope>) {
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentEnvelope>(WS_OUTGOING_CAPACITY);
        (
            AgentSink::Quic {
                tx,
                client_id: client_id.to_string(),
                agent_instance_id: "quic-inst".to_string(),
            },
            rx,
        )
    }

    #[test]
    fn sink_submit_result_sends_result_envelope() {
        type SinkFactory = fn(&str) -> (AgentSink, tokio::sync::mpsc::Receiver<AgentEnvelope>);
        for (label, make_sink, expected_client, expected_instance) in [
            ("ws", ws_sink as SinkFactory, "ws-client", "ws-inst"),
            ("quic", quic_sink as SinkFactory, "quic-client", "quic-inst"),
        ] {
            let (sink, mut rx) = make_sink(expected_client);
            let result = CommandResult {
                exit_code: Some(0),
                stdout: Some("hi".to_string()),
                stderr: Some(String::new()),
                duration_ms: Some(3),
                error: None,
            };
            assert_eq!(
                sink.submit_result("req-9".to_string(), result).unwrap(),
                webcodex_runner::ResultSubmission::Accepted,
                "{label}"
            );
            let env = rx.try_recv().expect("envelope was sent");
            match env {
                AgentEnvelope::Result { payload } => {
                    assert_eq!(payload.client_id, expected_client, "{label}");
                    assert_eq!(payload.agent_instance_id, expected_instance, "{label}");
                    assert_eq!(payload.request_id, "req-9");
                    assert_eq!(payload.exit_code, Some(0));
                    assert_eq!(payload.stdout.as_deref(), Some("hi"));
                }
                other => panic!("{label}: expected result, got {:?}", other.kind()),
            }
        }
    }

    #[test]
    fn sink_send_job_update_sends_job_update_envelope() {
        type SinkFactory = fn(&str) -> (AgentSink, tokio::sync::mpsc::Receiver<AgentEnvelope>);
        for (label, make_sink, expected_client) in [
            ("ws", ws_sink as SinkFactory, "ws-client"),
            ("quic", quic_sink as SinkFactory, "quic-client"),
        ] {
            let (sink, mut rx) = make_sink(expected_client);
            let body = ShellAgentJobUpdateRequest {
                client_id: expected_client.to_string(),
                agent_instance_id: sink.agent_instance_id().to_string(),
                job_id: "job-1".to_string(),
                request_id: Some("req-1".to_string()),
                status: "running".to_string(),
                stdout_chunk: Some(format!("{label}-chunk")),
                stderr_chunk: None,
                stdout_tail: None,
                stderr_tail: None,
                exit_code: None,
                duration_ms: None,
                error: None,
                validation_progress: None,
                finished: false,
            };
            sink.send_job_update(&body).unwrap();
            let env = rx.try_recv().expect("envelope was sent");
            match env {
                AgentEnvelope::JobUpdate { payload } => {
                    assert_eq!(payload.client_id, expected_client, "{label}");
                    assert_eq!(
                        payload.agent_instance_id,
                        sink.agent_instance_id(),
                        "{label}"
                    );
                    assert_eq!(payload.job_id, "job-1", "{label}");
                    assert_eq!(payload.status, "running", "{label}");
                    assert_eq!(
                        payload.stdout_chunk.as_deref(),
                        Some(format!("{label}-chunk").as_str()),
                        "{label}"
                    );
                }
                other => panic!("{label}: expected job_update, got {:?}", other.kind()),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn job_manager_stop_all_clears_queue_and_requests_running_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let jobs = JobManager::new(1);
        let stop_requested = Arc::new(AtomicBool::new(false));
        let mut running_command =
            configured_shell_job_command(&ShellConfig::default(), "sleep 60").unwrap();
        let running_child = Arc::new(Mutex::new(
            running_command
                .current_dir(tmp.path())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        ));
        let running_pid = lock_unpoison(&running_child).id();
        jobs.jobs.lock().unwrap().insert(
            "running-job".to_string(),
            RunningJob {
                client_id: "ws-client".to_string(),
                child: Some(Arc::clone(&running_child)),
                process_group_id: Some(running_pid),
                stop_requested: stop_requested.clone(),
            },
        );
        let (sink, mut rx) = ws_sink("ws-client");
        let request = ShellAgentShellRequest {
            request_id: "req-queued".to_string(),
            client_id: "ws-client".to_string(),
            kind: "start_job".to_string(),
            job_id: Some("queued-job".to_string()),
            cwd: Some(tmp.path().to_string_lossy().to_string()),
            path: None,
            content: None,
            max_bytes: None,
            old_text: None,
            pattern: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            line: None,
            create_dirs: false,
            command: ": > queued-started".to_string(),
            stdin: None,
            timeout_secs: 60,
            requested_by: "tester".to_string(),
            created_at: 0,
            validation: None,
            lsp: None,
            sandbox: None,
        };
        let mut rejected_request = request.clone();
        rejected_request.request_id = "req-after-shutdown".to_string();
        rejected_request.job_id = Some("job-after-shutdown".to_string());

        jobs.enqueue(
            sink,
            1,
            cfg.policy.clone(),
            cfg.shell.clone(),
            projects_dir(&cfg),
            request,
        );
        match rx.try_recv().expect("queued status was sent") {
            AgentEnvelope::JobUpdate { payload } => {
                assert_eq!(payload.job_id, "queued-job");
                assert_eq!(payload.status, "agent_queued");
            }
            other => panic!("expected job_update, got {:?}", other.kind()),
        }
        assert_eq!(jobs.queued.lock().unwrap().len(), 1);

        jobs.stop_all();

        assert!(stop_requested.load(Ordering::SeqCst));
        assert!(jobs.queued.lock().unwrap().is_empty());
        assert!(lock_unpoison(&running_child).try_wait().unwrap().is_some());
        assert_eq!(signal_process_group(running_pid, 0), Ok(false));
        assert!(
            !tmp.path().join("queued-started").exists(),
            "queued job started during shutdown"
        );

        let (rejected_sink, mut rejected_rx) = ws_sink("ws-client");
        jobs.enqueue(
            rejected_sink,
            1,
            cfg.policy.clone(),
            cfg.shell.clone(),
            projects_dir(&cfg),
            rejected_request,
        );
        assert!(jobs.queued.lock().unwrap().is_empty());
        match rejected_rx.try_recv().expect("shutdown rejection was sent") {
            AgentEnvelope::JobUpdate { payload } => {
                assert_eq!(payload.job_id, "job-after-shutdown");
                assert_eq!(payload.status, "failed");
                assert!(payload.finished);
                assert_eq!(payload.error.as_deref(), Some("runner is shutting down"));
            }
            other => panic!("expected job_update, got {:?}", other.kind()),
        }
    }

    #[test]
    fn file_request_kind_includes_anchor_edit_ops() {
        for kind in [
            "file_read",
            "file_write",
            "file_list",
            "file_project_overview",
            "file_replace_line_range",
            "file_insert_at_line",
            "file_delete_line_range",
            "file_replace_exact_block",
            "file_insert_before_pattern",
            "file_insert_after_pattern",
        ] {
            assert!(
                is_file_request_kind(kind),
                "{kind} should route to file handler"
            );
        }
        assert!(!is_file_request_kind("run_shell"));
    }

    #[test]
    fn project_overview_agent_request_returns_metadata_without_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let policy = project_policy(tmp.path());
        std::fs::write(tmp.path().join("Cargo.toml"), "private manifest content").unwrap();
        std::fs::write(tmp.path().join("README.md"), "private readme content").unwrap();
        std::fs::write(tmp.path().join(".env"), "TOKEN=not-returned").unwrap();
        let request = json_file_op_request(
            tmp.path(),
            "file_project_overview",
            ".",
            serde_json::json!({"max_depth": 2, "limit": 200}),
        );

        let output = line_edit_json(handle_file_request(&policy, &request));
        assert_eq!(output["schema_version"], 1);
        assert_eq!(output["deterministic"], true);
        assert!(output.to_string().contains("Cargo.toml"));
        assert!(!output.to_string().contains("private manifest content"));
        assert!(!output.to_string().contains("TOKEN=not-returned"));
        assert!(!output.to_string().contains(".env"));
        assert!(!output
            .to_string()
            .contains(&tmp.path().display().to_string()));
    }

    #[test]
    fn dispatch_request_anchor_edit_routes_to_file_handler() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let cwd = tmp.path().to_string_lossy().to_string();
        std::fs::write(tmp.path().join("anchor.txt"), "old block\n").unwrap();
        let (sink, mut rx) = ws_sink("ws-client");
        let jobs = JobManager::new(max_concurrent_jobs(&cfg));
        let request = ShellAgentShellRequest {
            request_id: "req-anchor".to_string(),
            client_id: "ws-client".to_string(),
            kind: "file_replace_exact_block".to_string(),
            job_id: None,
            cwd: Some(cwd),
            path: Some("anchor.txt".to_string()),
            content: Some("new block\n".to_string()),
            max_bytes: None,
            old_text: Some("old block\n".to_string()),
            pattern: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            line: None,
            create_dirs: false,
            command: String::new(),
            stdin: None,
            timeout_secs: 10,
            requested_by: "tester".to_string(),
            created_at: 0,
            validation: None,
            lsp: None,
            sandbox: None,
        };
        let pdir = projects_dir(&cfg);
        let lsp = webcodex_runner::LspSupervisor::default();
        let hot = runtime_config(&cfg);
        let ran =
            dispatch_request(&sink, &hot.snapshot(), &hot, &jobs, &pdir, &lsp, request).unwrap();
        assert!(ran);
        let env = rx.try_recv().expect("result envelope was sent");
        match env {
            AgentEnvelope::Result { payload } => {
                assert_eq!(payload.request_id, "req-anchor");
                assert_eq!(payload.exit_code, Some(0));
                let stdout = payload.stdout.expect("file handler returns JSON stdout");
                assert!(stdout.contains("\"changed\":true"), "stdout was {stdout}");
                assert_eq!(
                    std::fs::read_to_string(tmp.path().join("anchor.txt")).unwrap(),
                    "new block\n"
                );
            }
            other => panic!("expected result, got {:?}", other.kind()),
        }
    }

    #[test]
    fn dispatch_request_run_shell_sends_result_over_sink() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let jobs = JobManager::new(max_concurrent_jobs(&cfg));
        let pdir = projects_dir(&cfg);
        let hot = runtime_config(&cfg);

        type SinkFactory = fn(&str) -> (AgentSink, tokio::sync::mpsc::Receiver<AgentEnvelope>);
        for (label, make_sink, client_id, cmd) in [
            ("ws", ws_sink as SinkFactory, "ws-client", "printf wsok"),
            (
                "quic",
                quic_sink as SinkFactory,
                "quic-client",
                "printf quic-ok",
            ),
        ] {
            let (sink, mut rx) = make_sink(client_id);
            let request = ShellAgentShellRequest {
                request_id: format!("req-{label}"),
                client_id: client_id.to_string(),
                kind: "run_shell".to_string(),
                job_id: None,
                cwd: Some(tmp.path().to_string_lossy().to_string()),
                path: None,
                content: None,
                max_bytes: None,
                old_text: None,
                pattern: None,
                expected_sha256: None,
                expected_prefix: None,
                start_line: None,
                end_line: None,
                line: None,
                create_dirs: false,
                command: cmd.to_string(),
                stdin: None,
                timeout_secs: 10,
                requested_by: "tester".to_string(),
                created_at: 0,
                validation: None,
                lsp: None,
                sandbox: None,
            };
            let ran = dispatch_request(
                &sink,
                &hot.snapshot(),
                &hot,
                &jobs,
                &pdir,
                &webcodex_runner::LspSupervisor::default(),
                request,
            )
            .unwrap();
            assert!(ran, "{label}");
            let env = rx.try_recv().expect("result envelope was sent");
            match env {
                AgentEnvelope::Result { payload } => {
                    assert_eq!(payload.request_id, format!("req-{label}"));
                    assert_eq!(payload.exit_code, Some(0));
                    assert_eq!(
                        payload.stdout.as_deref(),
                        Some(cmd.split_whitespace().last().unwrap())
                    );
                }
                other => panic!("{label}: expected result, got {:?}", other.kind()),
            }
        }
    }

    fn project_policy(root: &Path) -> AgentPolicy {
        AgentPolicy {
            allow_cwd_anywhere: false,
            allowed_roots: vec![root.to_path_buf()],
            ..AgentPolicy::default()
        }
    }

    fn project_request(kind: &str, payload: serde_json::Value) -> ShellAgentShellRequest {
        ShellAgentShellRequest {
            request_id: format!("req-{}", kind),
            client_id: "oe".to_string(),
            kind: kind.to_string(),
            job_id: None,
            cwd: None,
            path: None,
            content: None,
            max_bytes: None,
            old_text: None,
            pattern: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            line: None,
            create_dirs: false,
            command: String::new(),
            stdin: Some(payload.to_string()),
            timeout_secs: 10,
            requested_by: "tester".to_string(),
            created_at: 0,
            validation: None,
            lsp: None,
            sandbox: None,
        }
    }

    fn project_ok(result: CommandResult) -> serde_json::Value {
        assert_eq!(result.exit_code, Some(0), "unexpected result: {:?}", result);
        assert!(
            result.error.is_none(),
            "unexpected error: {:?}",
            result.error
        );
        serde_json::from_str(result.stdout.as_deref().expect("stdout json")).unwrap()
    }

    fn project_err(result: CommandResult) -> String {
        if let Some(error) = result.error {
            return error;
        }
        assert_ne!(
            result.exit_code,
            Some(0),
            "unexpected success: {:?}",
            result
        );
        serde_json::from_str::<serde_json::Value>(result.stdout.as_deref().expect("error json"))
            .unwrap()["error_code"]
            .as_str()
            .expect("error_code")
            .to_string()
    }

    #[test]
    fn register_project_writes_valid_toml_into_projects_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo");
        let projects_dir = tmp.path().join("projects.d");
        std::fs::create_dir(&project_dir).unwrap();
        let policy = project_policy(tmp.path());
        let req = project_request(
            "register_project",
            serde_json::json!({
                "id": "demo",
                "name": "Demo",
                "path": project_dir.to_string_lossy(),
                "description": "A demo project",
                "allow_patch": false
            }),
        );

        let value = project_ok(handle_project_op(&policy, &projects_dir, &req));
        assert_eq!(value["created_config"], true);
        assert_eq!(value["overwritten"], false);

        let content = std::fs::read_to_string(projects_dir.join("demo.toml")).unwrap();
        let parsed = parse_agent_project_toml(&content).unwrap();
        assert_eq!(parsed.id, "demo");
        assert_eq!(parsed.name.as_deref(), Some("Demo"));
        assert_eq!(parsed.path, project_dir.to_string_lossy());
        assert!(!parsed.allow_patch);
    }

    #[test]
    fn create_post_rename_sync_failure_preserves_source_and_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let projects_dir = tmp.path().join("projects.d");
        let create_dir = tmp.path().join("created-after-rename");
        let policy = project_policy(tmp.path());
        webcodex_runner::projects::fail_next_project_parent_sync_after_rename();
        let error = project_err(handle_project_op(
            &policy,
            &projects_dir,
            &project_request(
                "create_project",
                serde_json::json!({
                    "id":"indeterminate", "name":"Indeterminate",
                    "description":"Preserve me", "path":create_dir.to_string_lossy(),
                    "allow_patch":true, "template":"basic", "git_init":true
                }),
            ),
        ));
        assert_eq!(error, "operation_indeterminate");
        assert!(projects_dir.join("indeterminate.toml").is_file());
        assert!(create_dir.join("README.md").is_file());
        assert!(create_dir.join(".gitignore").is_file());
        assert!(create_dir.join(".git").is_dir());
    }

    #[test]
    fn register_and_create_retries_converge_without_duplicate_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let projects_dir = tmp.path().join("projects.d");
        let register_dir = tmp.path().join("existing");
        std::fs::create_dir(&register_dir).unwrap();
        let policy = project_policy(tmp.path());
        let register = project_request(
            "register_project",
            serde_json::json!({
                "id":"registered", "name":"Registered",
                "path":register_dir.to_string_lossy(), "allow_patch":true
            }),
        );
        let first = project_ok(handle_project_op(&policy, &projects_dir, &register));
        let retry = project_ok(handle_project_op(&policy, &projects_dir, &register));
        assert_eq!(retry["recovered"], true);
        assert_eq!(retry["changed"], false);
        assert_eq!(retry["revision"], first["revision"]);

        let create_dir = tmp.path().join("created");
        let create = project_request(
            "create_project",
            serde_json::json!({
                "id":"created", "name":"Created", "description":"Fixture",
                "path":create_dir.to_string_lossy(), "allow_patch":true,
                "template":"basic", "git_init":true,
                "allow_existing_empty":false
            }),
        );
        let created = project_ok(handle_project_op(&policy, &projects_dir, &create));
        let readme_before = std::fs::read(create_dir.join("README.md")).unwrap();
        let recovered = project_ok(handle_project_op(&policy, &projects_dir, &create));
        assert_eq!(recovered["recovered"], true);
        assert_eq!(recovered["changed"], false);
        assert_eq!(recovered["revision"], created["revision"]);
        assert_eq!(
            std::fs::read(create_dir.join("README.md")).unwrap(),
            readme_before
        );
        assert!(create_dir.join(".git").is_dir());

        let mismatch = project_err(handle_project_op(
            &policy,
            &projects_dir,
            &project_request(
                "register_project",
                serde_json::json!({
                    "id":"registered", "name":"Different",
                    "path":register_dir.to_string_lossy(), "allow_patch":true
                }),
            ),
        ));
        assert_eq!(mismatch, "project_already_exists");
    }

    #[test]
    fn project_lifecycle_persists_state_and_unregister_preserves_source() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo");
        let projects_dir = tmp.path().join("projects.d");
        std::fs::create_dir(&project_dir).unwrap();
        std::fs::create_dir(project_dir.join(".git")).unwrap();
        std::fs::write(project_dir.join("keep.txt"), "keep").unwrap();
        let policy = project_policy(tmp.path());
        let registered = project_ok(handle_project_op(
            &policy,
            &projects_dir,
            &project_request(
                "register_project",
                serde_json::json!({
                    "id": "demo",
                    "name": "Demo",
                    "path": project_dir.to_string_lossy()
                }),
            ),
        ));
        let revision = registered["revision"].as_str().unwrap().to_string();

        let disabled = project_ok(handle_project_lifecycle_op(
            &policy,
            &projects_dir,
            &project_request(
                "project_lifecycle_disable",
                serde_json::json!({"project_id":"demo","expected_revision":revision}),
            ),
        ));
        assert_eq!(disabled["outcome"], "disabled");
        let retry_disabled = project_ok(handle_project_lifecycle_op(
            &policy,
            &projects_dir,
            &project_request(
                "project_lifecycle_disable",
                serde_json::json!({"project_id":"demo","expected_revision":registered["revision"]}),
            ),
        ));
        assert_eq!(retry_disabled["outcome"], "already_disabled");
        let disabled_revision = disabled["revision"].as_str().unwrap().to_string();
        let summaries = load_agent_project_summaries_from_dir(&projects_dir);
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].disabled);

        let stale = project_err(handle_project_lifecycle_op(
            &policy,
            &projects_dir,
            &project_request(
                "project_lifecycle_enable",
                serde_json::json!({"project_id":"demo","expected_revision":registered["revision"]}),
            ),
        ));
        assert_eq!(stale, "revision_conflict");

        let enabled = project_ok(handle_project_lifecycle_op(
            &policy,
            &projects_dir,
            &project_request(
                "project_lifecycle_enable",
                serde_json::json!({"project_id":"demo","expected_revision":disabled_revision}),
            ),
        ));
        assert_eq!(enabled["outcome"], "enabled");
        let retry_enabled = project_ok(handle_project_lifecycle_op(
            &policy,
            &projects_dir,
            &project_request(
                "project_lifecycle_enable",
                serde_json::json!({"project_id":"demo","expected_revision":disabled["revision"]}),
            ),
        ));
        assert_eq!(retry_enabled["outcome"], "already_enabled");

        let unregistered = project_ok(handle_project_lifecycle_op(
            &policy,
            &projects_dir,
            &project_request(
                "project_lifecycle_unregister",
                serde_json::json!({
                    "project_id":"demo",
                    "expected_revision":enabled["revision"]
                }),
            ),
        ));
        assert_eq!(unregistered["outcome"], "unregistered");
        assert!(!projects_dir.join("demo.toml").exists());
        assert!(project_dir.join("keep.txt").exists());
        assert!(project_dir.join(".git").is_dir());

        let repeated = project_ok(handle_project_lifecycle_op(
            &policy,
            &projects_dir,
            &project_request(
                "project_lifecycle_unregister",
                serde_json::json!({"project_id":"demo","expected_revision":enabled["revision"]}),
            ),
        ));
        assert_eq!(repeated["outcome"], "already_unregistered");

        let stale_tombstone = projects_dir.join(".demo.crash.toml.unregistering");
        std::fs::write(&stale_tombstone, "stale").unwrap();
        assert!(load_agent_project_summaries_from_dir(&projects_dir).is_empty());
        let recovered = project_ok(handle_project_lifecycle_op(
            &policy,
            &projects_dir,
            &project_request(
                "project_lifecycle_unregister",
                serde_json::json!({"project_id":"demo","expected_revision":enabled["revision"]}),
            ),
        ));
        assert_eq!(recovered["outcome"], "already_unregistered");
        assert!(!stale_tombstone.exists());
    }

    #[test]
    fn register_project_rejects_path_outside_allowed_roots() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let projects_dir = allowed.path().join("projects.d");
        let policy = project_policy(allowed.path());
        let req = project_request(
            "register_project",
            serde_json::json!({
                "id": "outside",
                "name": "Outside",
                "path": outside.path().to_string_lossy()
            }),
        );

        let err = project_err(handle_project_op(&policy, &projects_dir, &req));
        assert_eq!(err, "path_outside_allowed_roots");
        assert!(!projects_dir.join("outside.toml").exists());
    }

    #[test]
    fn register_project_rejects_dangerous_subpaths_without_explicit_root() {
        let policy = AgentPolicy {
            allow_cwd_anywhere: true,
            allowed_roots: Vec::new(),
            ..AgentPolicy::default()
        };

        for path in [
            "/etc/nginx",
            "/usr/local",
            "/var/lib",
            "/proc/self",
            "/dev/shm",
        ] {
            let err = validate_project_path_policy(&policy, Path::new(path)).unwrap_err();
            assert!(err.contains("dangerous system root"), "{path}: {err}");
        }

        validate_project_path_policy(&policy, Path::new("/usr2/local")).unwrap();
    }

    #[test]
    fn load_config_defaults_empty_allowed_roots_to_home() {
        let _guard = agent_init::TEST_ENV_LOCK.lock().unwrap();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        if let Some(home) = home {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("agent.toml");
            std::fs::write(
                &path,
                "server_url = \"http://x\"\ntoken = \"t\"\nclient_id = \"c\"\n",
            )
            .unwrap();
            let cfg = load_config(&path).unwrap();
            assert_eq!(
                cfg.policy.allowed_roots,
                vec![home],
                "empty allowed_roots must default to HOME"
            );
        }
    }

    #[test]
    fn load_config_defaults_allow_cwd_anywhere_to_false() {
        let _guard = agent_init::TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let base = "server_url = \"http://x\"\ntoken = \"t\"\nclient_id = \"c\"\n";

        // A config that omits `[policy]` entirely falls back to
        // `AgentPolicy::default()`; one that has `[policy]` without the field
        // falls back to the per-field serde default. Both must fail closed —
        // otherwise the agent runs with no filesystem boundary at all.
        for (label, body) in [
            ("no [policy] section", base.to_string()),
            (
                "[policy] without allow_cwd_anywhere",
                format!("{base}\n[policy]\nallow_raw_shell = true\n"),
            ),
        ] {
            let path = tmp.path().join("agent.toml");
            std::fs::write(&path, body).unwrap();
            let cfg = load_config(&path).unwrap();
            assert!(
                !cfg.policy.allow_cwd_anywhere,
                "{label}: allow_cwd_anywhere must default to false"
            );
        }
    }

    #[test]
    fn default_policy_denies_paths_outside_allowed_roots() {
        // The shipped default must not resolve an absolute path outside the
        // configured roots. `AgentPolicy::default()` has no roots at all, so
        // every path is out of bounds.
        let policy = AgentPolicy::default();
        assert!(!policy.allow_cwd_anywhere);
        let err = resolve_requested_path(&policy, Some("/tmp"), "/etc/passwd")
            .expect_err("default policy must not reach /etc/passwd");
        assert!(err.contains("outside allowed_roots"), "{err}");

        // With HOME as the root — what `effective_allowed_roots` fills in — a
        // path inside the root still resolves, so the default is restrictive
        // rather than broken.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("in-bounds.txt"), "ok").unwrap();
        let scoped = AgentPolicy {
            allowed_roots: vec![root.clone()],
            ..AgentPolicy::default()
        };
        resolve_requested_path(&scoped, Some(root.to_str().unwrap()), "in-bounds.txt")
            .expect("in-bounds path must still resolve under the fail-closed default");
    }

    #[test]
    fn load_config_explicit_allowed_roots_override_home_default() {
        let _guard = agent_init::TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent.toml");
        std::fs::write(
            &path,
            "server_url = \"http://x\"\ntoken = \"t\"\nclient_id = \"c\"\n[policy]\nallowed_roots = [\"/root/git\"]\n",
        )
        .unwrap();
        let cfg = load_config(&path).unwrap();
        assert_eq!(
            cfg.policy.allowed_roots,
            vec![PathBuf::from("/root/git")],
            "explicit allowed_roots must override the HOME default"
        );
    }

    #[test]
    fn load_config_empty_roots_without_home_and_no_cwd_anywhere_errors() {
        let _guard = agent_init::TEST_ENV_LOCK.lock().unwrap();
        let saved = std::env::var_os("HOME");
        std::env::remove_var("HOME");
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent.toml");
        std::fs::write(
            &path,
            "server_url = \"http://x\"\ntoken = \"t\"\nclient_id = \"c\"\n\
             [policy]\nallow_cwd_anywhere = false\n",
        )
        .unwrap();
        let err = load_config(&path).unwrap_err();
        assert!(err.contains("allowed_roots is empty"));
        match saved {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn register_project_overwrite_semantics_are_accurate() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo");
        let projects_dir = tmp.path().join("projects.d");
        std::fs::create_dir(&project_dir).unwrap();
        let policy = project_policy(tmp.path());
        let payload = |overwrite| {
            serde_json::json!({
                "id": "demo",
                "name": "Demo",
                "path": project_dir.to_string_lossy(),
                "overwrite": overwrite
            })
        };

        let first = project_ok(handle_project_op(
            &policy,
            &projects_dir,
            &project_request("register_project", payload(false)),
        ));
        assert_eq!(first["created_config"], true);
        assert_eq!(first["overwritten"], false);

        let retry = project_ok(handle_project_op(
            &policy,
            &projects_dir,
            &project_request("register_project", payload(false)),
        ));
        assert_eq!(retry["recovered"], true);
        assert_eq!(retry["changed"], false);

        let overwritten = project_ok(handle_project_op(
            &policy,
            &projects_dir,
            &project_request("register_project", payload(true)),
        ));
        assert_eq!(overwritten["created_config"], false);
        assert_eq!(overwritten["overwritten"], true);
    }

    #[test]
    fn create_project_basic_creates_readme_and_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("new-project");
        let projects_dir = tmp.path().join("projects.d");
        let policy = project_policy(tmp.path());
        let req = project_request(
            "create_project",
            serde_json::json!({
                "id": "basic",
                "name": "Basic",
                "path": project_dir.to_string_lossy(),
                "description": "Basic template",
                "template": "basic"
            }),
        );

        let value = project_ok(handle_project_op(&policy, &projects_dir, &req));
        assert_eq!(value["created_directory"], true);
        assert!(project_dir.join("README.md").exists());
        assert!(project_dir.join(".gitignore").exists());
        assert!(std::fs::read_to_string(project_dir.join("README.md"))
            .unwrap()
            .contains("Basic template"));
    }

    #[test]
    fn create_project_rejects_existing_non_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("existing");
        let projects_dir = tmp.path().join("projects.d");
        std::fs::create_dir(&project_dir).unwrap();
        let keep = project_dir.join("keep.txt");
        std::fs::write(&keep, "keep").unwrap();
        let policy = project_policy(tmp.path());
        let req = project_request(
            "create_project",
            serde_json::json!({
                "id": "existing",
                "name": "Existing",
                "path": project_dir.to_string_lossy(),
                "template": "basic",
                "allow_existing_empty": true
            }),
        );

        let err = project_err(handle_project_op(&policy, &projects_dir, &req));
        assert_eq!(err, "path_not_empty");
        assert_eq!(std::fs::read_to_string(keep).unwrap(), "keep");
    }

    #[test]
    fn create_project_rejects_unknown_template() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("new-project");
        let projects_dir = tmp.path().join("projects.d");
        let policy = project_policy(tmp.path());
        let req = project_request(
            "create_project",
            serde_json::json!({
                "id": "badtemplate",
                "name": "Bad Template",
                "path": project_dir.to_string_lossy(),
                "template": "cargo"
            }),
        );

        let err = project_err(handle_project_op(&policy, &projects_dir, &req));
        assert_eq!(err, "invalid_request");
        assert!(!project_dir.exists());
    }

    #[test]
    fn create_project_created_config_and_overwritten_semantics_are_accurate() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("empty-project");
        let projects_dir = tmp.path().join("projects.d");
        let policy = project_policy(tmp.path());
        let payload = |overwrite| {
            serde_json::json!({
                "id": "empty",
                "name": "Empty",
                "path": project_dir.to_string_lossy(),
                "template": "empty",
                "allow_existing_empty": true,
                "overwrite": overwrite
            })
        };

        let first = project_ok(handle_project_op(
            &policy,
            &projects_dir,
            &project_request("create_project", payload(false)),
        ));
        assert_eq!(first["created_directory"], true);
        assert_eq!(first["created_config"], true);
        assert_eq!(first["overwritten"], false);

        let second = project_ok(handle_project_op(
            &policy,
            &projects_dir,
            &project_request("create_project", payload(true)),
        ));
        assert_eq!(second["created_directory"], false);
        assert_eq!(second["created_config"], false);
        assert_eq!(second["overwritten"], true);
    }

    #[test]
    fn create_project_cleanup_removes_only_files_created_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("existing-empty");
        std::fs::create_dir(&project_dir).unwrap();
        let projects_dir_file = tmp.path().join("projects.d-is-file");
        std::fs::write(&projects_dir_file, "not a dir").unwrap();
        let policy = project_policy(tmp.path());
        let req = project_request(
            "create_project",
            serde_json::json!({
                "id": "cleanup",
                "name": "Cleanup",
                "path": project_dir.to_string_lossy(),
                "template": "basic",
                "allow_existing_empty": true
            }),
        );

        let err = project_err(handle_project_op(&policy, &projects_dir_file, &req));
        assert_eq!(err, "operation_failed");
        assert!(project_dir.exists());
        assert!(!project_dir.join("README.md").exists());
        assert!(!project_dir.join(".gitignore").exists());
    }

    #[test]
    fn create_project_does_not_delete_pre_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("existing");
        std::fs::create_dir(&project_dir).unwrap();
        let pre_existing = project_dir.join("pre-existing.txt");
        std::fs::write(&pre_existing, "original").unwrap();
        let projects_dir_file = tmp.path().join("projects.d-is-file");
        std::fs::write(&projects_dir_file, "not a dir").unwrap();
        let policy = project_policy(tmp.path());
        let req = project_request(
            "create_project",
            serde_json::json!({
                "id": "keep",
                "name": "Keep",
                "path": project_dir.to_string_lossy(),
                "template": "basic",
                "allow_existing_empty": true
            }),
        );

        let err = project_err(handle_project_op(&policy, &projects_dir_file, &req));
        assert_eq!(err, "path_not_empty");
        assert_eq!(std::fs::read_to_string(pre_existing).unwrap(), "original");
    }

    #[test]
    fn agent_project_cache_invalidate_refreshes_after_project_op() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("repo");
        let projects_dir = tmp.path().join("projects.d");
        std::fs::create_dir(&project_dir).unwrap();
        let mut cfg = test_config(projects_dir.clone());
        cfg.policy = project_policy(tmp.path());
        let mut cache = AgentProjectCache::default();
        assert!(cache.get(&cfg).is_empty());

        let req = project_request(
            "register_project",
            serde_json::json!({
                "id": "cached",
                "name": "Cached",
                "path": project_dir.to_string_lossy()
            }),
        );
        project_ok(handle_project_op(&cfg.policy, &projects_dir, &req));

        assert!(
            cache.get(&cfg).is_empty(),
            "cache should still be stale before invalidation"
        );
        cache.invalidate();
        let projects = cache.get(&cfg);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, "cached");
    }

    #[test]
    fn http_sink_client_id_matches_config() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path().join("config/projects.d"));
        let client = Client::new();
        let sink = AgentSink::Http(HttpSendConfig {
            client,
            server_url: cfg.server_url.clone(),
            token: cfg.token.clone(),
            client_id: cfg.client_id.clone(),
            agent_instance_id: "inst-1".to_string(),
            shutdown: Arc::new(AtomicBool::new(false)),
        });
        assert_eq!(sink.client_id(), "oe");
        assert_eq!(sink.agent_instance_id(), "inst-1");
    }

    #[test]
    fn empty_tokens_are_not_sent_as_credentials() {
        use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

        let request = build_ws_request("ws://127.0.0.1:8080/api/agents/ws", "").unwrap();
        assert!(request.headers().get(AUTHORIZATION).is_none());

        let request = build_ws_request("ws://127.0.0.1:8080/api/agents/ws", "   \t").unwrap();
        assert!(request.headers().get(AUTHORIZATION).is_none());

        let request = build_ws_request("ws://127.0.0.1:8080/api/agents/ws", "  abc123  ").unwrap();
        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Bearer abc123"
        );

        assert_eq!(non_empty_token(""), None);
        assert_eq!(non_empty_token("   \t"), None);
        assert_eq!(non_empty_token("  abc123  "), Some("abc123".to_string()));
    }

    #[test]
    fn empty_tokens_http_register_omits_authorization_header() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buf = [0u8; 16 * 1024];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(
                request.starts_with("POST /api/shell/agent/register "),
                "unexpected request: {request}"
            );
            assert!(
                !request.to_ascii_lowercase().contains("authorization:"),
                "empty token must not send Authorization header: {request}"
            );
            let body = r#"{"success":true,"client":null,"error":null}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_config(tmp.path().join("projects.d"));
        cfg.server_url = format!("http://{}", addr);
        cfg.token = "   \t".to_string();

        let client = Client::builder().no_proxy().build().unwrap();
        let mut project_cache = AgentProjectCache::default();
        let runtime = ReloadableAgentConfig::new(cfg.clone(), PathBuf::new());
        register(
            &client,
            &cfg,
            &runtime,
            &mut project_cache,
            None,
            "inst-empty-token",
            0,
        )
        .unwrap();
        server.join().unwrap();
    }

    // ------------------------------------------------------------------------
    // WebSocket session: Pong must be handled as keepalive, not unexpected
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn websocket_session_accepts_pong_without_error_or_disconnect() {
        use futures_util::{SinkExt, StreamExt};
        use tokio::net::TcpListener;
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        // Minimal WS server. It:
        //   1. reads the agent's Register,
        //   2. sends a Registered ack,
        //   3. sends a Pong (the frame that previously triggered the noisy
        //      "ignoring unexpected envelope: pong" path),
        //   4. sends a Ping and waits for the agent's Pong reply — if the
        //      agent had exited on the Pong in step 3 it would never reply,
        //      and this receive would time out (failing the test),
        //   5. drops the socket so the agent's session returns cleanly.
        //
        // This both guards the "Pong is not unexpected" regression and proves
        // the session stays alive after a Pong.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            // Read Register.
            let reg_msg = ws.next().await.unwrap().unwrap();
            let reg_env =
                AgentEnvelope::from_slice(reg_msg.into_text().unwrap().as_bytes()).unwrap();
            assert!(matches!(reg_env, AgentEnvelope::Register { .. }));

            // Ack register.
            let ack = AgentEnvelope::Registered {
                success: true,
                client: None,
                error: None,
            };
            ws.send(WsMessage::Text(ack.to_json().unwrap().into()))
                .await
                .unwrap();

            // Send a Pong — the agent must accept it as keepalive and stay
            // connected (this is the regression we are guarding against).
            let pong = AgentEnvelope::Pong { ts: 42 };
            ws.send(WsMessage::Text(pong.to_json().unwrap().into()))
                .await
                .unwrap();

            // Probe liveness: send a Ping and expect a Pong reply. If the
            // agent had broken out of its read loop on the Pong above, this
            // would time out.
            ws.send(WsMessage::Text(
                AgentEnvelope::Ping { ts: 7 }.to_json().unwrap().into(),
            ))
            .await
            .unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(2), ws.next())
                .await
                .expect("agent did not reply to ping after pong (session exited on pong)")
                .expect("stream open")
                .expect("ok message");
            match AgentEnvelope::from_slice(reply.into_text().unwrap().as_bytes()).unwrap() {
                AgentEnvelope::Pong { ts } => assert_eq!(ts, 7),
                other => panic!("expected pong reply, got {:?}", other.kind()),
            }

            // Drop the socket; the agent's reader will error/EOF and the
            // session returns cleanly. Avoids a close-handshake that can hang
            // on a current-thread test runtime.
            drop(ws);
        });

        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_config(tmp.path().join("config/projects.d"));
        cfg.server_url = format!("http://{}", addr);
        cfg.transport = Some(TRANSPORT_WEBSOCKET.to_string());
        let runtime = AgentRuntimeState::new(&cfg, PathBuf::new());

        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            websocket_session(&cfg, Vec::new(), "inst-1", &runtime),
        )
        .await
        .expect("websocket_session did not complete in time");

        // The session must end (server dropped the socket) and must NOT have
        // returned an error — a Pong is normal keepalive traffic.
        assert!(
            outcome.is_ok(),
            "websocket_session errored on Pong (regression): {:?}",
            outcome
        );

        server_task.await.unwrap();
    }
}
