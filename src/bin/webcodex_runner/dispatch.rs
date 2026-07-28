use super::external_tools::ExternalRoute;
use super::lsp::{handle_lsp_request, is_lsp_request_kind, LspSupervisor};
use super::validation::{handle_validation_request, is_validation_request_kind};
use super::{
    handle_project_op, run_shell_with_profiles_in_sandbox, AgentSink, HotAgentConfig,
    ReloadableAgentConfig,
};
use crate::shell_protocol::ShellAgentShellRequest;
use crate::{handle_file_request, is_file_request_kind, JobManager};
use std::path::Path;

/// Execute a single agent request (shell/file/job/lsp/validation) and send the
/// result over the active transport. This is the shared dispatch path used by
/// both the polling loop (`handle_one_poll`) and the WebSocket loop. It contains
/// no transport-specific code: all outgoing traffic goes through `sink`.
pub(crate) fn dispatch_request(
    sink: &AgentSink,
    config: &HotAgentConfig,
    runtime: &ReloadableAgentConfig,
    jobs: &JobManager,
    projects_dir: &Path,
    lsp: &LspSupervisor,
    request: ShellAgentShellRequest,
) -> Result<bool, String> {
    let policy = &config.policy;
    let shell = &config.shell;
    let external_tools = &config.external_tools;
    // Inspect requests must stay on the native execution path where Landlock
    // is applied in pre_exec. External providers are not an equivalent local
    // filesystem write boundary.
    let external_route = if request.sandbox.is_some() {
        ExternalRoute::Native
    } else {
        external_tools.route(policy, &request)
    };
    match external_route {
        ExternalRoute::Handled(result) => {
            return sink.submit_result_with_metadata(request.request_id, result, config, runtime);
        }
        ExternalRoute::NativeFallback(fallback) => {
            let request_id = request.request_id.clone();
            let result = if is_file_request_kind(&request.kind) {
                handle_file_request(policy, &request)
            } else {
                run_shell_with_profiles_in_sandbox(
                    config.generation,
                    policy,
                    shell,
                    projects_dir,
                    &jobs.prepared_profiles,
                    request.cwd.as_deref(),
                    &request.command,
                    request.stdin.as_deref(),
                    request.timeout_secs,
                    None,
                    request.sandbox.as_deref(),
                )
            };
            external_tools.complete_native_fallback(fallback, &result);
            return sink.submit_result_with_metadata(request_id, result, config, runtime);
        }
        ExternalRoute::Native => {}
    }
    match request.kind.as_str() {
        "start_job" | "start_validation_job" => {
            jobs.enqueue(
                sink.clone(),
                config.generation,
                policy.clone(),
                shell.clone(),
                projects_dir.to_path_buf(),
                request,
            );
            Ok(true)
        }
        "stop_job" => {
            if let Some(job_id) = request.job_id.as_deref() {
                if let Err(e) = jobs.stop(job_id) {
                    eprintln!("webcodex-runner stop_job error: {}", e);
                }
            }
            Ok(true)
        }
        kind if is_file_request_kind(kind) => {
            let request_id = request.request_id.clone();
            let result = handle_file_request(policy, &request);
            sink.submit_result_with_metadata(request_id, result, config, runtime)
        }
        "register_project" | "create_project" => {
            let request_id = request.request_id.clone();
            let result = handle_project_op(policy, projects_dir, &request);
            sink.submit_result_with_metadata(request_id, result, config, runtime)
        }
        kind if is_lsp_request_kind(kind) => {
            // Explicit LSP branch — must never fall through to shell execution.
            let request_id = request.request_id.clone();
            let result = handle_lsp_request(policy, projects_dir, lsp, &request);
            sink.submit_result_with_metadata(request_id, result, config, runtime)
        }
        kind if is_validation_request_kind(kind) => {
            // Explicit validation bridge branch — never fall through to shell.
            let request_id = request.request_id.clone();
            let result = handle_validation_request(policy, projects_dir, &request);
            sink.submit_result_with_metadata(request_id, result, config, runtime)
        }
        _ => {
            let request_id = request.request_id.clone();
            let result = run_shell_with_profiles_in_sandbox(
                config.generation,
                policy,
                shell,
                projects_dir,
                &jobs.prepared_profiles,
                request.cwd.as_deref(),
                &request.command,
                request.stdin.as_deref(),
                request.timeout_secs,
                None,
                request.sandbox.as_deref(),
            );
            sink.submit_result_with_metadata(request_id, result, config, runtime)
        }
    }
}

pub(crate) fn is_project_op(kind: &str) -> bool {
    kind == "register_project" || kind == "create_project"
}
