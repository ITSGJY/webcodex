use super::config::{
    max_concurrent_jobs, projects_dir, validate_quic_config, AgentConfig, HotAgentConfig,
    QuicClientConfig, ReloadableAgentConfig,
};
use super::lsp::LspSupervisor;
use super::projects::AgentProjectCache;
use crate::agent_init::{TRANSPORT_AUTO, TRANSPORT_POLLING, TRANSPORT_QUIC, TRANSPORT_WEBSOCKET};
use crate::shell_protocol::{
    read_quic_frame, write_quic_frame, AgentEnvelope, QuicFrameError, ShellAgentJobUpdateRequest,
    ShellAgentJobUpdateResponse, ShellAgentProjectSummary, ShellAgentResultRequest,
    ShellAgentResultResponse, AGENT_PROTOCOL_VERSION_QUIC_V1, AGENT_PROTOCOL_VERSION_WEBSOCKET_V1,
};
use crate::{
    build_register_request_with_provider_status, dispatch_request, handle_one_poll, register,
    AgentHttpError, AgentHttpErrorKind, CommandResult, JobManager, PollingRecoveryAction,
    RegisterRecoveryAction,
};
use reqwest::blocking::Client;
use std::fmt;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

/// WebSocket outgoing envelope channel capacity.
pub(crate) const WS_OUTGOING_CAPACITY: usize = 64;
/// WebSocket ping interval.
const WS_PING_INTERVAL: Duration = Duration::from_secs(30);
/// Bounded reconnect backoff after a transport disconnect or transient error.
const RECONNECT_BACKOFF_STEPS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
/// Reset reconnect backoff after a connection stayed up long enough to prove
/// the endpoint is healthy. Immediate flapping still escalates.
const RECONNECT_STABLE_RESET_AFTER: Duration = Duration::from_secs(60);
/// Bounded wait for the writer task to flush its last frame and close the
/// sink during shutdown. A split WebSocket sink's `close()` waits for the
/// peer's close acknowledgement, which is delivered through the read half;
/// once the read loop has broken the read half is no longer polled, so
/// `close()` can hang indefinitely on a half-closed socket. Bounding it
/// guarantees `websocket_session` (and therefore the reconnect loop) always
/// makes progress instead of stalling forever after a disconnect.
const WS_WRITER_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
/// Bounded wait for local agent jobs to acknowledge a process shutdown.
const JOB_SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Bounded wait for Tokio blocking tasks when the transport runtime exits.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// Granularity for signal-aware sleeps in the blocking polling loop.
const POLLING_SHUTDOWN_SLEEP_SLICE: Duration = Duration::from_millis(50);
/// Polling session recovery is persistent but capped: after reaching 10s,
/// repeated failures continue at 10s until recovery, shutdown, or a fatal
/// auth/protocol/config response.
const POLLING_RECOVERY_BACKOFF_STEPS: [Duration; 5] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];
/// The current server contract keeps an active polling instance online for
/// 60 seconds. Permit one lease window plus scheduling slack during deployment
/// replacement, but do not let a real duplicate runner retry forever.
const POLLING_LEASE_CONFLICT_MAX_WAIT: Duration = Duration::from_secs(75);
/// Result submission endpoint used by the polling transport sink.
const AGENT_RESULT_PATH: &str = "/api/shell/agent/result";
/// Bounded same-payload retry backoff for transient result submission
/// failures over the polling transport. After the last step the payload is
/// released with an explicit dropped outcome, so a single result can never
/// monopolize the polling loop or trigger outer re-registration recovery.
const RESULT_SUBMIT_RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

fn send_provider_metadata(
    tx: &tokio::sync::mpsc::Sender<AgentEnvelope>,
    runtime: &ReloadableAgentConfig,
    expected_generation: Option<u64>,
) {
    runtime.with_active(|config| {
        if expected_generation.is_some_and(|expected| expected != config.generation) {
            return;
        }
        let Some((mut status, revision)) = config.external_tools.claim_status_update() else {
            return;
        };
        status.config_reload = config.reload_status();
        if tx
            .try_send(AgentEnvelope::RuntimeMetadata {
                tool_providers: status,
            })
            .is_err()
        {
            config.external_tools.release_status_update(revision);
        } else {
            config.external_tools.mark_status_reported(revision);
        }
    });
}

#[derive(Clone)]
pub(crate) struct AgentRuntimeState {
    lsp: LspSupervisor,
    config: Arc<ReloadableAgentConfig>,
}

impl AgentRuntimeState {
    pub(crate) fn new(cfg: &AgentConfig, path: PathBuf) -> Self {
        Self {
            lsp: LspSupervisor::default(),
            config: Arc::new(ReloadableAgentConfig::new(cfg.clone(), path)),
        }
    }

    fn shutdown(&self) {
        self.lsp.shutdown();
    }
}

async fn stop_jobs_for_shutdown(jobs: &JobManager, poll_interval_ms: u64) {
    jobs.stop_all();
    let start = std::time::Instant::now();
    let sleep = Duration::from_millis(poll_interval_ms.clamp(50, 250));
    while jobs.has_work() && start.elapsed() < JOB_SHUTDOWN_DRAIN_TIMEOUT {
        let remaining = JOB_SHUTDOWN_DRAIN_TIMEOUT.saturating_sub(start.elapsed());
        tokio::time::sleep(sleep.min(remaining)).await;
    }
    if jobs.has_work() {
        eprintln!("webcodex-runner shutdown: active jobs did not stop within 2s; exiting");
    }
}

fn stop_jobs_for_polling_shutdown(jobs: &JobManager, poll_interval_ms: u64) {
    jobs.stop_all();
    let start = Instant::now();
    let sleep = Duration::from_millis(poll_interval_ms.clamp(50, 250));
    while jobs.has_work() && start.elapsed() < JOB_SHUTDOWN_DRAIN_TIMEOUT {
        let remaining = JOB_SHUTDOWN_DRAIN_TIMEOUT.saturating_sub(start.elapsed());
        std::thread::sleep(sleep.min(remaining));
    }
    if jobs.has_work() {
        eprintln!("webcodex-runner shutdown: active jobs did not stop within 2s; exiting");
    }
}

fn sleep_or_shutdown(delay: Duration, shutdown: &AtomicBool) -> bool {
    let start = Instant::now();
    while start.elapsed() < delay {
        if shutdown.load(Ordering::SeqCst) {
            return true;
        }
        let remaining = delay.saturating_sub(start.elapsed());
        std::thread::sleep(remaining.min(POLLING_SHUTDOWN_SLEEP_SLICE));
    }
    shutdown.load(Ordering::SeqCst)
}

fn install_polling_shutdown_flag() -> Arc<AtomicBool> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let listener_flag = Arc::clone(&shutdown);
    let _ = std::thread::Builder::new()
        .name("webcodex-runner-shutdown".to_string())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(shutdown_signal());
            listener_flag.store(true, Ordering::SeqCst);
        });
    shutdown
}

fn finish_polling_shutdown(jobs: &JobManager, poll_interval_ms: u64) {
    eprintln!("webcodex-runner received process shutdown signal; exiting");
    stop_jobs_for_polling_shutdown(jobs, poll_interval_ms);
}

fn complete_polling_shutdown(
    runtime: &AgentRuntimeState,
    jobs: &JobManager,
    poll_interval_ms: u64,
) -> Result<(), String> {
    runtime.config.begin_shutdown();
    finish_polling_shutdown(jobs, poll_interval_ms);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentTransportError {
    Transient(String),
    Fatal(String),
}

impl AgentTransportError {
    fn transient(message: impl Into<String>) -> Self {
        Self::Transient(message.into())
    }

    fn fatal(message: impl Into<String>) -> Self {
        Self::Fatal(message.into())
    }

    fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal(_))
    }

    fn into_message(self) -> String {
        match self {
            Self::Transient(message) | Self::Fatal(message) => message,
        }
    }
}

impl fmt::Display for AgentTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transient(message) | Self::Fatal(message) => f.write_str(message),
        }
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn is_fatal_auth_or_register_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    contains_any(
        &lower,
        &[
            "register rejected",
            "register_failed",
            "register_forbidden",
            "unauthorized",
            "forbidden",
            "invalid token",
            "bad token",
            "auth failed",
            "authentication",
            "expected registered ack",
            "register ack was not text",
            "register ack is not a valid envelope",
        ],
    )
}

fn is_fatal_config_or_tls_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    contains_any(
        &lower,
        &[
            "invalid websocket url",
            "server_url must be",
            "transport=quic requires",
            "[quic]",
            "certificate",
            "webpki",
            "notvalidforname",
            "unknownissuer",
            "invalid server name",
            "invalid dns",
            "no application protocol",
            "alpn mismatch",
        ],
    )
}

fn classify_session_error(message: impl Into<String>) -> AgentTransportError {
    let message = message.into();
    if is_fatal_auth_or_register_error(&message) || is_fatal_config_or_tls_error(&message) {
        AgentTransportError::fatal(message)
    } else {
        AgentTransportError::transient(message)
    }
}

fn redact_url_queries(message: &str) -> String {
    let mut remaining = message;
    let mut redacted = String::with_capacity(message.len());
    loop {
        let http = remaining.find("http://");
        let https = remaining.find("https://");
        let url_start = match (http, https) {
            (Some(http), Some(https)) => http.min(https),
            (Some(http), None) => http,
            (None, Some(https)) => https,
            (None, None) => {
                redacted.push_str(remaining);
                break;
            }
        };
        redacted.push_str(&remaining[..url_start]);
        let url = &remaining[url_start..];
        let url_end = url.find(char::is_whitespace).unwrap_or(url.len());
        let segment = &url[..url_end];
        if let Some(query_start) = segment.find('?') {
            redacted.push_str(&segment[..query_start]);
            redacted.push_str("?[redacted]");
        } else {
            redacted.push_str(segment);
        }
        remaining = &url[url_end..];
    }
    redacted
}

fn concise_log_error(message: &str, token: &str) -> String {
    let mut sanitized = redact_url_queries(message).replace(['\r', '\n'], " ");
    let token = token.trim();
    if !token.is_empty() {
        sanitized = sanitized.replace(token, "[redacted]");
    }
    const MAX_CHARS: usize = 180;
    if sanitized.chars().count() > MAX_CHARS {
        let mut out = sanitized.chars().take(MAX_CHARS).collect::<String>();
        out.push_str("...");
        out
    } else {
        sanitized
    }
}

fn server_log_label(server_url: &str) -> String {
    match url::Url::parse(server_url) {
        Ok(parsed) => {
            let Some(host) = parsed.host_str() else {
                return parsed.scheme().to_string();
            };
            let host = if host.contains(':') && !host.starts_with('[') {
                format!("[{}]", host)
            } else {
                host.to_string()
            };
            match parsed.port() {
                Some(port) => format!("{}://{}:{}", parsed.scheme(), host, port),
                None => format!("{}://{}", parsed.scheme(), host),
            }
        }
        Err(_) => server_url
            .split('?')
            .next()
            .unwrap_or(server_url)
            .trim_end_matches('/')
            .to_string(),
    }
}

fn enabled_projects_count(projects: &[ShellAgentProjectSummary]) -> usize {
    projects.iter().filter(|project| !project.disabled).count()
}

fn registered_log_line(cfg: &AgentConfig, actual_transport: &str, projects_count: usize) -> String {
    format!(
        "webcodex-runner registered client_id={} server={} preferred_transport={} actual_transport={} projects={}",
        cfg.client_id,
        server_log_label(&cfg.server_url),
        effective_transport(cfg),
        actual_transport,
        projects_count
    )
}

fn auto_quic_not_configured_log_line() -> &'static str {
    "webcodex-runner transport auto: quic not configured; skipping"
}

fn auto_trying_log_line(transport: &str) -> String {
    format!("webcodex-runner transport auto: {} trying", transport)
}

#[cfg(unix)]
async fn shutdown_signal() {
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            if let Some(signal) = sigterm.as_mut() {
                let _ = signal.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(unix)]
fn install_reload_listener(
    runtime: Arc<ReloadableAgentConfig>,
) -> Result<std::thread::JoinHandle<()>, String> {
    let signal_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "failed to initialize config reload signal listener".to_string())?;
    let mut sighup = {
        let _guard = signal_runtime.enter();
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .map_err(|_| "failed to install config reload signal listener".to_string())?
    };
    std::thread::Builder::new()
        .name("webcodex-runner-reload".to_string())
        .spawn(move || {
            signal_runtime.block_on(async move {
                while !runtime.is_stopping() {
                    match tokio::time::timeout(Duration::from_millis(100), sighup.recv()).await {
                        Ok(Some(_)) => {
                            runtime.reload();
                        }
                        Ok(None) => break,
                        Err(_) => {}
                    }
                }
            });
        })
        .map_err(|_| "failed to start config reload signal listener".to_string())
}

/// Minimal HTTP send configuration used by the polling `AgentSink`. We do not
/// store the whole `AgentConfig` here: policy and concurrency limits stay
/// with the agent config and are passed alongside the sink.
#[derive(Debug, Clone)]
pub(crate) struct HttpSendConfig {
    pub(crate) client: Client,
    pub(crate) server_url: String,
    pub(crate) token: String,
    pub(crate) client_id: String,
    pub(crate) agent_instance_id: String,
    pub(crate) shutdown: Arc<AtomicBool>,
}

/// Transport-neutral outgoing channel for an agent. Both the polling loop and
/// the WebSocket loop build an `AgentSink` and hand it to the shared
/// `dispatch_request` / `JobManager` execution path. This shared boundary lets
/// the agent speak either transport without duplicating execution logic.
#[derive(Debug, Clone)]
pub(crate) enum AgentSink {
    /// Polling transport: POST results/job_updates to the HTTP endpoints.
    Http(HttpSendConfig),
    /// WebSocket transport: push envelopes through an mpsc that a writer task
    /// drains onto the socket.
    WebSocket {
        tx: tokio::sync::mpsc::Sender<AgentEnvelope>,
        client_id: String,
        agent_instance_id: String,
    },
    /// QUIC transport: push envelopes through an mpsc that a single writer
    /// task drains onto the bidirectional stream.
    Quic {
        tx: tokio::sync::mpsc::Sender<AgentEnvelope>,
        client_id: String,
        agent_instance_id: String,
    },
}

/// Outcome of a result submission that no longer needs the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultSubmission {
    /// The server accepted the result.
    Accepted,
    /// The server permanently rejected this exact payload (e.g. the request
    /// expired, was cancelled, or this instance lost the lease). The payload
    /// has been logged once (bounded, redacted) and released; the caller must
    /// keep polling instead of retrying it.
    RejectedPermanent,
    /// A transient HTTP failure persisted through every bounded retry. The
    /// payload has been logged once (bounded, redacted) and released so the
    /// polling runner remains live without retrying forever or entering the
    /// unrelated re-registration recovery path.
    DroppedAfterRetryExhaustion,
}

/// Structured result failures that require the current agent/session to stop.
/// Polling HTTP transients never reach this type: they are retried in place
/// and become `DroppedAfterRetryExhaustion` if the bounded budget is exhausted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubmitResultError {
    /// 401/403: credentials are wrong or revoked. Never retried.
    FatalAuth(String),
    /// 404: endpoint missing or incompatible server. Never retried.
    FatalProtocol(String),
    /// Invalid HTTP URL or non-recoverable TLS configuration. Never retried.
    FatalConfig(String),
    /// A WebSocket/QUIC outgoing channel closed before the result could be
    /// queued. This is a transport-session failure, not an HTTP retry outcome.
    TransportClosed(String),
    /// Process shutdown interrupted an HTTP retry backoff. The polling loop
    /// handles this as a clean shutdown rather than an operational failure.
    Shutdown(String),
}

impl fmt::Display for SubmitResultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FatalAuth(message)
            | Self::FatalProtocol(message)
            | Self::FatalConfig(message)
            | Self::TransportClosed(message)
            | Self::Shutdown(message) => f.write_str(message),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultHttpErrorDisposition {
    RetryTransient,
    RejectPermanent,
    FatalAuth,
    FatalProtocol,
    FatalConfig,
}

fn result_http_error_disposition(kind: &AgentHttpErrorKind) -> ResultHttpErrorDisposition {
    match kind {
        AgentHttpErrorKind::ServerUnavailable
        | AgentHttpErrorKind::Status
        | AgentHttpErrorKind::RequestTimeout
        | AgentHttpErrorKind::Request
        | AgentHttpErrorKind::DecodeTransient => ResultHttpErrorDisposition::RetryTransient,
        AgentHttpErrorKind::ClientRejected => ResultHttpErrorDisposition::RejectPermanent,
        AgentHttpErrorKind::Auth => ResultHttpErrorDisposition::FatalAuth,
        AgentHttpErrorKind::NotFound | AgentHttpErrorKind::ProtocolDecode => {
            ResultHttpErrorDisposition::FatalProtocol
        }
        AgentHttpErrorKind::Config => ResultHttpErrorDisposition::FatalConfig,
    }
}

/// One bounded, redacted diagnostic line for a permanently rejected result.
/// Emitted exactly once per payload: permanent rejections are never retried,
/// so this cannot repeat for the same result.
fn permanent_result_rejection_log_line(request_id: &str, error: &str, token: &str) -> String {
    format!(
        "webcodex-runner result permanently rejected request_id={} error={}; dropping this result and continuing to poll",
        concise_log_error(request_id, token),
        concise_log_error(error, token)
    )
}

/// One bounded, redacted warning after all transient submission attempts have
/// failed. It makes the possible result loss explicit without exposing raw
/// response bodies, credentials, or multiline request errors.
fn dropped_result_log_line(request_id: &str, attempts: usize, error: &str, token: &str) -> String {
    format!(
        "webcodex-runner result submission retries exhausted request_id={} attempts={} error={}; dropping this result and continuing to poll",
        concise_log_error(request_id, token),
        attempts,
        concise_log_error(error, token)
    )
}

/// Submit one result over the polling HTTP transport. Transient failures are
/// retried in place with bounded backoff; permanent rejections release the
/// payload after a single bounded log line; exhausted transient failures also
/// release it with an explicit dropped outcome; only auth/protocol failures
/// surface as errors that terminate the polling agent.
fn submit_result_http(
    h: &HttpSendConfig,
    body: &ShellAgentResultRequest,
) -> Result<ResultSubmission, SubmitResultError> {
    let mut attempt = 0usize;
    loop {
        let error = match post_json_raw::<_, ShellAgentResultResponse>(
            &h.client,
            &h.server_url,
            &h.token,
            AGENT_RESULT_PATH,
            body,
        ) {
            Ok(resp) if resp.success => return Ok(ResultSubmission::Accepted),
            Ok(resp) => {
                // A structured `success: false` answer is an explicit server
                // decision about this payload; resending it cannot succeed.
                let reason = resp
                    .error
                    .unwrap_or_else(|| "result submission failed without error".to_string());
                eprintln!(
                    "{}",
                    permanent_result_rejection_log_line(&body.request_id, &reason, &h.token)
                );
                return Ok(ResultSubmission::RejectedPermanent);
            }
            Err(error) => error,
        };
        match result_http_error_disposition(&error.kind) {
            ResultHttpErrorDisposition::RejectPermanent => {
                eprintln!(
                    "{}",
                    permanent_result_rejection_log_line(
                        &body.request_id,
                        &error.to_string(),
                        &h.token
                    )
                );
                return Ok(ResultSubmission::RejectedPermanent);
            }
            ResultHttpErrorDisposition::FatalAuth => {
                return Err(SubmitResultError::FatalAuth(error.to_string()));
            }
            ResultHttpErrorDisposition::FatalProtocol => {
                return Err(SubmitResultError::FatalProtocol(error.to_string()));
            }
            ResultHttpErrorDisposition::FatalConfig => {
                return Err(SubmitResultError::FatalConfig(error.to_string()));
            }
            ResultHttpErrorDisposition::RetryTransient => {
                let Some(delay) = RESULT_SUBMIT_RETRY_BACKOFF.get(attempt).copied() else {
                    eprintln!(
                        "{}",
                        dropped_result_log_line(
                            &body.request_id,
                            attempt + 1,
                            &error.to_string(),
                            &h.token
                        )
                    );
                    return Ok(ResultSubmission::DroppedAfterRetryExhaustion);
                };
                attempt += 1;
                if sleep_or_shutdown(delay, h.shutdown.as_ref()) {
                    return Err(SubmitResultError::Shutdown(
                        "result submission retry interrupted by process shutdown".to_string(),
                    ));
                }
            }
        }
    }
}

impl AgentSink {
    pub(crate) fn client_id(&self) -> &str {
        match self {
            AgentSink::Http(h) => &h.client_id,
            AgentSink::WebSocket { client_id, .. } => client_id,
            AgentSink::Quic { client_id, .. } => client_id,
        }
    }

    /// Active agent process identity carried by this sink so every result /
    /// job_update submission includes it.
    pub(crate) fn agent_instance_id(&self) -> &str {
        match self {
            AgentSink::Http(h) => &h.agent_instance_id,
            AgentSink::WebSocket {
                agent_instance_id, ..
            } => agent_instance_id,
            AgentSink::Quic {
                agent_instance_id, ..
            } => agent_instance_id,
        }
    }

    /// Submit the result of a synchronous shell/file request. Mirrors the old
    /// `submit_result` free function but routes over the active transport.
    pub(crate) fn submit_result(
        &self,
        request_id: String,
        result: CommandResult,
    ) -> Result<ResultSubmission, SubmitResultError> {
        let body = ShellAgentResultRequest {
            client_id: self.client_id().to_string(),
            agent_instance_id: self.agent_instance_id().to_string(),
            request_id,
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
            duration_ms: result.duration_ms,
            error: result.error,
        };
        match self {
            AgentSink::Http(h) => submit_result_http(h, &body),
            AgentSink::WebSocket { tx, .. } | AgentSink::Quic { tx, .. } => {
                let env = AgentEnvelope::Result { payload: body };
                tx.blocking_send(env).map_err(|_| {
                    SubmitResultError::TransportClosed(
                        "agent transport result channel closed".to_string(),
                    )
                })?;
                Ok(ResultSubmission::Accepted)
            }
        }
    }

    pub(crate) fn submit_result_with_metadata(
        &self,
        request_id: String,
        result: CommandResult,
        config: &HotAgentConfig,
        runtime: &ReloadableAgentConfig,
    ) -> Result<ResultSubmission, SubmitResultError> {
        let submitted = self.submit_result(request_id, result);
        // Provider metadata is a best-effort follow-up on push transports, not
        // proof that a rejected or dropped result was accepted. Send it only
        // after the result reached the transport successfully.
        if matches!(&submitted, Ok(ResultSubmission::Accepted)) {
            self.send_provider_metadata_best_effort(config.generation, runtime);
        }
        submitted
    }

    fn send_provider_metadata_best_effort(&self, generation: u64, runtime: &ReloadableAgentConfig) {
        let (AgentSink::WebSocket { tx, .. } | AgentSink::Quic { tx, .. }) = self else {
            return;
        };
        send_provider_metadata(tx, runtime, Some(generation));
    }

    /// Push an incremental/final job update. Mirrors the old `send_job_update`
    /// free function. Job updates stay best-effort: callers ignore failures
    /// and the terminal state is still resolved by the final result path.
    pub(crate) fn send_job_update(&self, body: &ShellAgentJobUpdateRequest) -> Result<(), String> {
        match self {
            AgentSink::Http(h) => {
                let resp: ShellAgentJobUpdateResponse = post_json_raw(
                    &h.client,
                    &h.server_url,
                    &h.token,
                    "/api/shell/agent/job_update",
                    body,
                )
                .map_err(|e| e.to_string())?;
                if resp.success {
                    Ok(())
                } else {
                    Err(resp
                        .error
                        .unwrap_or_else(|| "job_update failed without error".to_string()))
                }
            }
            AgentSink::WebSocket { tx, .. } | AgentSink::Quic { tx, .. } => {
                let env = AgentEnvelope::JobUpdate {
                    payload: body.clone(),
                };
                tx.blocking_send(env)
                    .map_err(|_| "agent transport send failed".to_string())
            }
        }
    }
}

/// Send a JSON POST to the server and decode the response. Same wire behavior
/// as `post_json` but takes the raw connection bits so it can be used from
/// `AgentSink::Http` without an `AgentConfig`. Preserves the structured
/// `AgentHttpError` classification for callers that must act on it.
fn post_json_raw<T, R>(
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
    crate::post_json_with_auth(client, server_url, token, path, body)
}

pub(crate) fn non_empty_token(token: &str) -> Option<String> {
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

pub(crate) fn run_agent(cfg: AgentConfig, config_path: PathBuf, once: bool) -> Result<(), String> {
    // Generate the per-process agent instance identity once. It is stable for
    // the whole process lifetime, including across WebSocket reconnects, so the
    // server can treat this process as a single active lease for `client_id`.
    // It is not a secret and is never persisted to disk.
    let agent_instance_id = uuid::Uuid::new_v4().to_string();
    let transport = cfg
        .transport
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(TRANSPORT_WEBSOCKET)
        .to_string();
    // The LSP supervisor belongs to the agent process rather than any server
    // transport session and is shared across reconnects.
    let runtime = AgentRuntimeState::new(&cfg, config_path);
    #[cfg(unix)]
    let reload_listener = install_reload_listener(Arc::clone(&runtime.config))?;
    let result = match transport.as_str() {
        TRANSPORT_WEBSOCKET => run_websocket_agent(cfg, once, &agent_instance_id, &runtime),
        TRANSPORT_QUIC => run_quic_agent(cfg, once, &agent_instance_id, &runtime),
        TRANSPORT_AUTO => run_auto_agent(cfg, once, &agent_instance_id, &runtime),
        _ => run_polling_agent(cfg, once, &agent_instance_id, &runtime),
    };
    runtime.config.begin_shutdown();
    #[cfg(unix)]
    let _ = reload_listener.join();
    runtime.shutdown();
    result
}

pub(crate) fn effective_transport(cfg: &AgentConfig) -> &str {
    cfg.transport
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(TRANSPORT_WEBSOCKET)
}

pub(crate) fn auto_transport_plan(cfg: &AgentConfig) -> Vec<&'static str> {
    let mut plan = Vec::new();
    if cfg.quic.is_some() {
        plan.push(TRANSPORT_QUIC);
    }
    plan.push(TRANSPORT_WEBSOCKET);
    plan.push(TRANSPORT_POLLING);
    plan
}

#[derive(Debug, Clone)]
struct ReconnectBackoff {
    attempts: usize,
}

#[derive(Debug, Clone)]
struct PollingRecoveryBackoff {
    attempts: usize,
}

impl PollingRecoveryBackoff {
    fn new() -> Self {
        Self { attempts: 0 }
    }

    fn reset(&mut self) {
        self.attempts = 0;
    }

    fn next_delay(&mut self) -> Duration {
        let delay = POLLING_RECOVERY_BACKOFF_STEPS
            .get(self.attempts)
            .copied()
            .unwrap_or_else(|| {
                *POLLING_RECOVERY_BACKOFF_STEPS
                    .last()
                    .expect("polling recovery backoff is non-empty")
            });
        self.attempts = self.attempts.saturating_add(1);
        delay
    }
}

fn next_lease_conflict_delay(
    backoff: &mut PollingRecoveryBackoff,
    elapsed: Duration,
) -> Option<Duration> {
    if elapsed >= POLLING_LEASE_CONFLICT_MAX_WAIT {
        return None;
    }
    Some(
        backoff
            .next_delay()
            .min(POLLING_LEASE_CONFLICT_MAX_WAIT.saturating_sub(elapsed)),
    )
}

impl ReconnectBackoff {
    fn new() -> Self {
        Self { attempts: 0 }
    }

    fn reset(&mut self) {
        self.attempts = 0;
    }

    fn next_delay(&mut self) -> Duration {
        let delay = RECONNECT_BACKOFF_STEPS
            .get(self.attempts)
            .copied()
            .unwrap_or_else(|| {
                *RECONNECT_BACKOFF_STEPS
                    .last()
                    .expect("reconnect backoff is non-empty")
            });
        self.attempts = self.attempts.saturating_add(1);
        delay
    }
}

fn format_delay(delay: Duration) -> String {
    if delay.as_millis() % 1000 == 0 {
        format!("{}s", delay.as_secs())
    } else {
        format!("{}ms", delay.as_millis())
    }
}

fn schedule_reconnect(transport: &str, backoff: &mut ReconnectBackoff) -> Duration {
    let delay = backoff.next_delay();
    eprintln!(
        "webcodex-runner reconnect attempt scheduled transport={} delay={}",
        transport,
        format_delay(delay)
    );
    tracing::debug!(
        transport,
        delay_ms = delay.as_millis() as u64,
        "webcodex-runner reconnect attempt scheduled"
    );
    delay
}

fn reset_backoff_after_stable_session(backoff: &mut ReconnectBackoff, started_at: Instant) {
    if started_at.elapsed() >= RECONNECT_STABLE_RESET_AFTER {
        backoff.reset();
    }
}

fn run_auto_agent(
    cfg: AgentConfig,
    once: bool,
    agent_instance_id: &str,
    runtime: &AgentRuntimeState,
) -> Result<(), String> {
    let mut backoff = ReconnectBackoff::new();
    'supervisor: loop {
        if cfg.quic.is_none() {
            eprintln!("{}", auto_quic_not_configured_log_line());
        }
        for transport in auto_transport_plan(&cfg) {
            match transport {
                TRANSPORT_QUIC => {
                    eprintln!("{}", auto_trying_log_line(TRANSPORT_QUIC));
                    let session_started = Instant::now();
                    match run_quic_agent_single_session(&cfg, once, agent_instance_id, runtime) {
                        Ok(AgentSessionExit::Shutdown) => return Ok(()),
                        Ok(AgentSessionExit::Completed) if once => return Ok(()),
                        Ok(AgentSessionExit::Completed) => return Ok(()),
                        Ok(AgentSessionExit::TransportDisconnected) if once => return Ok(()),
                        Ok(AgentSessionExit::TransportDisconnected) => {
                            reset_backoff_after_stable_session(&mut backoff, session_started);
                            eprintln!("webcodex-runner quic connection closed; reconnecting");
                            std::thread::sleep(schedule_reconnect(TRANSPORT_QUIC, &mut backoff));
                            continue 'supervisor;
                        }
                        Err(e) => {
                            let e = classify_session_error(e);
                            if e.is_fatal() {
                                return Err(e.into_message());
                            }
                            let log_error = concise_log_error(&e.to_string(), &cfg.token);
                            eprintln!(
                                "webcodex-runner transport auto: quic unavailable: {}; trying websocket",
                                log_error
                            );
                            tracing::debug!(
                                transport = "quic",
                                error = %log_error,
                                "webcodex-runner auto transport attempt failed"
                            );
                        }
                    }
                }
                TRANSPORT_WEBSOCKET => {
                    eprintln!("{}", auto_trying_log_line(TRANSPORT_WEBSOCKET));
                    let session_started = Instant::now();
                    match run_websocket_agent_single_session(&cfg, agent_instance_id, runtime) {
                        Ok(AgentSessionExit::Shutdown) => return Ok(()),
                        Ok(AgentSessionExit::Completed) if once => return Ok(()),
                        Ok(AgentSessionExit::Completed) => return Ok(()),
                        Ok(AgentSessionExit::TransportDisconnected) if once => return Ok(()),
                        Ok(AgentSessionExit::TransportDisconnected) => {
                            reset_backoff_after_stable_session(&mut backoff, session_started);
                            eprintln!("webcodex-runner websocket connection closed; reconnecting");
                            std::thread::sleep(schedule_reconnect(
                                TRANSPORT_WEBSOCKET,
                                &mut backoff,
                            ));
                            continue 'supervisor;
                        }
                        Err(e) => {
                            let e = classify_session_error(e);
                            if once {
                                return Err(e.into_message());
                            }
                            if e.is_fatal() {
                                return Err(e.into_message());
                            }
                            let log_error = concise_log_error(&e.to_string(), &cfg.token);
                            eprintln!(
                                "webcodex-runner transport auto: websocket failed: {}; falling back to polling",
                                log_error
                            );
                            tracing::debug!(
                                transport = "websocket",
                                error = %log_error,
                                "webcodex-runner auto transport attempt failed"
                            );
                        }
                    }
                }
                TRANSPORT_POLLING => {
                    eprintln!("{}", auto_trying_log_line(TRANSPORT_POLLING));
                    return run_polling_agent(cfg, once, agent_instance_id, runtime);
                }
                _ => {}
            }
        }
    }
}

fn run_polling_agent(
    cfg: AgentConfig,
    once: bool,
    agent_instance_id: &str,
    runtime: &AgentRuntimeState,
) -> Result<(), String> {
    let shutdown = install_polling_shutdown_flag();
    run_polling_agent_with_shutdown(cfg, once, agent_instance_id, shutdown, runtime)
}

fn run_polling_agent_with_shutdown(
    cfg: AgentConfig,
    once: bool,
    agent_instance_id: &str,
    shutdown: Arc<AtomicBool>,
    runtime: &AgentRuntimeState,
) -> Result<(), String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("failed to create http client: {}", e))?;
    let jobs = JobManager::new(max_concurrent_jobs(&cfg));
    let mut project_cache = AgentProjectCache::default();
    let mut registered = false;
    let mut recovering = false;
    let mut session_refreshed_during_recovery = false;
    let mut recovery_backoff = PollingRecoveryBackoff::new();
    let mut lease_conflict_started: Option<Instant> = None;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return complete_polling_shutdown(runtime, &jobs, cfg.poll_interval_ms);
        }
        if !registered {
            match register(
                &client,
                &cfg,
                &runtime.config,
                &mut project_cache,
                agent_instance_id,
                jobs.prepared_profiles.len(),
            ) {
                Ok(projects_count) => {
                    registered = true;
                    lease_conflict_started = None;
                    recovery_backoff.reset();
                    if recovering {
                        eprintln!(
                            "webcodex-runner polling session refreshed during recovery client_id={}",
                            concise_log_error(&cfg.client_id, &cfg.token)
                        );
                        session_refreshed_during_recovery = true;
                    }
                    eprintln!(
                        "{}",
                        registered_log_line(&cfg, TRANSPORT_POLLING, projects_count)
                    );
                }
                Err(error) => match error.recovery_action() {
                    RegisterRecoveryAction::Fatal => return Err(error.into_message()),
                    RegisterRecoveryAction::Retry => {
                        recovering = true;
                        let delay = recovery_backoff.next_delay();
                        eprintln!(
                            "webcodex-runner transient register failure; retrying delay={} error={}",
                            format_delay(delay),
                            concise_log_error(&error.to_string(), &cfg.token)
                        );
                        if sleep_or_shutdown(delay, shutdown.as_ref()) {
                            return complete_polling_shutdown(runtime, &jobs, cfg.poll_interval_ms);
                        }
                    }
                    RegisterRecoveryAction::WaitForLease => {
                        recovering = true;
                        let started = lease_conflict_started.get_or_insert_with(Instant::now);
                        let elapsed = started.elapsed();
                        let Some(delay) = next_lease_conflict_delay(&mut recovery_backoff, elapsed)
                        else {
                            return Err(format!(
                                "active-instance lease conflict for client_id={} did not clear within {}",
                                concise_log_error(&cfg.client_id, &cfg.token),
                                format_delay(POLLING_LEASE_CONFLICT_MAX_WAIT)
                            ));
                        };
                        eprintln!(
                            "webcodex-runner active-instance lease conflict; waiting client_id={} delay={}",
                            concise_log_error(&cfg.client_id, &cfg.token),
                            format_delay(delay)
                        );
                        if sleep_or_shutdown(delay, shutdown.as_ref()) {
                            return complete_polling_shutdown(runtime, &jobs, cfg.poll_interval_ms);
                        }
                    }
                },
            }
            continue;
        }
        match handle_one_poll(
            &client,
            &cfg,
            &runtime.config,
            &jobs,
            &mut project_cache,
            agent_instance_id,
            &runtime.lsp,
            &shutdown,
        ) {
            Ok(ran_request) => {
                recovery_backoff.reset();
                lease_conflict_started = None;
                if recovering {
                    eprintln!(
                        "webcodex-runner polling recovery succeeded phase=poll client_id={}",
                        concise_log_error(&cfg.client_id, &cfg.token)
                    );
                    recovering = false;
                    session_refreshed_during_recovery = false;
                }
                if once {
                    while jobs.has_work() {
                        if sleep_or_shutdown(
                            Duration::from_millis(cfg.poll_interval_ms),
                            shutdown.as_ref(),
                        ) {
                            return complete_polling_shutdown(runtime, &jobs, cfg.poll_interval_ms);
                        }
                    }
                    return Ok(());
                }
                if !ran_request {
                    if sleep_or_shutdown(
                        Duration::from_millis(cfg.poll_interval_ms),
                        shutdown.as_ref(),
                    ) {
                        return complete_polling_shutdown(runtime, &jobs, cfg.poll_interval_ms);
                    }
                }
            }
            Err(e) => {
                match e.recovery_action() {
                    PollingRecoveryAction::Shutdown => {
                        return complete_polling_shutdown(runtime, &jobs, cfg.poll_interval_ms);
                    }
                    PollingRecoveryAction::Fatal => return Err(e.into_message()),
                    PollingRecoveryAction::RetryPoll => {
                        recovering = true;
                        // Refresh the same-instance registration once per
                        // recovery episode. This covers a server restart that
                        // lost in-memory session state without registering on
                        // every repeated 5xx response.
                        if !session_refreshed_during_recovery {
                            registered = false;
                        }
                        let delay = recovery_backoff.next_delay();
                        eprintln!(
                            "webcodex-runner transient poll failure; retrying delay={} error={}",
                            format_delay(delay),
                            concise_log_error(&e.to_string(), &cfg.token)
                        );
                        if sleep_or_shutdown(delay, shutdown.as_ref()) {
                            return complete_polling_shutdown(runtime, &jobs, cfg.poll_interval_ms);
                        }
                    }
                    PollingRecoveryAction::ReRegister => {
                        recovering = true;
                        registered = false;
                        session_refreshed_during_recovery = false;
                        let delay = recovery_backoff.next_delay();
                        eprintln!(
                            "webcodex-runner polling session lost; re-registering delay={} error={}",
                            format_delay(delay),
                            concise_log_error(&e.to_string(), &cfg.token)
                        );
                        if sleep_or_shutdown(delay, shutdown.as_ref()) {
                            return complete_polling_shutdown(runtime, &jobs, cfg.poll_interval_ms);
                        }
                    }
                }
            }
        }
    }
}

// ============================================================================
// Experimental custom QUIC agent transport
// ============================================================================
//
// A custom QUIC *stream* transport (NOT HTTP/3). The agent opens a single QUIC
// bidirectional stream to the server, sends a `Register` envelope carrying the
// agent token in `auth_token` (there is no HTTP middleware to set an
// `Authorization` header), reads a `Registered` ack, then handles `Request`,
// `Result`, `JobUpdate`, `Ping`, and `Pong` envelopes on that same serialized
// stream. WebSocket/polling behavior is unchanged.

/// Interval between agent-initiated keepalive Pings.
const QUIC_PING_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentSessionExit {
    Completed,
    TransportDisconnected,
    Shutdown,
}

/// Entry point for the QUIC transport. Runs a tokio current-thread runtime and
/// reconnects on session failure, mirroring `run_websocket_agent`.
fn run_quic_agent(
    cfg: AgentConfig,
    once: bool,
    agent_instance_id: &str,
    runtime: &AgentRuntimeState,
) -> Result<(), String> {
    let agent_instance_id = agent_instance_id.to_string();
    let runtime = runtime.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to create tokio runtime: {}", e))?;
    let result = rt.block_on(async move {
        let mut project_cache = AgentProjectCache::default();
        let mut backoff = ReconnectBackoff::new();
        loop {
            let projects = project_cache.get(&cfg);
            let session_started = Instant::now();
            match quic_session(&cfg, projects, &agent_instance_id, once, &runtime).await {
                Ok(AgentSessionExit::Shutdown) => {
                    project_cache.invalidate();
                    eprintln!("webcodex-runner quic shutdown complete");
                    return Ok(());
                }
                Ok(AgentSessionExit::Completed) => {
                    project_cache.invalidate();
                    return Ok(());
                }
                Ok(AgentSessionExit::TransportDisconnected) => {
                    project_cache.invalidate();
                    if once {
                        return Ok(());
                    }
                    reset_backoff_after_stable_session(&mut backoff, session_started);
                    eprintln!("webcodex-runner quic connection closed; reconnecting");
                    tokio::time::sleep(schedule_reconnect(TRANSPORT_QUIC, &mut backoff)).await;
                }
                Err(e) => {
                    let e = classify_session_error(e);
                    project_cache.invalidate();
                    if once {
                        return Err(e.into_message());
                    }
                    if e.is_fatal() {
                        return Err(e.into_message());
                    }
                    eprintln!("webcodex-runner quic error: {}; reconnecting", e);
                    tracing::debug!(
                        transport = "quic",
                        error = %e,
                        "webcodex-runner quic transient error"
                    );
                    tokio::time::sleep(schedule_reconnect(TRANSPORT_QUIC, &mut backoff)).await;
                }
            }
        }
    });
    rt.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    result
}

fn run_quic_agent_single_session(
    cfg: &AgentConfig,
    once: bool,
    agent_instance_id: &str,
    runtime: &AgentRuntimeState,
) -> Result<AgentSessionExit, String> {
    let runtime = runtime.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to create tokio runtime: {}", e))?;
    let result = rt.block_on(async move {
        let mut project_cache = AgentProjectCache::default();
        let projects = project_cache.get(cfg);
        quic_session(cfg, projects, agent_instance_id, once, &runtime).await
    });
    rt.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    result
}

/// Validate the `[quic]` config section. Returns a cloned, resolved config so
/// the session owns a concrete value (defaults applied).
pub(crate) fn resolve_quic_config(cfg: &AgentConfig) -> Result<QuicClientConfig, String> {
    let quic = cfg
        .quic
        .clone()
        .ok_or_else(|| "transport=quic requires a [quic] section in agent.toml".to_string())?;
    validate_quic_config(&quic)?;
    Ok(quic)
}

pub(crate) fn resolve_quic_server_addrs(server_addr: &str) -> Result<Vec<SocketAddr>, String> {
    let addrs = server_addr
        .to_socket_addrs()
        .map_err(|e| {
            format!(
                "failed to resolve [quic] server_addr '{}': {}",
                server_addr, e
            )
        })?
        .collect::<Vec<_>>();
    if addrs.is_empty() {
        return Err(format!(
            "[quic] server_addr '{}' resolved to no socket addresses",
            server_addr
        ));
    }
    Ok(addrs)
}

pub(crate) fn quic_client_bind_addr_for(server_addr: SocketAddr) -> SocketAddr {
    if server_addr.is_ipv6() {
        "[::]:0"
            .parse()
            .expect("hard-coded IPv6 client bind address is valid")
    } else {
        "0.0.0.0:0"
            .parse()
            .expect("hard-coded IPv4 client bind address is valid")
    }
}

/// The rustls crypto provider for the QUIC client. The dependency tree pulls
/// both `aws-lc-rs` and `ring`, so rustls cannot auto-select; pin aws-lc-rs
/// explicitly per config via `builder_with_provider` (thread-safe, no global
/// install).
fn rustls_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// Build the quinn-wrapped rustls client config for the QUIC transport. The
/// agent validates the server certificate against the Mozilla root store
/// (webpki-roots) using `server_name` as the SNI/verification name — TLS is
/// transport security, not authentication; the agent token still authenticates
/// the agent.
fn build_quic_client_crypto(
    quic: &QuicClientConfig,
) -> Result<quinn::crypto::rustls::QuicClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    // `RootCertStore` implements `Extend<TrustAnchor>` (in-place, infallible).
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut client_crypto = rustls::ClientConfig::builder_with_provider(rustls_provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("failed to select rustls protocol versions: {}", e))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    client_crypto.alpn_protocols = vec![quic.alpn.as_bytes().to_vec()];
    quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)
        .map_err(|e| format!("failed to build quinn client crypto: {}", e))
}

fn classify_quic_agent_connect_error(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    if lower.contains("certificate")
        || lower.contains("cert")
        || lower.contains("webpki")
        || lower.contains("notvalidforname")
        || lower.contains("unknownissuer")
    {
        "certificate verify failed; check [quic].server_name and the certificate SAN/issuer"
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "connect timeout; check UDP firewall/security group/NAT and that the server QUIC listener is enabled"
    } else if lower.contains("alpn") || lower.contains("no application protocol") {
        "handshake failed; check WEBCODEX_QUIC_ENABLED, listener bind, and ALPN"
    } else if lower.contains("applicationclosed")
        || lower.contains("connectionclosed")
        || lower.contains("closed")
    {
        "handshake failed; check WEBCODEX_QUIC_ENABLED, listener bind, and server availability"
    } else {
        "handshake failed"
    }
}

/// One QUIC connection lifecycle: connect, register, dispatch requests until
/// the stream closes or a fatal server error arrives. In `--once` mode,
/// completes one ping/pong after the ack then returns.
async fn quic_session(
    cfg: &AgentConfig,
    projects: Vec<ShellAgentProjectSummary>,
    agent_instance_id: &str,
    once: bool,
    runtime: &AgentRuntimeState,
) -> Result<AgentSessionExit, String> {
    let quic = resolve_quic_config(cfg)?;
    let client_crypto = build_quic_client_crypto(&quic)?;
    let client_config = quinn::ClientConfig::new(Arc::new(client_crypto));
    let server_addrs = resolve_quic_server_addrs(&quic.server_addr)?;
    let mut connect_errors = Vec::new();
    let mut client_endpoint = None;
    let mut conn = None;
    for server_addr in server_addrs {
        let endpoint = match quinn::Endpoint::client(quic_client_bind_addr_for(server_addr)) {
            Ok(endpoint) => endpoint,
            Err(e) => {
                connect_errors.push(format!(
                    "{}: failed to bind quic client endpoint: {}",
                    server_addr, e
                ));
                continue;
            }
        };
        let connect =
            match endpoint.connect_with(client_config.clone(), server_addr, &quic.server_name) {
                Ok(connect) => connect,
                Err(e) => {
                    connect_errors.push(format!(
                        "{}: failed to start quic connect: {}",
                        server_addr, e
                    ));
                    continue;
                }
            };
        match tokio::time::timeout(Duration::from_secs(quic.connect_timeout_secs), connect).await {
            Ok(Ok(connection)) => {
                client_endpoint = Some(endpoint);
                conn = Some(connection);
                break;
            }
            Err(_) => connect_errors.push(format!(
                "{} timed out after {}s; check UDP firewall/security group/NAT and that the server QUIC listener is enabled",
                server_addr, quic.connect_timeout_secs
            )),
            Ok(Err(e)) => {
                let raw = e.to_string();
                connect_errors.push(format!(
                    "{}: {} ({})",
                    server_addr,
                    classify_quic_agent_connect_error(&raw),
                    raw
                ));
            }
        }
    }
    let _client_endpoint = client_endpoint.ok_or_else(|| {
        format!(
            "quic connect to {} failed for all resolved addresses: {}",
            quic.server_addr,
            connect_errors.join("; ")
        )
    })?;
    let conn = conn.expect("client endpoint is set only after a successful QUIC connection");

    // ALPN is enforced by quinn during the TLS handshake: a connection only
    // completes when the client and server agree on a matching ALPN. A
    // mismatch fails the handshake (surfaced as the connect error above).

    // Open a single bidirectional stream for register/ack/keepalive.
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| format!("failed to open quic bidirectional stream: {}", e))?;

    // Register. The token is carried in `auth_token`; the server authenticates
    // it exactly like the websocket/polling paths. It is never logged.
    let projects_count = enabled_projects_count(&projects);
    let (register_payload, provider, provider_revision) =
        build_register_request_with_provider_status(
            cfg,
            &runtime.config,
            projects,
            AGENT_PROTOCOL_VERSION_QUIC_V1,
            agent_instance_id,
            0,
        );
    let reg_env = AgentEnvelope::Register {
        payload: register_payload,
        auth_token: non_empty_token(&cfg.token),
    };
    write_quic_frame(&mut send, &reg_env)
        .await
        .map_err(|e| format!("failed to send quic register: {}", e))?;

    // Wait for the Registered ack.
    let ack = tokio::time::timeout(Duration::from_secs(15), read_quic_frame(&mut recv))
        .await
        .map_err(|_| "quic register ack timed out".to_string())?
        .map_err(|e| format!("failed to read quic register ack: {}", e))?;
    match ack {
        AgentEnvelope::Registered { success: true, .. } => {
            provider.mark_status_reported(provider_revision);
        }
        AgentEnvelope::Registered { error, .. } => {
            return Err(format!(
                "register rejected by server: {}",
                error.unwrap_or_else(|| "no server error message".to_string())
            ));
        }
        AgentEnvelope::Error { code, message } => {
            return Err(format!(
                "server error during register {}: {}",
                code, message
            ));
        }
        other => return Err(format!("expected registered ack, got {}", other.kind())),
    }
    eprintln!(
        "{}",
        registered_log_line(cfg, TRANSPORT_QUIC, projects_count)
    );

    if once {
        // Complete one ping/pong round trip then exit, mirroring the websocket
        // `--once` semantics.
        let ping = AgentEnvelope::Ping {
            ts: chrono::Utc::now().timestamp(),
        };
        write_quic_frame(&mut send, &ping)
            .await
            .map_err(|e| format!("quic once ping send failed: {}", e))?;
        let resp = tokio::time::timeout(Duration::from_secs(10), read_quic_frame(&mut recv))
            .await
            .map_err(|_| "quic once pong timed out".to_string())?
            .map_err(|e| format!("quic once pong read failed: {}", e))?;
        match resp {
            AgentEnvelope::Pong { .. } => {}
            other => return Err(format!("expected pong, got {}", other.kind())),
        }
        let _ = write_quic_frame(
            &mut send,
            &AgentEnvelope::Goodbye {
                reason: Some("once complete".to_string()),
            },
        )
        .await;
        let _ = send.finish();
        return Ok(AgentSessionExit::Completed);
    }

    // Split into a single writer task and a reader/dispatch loop. Outgoing
    // Result/JobUpdate/Pong/Ping envelopes all pass through the channel so no
    // two tasks write the QUIC SendStream at the same time.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<AgentEnvelope>(WS_OUTGOING_CAPACITY);
    let writer_task = tokio::spawn(async move {
        while let Some(env) = out_rx.recv().await {
            if write_quic_frame(&mut send, &env).await.is_err() {
                break;
            }
        }
        let _ = send.finish();
    });

    let sink_handle = AgentSink::Quic {
        tx: out_tx.clone(),
        client_id: cfg.client_id.clone(),
        agent_instance_id: agent_instance_id.to_string(),
    };
    let jobs = JobManager::new(max_concurrent_jobs(cfg));
    let mut ping_interval = tokio::time::interval(QUIC_PING_INTERVAL);
    ping_interval.tick().await; // skip immediate first tick
    let mut shutdown = Box::pin(shutdown_signal());
    let mut shutdown_requested = false;
    let mut session_error: Option<String> = None;

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                eprintln!("webcodex-runner received process shutdown signal; exiting");
                runtime.config.begin_shutdown();
                shutdown_requested = true;
                break;
            }
            frame = read_quic_frame(&mut recv) => {
                let env = match frame {
                    Ok(env) => env,
                    Err(QuicFrameError::EmptyStream) => {
                        tracing::debug!(
                            transport = "quic",
                            "webcodex-runner quic stream closed by peer"
                        );
                        break;
                    }
                    Err(e) => {
                        session_error = Some(format!("quic stream read error: {}", e));
                        break;
                    }
                };
                match env {
                    AgentEnvelope::Request { request } => {
                        let sink_handle = sink_handle.clone();
                        let config = Arc::clone(&runtime.config);
                        let hot = config.snapshot();
                        let jobs = jobs.clone();
                        let projects_dir = projects_dir(cfg);
                        let lsp = runtime.lsp.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = dispatch_request(
                                &sink_handle,
                                &hot,
                                &config,
                                &jobs,
                                &projects_dir,
                                &lsp,
                                request,
                            );
                        });
                    }
                    AgentEnvelope::Ping { ts } => {
                        let _ = out_tx.send(AgentEnvelope::Pong { ts }).await;
                    }
                    AgentEnvelope::Pong { .. } => {
                        // Normal keepalive response.
                    }
                    AgentEnvelope::Registered { .. } => {
                        // Ignore a redundant ack.
                    }
                    AgentEnvelope::Error { code, message } => {
                        session_error = Some(format!("server error {}: {}", code, message));
                        break;
                    }
                    other => {
                        eprintln!(
                            "webcodex-runner quic ignoring unexpected envelope: {}",
                            other.kind()
                        );
                    }
                }
            }
            _ = ping_interval.tick() => {
                tracing::debug!(
                    transport = "quic",
                    "webcodex-runner quic keepalive ping"
                );
                send_provider_metadata(&out_tx, &runtime.config, None);
                let _ = out_tx.send(AgentEnvelope::Ping {
                    ts: chrono::Utc::now().timestamp(),
                }).await;
            }
        }
    }

    let graceful_writer_shutdown = shutdown_requested;
    if shutdown_requested {
        stop_jobs_for_shutdown(&jobs, cfg.poll_interval_ms).await;
        let _ = out_tx
            .send(AgentEnvelope::Goodbye {
                reason: Some("process shutdown".to_string()),
            })
            .await;
    } else if jobs.has_work() {
        tracing::warn!(
            transport = "quic",
            "webcodex-runner quic disconnected with active jobs; reconnecting without waiting"
        );
    }
    drop(sink_handle);
    drop(out_tx);
    let mut writer_task = writer_task;
    if graceful_writer_shutdown {
        if tokio::time::timeout(WS_WRITER_CLOSE_TIMEOUT, &mut writer_task)
            .await
            .is_err()
        {
            writer_task.abort();
        }
    } else {
        writer_task.abort();
    }
    if let Some(error) = session_error {
        return Err(error);
    }
    Ok(if shutdown_requested {
        AgentSessionExit::Shutdown
    } else {
        AgentSessionExit::TransportDisconnected
    })
}

// ============================================================================
// WebSocket agent transport
// ============================================================================
//
// The WebSocket mode keeps one long-lived connection to the server. The server
// pushes `Request` envelopes; the agent executes them via the same
// `dispatch_request` path the polling loop uses, and sends `Result` /
// `JobUpdate` envelopes back. Polling is unchanged and remains the fallback.

/// Convert an `http(s)://` server URL into a `ws(s)://` URL plus path.
pub(crate) fn server_url_to_ws(server_url: &str, path: &str) -> Result<String, String> {
    let base = server_url.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{}{}", rest, path)
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{}{}", rest, path)
    } else if base.starts_with("ws://") || base.starts_with("wss://") {
        format!("{}{}", base, path)
    } else {
        return Err(format!(
            "server_url must be http(s)://... for websocket transport; got {}",
            server_log_label(server_url)
        ));
    };
    Ok(ws)
}

/// Build a WebSocket handshake request, carrying a Bearer token only when the
/// configured token is non-empty. Open-mode agents intentionally send no
/// credential so the server must have explicit anonymous mode enabled.
pub(crate) fn build_ws_request(
    ws_url: &str,
    token: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = ws_url.into_client_request().map_err(|e| {
        format!(
            "invalid websocket url for {}: {}",
            server_log_label(ws_url),
            e
        )
    })?;
    if let Some(token) = non_empty_token(token) {
        let value = format!("Bearer {}", token);
        let header_value = tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&value)
            .map_err(|e| format!("invalid token header value: {}", e))?;
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
            header_value,
        );
    }
    Ok(request)
}

/// Entry point for the WebSocket transport. Runs a tokio current-thread
/// runtime and reconnects on session failure.
fn run_websocket_agent(
    cfg: AgentConfig,
    once: bool,
    agent_instance_id: &str,
    runtime: &AgentRuntimeState,
) -> Result<(), String> {
    let agent_instance_id = agent_instance_id.to_string();
    let runtime = runtime.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to create tokio runtime: {}", e))?;
    let result = rt.block_on(async move {
        let mut project_cache = AgentProjectCache::default();
        let mut backoff = ReconnectBackoff::new();
        loop {
            let projects = project_cache.get(&cfg);
            let session_started = Instant::now();
            match websocket_session(&cfg, projects, &agent_instance_id, &runtime).await {
                Ok(AgentSessionExit::Shutdown) => {
                    project_cache.invalidate();
                    eprintln!("webcodex-runner websocket shutdown complete");
                    return Ok(());
                }
                Ok(AgentSessionExit::Completed) => {
                    project_cache.invalidate();
                    return Ok(());
                }
                Ok(AgentSessionExit::TransportDisconnected) => {
                    project_cache.invalidate();
                    if once {
                        return Ok(());
                    }
                    reset_backoff_after_stable_session(&mut backoff, session_started);
                    eprintln!("webcodex-runner websocket connection closed; reconnecting");
                    tokio::time::sleep(schedule_reconnect(TRANSPORT_WEBSOCKET, &mut backoff)).await;
                }
                Err(e) => {
                    let e = classify_session_error(e);
                    project_cache.invalidate();
                    if once {
                        return Err(e.into_message());
                    }
                    if e.is_fatal() {
                        return Err(e.into_message());
                    }
                    eprintln!("webcodex-runner websocket error: {}; reconnecting", e);
                    tracing::debug!(
                        transport = "websocket",
                        error = %e,
                        "webcodex-runner websocket transient error"
                    );
                    tokio::time::sleep(schedule_reconnect(TRANSPORT_WEBSOCKET, &mut backoff)).await;
                }
            }
        }
    });
    rt.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    result
}

fn run_websocket_agent_single_session(
    cfg: &AgentConfig,
    agent_instance_id: &str,
    runtime: &AgentRuntimeState,
) -> Result<AgentSessionExit, String> {
    let runtime = runtime.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to create tokio runtime: {}", e))?;
    let result = rt.block_on(async move {
        let mut project_cache = AgentProjectCache::default();
        let projects = project_cache.get(cfg);
        websocket_session(cfg, projects, agent_instance_id, &runtime).await
    });
    rt.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    result
}

/// One WebSocket connection lifecycle: connect, register, then serve requests
/// until the socket closes or a fatal server error arrives.
pub(crate) async fn websocket_session(
    cfg: &AgentConfig,
    projects: Vec<ShellAgentProjectSummary>,
    agent_instance_id: &str,
    runtime: &AgentRuntimeState,
) -> Result<AgentSessionExit, String> {
    websocket_session_with_shutdown(cfg, projects, agent_instance_id, runtime, shutdown_signal())
        .await
}

async fn websocket_session_with_shutdown<F>(
    cfg: &AgentConfig,
    projects: Vec<ShellAgentProjectSummary>,
    agent_instance_id: &str,
    runtime: &AgentRuntimeState,
    shutdown: F,
) -> Result<AgentSessionExit, String>
where
    F: std::future::Future<Output = ()>,
{
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let ws_url = server_url_to_ws(&cfg.server_url, "/api/agents/ws")?;
    let request = build_ws_request(&ws_url, &cfg.token)?;
    let (mut ws_stream, _resp) = tokio::time::timeout(
        Duration::from_secs(cfg.websocket_connect_timeout_secs),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| {
        format!(
            "websocket connect timed out after {}s",
            cfg.websocket_connect_timeout_secs
        )
    })?
    .map_err(|e| format!("websocket connect failed: {}", e))?;

    // Register over the socket. The prepared-profile cache is empty at
    // registration time (snapshots are prepared lazily on first use), so
    // `prepared_cache_count` is reported as 0 here.
    let projects_count = enabled_projects_count(&projects);
    let (register_payload, provider, provider_revision) =
        build_register_request_with_provider_status(
            cfg,
            &runtime.config,
            projects,
            AGENT_PROTOCOL_VERSION_WEBSOCKET_V1,
            agent_instance_id,
            0,
        );
    let reg_env = AgentEnvelope::Register {
        payload: register_payload,
        auth_token: None,
    };
    let reg_json =
        serde_json::to_string(&reg_env).map_err(|e| format!("failed to encode register: {}", e))?;
    ws_stream
        .send(WsMessage::Text(reg_json.into()))
        .await
        .map_err(|e| format!("failed to send register: {}", e))?;

    // Wait for Registered ack.
    let ack_msg = ws_stream
        .next()
        .await
        .ok_or_else(|| "server closed before register ack".to_string())?
        .map_err(|e| format!("failed to read register ack: {}", e))?;
    let ack_text = ack_msg
        .into_text()
        .map_err(|_| "register ack was not text".to_string())?;
    let ack = AgentEnvelope::from_slice(ack_text.as_bytes())
        .map_err(|e| format!("register ack is not a valid envelope: {}", e))?;
    match ack {
        AgentEnvelope::Registered { success: true, .. } => {
            provider.mark_status_reported(provider_revision);
        }
        AgentEnvelope::Registered { error, .. } => {
            return Err(format!(
                "register rejected by server: {}",
                error.unwrap_or_else(|| "no server error message".to_string())
            ));
        }
        AgentEnvelope::Error { code, message } => {
            return Err(format!(
                "server error during register {}: {}",
                code, message
            ));
        }
        other => return Err(format!("expected registered ack, got {}", other.kind())),
    }
    eprintln!(
        "{}",
        registered_log_line(cfg, TRANSPORT_WEBSOCKET, projects_count)
    );

    // Split socket into writer (drains outgoing envelopes) and reader.
    let (mut sink, mut stream) = ws_stream.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<AgentEnvelope>(WS_OUTGOING_CAPACITY);
    let writer_task = tokio::spawn(async move {
        while let Some(env) = out_rx.recv().await {
            match serde_json::to_string(&env) {
                Ok(json) => {
                    if sink.send(WsMessage::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // The sink is dropped here. We intentionally do NOT call
        // `sink.close()`: on a split WebSocket sink the close handshake is
        // delivered through the read half, which is no longer polled once
        // the read loop breaks, so an unbounded `close().await` can hang and
        // block the reconnect loop. Dropping the sink lets the OS tear down
        // the socket; the server reconciles via its disconnect path either
        // way.
    });

    let sink_handle = AgentSink::WebSocket {
        tx: out_tx.clone(),
        client_id: cfg.client_id.clone(),
        agent_instance_id: agent_instance_id.to_string(),
    };
    let jobs = JobManager::new(max_concurrent_jobs(cfg));
    let mut ping_interval = tokio::time::interval(WS_PING_INTERVAL);
    ping_interval.tick().await; // skip immediate first tick
    let mut shutdown = Box::pin(shutdown);
    let mut quit_after_session = false;
    let mut session_error: Option<String> = None;

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                runtime.config.begin_shutdown();
                quit_after_session = true;
                eprintln!("webcodex-runner received process shutdown signal; exiting");
                break;
            }
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        tracing::debug!(
                            transport = "websocket",
                            error = ?e,
                            "webcodex-runner websocket read error"
                        );
                        break;
                    }
                    None => {
                        tracing::debug!(
                            transport = "websocket",
                            "webcodex-runner websocket stream ended"
                        );
                        break;
                    }
                };
                if let WsMessage::Close(frame) = msg {
                    if let Some(frame) = frame {
                        tracing::debug!(
                            transport = "websocket",
                            close_code = ?frame.code,
                            close_reason = %frame.reason,
                            "webcodex-runner websocket close frame received"
                        );
                    } else {
                        tracing::debug!(
                            transport = "websocket",
                            "webcodex-runner websocket close frame received"
                        );
                    }
                    break;
                }
                let text = match msg.into_text() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let env = match AgentEnvelope::from_slice(text.as_bytes()) {
                    Ok(env) => env,
                    Err(e) => {
                        eprintln!("webcodex-runner websocket malformed envelope: {}", e);
                        continue;
                    }
                };
                match env {
                    AgentEnvelope::Request { request } => {
                        let sink_handle = sink_handle.clone();
                        let config = Arc::clone(&runtime.config);
                        let hot = config.snapshot();
                        let jobs = jobs.clone();
                        let projects_dir = projects_dir(&cfg);
                        let lsp = runtime.lsp.clone();
                        // Execution is blocking (shell/file/jobs/lsp); run it off
                        // the async runtime thread. dispatch_request sends
                        // results/updates via the shared AgentSink.
                        tokio::task::spawn_blocking(move || {
                            let _ = dispatch_request(
                                &sink_handle,
                                &hot,
                                &config,
                                &jobs,
                                &projects_dir,
                                &lsp,
                                request,
                            );
                        });
                    }
                    AgentEnvelope::Ping { ts } => {
                        let _ = out_tx.send(AgentEnvelope::Pong { ts }).await;
                    }
                    AgentEnvelope::Pong { .. } => {
                        // Normal keepalive response from the server to our
                        // Ping. This is expected liveness traffic: do not
                        // log at info level, do not disconnect, do not treat
                        // it as an unexpected envelope. Staying silent here
                        // keeps the agent log quiet during idle periods.
                    }
                    AgentEnvelope::Error { code, message } => {
                        session_error = Some(format!("server error {}: {}", code, message));
                        break;
                    }
                    other => {
                        eprintln!(
                            "webcodex-runner websocket ignoring unexpected envelope: {}",
                            other.kind()
                        );
                    }
                }
            }
            _ = ping_interval.tick() => {
                tracing::debug!(
                    transport = "websocket",
                    "webcodex-runner websocket keepalive ping"
                );
                send_provider_metadata(&out_tx, &runtime.config, None);
                let _ = out_tx.send(AgentEnvelope::Ping {
                    ts: chrono::Utc::now().timestamp(),
                }).await;
            }
        }
    }

    let graceful_writer_shutdown = quit_after_session;
    if quit_after_session {
        stop_jobs_for_shutdown(&jobs, cfg.poll_interval_ms).await;
        let _ = out_tx
            .send(AgentEnvelope::Goodbye {
                reason: Some("process shutdown".to_string()),
            })
            .await;
    } else if jobs.has_work() {
        tracing::warn!(
            transport = "websocket",
            "webcodex-runner websocket disconnected with active jobs; reconnecting without waiting"
        );
    }

    // Shutdown: drop the senders and read half so the underlying socket can be
    // torn down. Only process shutdown gets a brief grace window to flush the
    // Goodbye frame; ordinary transport disconnects abort the writer so active
    // job sender clones cannot delay reconnect.
    drop(sink_handle);
    drop(out_tx);
    drop(stream);
    let mut writer_task = writer_task;
    if graceful_writer_shutdown {
        if tokio::time::timeout(WS_WRITER_CLOSE_TIMEOUT, &mut writer_task)
            .await
            .is_err()
        {
            writer_task.abort();
        }
    } else {
        writer_task.abort();
    }
    if let Some(error) = session_error {
        return Err(error);
    }
    Ok(if quit_after_session {
        AgentSessionExit::Shutdown
    } else {
        AgentSessionExit::TransportDisconnected
    })
}

#[cfg(test)]
mod tests {
    use super::super::config::{AgentPolicy, ShellConfig};
    use super::*;
    use crate::shell_protocol::{
        ShellAgentShellRequest, ShellClientCapabilities, AGENT_PROTOCOL_VERSION_WEBSOCKET_V1,
    };
    use futures_util::{SinkExt, StreamExt};
    use std::io::{Read, Write};
    use std::net::{TcpListener as StdTcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;
    use std::thread;
    use tokio::net::TcpListener;
    use tokio::sync::{mpsc, oneshot};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    fn test_agent_config(server_url: String) -> AgentConfig {
        AgentConfig {
            server_url,
            token: "test-token".to_string(),
            client_id: "oe".to_string(),
            display_name: Some("OE agent".to_string()),
            owner: Some("tester".to_string()),
            hostname: Some("oe-host".to_string()),
            projects_dir: None,
            poll_interval_ms: 10,
            capabilities: Some(ShellClientCapabilities {
                git: true,
                ..ShellClientCapabilities::default()
            }),
            max_concurrent_jobs: Some(1),
            // Transport tests run jobs in a temp dir and are not about the
            // filesystem boundary; AgentPolicy::default() is fail-closed.
            policy: AgentPolicy {
                allow_cwd_anywhere: true,
                ..AgentPolicy::default()
            },
            transport: Some(TRANSPORT_WEBSOCKET.to_string()),
            websocket_connect_timeout_secs:
                crate::webcodex_runner::default_websocket_connect_timeout_secs(),
            quic: None,
            shell: ShellConfig::default(),
            tool_providers: Default::default(),
        }
    }

    fn polling_agent_config(server_url: String, projects_dir: PathBuf) -> AgentConfig {
        let mut cfg = test_agent_config(server_url);
        cfg.transport = Some(TRANSPORT_POLLING.to_string());
        cfg.projects_dir = Some(projects_dir);
        cfg
    }

    fn test_runtime(cfg: &AgentConfig) -> AgentRuntimeState {
        AgentRuntimeState::new(cfg, PathBuf::new())
    }

    fn test_project(id: &str) -> ShellAgentProjectSummary {
        ShellAgentProjectSummary {
            id: id.to_string(),
            name: Some(id.to_string()),
            path: format!("/tmp/{}", id),
            allow_patch: true,
            kind: Some("repo".to_string()),
            description: None,
            hooks: vec!["check".to_string()],
            disabled: false,
            git_branch: None,
            git_head: None,
            git_dirty: None,
            updated_at: 123,
            shell_profile: None,
        }
    }

    fn header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|window| window == b"\r\n\r\n")
    }

    fn content_length(headers: &str) -> usize {
        headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0)
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = Vec::new();
        loop {
            let mut chunk = [0u8; 1024];
            let n = stream.read(&mut chunk).expect("read request");
            assert!(n > 0, "client closed before sending a complete request");
            buf.extend_from_slice(&chunk[..n]);
            let Some(end) = header_end(&buf) else {
                continue;
            };
            let headers = String::from_utf8_lossy(&buf[..end]);
            let expected = end + 4 + content_length(&headers);
            if buf.len() >= expected {
                break;
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn request_path(request: &str) -> &str {
        request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("")
    }

    fn write_http_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
        write!(
            stream,
            "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            status,
            content_type,
            body.as_bytes().len(),
            body
        )
        .unwrap();
    }

    fn start_polling_http_server(
        poll_status: &str,
        poll_content_type: &str,
        poll_body: &str,
    ) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let poll_count = Arc::new(AtomicUsize::new(0));
        let server_poll_count = Arc::clone(&poll_count);
        let poll_status = poll_status.to_string();
        let poll_content_type = poll_content_type.to_string();
        let poll_body = poll_body.to_string();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            assert_eq!(request_path(&request), "/api/shell/agent/register");
            write_http_response(
                &mut stream,
                "200 OK",
                "application/json",
                r#"{"success":true,"client":null,"error":null}"#,
            );

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            assert_eq!(request_path(&request), "/api/shell/agent/poll");
            server_poll_count.fetch_add(1, Ordering::SeqCst);
            write_http_response(&mut stream, &poll_status, &poll_content_type, &poll_body);
        });
        (format!("http://{}", addr), poll_count, server)
    }

    fn start_auto_fallback_http_server(
        poll_status: &str,
        poll_content_type: &str,
        poll_body: &str,
    ) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let poll_count = Arc::new(AtomicUsize::new(0));
        let server_poll_count = Arc::clone(&poll_count);
        let poll_status = poll_status.to_string();
        let poll_content_type = poll_content_type.to_string();
        let poll_body = poll_body.to_string();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            assert_eq!(request_path(&request), "/api/agents/ws");
            write_http_response(
                &mut stream,
                "503 Service Unavailable",
                "text/plain",
                "websocket unavailable",
            );

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            assert_eq!(request_path(&request), "/api/shell/agent/register");
            write_http_response(
                &mut stream,
                "200 OK",
                "application/json",
                r#"{"success":true,"client":null,"error":null}"#,
            );

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            assert_eq!(request_path(&request), "/api/shell/agent/poll");
            server_poll_count.fetch_add(1, Ordering::SeqCst);
            write_http_response(&mut stream, &poll_status, &poll_content_type, &poll_body);

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            assert_eq!(request_path(&request), "/api/shell/agent/register");
            write_http_response(
                &mut stream,
                "200 OK",
                "application/json",
                r#"{"success":true,"client":null,"error":null}"#,
            );

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            assert_eq!(request_path(&request), "/api/shell/agent/poll");
            server_poll_count.fetch_add(1, Ordering::SeqCst);
            write_http_response(
                &mut stream,
                "404 Not Found",
                "application/json",
                r#"{"success":false,"error":"poll endpoint missing"}"#,
            );
        });
        (format!("http://{}", addr), poll_count, server)
    }

    fn run_polling_agent_against_server(
        poll_status: &str,
        poll_content_type: &str,
        poll_body: &str,
        once: bool,
    ) -> (Result<(), String>, usize) {
        let (server_url, poll_count, server) =
            start_polling_http_server(poll_status, poll_content_type, poll_body);
        let tmp = tempfile::tempdir().unwrap();
        let cfg = polling_agent_config(server_url, tmp.path().join("projects.d"));
        let runtime = test_runtime(&cfg);
        let shutdown = Arc::new(AtomicBool::new(false));
        let failsafe = Arc::clone(&shutdown);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            failsafe.store(true, Ordering::SeqCst);
        });
        let result =
            run_polling_agent_with_shutdown(cfg, once, "inst-poll-test", shutdown, &runtime);
        server.join().unwrap();
        (result, poll_count.load(Ordering::SeqCst))
    }

    /// Scripted step for the sequential fake agent HTTP server. Each step
    /// asserts the endpoint the runner is expected to call next, which is
    /// what distinguishes "kept polling" from "re-registered or resubmitted".
    enum ScriptStep {
        Register,
        RegisterResponse {
            status: &'static str,
            body: &'static str,
        },
        RegisterTypedResponse {
            status: &'static str,
            content_type: &'static str,
            body: &'static str,
        },
        PollDeliver(&'static str),
        PollEmpty,
        PollResponse {
            status: &'static str,
            body: &'static str,
        },
        PollTypedResponse {
            status: &'static str,
            content_type: &'static str,
            body: &'static str,
        },
        PollOversized {
            declared_len: usize,
        },
        PollClose,
        Result {
            status: &'static str,
            body: &'static str,
        },
    }

    struct ScriptedServer {
        server_url: String,
        requests: Arc<Mutex<Vec<(String, String)>>>,
        shutdown: Arc<AtomicBool>,
        handle: thread::JoinHandle<()>,
    }

    fn sync_file_request(request_id: &str) -> ShellAgentShellRequest {
        ShellAgentShellRequest {
            request_id: request_id.to_string(),
            client_id: "oe".to_string(),
            kind: "file_read".to_string(),
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
            stdin: None,
            timeout_secs: 5,
            requested_by: "tester".to_string(),
            created_at: 0,
            validation: None,
            lsp: None,
            sandbox: None,
        }
    }

    fn accept_with_deadline(listener: &StdTcpListener, deadline: Duration) -> TcpStream {
        let start = Instant::now();
        listener.set_nonblocking(true).unwrap();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    listener.set_nonblocking(false).unwrap();
                    return stream;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        start.elapsed() < deadline,
                        "scripted server timed out waiting for the next agent request"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("scripted server accept failed: {e}"),
            }
        }
    }

    fn start_scripted_agent_server(steps: Vec<ScriptStep>) -> ScriptedServer {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = Arc::clone(&shutdown);
        let handle = thread::spawn(move || {
            for step in steps {
                let mut stream = accept_with_deadline(&listener, Duration::from_secs(8));
                let request = read_http_request(&mut stream);
                let path = request_path(&request).to_string();
                let body = request
                    .find("\r\n\r\n")
                    .map(|index| request[index + 4..].to_string())
                    .unwrap_or_default();
                recorded.lock().unwrap().push((path.clone(), body));
                match step {
                    ScriptStep::Register => {
                        assert_eq!(path, "/api/shell/agent/register");
                        write_http_response(
                            &mut stream,
                            "200 OK",
                            "application/json",
                            r#"{"success":true,"client":null,"error":null}"#,
                        );
                    }
                    ScriptStep::RegisterResponse { status, body } => {
                        assert_eq!(path, "/api/shell/agent/register");
                        write_http_response(&mut stream, status, "application/json", body);
                    }
                    ScriptStep::RegisterTypedResponse {
                        status,
                        content_type,
                        body,
                    } => {
                        assert_eq!(path, "/api/shell/agent/register");
                        write_http_response(&mut stream, status, content_type, body);
                    }
                    ScriptStep::PollDeliver(request_id) => {
                        assert_eq!(path, "/api/shell/agent/poll", "expected poll, got {path}");
                        let request_json =
                            serde_json::to_string(&sync_file_request(request_id)).unwrap();
                        let body = format!(
                            r#"{{"success":true,"request":{},"error":null}}"#,
                            request_json
                        );
                        write_http_response(&mut stream, "200 OK", "application/json", &body);
                    }
                    ScriptStep::PollEmpty => {
                        assert_eq!(path, "/api/shell/agent/poll", "expected poll, got {path}");
                        write_http_response(
                            &mut stream,
                            "200 OK",
                            "application/json",
                            r#"{"success":true,"request":null,"error":null}"#,
                        );
                    }
                    ScriptStep::PollResponse { status, body } => {
                        assert_eq!(path, "/api/shell/agent/poll", "expected poll, got {path}");
                        write_http_response(&mut stream, status, "application/json", body);
                    }
                    ScriptStep::PollTypedResponse {
                        status,
                        content_type,
                        body,
                    } => {
                        assert_eq!(path, "/api/shell/agent/poll", "expected poll, got {path}");
                        write_http_response(&mut stream, status, content_type, body);
                    }
                    ScriptStep::PollOversized { declared_len } => {
                        assert_eq!(path, "/api/shell/agent/poll", "expected poll, got {path}");
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            declared_len
                        )
                        .unwrap();
                    }
                    ScriptStep::PollClose => {
                        assert_eq!(path, "/api/shell/agent/poll", "expected poll, got {path}");
                        // Dropping the accepted socket without a response
                        // exercises connection-closed / early-EOF recovery.
                    }
                    ScriptStep::Result { status, body } => {
                        assert_eq!(
                            path, "/api/shell/agent/result",
                            "expected result submission, got {path}"
                        );
                        write_http_response(&mut stream, status, "application/json", body);
                    }
                }
            }
            server_shutdown.store(true, Ordering::SeqCst);
        });
        ScriptedServer {
            server_url: format!("http://{}", addr),
            requests,
            shutdown,
            handle,
        }
    }

    fn run_polling_agent_against_scripted_server(
        server: &ScriptedServer,
        once: bool,
    ) -> Result<(), String> {
        let runner_shutdown = Arc::clone(&server.shutdown);
        let failsafe_shutdown = Arc::clone(&server.shutdown);
        let server_url = server.server_url.clone();
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        thread::spawn(move || {
            let tmp = tempfile::tempdir().unwrap();
            let cfg = polling_agent_config(server_url, tmp.path().join("projects.d"));
            let runtime = test_runtime(&cfg);
            let result = run_polling_agent_with_shutdown(
                cfg,
                once,
                "inst-script",
                runner_shutdown,
                &runtime,
            );
            let _ = result_tx.send(result);
        });
        match result_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(result) => result,
            Err(error) => {
                failsafe_shutdown.store(true, Ordering::SeqCst);
                panic!("scripted polling runner exceeded hard timeout: {error}");
            }
        }
    }

    fn recorded_result_bodies(requests: &Mutex<Vec<(String, String)>>) -> Vec<String> {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/api/shell/agent/result")
            .map(|(_, body)| body.clone())
            .collect()
    }

    fn recorded_paths(requests: &Mutex<Vec<(String, String)>>) -> Vec<String> {
        requests
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _)| path.clone())
            .collect()
    }

    #[test]
    fn polling_502_reregisters_once_then_processes_request() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollResponse {
                status: "502 Bad Gateway",
                body: "<html>\n<h1>Bad Gateway</h1>\n</html>",
            },
            ScriptStep::Register,
            ScriptStep::PollDeliver("req-after-502"),
            ScriptStep::Result {
                status: "200 OK",
                body: r#"{"success":true}"#,
            },
            ScriptStep::PollEmpty,
        ]);
        let started = Instant::now();
        run_polling_agent_against_scripted_server(&server, false)
            .expect("a transient poll 502 must recover");
        server.handle.join().unwrap();

        assert!(
            started.elapsed() >= Duration::from_millis(450),
            "poll recovery skipped its first backoff"
        );
        assert_eq!(
            recorded_paths(&server.requests),
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/result",
                "/api/shell/agent/poll",
            ],
            "the first transient poll failure refreshes the same-instance session once"
        );
        let results = recorded_result_bodies(&server.requests);
        assert_eq!(results.len(), 1);
        assert!(results[0].contains("req-after-502"), "{results:?}");
    }

    #[test]
    fn polling_503_and_504_stay_live_without_registration_storm() {
        for status in ["503 Service Unavailable", "504 Gateway Timeout"] {
            let server = start_scripted_agent_server(vec![
                ScriptStep::Register,
                ScriptStep::PollResponse {
                    status,
                    body: "proxy unavailable",
                },
                ScriptStep::Register,
                ScriptStep::PollEmpty,
            ]);
            run_polling_agent_against_scripted_server(&server, false)
                .expect("gateway failure must recover");
            server.handle.join().unwrap();
            assert_eq!(
                recorded_paths(&server.requests),
                vec![
                    "/api/shell/agent/register",
                    "/api/shell/agent/poll",
                    "/api/shell/agent/register",
                    "/api/shell/agent/poll",
                ],
                "status {status}"
            );
        }
    }

    #[test]
    fn polling_connection_closed_enters_session_recovery() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollClose,
            ScriptStep::Register,
            ScriptStep::PollEmpty,
        ]);
        run_polling_agent_against_scripted_server(&server, false)
            .expect("a closed poll connection must recover");
        server.handle.join().unwrap();
        assert_eq!(
            recorded_paths(&server.requests),
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
            ]
        );
    }

    #[test]
    fn polling_repeated_transients_back_off_and_refresh_only_once() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollResponse {
                status: "502 Bad Gateway",
                body: "bad gateway",
            },
            ScriptStep::Register,
            ScriptStep::PollResponse {
                status: "503 Service Unavailable",
                body: "unavailable",
            },
            ScriptStep::PollResponse {
                status: "504 Gateway Timeout",
                body: "timeout",
            },
            ScriptStep::PollEmpty,
        ]);
        let started = Instant::now();
        run_polling_agent_against_scripted_server(&server, false)
            .expect("consecutive gateway failures must recover");
        let elapsed = started.elapsed();
        server.handle.join().unwrap();

        assert!(
            elapsed >= Duration::from_millis(1_850),
            "repeated failures did not apply 500ms/500ms/1s recovery delays: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "bounded recovery took unexpectedly long: {elapsed:?}"
        );
        assert_eq!(
            recorded_paths(&server.requests),
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/poll",
                "/api/shell/agent/poll",
            ],
            "one recovery episode must not re-register on every 5xx"
        );
    }

    #[test]
    fn polling_truncated_json_recovers_and_stays_live() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollResponse {
                status: "200 OK",
                body: r#"{"success":true,"request":"#,
            },
            ScriptStep::Register,
            ScriptStep::PollEmpty,
        ]);
        run_polling_agent_against_scripted_server(&server, false)
            .expect("an incomplete poll response must recover");
        server.handle.join().unwrap();
        assert_eq!(
            recorded_paths(&server.requests),
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
            ]
        );
    }

    #[test]
    fn polling_register_truncated_json_recovers_and_stays_live() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::RegisterResponse {
                status: "200 OK",
                body: r#"{"success":true,"client":"#,
            },
            ScriptStep::Register,
            ScriptStep::PollEmpty,
        ]);
        run_polling_agent_against_scripted_server(&server, false)
            .expect("an incomplete register response must recover");
        server.handle.join().unwrap();
        assert_eq!(
            recorded_paths(&server.requests),
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
            ]
        );
    }

    #[test]
    fn polling_http_200_html_bad_gateway_recovers() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollTypedResponse {
                status: "200 OK",
                content_type: "text/html; charset=utf-8",
                body: "<!doctype html>\n<html><h1>Bad Gateway</h1></html>",
            },
            ScriptStep::Register,
            ScriptStep::PollEmpty,
        ]);
        run_polling_agent_against_scripted_server(&server, false)
            .expect("a poll proxy error page must recover");
        server.handle.join().unwrap();
        assert_eq!(
            recorded_paths(&server.requests),
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
            ]
        );
    }

    #[test]
    fn polling_register_http_200_html_service_unavailable_recovers() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::RegisterTypedResponse {
                status: "200 OK",
                content_type: "text/html",
                body: "<html>\n<h1>Service Unavailable</h1>\n</html>",
            },
            ScriptStep::Register,
            ScriptStep::PollEmpty,
        ]);
        run_polling_agent_against_scripted_server(&server, false)
            .expect("a register proxy error page must recover");
        server.handle.join().unwrap();
        assert_eq!(
            recorded_paths(&server.requests),
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
            ]
        );
    }

    #[test]
    fn polling_complete_schema_mismatch_is_terminal_without_recovery() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollResponse {
                status: "200 OK",
                body: r#"{"success":"yes","request":null,"error":null}"#,
            },
        ]);
        let error = run_polling_agent_against_scripted_server(&server, false)
            .expect_err("a complete incompatible poll response must stop");
        server.handle.join().unwrap();
        assert!(
            error.contains("poll response incompatible with server protocol"),
            "{error}"
        );
        assert!(error.contains("serde_category=data"), "{error}");
        assert!(!error.contains("\"yes\""), "{error}");
        assert_eq!(
            recorded_paths(&server.requests),
            vec!["/api/shell/agent/register", "/api/shell/agent/poll"],
            "protocol incompatibility must neither re-register nor poll again"
        );
    }

    #[test]
    fn polling_unknown_json_shape_is_terminal_without_recovery() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollResponse {
                status: "200 OK",
                body: r#"{"unexpected":true}"#,
            },
        ]);
        let error = run_polling_agent_against_scripted_server(&server, false)
            .expect_err("an unknown complete poll shape must stop");
        server.handle.join().unwrap();
        assert!(
            error.contains("poll response incompatible with server protocol"),
            "{error}"
        );
        assert!(error.contains("serde_category=data"), "{error}");
        assert!(!error.contains("unexpected"), "{error}");
        assert_eq!(
            recorded_paths(&server.requests),
            vec!["/api/shell/agent/register", "/api/shell/agent/poll"]
        );
    }

    #[test]
    fn polling_register_complete_schema_mismatch_is_terminal_without_retry() {
        let server = start_scripted_agent_server(vec![ScriptStep::RegisterResponse {
            status: "200 OK",
            body: r#"{"success":"yes","client":null,"error":null}"#,
        }]);
        let error = run_polling_agent_against_scripted_server(&server, false)
            .expect_err("a complete incompatible register response must stop");
        server.handle.join().unwrap();
        assert!(
            error.contains("register response incompatible with server protocol"),
            "{error}"
        );
        assert!(error.contains("serde_category=data"), "{error}");
        assert!(!error.contains("\"yes\""), "{error}");
        assert_eq!(
            recorded_paths(&server.requests),
            vec!["/api/shell/agent/register"]
        );
    }

    #[test]
    fn polling_register_unknown_json_shape_is_terminal_without_retry() {
        let server = start_scripted_agent_server(vec![ScriptStep::RegisterResponse {
            status: "200 OK",
            body: r#"{"unexpected":true}"#,
        }]);
        let error = run_polling_agent_against_scripted_server(&server, false)
            .expect_err("an unknown complete register shape must stop");
        server.handle.join().unwrap();
        assert!(
            error.contains("register response incompatible with server protocol"),
            "{error}"
        );
        assert!(error.contains("serde_category=data"), "{error}");
        assert!(!error.contains("unexpected"), "{error}");
        assert_eq!(
            recorded_paths(&server.requests),
            vec!["/api/shell/agent/register"]
        );
    }

    #[test]
    fn polling_oversized_response_is_terminal_without_loading_the_body() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollOversized {
                declared_len: crate::AGENT_HTTP_RESPONSE_BODY_MAX_BYTES + 1,
            },
        ]);
        let error = run_polling_agent_against_scripted_server(&server, false)
            .expect_err("an oversized poll response must stop at the protocol boundary");
        server.handle.join().unwrap();
        assert!(
            error.contains("poll response incompatible with server protocol"),
            "{error}"
        );
        assert!(
            error.contains("declared response body exceeds limit_bytes=33554432"),
            "{error}"
        );
        assert_eq!(
            recorded_paths(&server.requests),
            vec!["/api/shell/agent/register", "/api/shell/agent/poll"]
        );
    }

    #[test]
    fn polling_404_and_non_session_400_are_terminal_without_retry() {
        for (status, body, expected) in [
            (
                "404 Not Found",
                r#"{"success":false,"error":"missing"}"#,
                "poll endpoint missing or incompatible server",
            ),
            (
                "400 Bad Request",
                r#"{"success":false,"error":"invalid poll payload"}"#,
                "server permanently rejected polling",
            ),
        ] {
            let server = start_scripted_agent_server(vec![
                ScriptStep::Register,
                ScriptStep::PollResponse { status, body },
            ]);
            let error = run_polling_agent_against_scripted_server(&server, false)
                .expect_err("permanent poll failure must stop");
            server.handle.join().unwrap();
            assert!(error.contains(expected), "{error}");
            assert_eq!(
                recorded_paths(&server.requests),
                vec!["/api/shell/agent/register", "/api/shell/agent/poll"],
                "status {status} must not retry or re-register"
            );
        }
    }

    #[test]
    fn polling_unknown_session_reregisters_then_resumes() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollResponse {
                status: "400 Bad Request",
                body: r#"{"success":false,"error":"unknown shell client: oe"}"#,
            },
            ScriptStep::Register,
            ScriptStep::PollEmpty,
        ]);
        run_polling_agent_against_scripted_server(&server, false)
            .expect("an explicitly missing polling session must re-register");
        server.handle.join().unwrap();
        assert_eq!(
            recorded_paths(&server.requests),
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
            ]
        );
    }

    #[test]
    fn polling_initial_register_502_recovers_without_supervisor_restart() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::RegisterResponse {
                status: "502 Bad Gateway",
                body: "<html>bad gateway</html>",
            },
            ScriptStep::Register,
            ScriptStep::PollEmpty,
        ]);
        let started = Instant::now();
        run_polling_agent_against_scripted_server(&server, false)
            .expect("initial transient register failure must recover");
        server.handle.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(450));
        assert_eq!(
            recorded_paths(&server.requests),
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
            ]
        );
    }

    #[test]
    fn polling_recovery_register_502_retries_then_resumes() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollResponse {
                status: "502 Bad Gateway",
                body: "bad gateway",
            },
            ScriptStep::RegisterResponse {
                status: "502 Bad Gateway",
                body: "bad gateway",
            },
            ScriptStep::Register,
            ScriptStep::PollEmpty,
        ]);
        run_polling_agent_against_scripted_server(&server, false)
            .expect("transient recovery register failure must recover");
        server.handle.join().unwrap();
        assert_eq!(
            recorded_paths(&server.requests),
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/register",
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
            ]
        );
    }

    #[test]
    fn polling_active_instance_lease_conflict_waits_then_registers() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::RegisterResponse {
                status: "400 Bad Request",
                body: r#"{"success":false,"error":"agent client oe is already online with a different instance"}"#,
            },
            ScriptStep::Register,
            ScriptStep::PollEmpty,
        ]);
        let started = Instant::now();
        run_polling_agent_against_scripted_server(&server, false)
            .expect("temporary active-instance lease must be retried");
        server.handle.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(450));
        assert_eq!(
            recorded_paths(&server.requests),
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
            ]
        );
    }

    #[test]
    fn polling_register_auth_404_and_identity_mismatch_are_terminal() {
        for (status, body, expected) in [
            (
                "401 Unauthorized",
                r#"{"success":false,"error":"invalid token"}"#,
                "authentication failed",
            ),
            (
                "403 Forbidden",
                r#"{"success":false,"error":"forbidden"}"#,
                "authentication failed",
            ),
            (
                "404 Not Found",
                r#"{"success":false,"error":"missing"}"#,
                "endpoint missing or incompatible server",
            ),
            (
                "400 Bad Request",
                r#"{"success":false,"error":"agent token owner is 'alice'; cannot register owner 'bob'"}"#,
                "server rejected /api/shell/agent/register request",
            ),
            (
                "400 Bad Request",
                r#"{"success":false,"error":"agent client identity is unavailable"}"#,
                "server rejected /api/shell/agent/register request",
            ),
        ] {
            let server =
                start_scripted_agent_server(vec![ScriptStep::RegisterResponse { status, body }]);
            let error = run_polling_agent_against_scripted_server(&server, false)
                .expect_err("fatal register response must stop");
            server.handle.join().unwrap();
            assert!(error.contains(expected), "{error}");
            assert_eq!(
                recorded_paths(&server.requests),
                vec!["/api/shell/agent/register"],
                "status {status} must not retry"
            );
        }
    }

    #[test]
    fn polling_once_retries_transport_failures_until_one_successful_poll() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::RegisterResponse {
                status: "502 Bad Gateway",
                body: "bad gateway",
            },
            ScriptStep::Register,
            ScriptStep::PollResponse {
                status: "503 Service Unavailable",
                body: "unavailable",
            },
            ScriptStep::PollEmpty,
        ]);
        run_polling_agent_against_scripted_server(&server, true)
            .expect("--once must complete after one successful poll");
        server.handle.join().unwrap();
        assert_eq!(
            recorded_paths(&server.requests),
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/poll",
            ]
        );
    }

    #[test]
    fn polling_shutdown_interrupts_session_recovery_without_extra_request() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollResponse {
                status: "502 Bad Gateway",
                body: "bad gateway",
            },
        ]);
        let started = Instant::now();
        run_polling_agent_against_scripted_server(&server, false)
            .expect("shutdown during recovery must be a clean exit");
        let elapsed = started.elapsed();
        server.handle.join().unwrap();
        assert!(
            elapsed < Duration::from_secs(1),
            "shutdown did not interrupt recovery promptly: {elapsed:?}"
        );
        assert_eq!(
            recorded_paths(&server.requests),
            vec!["/api/shell/agent/register", "/api/shell/agent/poll"],
            "shutdown must not leak a re-register request"
        );
    }

    #[test]
    fn polling_result_permanent_400_is_dropped_once_and_polling_continues() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollDeliver("req-expired"),
            ScriptStep::Result {
                status: "400 Bad Request",
                body: r#"{"success":false,"error":"unknown or expired shell request: req-expired"}"#,
            },
            // The old path treated this as general retryable recovery, adding
            // a sleep and re-register before the next poll. The next call must
            // now be a poll, with neither recovery churn nor resubmission.
            ScriptStep::PollDeliver("req-next"),
            ScriptStep::Result {
                status: "200 OK",
                body: r#"{"success":true}"#,
            },
            ScriptStep::PollEmpty,
        ]);
        run_polling_agent_against_scripted_server(&server, false)
            .expect("script completion should shut the polling runner down cleanly");
        server.handle.join().unwrap();

        let result_bodies = recorded_result_bodies(&server.requests);
        assert_eq!(
            result_bodies.len(),
            2,
            "the permanently rejected request result must be submitted once"
        );
        assert!(
            result_bodies[0].contains("req-expired"),
            "{result_bodies:?}"
        );
        assert!(result_bodies[1].contains("req-next"), "{result_bodies:?}");
        let paths: Vec<String> = server
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _)| path.clone())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/result",
                "/api/shell/agent/poll",
                "/api/shell/agent/result",
                "/api/shell/agent/poll",
            ],
        );
    }

    #[test]
    fn polling_result_transient_500_retries_same_payload_then_succeeds() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollDeliver("req-transient"),
            ScriptStep::Result {
                status: "500 Internal Server Error",
                body: r#"{"success":false,"error":"temporary backend failure"}"#,
            },
            ScriptStep::Result {
                status: "200 OK",
                body: r#"{"success":true}"#,
            },
            ScriptStep::PollEmpty,
        ]);
        run_polling_agent_against_scripted_server(&server, false)
            .expect("script completion should shut the polling runner down cleanly");
        server.handle.join().unwrap();

        let result_bodies = recorded_result_bodies(&server.requests);
        assert_eq!(
            result_bodies.len(),
            2,
            "a transient failure must retry the same result payload"
        );
        assert!(
            result_bodies[0].contains("req-transient"),
            "{result_bodies:?}"
        );
        assert!(
            result_bodies[1].contains("req-transient"),
            "{result_bodies:?}"
        );
    }

    #[test]
    fn polling_result_503_retries_same_payload_then_continues() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollDeliver("req-503"),
            ScriptStep::Result {
                status: "503 Service Unavailable",
                body: r#"{"success":false,"error":"temporary gateway failure"}"#,
            },
            ScriptStep::Result {
                status: "200 OK",
                body: r#"{"success":true}"#,
            },
            ScriptStep::PollEmpty,
        ]);
        run_polling_agent_against_scripted_server(&server, false)
            .expect("script completion should shut the polling runner down cleanly");
        server.handle.join().unwrap();

        let result_bodies = recorded_result_bodies(&server.requests);
        assert_eq!(result_bodies.len(), 2);
        assert_eq!(
            result_bodies[0], result_bodies[1],
            "503 must retry the exact result body"
        );
        assert!(result_bodies[0].contains("req-503"), "{result_bodies:?}");
        let paths: Vec<String> = server
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _)| path.clone())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/result",
                "/api/shell/agent/result",
                "/api/shell/agent/poll",
            ],
            "503 recovery must neither re-register nor stop polling"
        );
    }

    #[test]
    fn polling_result_server_unavailable_retry_exhaustion_drops_then_continues() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollDeliver("req-exhausted"),
            ScriptStep::Result {
                status: "503 Service Unavailable",
                body: r#"{"success":false,"error":"temporary gateway failure"}"#,
            },
            ScriptStep::Result {
                status: "503 Service Unavailable",
                body: r#"{"success":false,"error":"temporary gateway failure"}"#,
            },
            ScriptStep::Result {
                status: "503 Service Unavailable",
                body: r#"{"success":false,"error":"temporary gateway failure"}"#,
            },
            ScriptStep::Result {
                status: "503 Service Unavailable",
                body: r#"{"success":false,"error":"temporary gateway failure"}"#,
            },
            ScriptStep::PollEmpty,
        ]);
        run_polling_agent_against_scripted_server(&server, false)
            .expect("script completion should shut the polling runner down cleanly");
        server.handle.join().unwrap();

        let result_bodies = recorded_result_bodies(&server.requests);
        assert_eq!(
            result_bodies.len(),
            RESULT_SUBMIT_RETRY_BACKOFF.len() + 1,
            "retry exhaustion must stop after the fixed total attempt count"
        );
        assert!(
            result_bodies.iter().all(|body| body == &result_bodies[0]),
            "every bounded retry must use the exact original payload"
        );
        let paths: Vec<String> = server
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _)| path.clone())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/result",
                "/api/shell/agent/result",
                "/api/shell/agent/result",
                "/api/shell/agent/result",
                "/api/shell/agent/poll",
            ],
            "exhaustion must release the result and poll without re-registering"
        );
    }

    #[test]
    fn submit_result_503_retry_exhaustion_returns_dropped_outcome() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Result {
                status: "503 Service Unavailable",
                body: r#"{"success":false,"error":"temporary gateway failure"}"#,
            },
            ScriptStep::Result {
                status: "503 Service Unavailable",
                body: r#"{"success":false,"error":"temporary gateway failure"}"#,
            },
            ScriptStep::Result {
                status: "503 Service Unavailable",
                body: r#"{"success":false,"error":"temporary gateway failure"}"#,
            },
            ScriptStep::Result {
                status: "503 Service Unavailable",
                body: r#"{"success":false,"error":"temporary gateway failure"}"#,
            },
        ]);
        let sink = AgentSink::Http(HttpSendConfig {
            client: Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            server_url: server.server_url.clone(),
            token: "test-token".to_string(),
            client_id: "oe".to_string(),
            agent_instance_id: "inst-exhausted".to_string(),
            shutdown: Arc::new(AtomicBool::new(false)),
        });
        let outcome = sink
            .submit_result(
                "req-exhausted-outcome".to_string(),
                CommandResult {
                    exit_code: Some(0),
                    stdout: Some("done".to_string()),
                    stderr: None,
                    duration_ms: Some(1),
                    error: None,
                },
            )
            .unwrap();
        server.handle.join().unwrap();

        assert_eq!(outcome, ResultSubmission::DroppedAfterRetryExhaustion);
        assert_eq!(
            recorded_result_bodies(&server.requests).len(),
            RESULT_SUBMIT_RETRY_BACKOFF.len() + 1
        );
    }

    #[test]
    fn submit_result_retry_backoff_is_shutdown_aware() {
        let server = start_scripted_agent_server(vec![ScriptStep::Result {
            status: "503 Service Unavailable",
            body: r#"{"success":false,"error":"temporary gateway failure"}"#,
        }]);
        let sink = AgentSink::Http(HttpSendConfig {
            client: Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            server_url: server.server_url.clone(),
            token: "test-token".to_string(),
            client_id: "oe".to_string(),
            agent_instance_id: "inst-shutdown".to_string(),
            shutdown: Arc::new(AtomicBool::new(true)),
        });
        let started = Instant::now();
        let error = sink
            .submit_result(
                "req-shutdown".to_string(),
                CommandResult {
                    exit_code: Some(0),
                    stdout: Some("done".to_string()),
                    stderr: None,
                    duration_ms: Some(1),
                    error: None,
                },
            )
            .expect_err("shutdown must interrupt the retry backoff");
        server.handle.join().unwrap();

        assert!(matches!(error, SubmitResultError::Shutdown(_)), "{error:?}");
        assert_eq!(recorded_result_bodies(&server.requests).len(), 1);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "shutdown-aware result backoff did not return promptly"
        );
    }

    #[test]
    fn result_submission_gateway_and_connection_classes_are_transient() {
        for status in [
            reqwest::StatusCode::BAD_GATEWAY,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            reqwest::StatusCode::GATEWAY_TIMEOUT,
        ] {
            let error = AgentHttpError::status(AGENT_RESULT_PATH, status, "{}");
            assert_eq!(error.kind, AgentHttpErrorKind::ServerUnavailable);
            assert_eq!(
                result_http_error_disposition(&error.kind),
                ResultHttpErrorDisposition::RetryTransient,
                "status {status} must enter bounded result retry"
            );
        }
        assert_eq!(
            result_http_error_disposition(&AgentHttpErrorKind::ServerUnavailable),
            ResultHttpErrorDisposition::RetryTransient,
            "connection-refused/reset/closed classification must enter bounded retry"
        );
        for kind in [
            AgentHttpErrorKind::Status,
            AgentHttpErrorKind::RequestTimeout,
            AgentHttpErrorKind::Request,
            AgentHttpErrorKind::DecodeTransient,
        ] {
            assert_eq!(
                result_http_error_disposition(&kind),
                ResultHttpErrorDisposition::RetryTransient,
                "{kind:?} must enter bounded result retry"
            );
        }
        assert_eq!(
            result_http_error_disposition(&AgentHttpErrorKind::ClientRejected),
            ResultHttpErrorDisposition::RejectPermanent
        );
        assert_eq!(
            result_http_error_disposition(&AgentHttpErrorKind::Auth),
            ResultHttpErrorDisposition::FatalAuth
        );
        assert_eq!(
            result_http_error_disposition(&AgentHttpErrorKind::NotFound),
            ResultHttpErrorDisposition::FatalProtocol
        );
        assert_eq!(
            result_http_error_disposition(&AgentHttpErrorKind::ProtocolDecode),
            ResultHttpErrorDisposition::FatalProtocol
        );
        assert_eq!(
            result_http_error_disposition(&AgentHttpErrorKind::Config),
            ResultHttpErrorDisposition::FatalConfig
        );
    }

    #[test]
    fn polling_result_401_and_403_are_terminal_auth_errors_without_credentials() {
        for status in ["401 Unauthorized", "403 Forbidden"] {
            let server = start_scripted_agent_server(vec![
                ScriptStep::Register,
                ScriptStep::PollDeliver("req-auth"),
                ScriptStep::Result {
                    status,
                    body: r#"{"success":false,"error":"unauthorized token=SECRET-BODY-TOKEN"}"#,
                },
            ]);
            let error = run_polling_agent_against_scripted_server(&server, false)
                .expect_err("auth rejection on result submission must stop the agent");
            server.handle.join().unwrap();

            assert_eq!(
                recorded_result_bodies(&server.requests).len(),
                1,
                "an auth failure must not retry the result"
            );
            assert!(
                error.contains("authentication failed for /api/shell/agent/result"),
                "{error}"
            );
            assert!(error.contains("check agent token/config"), "{error}");
            assert!(!error.contains("test-token"), "{error}");
            assert!(!error.contains("SECRET-BODY-TOKEN"), "{error}");
        }
    }

    #[test]
    fn polling_result_404_is_terminal_protocol_error_without_retry() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollDeliver("req-missing-endpoint"),
            ScriptStep::Result {
                status: "404 Not Found",
                body: r#"{"success":false,"error":"token=SECRET-BODY-TOKEN"}"#,
            },
        ]);
        let error = run_polling_agent_against_scripted_server(&server, false)
            .expect_err("missing result endpoint must stop the polling agent");
        server.handle.join().unwrap();

        assert_eq!(
            recorded_result_bodies(&server.requests).len(),
            1,
            "404 must not retry the result"
        );
        assert!(
            error.contains("endpoint missing or incompatible server for /api/shell/agent/result"),
            "{error}"
        );
        assert!(!error.contains("test-token"), "{error}");
        assert!(!error.contains("SECRET-BODY-TOKEN"), "{error}");
    }

    #[test]
    fn polling_result_success_submits_once_and_continues() {
        let server = start_scripted_agent_server(vec![
            ScriptStep::Register,
            ScriptStep::PollDeliver("req-success"),
            ScriptStep::Result {
                status: "200 OK",
                body: r#"{"success":true}"#,
            },
            ScriptStep::PollEmpty,
        ]);
        run_polling_agent_against_scripted_server(&server, false)
            .expect("script completion should shut the polling runner down cleanly");
        server.handle.join().unwrap();

        let result_bodies = recorded_result_bodies(&server.requests);
        assert_eq!(result_bodies.len(), 1);
        assert!(
            result_bodies[0].contains("req-success"),
            "{result_bodies:?}"
        );
        let paths: Vec<String> = server
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _)| path.clone())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/api/shell/agent/register",
                "/api/shell/agent/poll",
                "/api/shell/agent/result",
                "/api/shell/agent/poll",
            ]
        );
    }

    #[test]
    fn permanent_rejection_log_line_is_bounded_and_redacted() {
        let token = "DO_NOT_LEAK_THIS_TOKEN";
        let noisy_error = format!(
            "server rejected /api/shell/agent/result request: HTTP 400 Bad Request: token={} url=https://host/path?token={}\n{}",
            token,
            token,
            "<html><body>huge proxy page</body></html>".repeat(200)
        );
        let line = permanent_result_rejection_log_line("req-noisy", &noisy_error, token);
        assert!(line.contains("request_id=req-noisy"), "{line}");
        assert!(!line.contains(token), "{line}");
        assert!(!line.contains("?token="), "{line}");
        assert!(!line.contains('\n'), "{line}");
        assert!(
            line.chars().count() < 320,
            "log line not bounded: {} chars",
            line.chars().count()
        );
    }

    #[test]
    fn dropped_result_log_line_is_bounded_and_redacted() {
        let token = "DO_NOT_LEAK_THIS_TOKEN";
        let line = dropped_result_log_line(
            &format!("req-\n{}{}", token, "x".repeat(500)),
            RESULT_SUBMIT_RETRY_BACKOFF.len() + 1,
            &format!(
                "server unavailable token={} {}",
                token,
                "<html>proxy response</html>".repeat(200)
            ),
            token,
        );
        assert!(line.contains("attempts=4"), "{line}");
        assert!(!line.contains(token), "{line}");
        assert!(!line.contains('\n'), "{line}");
        assert!(
            line.chars().count() < 520,
            "log line not bounded: {} chars",
            line.chars().count()
        );
    }

    async fn read_register(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> crate::shell_protocol::ShellClientRegisterRequest {
        let msg = ws
            .next()
            .await
            .expect("agent sent register")
            .expect("register message is ok");
        match AgentEnvelope::from_slice(msg.into_text().unwrap().as_bytes()).unwrap() {
            AgentEnvelope::Register { payload, .. } => payload,
            other => panic!("expected register envelope, got {}", other.kind()),
        }
    }

    async fn send_registered_ack(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) {
        let ack = AgentEnvelope::Registered {
            success: true,
            client: None,
            error: None,
        };
        ws.send(WsMessage::Text(ack.to_json().unwrap().into()))
            .await
            .unwrap();
    }

    async fn send_register_rejected_ack(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) {
        let ack = AgentEnvelope::Registered {
            success: false,
            client: None,
            error: Some("unauthorized".to_string()),
        };
        ws.send(WsMessage::Text(ack.to_json().unwrap().into()))
            .await
            .unwrap();
    }

    fn start_job_request(cwd: &Path, command: &str) -> ShellAgentShellRequest {
        ShellAgentShellRequest {
            request_id: "req-active-job".to_string(),
            client_id: "oe".to_string(),
            kind: "start_job".to_string(),
            job_id: Some("job-active".to_string()),
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
            timeout_secs: 5,
            requested_by: "tester".to_string(),
            created_at: 0,
            validation: None,
            lsp: None,
            sandbox: None,
        }
    }

    #[test]
    fn reconnect_backoff_is_bounded_exponential() {
        let mut backoff = ReconnectBackoff::new();
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
        assert_eq!(backoff.next_delay(), Duration::from_secs(2));
        assert_eq!(backoff.next_delay(), Duration::from_secs(5));
        assert_eq!(backoff.next_delay(), Duration::from_secs(10));
        assert_eq!(backoff.next_delay(), Duration::from_secs(30));
        assert_eq!(backoff.next_delay(), Duration::from_secs(30));
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn polling_recovery_backoff_is_bounded_and_resets() {
        let mut backoff = PollingRecoveryBackoff::new();
        assert_eq!(backoff.next_delay(), Duration::from_millis(500));
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
        assert_eq!(backoff.next_delay(), Duration::from_secs(2));
        assert_eq!(backoff.next_delay(), Duration::from_secs(5));
        assert_eq!(backoff.next_delay(), Duration::from_secs(10));
        assert_eq!(backoff.next_delay(), Duration::from_secs(10));
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(500));
    }

    #[test]
    fn polling_lease_conflict_retry_has_a_finite_total_wait() {
        let mut backoff = PollingRecoveryBackoff::new();
        assert_eq!(
            next_lease_conflict_delay(
                &mut backoff,
                POLLING_LEASE_CONFLICT_MAX_WAIT - Duration::from_millis(250)
            ),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            next_lease_conflict_delay(&mut backoff, POLLING_LEASE_CONFLICT_MAX_WAIT),
            None
        );
    }

    #[test]
    fn transport_error_classification_separates_transient_and_fatal() {
        let transient = classify_session_error("websocket connect failed: connection refused");
        assert!(!transient.is_fatal(), "{transient}");

        let fatal = classify_session_error("register rejected by server: unauthorized");
        assert!(fatal.is_fatal(), "{fatal}");

        let fatal = classify_session_error(
            "quic connect failed: certificate verify failed; check server_name",
        );
        assert!(fatal.is_fatal(), "{fatal}");
    }

    #[test]
    fn auto_log_lines_are_concise_and_redacted() {
        assert_eq!(
            auto_quic_not_configured_log_line(),
            "webcodex-runner transport auto: quic not configured; skipping"
        );
        assert_eq!(
            auto_trying_log_line(TRANSPORT_WEBSOCKET),
            "webcodex-runner transport auto: websocket trying"
        );
        assert_eq!(
            auto_trying_log_line(TRANSPORT_POLLING),
            "webcodex-runner transport auto: polling trying"
        );

        let token = "DO_NOT_LEAK_THIS_TOKEN";
        let concise = concise_log_error(
            "websocket connect failed: token=DO_NOT_LEAK_THIS_TOKEN\nwhile connecting",
            token,
        );
        assert!(!concise.contains(token), "{concise}");
        assert!(!concise.contains('\n'), "{concise}");
    }

    #[test]
    fn registered_log_includes_actual_transport_without_url_query_or_token() {
        let token = "DO_NOT_LEAK_THIS_TOKEN";
        let mut cfg = test_agent_config(format!(
            "https://webcodex.example.test/agent/path?token={}",
            token
        ));
        cfg.token = token.to_string();
        cfg.transport = Some(TRANSPORT_AUTO.to_string());

        let line = registered_log_line(&cfg, TRANSPORT_POLLING, 11);
        assert!(line.contains("client_id=oe"), "{line}");
        assert!(
            line.contains("server=https://webcodex.example.test"),
            "{line}"
        );
        assert!(line.contains("preferred_transport=auto"), "{line}");
        assert!(line.contains("actual_transport=polling"), "{line}");
        assert!(line.contains("projects=11"), "{line}");
        assert!(!line.contains(token), "{line}");
        assert!(!line.contains("/agent/path"), "{line}");
        assert!(!line.contains("?token="), "{line}");
    }

    #[test]
    fn auto_websocket_failure_falls_back_to_polling() {
        let (server_url, poll_count, server) = start_auto_fallback_http_server(
            "502 Bad Gateway",
            "text/html",
            "<html>bad gateway</html>",
        );
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_agent_config(server_url);
        cfg.transport = Some(TRANSPORT_AUTO.to_string());
        cfg.projects_dir = Some(tmp.path().join("projects.d"));
        cfg.websocket_connect_timeout_secs = 1;

        let runtime = test_runtime(&cfg);
        let err = run_auto_agent(cfg, false, "inst-auto-fallback", &runtime)
            .expect_err("terminal polling 404 should stop after recovering the first 502");
        server.join().unwrap();
        assert_eq!(poll_count.load(Ordering::SeqCst), 2);
        assert!(
            err.contains("poll endpoint missing or incompatible server"),
            "{err}"
        );
    }

    #[test]
    fn polling_502_html_is_transient_and_sanitized() {
        let nginx_html = "<html>\n<head><title>502 Bad Gateway</title></head>\n<body>\n<center><h1>502 Bad Gateway</h1></center>\n<hr><center>nginx/1.31.1</center>\n</body>\n</html>";
        let error = AgentHttpError::status(
            "/api/shell/agent/poll",
            reqwest::StatusCode::BAD_GATEWAY,
            nginx_html,
        );
        let poll_error = crate::PollError::from_http(error, "oe");
        assert_eq!(
            poll_error.recovery_action(),
            PollingRecoveryAction::RetryPoll
        );
        let message = poll_error.to_string();
        assert!(
            message.contains(
                "server unavailable while polling /api/shell/agent/poll: HTTP 502 Bad Gateway"
            ),
            "{message}"
        );
        assert!(!message.contains("<html"), "{message}");
        assert!(!message.contains("nginx/1.31.1"), "{message}");
        assert!(!message.contains("<center><h1>502 Bad Gateway</h1></center>"));
    }

    #[test]
    fn polling_503_and_504_are_transient_server_unavailable() {
        for (status, expected) in [
            (
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                "server unavailable while polling /api/shell/agent/poll: HTTP 503 Service Unavailable",
            ),
            (
                reqwest::StatusCode::GATEWAY_TIMEOUT,
                "server unavailable while polling /api/shell/agent/poll: HTTP 504 Gateway Timeout",
            ),
        ] {
            let error = AgentHttpError::status("/api/shell/agent/poll", status, "proxy unavailable");
            let poll_error = crate::PollError::from_http(error, "oe");
            assert_eq!(
                poll_error.recovery_action(),
                PollingRecoveryAction::RetryPoll
            );
            let message = poll_error.to_string();
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("proxy unavailable"), "{message}");
        }
    }

    #[test]
    fn polling_401_and_403_are_terminal_auth_errors() {
        for (status, expected) in [
            (
                "401 Unauthorized",
                "authentication failed while polling /api/shell/agent/poll: HTTP 401 Unauthorized; check agent token/config",
            ),
            (
                "403 Forbidden",
                "authentication failed while polling /api/shell/agent/poll: HTTP 403 Forbidden; check agent token/config",
            ),
        ] {
            let (result, poll_count) = run_polling_agent_against_server(
                status,
                "application/json",
                r#"{"error":"unauthorized"}"#,
                false,
            );
            let error = result.expect_err("auth poll response must stop the foreground agent");

            assert_eq!(poll_count, 1);
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("unauthorized\""), "{error}");
        }
    }

    #[test]
    fn polling_idle_empty_response_remains_successful_once() {
        let (result, poll_count) = run_polling_agent_against_server(
            "200 OK",
            "application/json",
            r#"{"success":true,"request":null,"error":null}"#,
            true,
        );

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(poll_count, 1);
    }

    #[test]
    fn polling_shutdown_interrupts_retry_sleep() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let trigger = Arc::clone(&shutdown);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            trigger.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        assert!(sleep_or_shutdown(Duration::from_secs(5), shutdown.as_ref()));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "shutdown-aware polling sleep did not return promptly"
        );
    }

    #[tokio::test]
    async fn websocket_close_returns_transport_disconnect_not_shutdown() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _register = read_register(&mut ws).await;
            send_registered_ack(&mut ws).await;
            ws.send(WsMessage::Close(None)).await.unwrap();
            if let Ok(Some(Ok(msg))) =
                tokio::time::timeout(Duration::from_millis(200), ws.next()).await
            {
                if msg.is_text() {
                    let env =
                        AgentEnvelope::from_slice(msg.into_text().unwrap().as_bytes()).unwrap();
                    assert!(
                        !matches!(env, AgentEnvelope::Goodbye { .. }),
                        "ordinary transport disconnect must not send Goodbye"
                    );
                }
            }
        });

        let cfg = test_agent_config(format!("http://{}", addr));
        let exit = tokio::time::timeout(
            Duration::from_secs(5),
            websocket_session(
                &cfg,
                vec![test_project("close-test")],
                "inst-close",
                &test_runtime(&cfg),
            ),
        )
        .await
        .expect("session completed")
        .expect("session should not error");

        assert_eq!(exit, AgentSessionExit::TransportDisconnected);
        assert_ne!(exit, AgentSessionExit::Shutdown);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn websocket_disconnect_with_active_job_returns_without_waiting_for_job() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let request = start_job_request(cwd.path(), "sleep 2");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _register = read_register(&mut ws).await;
            send_registered_ack(&mut ws).await;
            ws.send(WsMessage::Text(
                AgentEnvelope::Request { request }.to_json().unwrap().into(),
            ))
            .await
            .unwrap();

            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let msg = ws.next().await.unwrap().unwrap();
                    if !msg.is_text() {
                        continue;
                    }
                    match AgentEnvelope::from_slice(msg.into_text().unwrap().as_bytes()).unwrap() {
                        AgentEnvelope::JobUpdate { payload }
                            if payload.job_id == "job-active" && !payload.finished =>
                        {
                            break;
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("agent did not report active job");

            ws.send(WsMessage::Close(None)).await.unwrap();
        });

        let cfg = test_agent_config(format!("http://{}", addr));
        let started = Instant::now();
        let exit = tokio::time::timeout(
            Duration::from_millis(900),
            websocket_session(
                &cfg,
                vec![test_project("active-job-test")],
                "inst-active-job",
                &test_runtime(&cfg),
            ),
        )
        .await
        .expect("session must return promptly after disconnect despite active job")
        .expect("session should not error");

        assert_eq!(exit, AgentSessionExit::TransportDisconnected);
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "session waited for the active job instead of reconnecting"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn websocket_register_rejected_is_fatal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _register = read_register(&mut ws).await;
            send_register_rejected_ack(&mut ws).await;
        });

        let cfg = test_agent_config(format!("http://{}", addr));
        let error = websocket_session(
            &cfg,
            vec![test_project("reject-test")],
            "inst-reject",
            &test_runtime(&cfg),
        )
        .await
        .expect_err("register rejection must error");
        let classified = classify_session_error(error);
        assert!(classified.is_fatal(), "{classified}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn strict_websocket_transient_connect_failure_reconnects() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (first_stream, _) = listener.accept().await.unwrap();
            drop(first_stream);

            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _register = read_register(&mut ws).await;
            send_register_rejected_ack(&mut ws).await;
        });

        let cfg = test_agent_config(format!("http://{}", addr));
        let runtime = test_runtime(&cfg);
        let started = Instant::now();
        let runner = tokio::task::spawn_blocking(move || {
            run_websocket_agent(cfg, false, "inst-retry", &runtime)
        });
        let error = tokio::time::timeout(Duration::from_secs(5), runner)
            .await
            .expect("strict websocket retry did not finish after fatal register rejection")
            .unwrap()
            .expect_err("register rejection after reconnect must be fatal");

        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "strict websocket did not wait for reconnect backoff after transient connect failure"
        );
        assert!(error.contains("register rejected"), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn auto_websocket_register_rejected_is_fatal_without_polling_fallback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _register = read_register(&mut ws).await;
            send_register_rejected_ack(&mut ws).await;
        });

        let mut cfg = test_agent_config(format!("http://{}", addr));
        cfg.transport = Some(TRANSPORT_AUTO.to_string());
        let runtime = test_runtime(&cfg);
        let runner = tokio::task::spawn_blocking(move || {
            run_auto_agent(cfg, false, "inst-auto-reject", &runtime)
        });
        let error = tokio::time::timeout(Duration::from_secs(5), runner)
            .await
            .expect("auto websocket register rejection did not return")
            .unwrap()
            .expect_err("fatal register rejection must not fall back to polling");

        assert!(error.contains("register rejected"), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn websocket_disconnect_loop_reregisters_client_projects_and_capabilities() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (reg_tx, mut reg_rx) = mpsc::channel(2);
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let register = read_register(&mut ws).await;
                reg_tx.send(register).await.unwrap();
                send_registered_ack(&mut ws).await;
                ws.send(WsMessage::Close(None)).await.unwrap();
            }
        });

        let cfg = test_agent_config(format!("http://{}", addr));
        let projects = vec![test_project("repo-one")];
        for instance in ["inst-reconnect", "inst-reconnect"] {
            let exit = tokio::time::timeout(
                Duration::from_secs(5),
                websocket_session(&cfg, projects.clone(), instance, &test_runtime(&cfg)),
            )
            .await
            .expect("session completed")
            .expect("session should not error");
            assert_eq!(exit, AgentSessionExit::TransportDisconnected);
        }

        let first = reg_rx.recv().await.expect("first register");
        let second = reg_rx.recv().await.expect("second register");
        for register in [first, second] {
            assert_eq!(register.client_id, "oe");
            assert_eq!(register.agent_instance_id, "inst-reconnect");
            assert_eq!(
                register.agent_protocol_version.as_deref(),
                Some(AGENT_PROTOCOL_VERSION_WEBSOCKET_V1)
            );
            let caps = register.capabilities.expect("capabilities");
            assert!(caps.shell);
            assert!(caps.file_read);
            assert!(caps.file_write);
            assert!(caps.jobs);
            assert!(caps.async_jobs);
            assert!(caps.async_shell_jobs);
            assert!(caps.git);
            let projects = register.projects.expect("projects");
            assert_eq!(projects.len(), 1);
            assert_eq!(projects[0].id, "repo-one");
        }

        server.await.unwrap();
    }

    #[tokio::test]
    async fn websocket_process_shutdown_exits_gracefully() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (registered_tx, registered_rx) = oneshot::channel();
        let (goodbye_tx, goodbye_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _register = read_register(&mut ws).await;
            send_registered_ack(&mut ws).await;
            registered_tx.send(()).unwrap();
            let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("agent did not send shutdown goodbye")
                .expect("stream open")
                .expect("message ok");
            match AgentEnvelope::from_slice(msg.into_text().unwrap().as_bytes()).unwrap() {
                AgentEnvelope::Goodbye { reason } => goodbye_tx.send(reason).unwrap(),
                other => panic!("expected goodbye, got {}", other.kind()),
            }
        });

        let cfg = test_agent_config(format!("http://{}", addr));
        let runtime = test_runtime(&cfg);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let session = tokio::spawn(async move {
            websocket_session_with_shutdown(
                &cfg,
                vec![test_project("shutdown-test")],
                "inst-shutdown",
                &runtime,
                async {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });

        registered_rx.await.unwrap();
        shutdown_tx.send(()).unwrap();
        let exit = tokio::time::timeout(Duration::from_secs(5), session)
            .await
            .expect("shutdown completed")
            .unwrap()
            .expect("session should not error");
        assert_eq!(exit, AgentSessionExit::Shutdown);
        assert_eq!(
            goodbye_rx.await.unwrap().as_deref(),
            Some("process shutdown")
        );
        server.await.unwrap();
    }
}
