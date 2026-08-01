//! Structured read-only workspace tools executed on a Workflow Session's SSH
//! resource.
//!
//! Every request resolves the remote workspace root once (`Session
//! default_cwd` > SSH resource `default_cwd` > remote login default), then
//! runs one fixed, read-only command through the existing authenticated SSH
//! transport ([`SshConnectionPool`]) on an independent exec channel. Nothing
//! here reuses an SSH persistent shell process: variables, functions, cwd,
//! umask, and redirections the user set in a persistent shell cannot
//! influence structured tools.
//!
//! This module is deliberately narrow. It only:
//! - resolves the remote workspace root and rejects a root that cannot be
//!   entered or observed;
//! - validates a project-relative path against absolute/URI/`..`/NUL/control/
//!   Windows-form/overlong inputs;
//! - builds fixed read-only command texts (file reads/lists, bounded scans,
//!   `rg`/grep search, Git reads);
//! - executes them over the existing pool with the current config generation,
//!   timeouts, and output limits;
//! - returns the bounded raw stdout/stderr so the Server can parse it with the
//!   same shapes as the existing local tools.
//!
//! No second SSH configuration, second pool, independent authentication,
//! remote daemon, or uploaded helper is introduced. SSH control parameters,
//! host, ControlPath, key, password, and the full remote command are never
//! returned to the Server.

use super::config::SshConfig;
use super::output::{err_cmd, CommandResult};
use super::ssh::SshConnectionPool;
use super::AgentPolicy;
use crate::shell_protocol::{
    RemoteWorkspaceReadOutcome, RemoteWorkspaceReadRequest, RemoteWorkspaceReadResponse,
    REMOTE_WORKSPACE_READ_RESULT_FORMAT,
};
use std::time::Instant;

/// Upper bound for one structured remote read result.
const REMOTE_READ_MAX_OUTPUT_BYTES: usize = 240 * 1024;
const REMOTE_READ_ENVELOPE_MAX_BYTES: usize = 256 * 1024;
const REMOTE_READ_FILE_MAX_BYTES: usize = 48 * 1024;
const STDOUT_TRUNCATED_SENTINEL: &str = "\0webcodex_stdout_truncated";
/// Longest accepted project-relative path.
const MAX_REMOTE_PATH_BYTES: usize = 4096;
/// `rg`/grep fallback budget used by the bounded search wrapper.

/// Execute one structured read-only operation against the Workflow Session's
/// SSH resource and return the bounded raw stdout/stderr plus exit code.
/// `session_cwd` is the Session's remote `default_cwd`; the effective root is
/// that value, else the resource's `default_cwd`, else the remote login
/// directory.
pub(crate) fn run_remote_workspace_read(
    pool: &SshConnectionPool,
    generation: u64,
    config: &SshConfig,
    policy: &AgentPolicy,
    resource_name: &str,
    session_id: &str,
    session_cwd: Option<&str>,
    read: &RemoteWorkspaceReadRequest,
) -> CommandResult {
    let start = Instant::now();
    let raw = run_remote_workspace_read_inner(
        pool,
        generation,
        config,
        policy,
        resource_name,
        session_id,
        session_cwd,
        read,
    );
    let mut raw = raw;
    let stdout_truncated = raw.stderr.as_mut().is_some_and(|stderr| {
        if let Some(index) = stderr.find(STDOUT_TRUNCATED_SENTINEL) {
            stderr.truncate(index);
            true
        } else {
            false
        }
    });
    let outcome = if let Some(error) = raw.error {
        RemoteWorkspaceReadOutcome::Failure {
            error_kind: remote_error_kind(&error).to_string(),
            message: sanitize_remote_error(&error),
            command_started: raw.exit_code.is_some(),
            command_completed: raw.exit_code.is_some(),
            exit_code: raw.exit_code,
        }
    } else if raw.exit_code == Some(0)
        || (read.operation == "search_project_text" && matches!(raw.exit_code, Some(1 | 141)))
    {
        RemoteWorkspaceReadOutcome::Success {
            exit_code: raw.exit_code.unwrap_or(0),
            stdout: raw.stdout.unwrap_or_default(),
            stdout_truncated,
        }
    } else {
        RemoteWorkspaceReadOutcome::Failure {
            error_kind: operation_error_kind(read, raw.exit_code).to_string(),
            message: operation_failure_message(read),
            command_started: true,
            command_completed: true,
            exit_code: raw.exit_code,
        }
    };
    let response = bound_remote_workspace_response(RemoteWorkspaceReadResponse {
        format: REMOTE_WORKSPACE_READ_RESULT_FORMAT.to_string(),
        operation: read.operation.clone(),
        outcome,
    });
    super::output::ok_cmd(
        start,
        serde_json::to_value(response).unwrap_or_else(|_| {
            serde_json::json!({
                "format": REMOTE_WORKSPACE_READ_RESULT_FORMAT,
                "operation": read.operation,
                "status": "failure",
                "error_kind": "ssh_workspace_protocol_failure",
                "message": "failed to serialize SSH workspace response",
                "command_started": false,
                "command_completed": false
            })
        }),
    )
}

fn bound_remote_workspace_response(
    mut response: RemoteWorkspaceReadResponse,
) -> RemoteWorkspaceReadResponse {
    loop {
        let size = serde_json::to_vec(&response)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if size <= REMOTE_READ_ENVELOPE_MAX_BYTES {
            return response;
        }
        let RemoteWorkspaceReadOutcome::Success {
            stdout,
            stdout_truncated,
            ..
        } = &mut response.outcome
        else {
            return protocol_oversize_failure(response.operation);
        };
        if stdout.is_empty() {
            return protocol_oversize_failure(response.operation);
        }
        let remove = size
            .saturating_sub(REMOTE_READ_ENVELOPE_MAX_BYTES)
            .saturating_add(1024)
            .min(stdout.len());
        let mut keep = stdout.len().saturating_sub(remove);
        while keep > 0 && !stdout.is_char_boundary(keep) {
            keep -= 1;
        }
        stdout.truncate(keep);
        *stdout_truncated = true;
    }
}

fn protocol_oversize_failure(operation: String) -> RemoteWorkspaceReadResponse {
    RemoteWorkspaceReadResponse {
        format: REMOTE_WORKSPACE_READ_RESULT_FORMAT.to_string(),
        operation,
        outcome: RemoteWorkspaceReadOutcome::Failure {
            error_kind: "ssh_workspace_protocol_failure".to_string(),
            message: "SSH workspace result envelope exceeded its protocol limit".to_string(),
            command_started: true,
            command_completed: true,
            exit_code: None,
        },
    }
}

fn remote_error_kind(error: &str) -> &str {
    error
        .split(':')
        .next()
        .unwrap_or("ssh_workspace_transport_failure")
}

fn sanitize_remote_error(error: &str) -> String {
    let kind = remote_error_kind(error);
    match kind {
        "ssh_workspace_root_unavailable" => "remote workspace root is unavailable".to_string(),
        "ssh_workspace_root_invalid" => "remote workspace root is invalid".to_string(),
        "ssh_workspace_path_invalid" => "remote workspace path is invalid".to_string(),
        "ssh_command_spawn_failed" | "ssh_command_wait_failed" => {
            "SSH workspace transport failed".to_string()
        }
        _ if error.to_ascii_lowercase().contains("timed out") => {
            "SSH workspace operation timed out".to_string()
        }
        _ => "SSH workspace operation failed".to_string(),
    }
}

fn operation_error_kind(read: &RemoteWorkspaceReadRequest, exit_code: Option<i32>) -> &str {
    match exit_code {
        Some(120) => "ssh_workspace_containment_unavailable",
        Some(121) => "ssh_workspace_containment_denied",
        Some(122) => "ssh_workspace_target_unavailable",
        Some(123) => "ssh_workspace_file_not_text",
        Some(124) => "ssh_workspace_output_limit_exceeded",
        Some(125) => "ssh_workspace_read_capability_unavailable",
        Some(3) if read.operation.starts_with("git_") => "ssh_workspace_not_git_repository",
        _ => "ssh_workspace_operation_failed",
    }
}

fn operation_failure_message(read: &RemoteWorkspaceReadRequest) -> String {
    format!("SSH workspace {} failed", read.operation)
}

fn run_remote_workspace_read_inner(
    pool: &SshConnectionPool,
    generation: u64,
    config: &SshConfig,
    policy: &AgentPolicy,
    resource_name: &str,
    session_id: &str,
    session_cwd: Option<&str>,
    read: &RemoteWorkspaceReadRequest,
) -> CommandResult {
    let start = Instant::now();
    let timeout_secs = read.timeout_secs.min(policy.max_timeout_secs).max(1);
    let max_output_bytes = policy.max_output_bytes.min(REMOTE_READ_MAX_OUTPUT_BYTES);

    // Resolve and pin the authoritative remote root inside the request. The
    // same generation + config + resource are used for root resolution and for
    // the exec that follows, so a config reload cannot split one request
    // across two generations.
    let prepared = match pool.prepare_command(
        generation,
        config,
        resource_name,
        session_id,
        session_cwd,
        "pwd -P",
    ) {
        Ok(prepared) => prepared,
        Err(error) => return err_cmd(start, error),
    };
    let key = prepared.key.clone();
    let mut root_command = prepared.command;
    let root = run_single_remote_exec(
        &mut root_command,
        &key,
        pool,
        max_output_bytes,
        timeout_secs,
    );
    let authoritative_root = match authoritative_remote_root(root) {
        Ok(root) => root,
        Err(error) => return err_cmd(start, error),
    };

    let relative = match validate_remote_relative_path(&read.path) {
        Ok(relative) => relative,
        Err(error) => return err_cmd(start, error),
    };

    if let Some(paths) = read.paths.as_ref() {
        for path in paths {
            let normalized = match validate_remote_relative_path(path) {
                Ok(path) => path,
                Err(error) => return err_cmd(start, error),
            };
            if read.operation == "git_diff_hunks" && normalized == "." {
                return err_cmd(
                    start,
                    "ssh_workspace_path_invalid: git_diff_hunks requires a concrete project-relative path; command was not started".to_string(),
                );
            }
        }
    }

    let body = match build_remote_read_command(read, &relative) {
        Ok(body) => body,
        Err(error) => return err_cmd(start, error),
    };
    let prepared = match pool.prepare_command(
        generation,
        config,
        resource_name,
        session_id,
        Some(&authoritative_root),
        &body,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return err_cmd(start, error),
    };
    let key = prepared.key.clone();
    let mut command = prepared.command;
    run_single_remote_exec(&mut command, &key, pool, max_output_bytes, timeout_secs)
}

/// Resolve the authoritative remote workspace root from the `pwd -P` probe.
/// Fails closed when the root cannot be entered or observed.
fn authoritative_remote_root(probe: CommandResult) -> Result<String, String> {
    if probe.exit_code != Some(0) {
        let detail = probe
            .stderr
            .as_deref()
            .filter(|stderr| !stderr.trim().is_empty())
            .unwrap_or("remote workspace root is unavailable");
        return Err(format!(
            "ssh_workspace_root_unavailable: cannot enter the remote workspace root ({detail}); command was not started"
        ));
    }
    let cwd = probe
        .stdout
        .as_deref()
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .ok_or_else(|| {
            "ssh_workspace_root_unavailable: remote workspace root could not be observed; command was not started".to_string()
        })?;
    if cwd.len() > MAX_REMOTE_PATH_BYTES || cwd.chars().any(char::is_control) || cwd == "/" {
        return Err(
            "ssh_workspace_root_invalid: remote workspace root is not a usable absolute path; command was not started".to_string(),
        );
    }
    Ok(cwd.to_string())
}

/// Validate a project-relative path and normalize it for remote use. `.` stays
/// `.` (the remote workspace root). Absolute paths, URIs, NUL/control
/// characters, parent traversal, Windows drive/UNC forms, and overlong paths
/// are rejected.
pub(crate) fn validate_remote_relative_path(raw: &str) -> Result<String, String> {
    if raw.contains('\0') || raw.chars().any(char::is_control) {
        return Err(
            "ssh_workspace_path_invalid: path cannot contain NUL or control characters; command was not started".to_string(),
        );
    }
    if raw.len() > MAX_REMOTE_PATH_BYTES {
        return Err(
            "ssh_workspace_path_invalid: path is too long; command was not started".to_string(),
        );
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(
            "ssh_workspace_path_invalid: path cannot be empty; command was not started".to_string(),
        );
    }
    if trimmed == "." {
        return Ok(".".to_string());
    }
    if trimmed.starts_with('/') {
        return Err(
            "ssh_workspace_path_invalid: path must be project-relative; command was not started"
                .to_string(),
        );
    }
    if trimmed.contains("://") {
        return Err(
            "ssh_workspace_path_invalid: URI paths are not accepted; command was not started"
                .to_string(),
        );
    }
    // Windows drive letters (`C:\...`) and UNC (`\\...`).
    if trimmed.contains('\\') {
        return Err(
            "ssh_workspace_path_invalid: Windows drive or UNC path forms are not accepted; command was not started".to_string(),
        );
    }
    let mut normalized = String::new();
    for component in trimmed.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                return Err(
                    "ssh_workspace_path_invalid: path cannot contain parent traversal; command was not started".to_string(),
                )
            }
            part => {
                if !normalized.is_empty() {
                    normalized.push('/');
                }
                normalized.push_str(part);
            }
        }
    }
    Ok(if normalized.is_empty() {
        ".".to_string()
    } else {
        normalized
    })
}

/// Build the fixed read-only remote command for one operation. User-controlled
/// strings are validated above and then inserted as single-quoted arguments
/// through the shared shell-quoting helper; no option, pipe, redirection, or
/// extra command is accepted.
fn build_remote_read_command(
    read: &RemoteWorkspaceReadRequest,
    relative: &str,
) -> Result<String, String> {
    match read.operation.as_str() {
        "read_file" => Ok(read_file_command(read, relative)),
        "list_project_files" => {
            Ok(format!(
                "{guard}cd -- \"$target\" || exit 122; find . -mindepth 1 -maxdepth 1 -printf '%f\\t%y\\n' | awk -F '\\t' '{{print $1 ($2 == \"d\" ? \"/\" : \"\")}}' | sort | head -n {limit}",
                guard = physical_containment_guard(relative),
                limit = read.limit.unwrap_or(200).clamp(1, 500).saturating_add(1),
            ))
        }
        "project_overview" => {
            // Bounded directory scan: enumerate the whole tree as `%y %p`
            // records with a shallow max depth, never file contents. The Server
            // parses these into the overview shape.
            Ok(format!(
                "{guard}cd -- \"$target\" || exit 122; find . -mindepth 1 -maxdepth {depth} \\( -type f -o -type d \\) -printf '%y %p\\n' 2>/dev/null | head -n {limit}",
                guard = physical_containment_guard(relative),
                depth = project_overview_depth(read),
                limit = project_overview_limit(read).saturating_add(1),
            ))
        }
        "search_project_text" => Ok(format!(
            "{}{}",
            physical_containment_guard(relative),
            search_command(read)
        )),
        "list_project_tracked_files" => {
            let pathspec = if relative == "." {
                String::new()
            } else {
                format!(
                    " -- {}",
                    super::shell::shell_quote(&format!(
                        ":(literal){}",
                        relative.trim_end_matches('/')
                    ))
                )
            };
            Ok(format!(
                "if git rev-parse --git-dir >/dev/null 2>&1; then git ls-files -z --cached{pathspec} | head -z -c 1048576 2>/dev/null; else exit 3; fi"
            ))
        }
        "git_status" => Ok(git_status_command().to_string()),
        "git_diff_summary" => Ok(git_diff_summary_command()),
        "git_diff" => Ok(git_diff_command(read)),
        "git_diff_hunks" => Ok(git_diff_hunks_command(read, relative)),
        "git_log" => Ok(git_log_command(read)),
        other => Err(format!(
            "ssh_resource_unsupported_for_request: SSH workspace read does not support operation '{other}'; command was not started"
        )),
    }
}

fn read_file_command(read: &RemoteWorkspaceReadRequest, relative: &str) -> String {
    let start = read.start_line.unwrap_or(1).max(1);
    let end = read.end_line.unwrap_or(start.saturating_add(1999));
    let max = read
        .max_bytes
        .unwrap_or(REMOTE_READ_FILE_MAX_BYTES)
        .min(REMOTE_READ_FILE_MAX_BYTES);
    if end < start {
        return "exit 2".to_string();
    }
    let script = r#"import hashlib,json,sys
p=sys.argv[1]; start=int(sys.argv[2]); end=int(sys.argv[3]); maxb=int(sys.argv[4])
h=hashlib.sha256(); total=0; selected=[]; selected_bytes=0
try:
 f=open(p,'rb')
except OSError:
 sys.exit(122)
try:
 for raw in f:
  h.update(raw); total += 1
  if b'\x00' in raw: sys.exit(123)
  try: line=raw.decode('utf-8')
  except UnicodeDecodeError: sys.exit(123)
  if start <= total <= end:
   if line.endswith('\n'): line=line[:-1]
   if line.endswith('\r'): line=line[:-1]
   extra=len(line.encode('utf-8')) + (1 if selected else 0)
   if selected_bytes + extra > maxb: sys.exit(124)
   selected.append(line); selected_bytes += extra
except OSError:
 sys.exit(122)
print(json.dumps({'format':'webcodex.file_read_range.v1','content':'\n'.join(selected),'sha256':h.hexdigest(),'total_lines':total,'start_line':start,'limit':end-start+1},separators=(',',':')))
"#;
    format!(
        "{guard}command -v python3 >/dev/null 2>&1 || exit 125; python3 -c {script} \"$target\" {start} {end} {max}",
        guard = physical_containment_guard(relative),
        script = super::shell::shell_quote(script),
    )
}

/// POSIX guard that refuses a read target whose physical path escapes the
/// authoritative remote workspace root (`$PWD`). A symlink pointing outside
/// the root therefore fails closed; a symlink that cannot be resolved also
/// fails closed (no loose fallback). `relative` is the already-validated
/// project-relative path (no `..`, no absolute, no NUL/control).
///
/// The guard is intentionally conservative: remote `realpath` resolves the
/// complete target and the component-boundary check accepts only the physical
/// root itself or one of its descendants. Missing `realpath`, an unresolved
/// target, or any symlink escape is a fail-closed rejection.
fn physical_containment_guard(relative: &str) -> String {
    let safe = super::shell::shell_quote(relative);
    format!(
        r#"root=$PWD
command -v realpath >/dev/null 2>&1 || exit 120
target=$(realpath -- {safe} 2>/dev/null) || exit 122
case "$target" in
  "$root"|"$root"/*) : ;;
  *) exit 121 ;;
esac
"#
    )
}

fn search_command(read: &RemoteWorkspaceReadRequest) -> String {
    let pattern = super::shell::shell_quote(read.pattern.as_deref().unwrap_or(""));
    let relative = read.path.trim();
    let target = super::shell::shell_quote(if relative.is_empty() || relative == "." {
        "."
    } else {
        relative
    });
    let mode = read.result_mode.as_deref().unwrap_or("matches");
    let before = read.context_before.unwrap_or(0).min(20);
    let after = read.context_after.unwrap_or(0).min(20);
    let limit = read.limit.unwrap_or(50).clamp(1, 200);
    let mode_args = match mode {
        "matches" => {
            format!("--with-filename --null --line-number --no-heading -B {before} -A {after}")
        }
        "files_with_matches" => "--files-with-matches".to_string(),
        "count" => "--count --null".to_string(),
        _ => return "exit 2".to_string(),
    };
    let head = if mode == "matches" && (before > 0 || after > 0) {
        limit
            .saturating_add(1)
            .saturating_mul(before.saturating_add(after).saturating_add(2))
            .saturating_add(1)
    } else {
        limit.saturating_add(1)
    };
    let mut globs = String::new();
    for glob in read.include_globs.iter().flatten() {
        globs.push_str(" --glob ");
        globs.push_str(&super::shell::shell_quote(glob));
    }
    for glob in read.exclude_globs.iter().flatten() {
        globs.push_str(" --glob ");
        globs.push_str(&super::shell::shell_quote(&format!("!{glob}")));
    }
    for glob in protected_rg_globs() {
        globs.push_str(" --glob ");
        globs.push_str(&super::shell::shell_quote(glob));
    }
    let grep_excludes = protected_grep_excludes().join(" ");
    let requires_rg = mode != "matches"
        || read.include_globs.as_ref().is_some_and(|v| !v.is_empty())
        || read.exclude_globs.as_ref().is_some_and(|v| !v.is_empty());
    let fallback = if requires_rg {
        "printf '%s\\n' '{\"webcodex_search\":{\"backend\":\"grep\",\"feature_unavailable\":true}}'; exit 0".to_string()
    } else {
        format!(
            "status_file=${{TMPDIR:-/tmp}}/webcodex-search-$$; trap 'rm -f -- \"$status_file\"' EXIT HUP INT TERM; {{ grep -rnI --null {grep_excludes} -B {before} -A {after} -e {pattern} -- {target} 2>/dev/null; echo $? > \"$status_file\"; }} | head -n {head}; status=2; [ -f \"$status_file\" ] && read -r status < \"$status_file\"; printf '%s\\n' '{{\"webcodex_search\":{{\"backend\":\"grep\",\"feature_unavailable\":false}}}}'; exit \"$status\""
        )
    };
    format!(
        "if command -v rg >/dev/null 2>&1; then status_file=${{TMPDIR:-/tmp}}/webcodex-search-$$; trap 'rm -f -- \"$status_file\"' EXIT HUP INT TERM; {{ rg {mode_args} --color never --hidden --sort path{globs} -e {pattern} -- {target} 2>/dev/null; echo $? > \"$status_file\"; }} | head -n {head}; status=2; [ -f \"$status_file\" ] && read -r status < \"$status_file\"; printf '%s\\n' '{{\"webcodex_search\":{{\"backend\":\"rg\",\"feature_unavailable\":false}}}}'; exit \"$status\"; else {fallback}; fi"
    )
}

fn protected_rg_globs() -> &'static [&'static str] {
    webcodex_core::sensitive_paths::SEARCH_RG_EXCLUDE_GLOBS
}

fn protected_grep_excludes() -> &'static [&'static str] {
    webcodex_core::sensitive_paths::SEARCH_GREP_EXCLUDES
}

fn git_status_command() -> &'static str {
    "export GIT_PAGER=cat; git rev-parse --git-dir >/dev/null 2>&1 || exit 3; git --no-pager status --porcelain"
}

fn git_diff_summary_command() -> String {
    format!(
        "export GIT_PAGER=cat; git rev-parse --git-dir >/dev/null 2>&1 || exit 3; git status --porcelain; printf '\\n{}\\n'; git diff --stat",
        "@@WEBCODEX_DIFF_SUMMARY_SEP@@"
    )
}

fn git_diff_command(read: &RemoteWorkspaceReadRequest) -> String {
    let mut parts = vec!["export GIT_PAGER=cat; git rev-parse --git-dir >/dev/null 2>&1 || exit 3; git --no-pager diff".to_string()];
    if read.cached == Some(true) {
        parts.push("--cached".to_string());
    }
    if let Some(paths) = read.paths.as_deref() {
        if !paths.is_empty() {
            parts.push("--".to_string());
            for path in paths {
                parts.push(super::shell::shell_quote(path));
            }
        }
    }
    parts.join(" ")
}

fn git_diff_hunks_command(read: &RemoteWorkspaceReadRequest, _relative: &str) -> String {
    let mut parts = vec![
        "export GIT_PAGER=cat".to_string(),
        "git rev-parse --git-dir >/dev/null 2>&1 || exit 3; git --no-pager diff".to_string(),
    ];
    if read.cached == Some(true) {
        parts.push("--cached".to_string());
    }
    parts.push("--unified=80".to_string());
    if let Some(paths) = read.paths.as_deref() {
        if !paths.is_empty() {
            parts.push("--".to_string());
            parts.extend(paths.iter().map(|path| super::shell::shell_quote(path)));
        }
    }
    parts.join(" ")
}

fn git_log_command(read: &RemoteWorkspaceReadRequest) -> String {
    let limit = read
        .limit
        .map(|limit| limit.clamp(1, 100))
        .unwrap_or(20)
        .saturating_add(1);
    let skip = read.skip.unwrap_or(0).min(10_000);
    format!(
        "export GIT_PAGER=cat; git rev-parse --git-dir >/dev/null 2>&1 || exit 3; if git rev-parse --verify HEAD >/dev/null 2>&1; then git --no-pager log --decorate=short --date=iso-strict --pretty=format:'%H%x1f%h%x1f%D%x1f%an%x1f%ae%x1f%aI%x1f%s%x1e' -n {limit} --skip {skip}; else exit 0; fi"
    )
}

fn project_overview_limit(read: &RemoteWorkspaceReadRequest) -> usize {
    read.limit.map(|limit| limit.clamp(20, 500)).unwrap_or(200)
}

fn project_overview_depth(read: &RemoteWorkspaceReadRequest) -> usize {
    read.depth.map(|depth| depth.clamp(1, 4)).unwrap_or(2)
}

/// Run one prepared remote exec and capture bounded stdout/stderr. The
/// control socket is shared (reusing the authenticated transport); every
/// structured read uses its own exec channel. A transport failure invalidates
/// the pool entry for the next request; the just-submitted request is never
/// retried and never falls back to the local project.
fn run_single_remote_exec(
    command: &mut std::process::Command,
    key: &super::ssh::SshConnectionKey,
    pool: &SshConnectionPool,
    max_output_bytes: usize,
    timeout_secs: u64,
) -> CommandResult {
    use std::process::Stdio;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            return err_cmd(
                Instant::now(),
                "ssh_command_spawn_failed: command was not started".to_string(),
            )
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (stdout_tx, stdout_rx) = std::sync::mpsc::sync_channel(1);
    let (stderr_tx, stderr_rx) = std::sync::mpsc::sync_channel(1);
    if let Some(stdout) = stdout {
        std::thread::spawn(move || {
            let _ = stdout_tx.send(read_bounded_prefix(stdout, max_output_bytes));
        });
    } else {
        drop(stdout_tx);
    }
    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            let _ = stderr_tx.send(read_bounded_prefix(stderr, max_output_bytes));
        });
    } else {
        drop(stderr_tx);
    }
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed() >= std::time::Duration::from_secs(timeout_secs) {
                    let _ = terminate_child(&mut child);
                    let stderr = format!("command timed out after {timeout_secs} seconds");
                    let stdout = stdout_rx
                        .recv_timeout(std::time::Duration::from_millis(250))
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or_default();
                    return CommandResult {
                        exit_code: Some(-1),
                        stdout: Some(String::from_utf8_lossy(&stdout.0).into_owned()),
                        stderr: Some(stderr),
                        duration_ms: Some(start.elapsed().as_millis() as u64),
                        error: Some("command timed out".to_string()),
                    };
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => {
                let _ = terminate_child(&mut child);
                pool.invalidate_after_transport_failure(key);
                return err_cmd(
                    start,
                    "ssh_command_wait_failed: command may have started and was not retried"
                        .to_string(),
                );
            }
        }
    };
    let drain_deadline = std::time::Duration::from_secs(2);
    let stdout = stdout_rx
        .recv_timeout(drain_deadline)
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let stderr = stderr_rx
        .recv_timeout(drain_deadline)
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let exit_code = status.and_then(|status| status.code());
    if super::ssh::is_transport_failure(exit_code, Some(&String::from_utf8_lossy(&stderr.0))) {
        pool.invalidate_after_transport_failure(key);
    }
    let mut stderr_text = String::from_utf8_lossy(&stderr.0).into_owned();
    if stdout.1 {
        stderr_text.push_str(STDOUT_TRUNCATED_SENTINEL);
    }
    CommandResult {
        exit_code,
        stdout: Some(String::from_utf8_lossy(&stdout.0).into_owned()),
        stderr: Some(stderr_text),
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: None,
    }
}

fn terminate_child(child: &mut std::process::Child) -> Result<std::process::ExitStatus, String> {
    #[cfg(unix)]
    {
        let pid = child.id();
        if pid != 0 {
            // SAFETY: the SSH child creates its own process group before exec,
            // so this only signals the command started by this invocation.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Ok(None) => return Err("SSH child did not exit after termination".to_string()),
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn read_bounded_prefix(
    mut pipe: impl std::io::Read,
    max_bytes: usize,
) -> Result<(Vec<u8>, bool), String> {
    let mut output = Vec::with_capacity(max_bytes.min(64 * 1024));
    let mut chunk = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = pipe.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            return Ok((output, truncated));
        }
        let remaining = max_bytes.saturating_sub(output.len());
        if remaining > 0 {
            output.extend_from_slice(&chunk[..read.min(remaining)]);
        }
        if read > remaining {
            truncated = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_validation_rejects_absolute_uri_traversal_control_and_windows_forms() {
        for bad in [
            "/etc/passwd",
            "file:///etc/passwd",
            "../escape",
            "a/../../b",
            "a\0b",
            "a\tb",
            "C:\\Windows",
            "\\\\server\\share",
        ] {
            assert!(
                validate_remote_relative_path(bad).is_err(),
                "expected rejection: {bad:?}"
            );
        }
        assert_eq!(validate_remote_relative_path(".").unwrap(), ".");
        assert_eq!(
            validate_remote_relative_path("src/main.rs").unwrap(),
            "src/main.rs"
        );
        assert_eq!(
            validate_remote_relative_path("./src//lib.rs").unwrap(),
            "src/lib.rs"
        );
    }
}

/// Real-sshd integration tests. These build a Runner-local trap repository and
/// an SSH remote repository with the same file name but different content, then
/// verify the structured reads come from the remote workspace and never touch
/// the local trap repo. Skipped when `sshd` is unavailable.
#[cfg(all(test, unix))]
mod ssh_integration {
    use super::super::config::{AgentPolicy, SshConfig, SshResourceConfig};
    use super::super::ssh::SshConnectionPool;
    use super::{
        run_remote_workspace_read, RemoteWorkspaceReadOutcome, RemoteWorkspaceReadRequest,
        RemoteWorkspaceReadResponse, REMOTE_READ_ENVELOPE_MAX_BYTES, REMOTE_READ_FILE_MAX_BYTES,
        REMOTE_READ_MAX_OUTPUT_BYTES,
    };
    use sha2::Digest;
    use std::collections::BTreeMap;
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    const SESSION: &str = "wc_sess_ws_read";

    struct TestSshServer {
        _temp: tempfile::TempDir,
        child: Child,
        client_config: PathBuf,
        alias: String,
        remote_cwd: PathBuf,
    }

    impl Drop for TestSshServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn executable_on_path(name: &str) -> Option<PathBuf> {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(name))
                .find(|candidate| candidate.is_file())
        })
    }

    fn generate_key(path: &Path) {
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(path)
            .status()
            .expect("run ssh-keygen for test SSH daemon");
        assert!(status.success(), "ssh-keygen failed");
    }

    impl TestSshServer {
        fn start() -> Option<Self> {
            let sshd = executable_on_path("sshd")?;
            if Command::new(&sshd)
                .arg("-V")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_err()
            {
                return None;
            }
            let temp = tempfile::tempdir().expect("create SSH test directory");
            let remote_cwd = temp.path().join("remote");
            std::fs::create_dir(&remote_cwd).expect("create remote cwd");
            let host_key = temp.path().join("host_ed25519");
            let user_key = temp.path().join("user_ed25519");
            generate_key(&host_key);
            generate_key(&user_key);
            let authorized_keys = temp.path().join("authorized_keys");
            std::fs::copy(user_key.with_extension("pub"), &authorized_keys)
                .expect("copy test public key");
            let listener = TcpListener::bind("127.0.0.1:0").expect("reserve SSH test port");
            let port = listener.local_addr().expect("read SSH test port").port();
            drop(listener);
            let user = String::from_utf8(
                Command::new("id")
                    .args(["-un"])
                    .output()
                    .expect("resolve SSH test user")
                    .stdout,
            )
            .expect("SSH test user is UTF-8")
            .trim()
            .to_string();
            let server_config = temp.path().join("sshd_config");
            std::fs::write(
                &server_config,
                format!(
                    "ListenAddress 127.0.0.1\nPort {port}\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nChallengeResponseAuthentication no\nPubkeyAuthentication yes\nPermitRootLogin yes\nStrictModes no\nUsePAM no\nPrintMotd no\nLogLevel ERROR\n",
                    host_key.display(),
                    temp.path().join("sshd.pid").display(),
                    authorized_keys.display(),
                ),
            )
            .expect("write SSH daemon config");
            let alias = "webcodex-test-ssh".to_string();
            let client_config = temp.path().join("ssh_config");
            std::fs::write(
                &client_config,
                format!(
                    "Host {alias}\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n  IdentityFile {}\n  IdentitiesOnly yes\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n  GlobalKnownHostsFile /dev/null\n  LogLevel ERROR\n",
                    user_key.display(),
                ),
            )
            .expect("write SSH client config");
            let checked = Command::new(&sshd)
                .arg("-t")
                .arg("-f")
                .arg(&server_config)
                .output()
                .expect("validate SSH test daemon config");
            assert!(
                checked.status.success(),
                "invalid SSH test daemon config: {}",
                String::from_utf8_lossy(&checked.stderr)
            );
            let mut child = Command::new(&sshd)
                .arg("-D")
                .arg("-e")
                .arg("-f")
                .arg(&server_config)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start SSH test daemon");
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return Some(Self {
                        _temp: temp,
                        child,
                        client_config,
                        alias,
                        remote_cwd,
                    });
                }
                if let Some(status) = child.try_wait().expect("poll SSH test daemon") {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.stderr.take() {
                        use std::io::Read;
                        let _ = pipe.read_to_string(&mut stderr);
                    }
                    panic!("SSH test daemon exited early ({status}): {stderr}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!("SSH test daemon did not listen within five seconds");
        }
    }

    fn config_for(server: &TestSshServer) -> SshConfig {
        let mut resources = BTreeMap::new();
        resources.insert(
            "tmp".to_string(),
            SshResourceConfig {
                host: server.alias.clone(),
                default_cwd: Some(server.remote_cwd.to_string_lossy().into_owned()),
            },
        );
        resources.insert(
            "alt".to_string(),
            SshResourceConfig {
                host: server.alias.clone(),
                default_cwd: Some(server.remote_cwd.to_string_lossy().into_owned()),
            },
        );
        SshConfig { resources }
    }

    fn read_request(operation: &str, path: &str) -> RemoteWorkspaceReadRequest {
        RemoteWorkspaceReadRequest {
            operation: operation.to_string(),
            path: path.to_string(),
            pattern: None,
            include_globs: None,
            exclude_globs: None,
            result_mode: None,
            context_before: None,
            context_after: None,
            limit: None,
            offset: None,
            depth: None,
            start_line: None,
            end_line: None,
            with_line_numbers: None,
            max_bytes: None,
            cached: None,
            paths: None,
            skip: None,
            timeout_secs: 30,
        }
    }

    fn run_read(
        pool: &SshConnectionPool,
        config: &SshConfig,
        session_cwd: Option<&str>,
        read: &RemoteWorkspaceReadRequest,
    ) -> super::super::output::CommandResult {
        run_remote_workspace_read(
            pool,
            7,
            config,
            &AgentPolicy::default(),
            "tmp",
            SESSION,
            session_cwd,
            read,
        )
    }

    fn parsed_response(
        result: &super::super::output::CommandResult,
    ) -> RemoteWorkspaceReadResponse {
        serde_json::from_str(result.stdout.as_deref().expect("typed response stdout"))
            .expect("valid typed remote workspace response")
    }

    fn success_stdout(result: &super::super::output::CommandResult) -> String {
        match parsed_response(result).outcome {
            RemoteWorkspaceReadOutcome::Success { stdout, .. } => stdout,
            other => panic!("expected success outcome, got {other:?}"),
        }
    }

    fn git_init(path: &Path) {
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(path)
            .status()
            .expect("git init");
        assert!(status.success(), "git init failed");
    }

    fn git_commit_all(path: &Path, message: &str) {
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(path)
            .status()
            .expect("git add");
        Command::new("git")
            .args([
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=test",
                "commit",
                "-q",
                "-m",
                message,
            ])
            .current_dir(path)
            .status()
            .expect("git commit");
    }

    #[test]
    fn reads_come_from_remote_workspace_not_local_trap_repo() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = config_for(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        // Remote repo: same file name, different content than the local trap.
        std::fs::write(server.remote_cwd.join("README.md"), "remote README\n").unwrap();
        std::fs::write(server.remote_cwd.join("main.rs"), "fn remote_main() {}\n").unwrap();
        std::fs::create_dir(server.remote_cwd.join("src")).unwrap();
        std::fs::write(server.remote_cwd.join("src/lib.rs"), "pub fn remote() {}\n").unwrap();
        git_init(&server.remote_cwd);
        git_commit_all(&server.remote_cwd, "remote initial");

        // Runner-local trap repo with the SAME file names but different content.
        let trap = tempfile::tempdir().unwrap();
        std::fs::write(trap.path().join("README.md"), "TRAP README\n").unwrap();
        std::fs::write(trap.path().join("main.rs"), "fn trap_main() {}\n").unwrap();

        // read_file must return remote content.
        let read = read_request("read_file", "README.md");
        let result = run_read(
            &pool,
            &config,
            Some(&server.remote_cwd.to_string_lossy()),
            &read,
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert!(result.error.is_none(), "{result:?}");
        let stdout = result.stdout.as_deref().unwrap_or("");
        assert!(
            stdout.contains("remote README"),
            "remote read_file returned {stdout:?}"
        );
        assert!(
            !stdout.contains("TRAP"),
            "read_file leaked the local trap repo: {stdout:?}"
        );

        // list_project_files must list only remote files.
        let read = read_request("list_project_files", ".");
        let result = run_read(
            &pool,
            &config,
            Some(&server.remote_cwd.to_string_lossy()),
            &read,
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        let stdout = result.stdout.as_deref().unwrap_or("");
        assert!(stdout.contains("README.md"), "{stdout:?}");
        assert!(
            stdout.contains("src/"),
            "expected src/ dir entry: {stdout:?}"
        );

        // Git status comes from the remote repo.
        let read = read_request("git_status", ".");
        let result = run_read(
            &pool,
            &config,
            Some(&server.remote_cwd.to_string_lossy()),
            &read,
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        let stdout = result.stdout.as_deref().unwrap_or("");
        // Clean remote repo: no porcelain lines after the initial commit.
        assert!(
            !stdout.contains("README.md"),
            "git_status should be clean for remote: {stdout:?}"
        );

        // Git log shows the remote commit subject.
        let read = read_request("git_log", ".");
        let result = run_read(
            &pool,
            &config,
            Some(&server.remote_cwd.to_string_lossy()),
            &read,
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        let stdout = result.stdout.as_deref().unwrap_or("");
        assert!(
            stdout.contains("remote initial"),
            "git_log from remote repo: {stdout:?}"
        );

        // The local trap repo was never touched.
        assert!(
            !trap.path().join(".git").exists(),
            "remote reads must not git-init the local trap repo"
        );
        assert_eq!(
            std::fs::read_to_string(trap.path().join("README.md")).unwrap(),
            "TRAP README\n",
            "local trap repo content must be unchanged"
        );
    }

    #[test]
    fn session_cwd_wins_over_resource_default_and_login_default() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = config_for(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        // Session cwd: a nested dir under the resource default.
        let session_dir = server.remote_cwd.join("session-dir");
        std::fs::create_dir(&session_dir).unwrap();
        std::fs::write(session_dir.join("who.txt"), "session\n").unwrap();
        std::fs::write(server.remote_cwd.join("who.txt"), "resource\n").unwrap();

        // Session cwd present → read from session dir.
        let read = read_request("read_file", "who.txt");
        let result = run_read(&pool, &config, Some(&session_dir.to_string_lossy()), &read);
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert!(
            result
                .stdout
                .as_deref()
                .is_some_and(|out| out.contains("session")),
            "{result:?}"
        );

        // Session cwd absent → resource default_cwd is the root.
        let result = run_read(&pool, &config, None, &read);
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert!(
            result
                .stdout
                .as_deref()
                .is_some_and(|out| out.contains("resource")),
            "{result:?}"
        );
    }

    #[test]
    fn missing_root_fails_closed_and_bad_paths_are_rejected() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = config_for(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());

        // Nonexistent session cwd → root unavailable, fail closed.
        let read = read_request("read_file", "README.md");
        let missing = server.remote_cwd.join("does-not-exist");
        let result = run_read(&pool, &config, Some(&missing.to_string_lossy()), &read);
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert!(matches!(
            parsed_response(&result).outcome,
            RemoteWorkspaceReadOutcome::Failure { ref error_kind, command_started: false, .. }
                if error_kind == "ssh_workspace_root_unavailable"
        ));

        // Absolute and `..` paths rejected before any exec.
        for bad in ["/etc/passwd", "../escape", "a/../../b", "C:\\Windows"] {
            let read = read_request("read_file", bad);
            let result = run_read(
                &pool,
                &config,
                Some(&server.remote_cwd.to_string_lossy()),
                &read,
            );
            assert_eq!(
                result.exit_code,
                Some(0),
                "{bad:?} should return typed failure: {result:?}"
            );
            assert!(
                matches!(
                    parsed_response(&result).outcome,
                    RemoteWorkspaceReadOutcome::Failure { ref error_kind, command_started: false, .. }
                        if error_kind == "ssh_workspace_path_invalid"
                ),
                "{bad:?} → {result:?}"
            );
        }

        // Paths with spaces, single quotes, leading dash remain safe.
        let weird = server.remote_cwd.join("with space");
        std::fs::create_dir(&weird).unwrap();
        std::fs::write(weird.join("a'b-file -x"), "safe\n").unwrap();
        let read = read_request("read_file", "with space/a'b-file -x");
        let result = run_read(
            &pool,
            &config,
            Some(&server.remote_cwd.to_string_lossy()),
            &read,
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert!(
            result
                .stdout
                .as_deref()
                .is_some_and(|out| out.contains("safe")),
            "{result:?}"
        );
    }

    #[test]
    fn directory_symlinks_and_protected_search_fail_closed() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = config_for(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let outside = server._temp.path().join("outside-tree");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("outside.txt"), "OUTSIDE_TREE_MARKER\n").unwrap();
        std::os::unix::fs::symlink("../outside-tree", server.remote_cwd.join("relative-out"))
            .unwrap();
        std::os::unix::fs::symlink(&outside, server.remote_cwd.join("absolute-out")).unwrap();

        for operation in [
            "list_project_files",
            "project_overview",
            "search_project_text",
        ] {
            for path in ["relative-out", "absolute-out"] {
                let mut read = read_request(operation, path);
                if operation == "search_project_text" {
                    read.pattern = Some("OUTSIDE_TREE_MARKER".to_string());
                }
                let result = run_read(
                    &pool,
                    &config,
                    Some(&server.remote_cwd.to_string_lossy()),
                    &read,
                );
                assert!(
                    matches!(
                        parsed_response(&result).outcome,
                        RemoteWorkspaceReadOutcome::Failure { ref error_kind, exit_code: Some(121), .. }
                            if error_kind == "ssh_workspace_containment_denied"
                    ),
                    "{operation} {path}: {result:?}"
                );
                assert!(!result
                    .stdout
                    .as_deref()
                    .unwrap_or("")
                    .contains("OUTSIDE_TREE_MARKER"));
            }
        }

        std::fs::write(server.remote_cwd.join("inside.txt"), "inside remote\n").unwrap();
        std::os::unix::fs::symlink("inside.txt", server.remote_cwd.join("inside-link")).unwrap();
        let result = run_read(
            &pool,
            &config,
            Some(&server.remote_cwd.to_string_lossy()),
            &read_request("read_file", "inside-link"),
        );
        assert!(
            success_stdout(&result).contains("inside remote"),
            "{result:?}"
        );

        let marker = "PROTECTED_SEARCH_UNIQUE_MARKER";
        for (path, is_dir) in [
            (".env", false),
            (".env.local", false),
            ("private.pem", false),
            ("agent.toml", false),
            ("webcodex.env", false),
            ("secrets/token.txt", true),
            ("tokens/token.txt", true),
        ] {
            let target = server.remote_cwd.join(path);
            if is_dir {
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            }
            std::fs::write(target, format!("{marker}\n")).unwrap();
        }
        let mut search = read_request("search_project_text", ".");
        search.pattern = Some(marker.to_string());
        search.include_globs = Some(vec!["**/*".to_string(), "**/.env*".to_string()]);
        let result = run_read(
            &pool,
            &config,
            Some(&server.remote_cwd.to_string_lossy()),
            &search,
        );
        let output = success_stdout(&result);
        assert!(
            !output.contains(marker),
            "protected marker leaked: {output:?}"
        );
        for protected in [
            ".env",
            "secrets/",
            "tokens/",
            "private.pem",
            "agent.toml",
            "webcodex.env",
        ] {
            assert!(
                !output.contains(protected),
                "protected path leaked: {output:?}"
            );
        }
    }

    #[test]
    fn symlink_escaping_root_fails_closed() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = config_for(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        // Target genuinely OUTSIDE the remote workspace root.
        let outside = server._temp.path().join("outside-root-secret.txt");
        std::fs::write(&outside, "outside secret\n").unwrap();
        let link = server.remote_cwd.join("link.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let read = read_request("read_file", "link.txt");
        let result = run_read(
            &pool,
            &config,
            Some(&server.remote_cwd.to_string_lossy()),
            &read,
        );
        // The containment guard must reject the symlink escape before reading.
        let stdout = result.stdout.as_deref().unwrap_or("");
        assert!(
            !stdout.contains("outside secret"),
            "symlink read escaped the remote root: {result:?}"
        );
        assert_eq!(
            result.exit_code,
            Some(0),
            "typed failure must cross transport: {result:?}"
        );
        assert!(matches!(
            parsed_response(&result).outcome,
            RemoteWorkspaceReadOutcome::Failure { ref error_kind, exit_code: Some(121), .. }
                if error_kind == "ssh_workspace_containment_denied"
        ));
    }

    #[test]
    fn read_file_streaming_text_contract_covers_line_endings_and_unicode() {
        let Some(server) = TestSshServer::start() else {
            return;
        };
        let config = config_for(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let fixtures: &[(&str, &[u8], usize, usize, &str, usize)] = &[
            ("lf.txt", b"a\n\nb\n", 1, 10, "a\n\nb", 3),
            ("crlf.txt", b"a\r\n\r\nb\r\n", 1, 10, "a\n\nb", 3),
            ("no-final.txt", b"a\nb", 2, 5, "b", 2),
            ("past-end.txt", b"a\n", 9, 2, "", 1),
            ("unicode.txt", "猫\n雪".as_bytes(), 1, 2, "猫\n雪", 2),
        ];
        for (name, bytes, start, limit, expected, total) in fixtures {
            std::fs::write(server.remote_cwd.join(name), bytes).unwrap();
            let mut read = read_request("read_file", name);
            read.start_line = Some(*start);
            read.end_line = Some(start.saturating_add(*limit).saturating_sub(1));
            read.max_bytes = Some(REMOTE_READ_FILE_MAX_BYTES);
            let payload: serde_json::Value = serde_json::from_str(&success_stdout(&run_read(
                &pool,
                &config,
                Some(&server.remote_cwd.to_string_lossy()),
                &read,
            )))
            .unwrap();
            assert_eq!(payload["content"], *expected, "fixture {name}");
            assert_eq!(payload["total_lines"], *total, "fixture {name}");
            assert_eq!(payload["start_line"], *start, "fixture {name}");
            assert_eq!(payload["limit"], *limit, "fixture {name}");
            assert_eq!(
                payload["sha256"],
                format!("{:x}", sha2::Sha256::digest(bytes))
            );
        }
        for (name, bytes) in [
            ("nul.bin", b"a\0b".as_slice()),
            ("invalid.bin", &[0xff, 0xfe][..]),
        ] {
            std::fs::write(server.remote_cwd.join(name), bytes).unwrap();
            let response = parsed_response(&run_read(
                &pool,
                &config,
                Some(&server.remote_cwd.to_string_lossy()),
                &read_request("read_file", name),
            ));
            assert!(
                matches!(response.outcome, RemoteWorkspaceReadOutcome::Failure { ref error_kind, .. } if error_kind == "ssh_workspace_file_not_text")
            );
        }
    }

    #[test]
    fn empty_git_repository_returns_successful_empty_log() {
        let Some(server) = TestSshServer::start() else {
            return;
        };
        git_init(&server.remote_cwd);
        let config = config_for(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let response = parsed_response(&run_read(
            &pool,
            &config,
            Some(&server.remote_cwd.to_string_lossy()),
            &read_request("git_log", "."),
        ));
        assert!(
            matches!(response.outcome, RemoteWorkspaceReadOutcome::Success { exit_code: 0, ref stdout, stdout_truncated: false } if stdout.is_empty())
        );
    }

    #[test]
    fn large_git_diff_keeps_parseable_bounded_envelope_and_prefix() {
        let Some(server) = TestSshServer::start() else {
            return;
        };
        git_init(&server.remote_cwd);
        std::fs::write(server.remote_cwd.join("large.txt"), "base\n").unwrap();
        git_commit_all(&server.remote_cwd, "base");
        let secret_tail = "SHOULD_NOT_APPEAR_AFTER_PREFIX";
        let mut content = "x".repeat(REMOTE_READ_MAX_OUTPUT_BYTES * 2);
        content.push_str(secret_tail);
        std::fs::write(server.remote_cwd.join("large.txt"), content).unwrap();
        let config = config_for(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let result = run_read(
            &pool,
            &config,
            Some(&server.remote_cwd.to_string_lossy()),
            &read_request("git_diff", "."),
        );
        let raw = result.stdout.as_deref().unwrap();
        assert!(raw.len() < REMOTE_READ_ENVELOPE_MAX_BYTES);
        assert!(!raw.contains("[output truncated"));
        assert!(!raw.contains(secret_tail));
        assert!(matches!(
            parsed_response(&result).outcome,
            RemoteWorkspaceReadOutcome::Success {
                stdout_truncated: true,
                ..
            }
        ));
    }
}
