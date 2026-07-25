use crate::shell_protocol::{
    ProviderCallSummary, ShellAgentProjectSummary, ShellFileOpRequest, ShellRunRequest,
    ToolProvidersStatus,
};
use sha2::{Digest, Sha256};

const MAX_CLIENT_ID_LEN: usize = 80;
const MAX_CLIENT_FIELD_LEN: usize = 200;
/// Max length for `agent_instance_id`. A UUID v4 is 36 chars; allow headroom
/// for future formats but bound it so a malicious peer cannot stash huge
/// strings in the registry.
const MAX_AGENT_INSTANCE_ID_LEN: usize = 128;
pub(super) const MAX_COMMAND_LEN: usize = 8_000;
const MAX_CWD_LEN: usize = 1_024;
const MAX_FILE_PATH_LEN: usize = 2_048;
const MAX_FILE_CONTENT_BYTES: usize = 512 * 1024;
const MAX_STRUCTURED_EDIT_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
const MAX_ARTIFACT_PAYLOAD_BYTES: usize = 15 * 1024 * 1024;
const MAX_CHECKPOINT_PAYLOAD_BYTES: usize = 15 * 1024 * 1024;
pub(super) const MAX_RUN_STDIN_BYTES: usize = 15 * 1024 * 1024;
const MAX_SYNC_WAIT_SECS: u64 = 120;
const MAX_COMMAND_TIMEOUT_SECS: u64 = 24 * 60 * 60;
const MAX_PROVIDER_TEXT_CHARS: usize = 120;
const MAX_PROVIDER_TOOL_NAMES: usize = 64;

fn bounded_provider_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_PROVIDER_TEXT_CHARS)
        .collect()
}

fn safe_provider_identifier(value: &str) -> Option<String> {
    let value = bounded_provider_text(value);
    (!value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-.:".contains(character)))
    .then_some(value)
}

fn safe_provider_version(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, ' ' | '.' | '_' | '-' | '+' | '(' | ')')
        }))
    .then(|| bounded_provider_text(value))
}

/// Normalize untrusted provider metadata without making agent traffic fail.
/// Unknown fields are already discarded by serde; unknown enum-like values or
/// unsafe strings drop the entire optional update so tool completion continues.
pub(super) fn normalize_tool_providers(
    status: Option<ToolProvidersStatus>,
) -> Option<ToolProvidersStatus> {
    let mut status = status?;
    if !matches!(
        status.strategy.as_str(),
        "native" | "claude_code" | "claude_code_then_native"
    ) || !matches!(
        status.claude_code.process_state.as_str(),
        "not_started"
            | "starting"
            | "initializing"
            | "discovering"
            | "mapping"
            | "running"
            | "stopped"
    ) {
        return None;
    }
    status.claude_code.version = status
        .claude_code
        .version
        .as_deref()
        .and_then(safe_provider_version);
    let mut names = status
        .claude_code
        .discovered_tool_names
        .iter()
        .filter_map(|name| safe_provider_identifier(name))
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names.truncate(MAX_PROVIDER_TOOL_NAMES);
    status.claude_code.discovered_tool_names = names;
    if status.claude_code.capabilities.len() > 2
        || status.claude_code.capabilities.iter().any(|(name, state)| {
            !matches!(name.as_str(), "edit_file" | "search_project_text")
                || !matches!(state.as_str(), "available" | "unmapped" | "schema_mismatch")
        })
    {
        return None;
    }
    status.claude_code.last_error_code = status
        .claude_code
        .last_error_code
        .as_deref()
        .and_then(safe_provider_identifier);
    status.claude_code.last_call = status
        .claude_code
        .last_call
        .and_then(normalize_provider_call);
    Some(status)
}

fn normalize_provider_call(mut call: ProviderCallSummary) -> Option<ProviderCallSummary> {
    if !matches!(
        call.capability.as_str(),
        "edit_file" | "search_project_text"
    ) || !matches!(call.selected_provider.as_str(), "claude_code" | "native")
        || !matches!(call.result.as_str(), "success" | "failure")
        || !call.write_state.as_deref().map_or(true, |state| {
            matches!(state, "not_submitted" | "confirmed" | "uncertain")
        })
    {
        return None;
    }
    if (call.capability == "search_project_text" && call.write_state.is_some())
        || (call.capability == "edit_file" && call.write_state.is_none())
        || (call.fallback_used && call.selected_provider != "native")
    {
        return None;
    }
    call.duration_ms = call.duration_ms.min(24 * 60 * 60 * 1000);
    call.error_code = call
        .error_code
        .as_deref()
        .and_then(safe_provider_identifier);
    Some(call)
}

pub(super) fn validate_id(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_CLIENT_ID_LEN {
        return Err(format!(
            "{} must be 1..={} characters",
            field, MAX_CLIENT_ID_LEN
        ));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(format!(
            "{} may only contain ASCII letters, digits, '-', '_', and '.'",
            field
        ));
    }
    Ok(())
}

/// Validate `agent_instance_id`. It must be a non-empty, bounded ASCII string.
/// We accept the canonical UUID v4 format (`8-4-4-4-12` hex with dashes) and
/// also any short alphanumeric/dash string so future identity formats keep
/// working, but we reject empty / oversized / control-char values. This is not
/// a secret, so the value itself may appear in logs and `runtime_status`.
pub(super) fn validate_agent_instance_id(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("agent_instance_id must not be empty".to_string());
    }
    if value.len() > MAX_AGENT_INSTANCE_ID_LEN {
        return Err(format!(
            "agent_instance_id is too long; maximum is {} characters",
            MAX_AGENT_INSTANCE_ID_LEN
        ));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(
            "agent_instance_id may only contain ASCII letters, digits, '-', and '_'".to_string(),
        );
    }
    Ok(())
}

pub(super) fn validate_optional_field(value: &Option<String>, field: &str) -> Result<(), String> {
    if let Some(value) = value {
        if value.chars().count() > MAX_CLIENT_FIELD_LEN {
            return Err(format!(
                "{} is too long; maximum is {} characters",
                field, MAX_CLIENT_FIELD_LEN
            ));
        }
        if value.contains('\0') {
            return Err(format!("{} cannot contain NUL bytes", field));
        }
    }
    Ok(())
}

pub(super) fn validate_file_request(body: &ShellFileOpRequest) -> Result<(), String> {
    validate_id(&body.client_id, "client_id")?;
    match body.op.as_str() {
        "read"
        | "write"
        | "list"
        | "project_overview"
        | "replace_line_range"
        | "insert_at_line"
        | "delete_line_range"
        | "replace_exact_block"
        | "insert_before_pattern"
        | "insert_after_pattern"
        | "replace_in_file"
        | "write_project_file"
        | "apply_text_edits"
        | "save_project_artifact"
        | "read_project_artifact_metadata"
        | "read_project_artifact"
        | "artifact_upload_begin"
        | "artifact_upload_chunk"
        | "artifact_upload_finish"
        | "artifact_upload_abort"
        | "checkpoint_create"
        | "checkpoint_restore" => {}
        _ => {
            return Err(
                "op must be one of read, write, list, project_overview, replace_line_range, insert_at_line, delete_line_range, replace_exact_block, insert_before_pattern, insert_after_pattern, replace_in_file, write_project_file, apply_text_edits, save_project_artifact, read_project_artifact_metadata, read_project_artifact, artifact_upload_begin, artifact_upload_chunk, artifact_upload_finish, artifact_upload_abort, checkpoint_create, checkpoint_restore"
                    .to_string(),
            )
        }
    }
    let line_edit = matches!(
        body.op.as_str(),
        "replace_line_range" | "insert_at_line" | "delete_line_range"
    );
    let replace_exact_block = body.op == "replace_exact_block";
    let insert_pattern = matches!(
        body.op.as_str(),
        "insert_before_pattern" | "insert_after_pattern"
    );
    let anchor_edit = replace_exact_block || insert_pattern;
    let structured_edit_payload =
        matches!(body.op.as_str(), "replace_in_file" | "write_project_file");
    let artifact_payload = matches!(
        body.op.as_str(),
        "save_project_artifact"
            | "read_project_artifact_metadata"
            | "read_project_artifact"
            | "artifact_upload_begin"
            | "artifact_upload_chunk"
            | "artifact_upload_finish"
            | "artifact_upload_abort"
    );
    let checkpoint_payload = matches!(body.op.as_str(), "checkpoint_create" | "checkpoint_restore");

    let path = body.path.trim();
    if path.is_empty() {
        return Err("path cannot be empty".to_string());
    }
    if body.path.len() > MAX_FILE_PATH_LEN {
        return Err(format!(
            "path is too long; maximum is {} bytes",
            MAX_FILE_PATH_LEN
        ));
    }
    if body.path.contains('\0') {
        return Err("path cannot contain NUL bytes".to_string());
    }
    if let Some(cwd) = &body.cwd {
        if cwd.len() > MAX_CWD_LEN {
            return Err(format!("cwd is too long; maximum is {} bytes", MAX_CWD_LEN));
        }
        if cwd.contains('\0') {
            return Err("cwd cannot contain NUL bytes".to_string());
        }
    }

    validate_sha256(&body.expected_sha256)?;
    if body.expected_sha256.is_some() && body.op != "write" && !line_edit && !replace_exact_block {
        return Err(
            "expected_sha256 is only allowed for op=write, replace_exact_block, or line edit ops"
                .to_string(),
        );
    }
    if let Some(prefix) = &body.expected_prefix {
        if !line_edit {
            return Err("expected_prefix is only allowed for line edit ops".to_string());
        }
        if prefix.contains('\0') {
            return Err("expected_prefix cannot contain NUL bytes".to_string());
        }
    }
    if body.create_dirs && body.op != "write" {
        return Err("create_dirs is only allowed for op=write".to_string());
    }

    if let Some(content) = &body.content {
        let max_content_bytes = if artifact_payload {
            MAX_ARTIFACT_PAYLOAD_BYTES
        } else if checkpoint_payload {
            MAX_CHECKPOINT_PAYLOAD_BYTES
        } else if structured_edit_payload {
            MAX_STRUCTURED_EDIT_PAYLOAD_BYTES
        } else {
            MAX_FILE_CONTENT_BYTES
        };
        if content.len() > max_content_bytes {
            return Err(format!(
                "content is too large; maximum is {} bytes",
                max_content_bytes
            ));
        }
        if body.op != "write"
            && body.op != "project_overview"
            && body.op != "replace_line_range"
            && body.op != "insert_at_line"
            && body.op != "apply_text_edits"
            && !structured_edit_payload
            && !artifact_payload
            && !checkpoint_payload
            && !anchor_edit
        {
            return Err(
                "content is only allowed for op=write, project_overview options, line edit insert/replace, apply_text_edits, structured edit tools, artifact tools, checkpoint tools, or anchor edit tools"
                    .to_string(),
            );
        }
    }
    if let Some(old_text) = &body.old_text {
        if !replace_exact_block {
            return Err("old_text is only allowed for op=replace_exact_block".to_string());
        }
        if old_text.contains('\0') {
            return Err("old_text cannot contain NUL bytes".to_string());
        }
    }
    if let Some(pattern) = &body.pattern {
        if !insert_pattern {
            return Err("pattern is only allowed for insert pattern ops".to_string());
        }
        if pattern.contains('\0') {
            return Err("pattern cannot contain NUL bytes".to_string());
        }
    }

    if body.op == "write" && body.content.is_none() {
        return Err("content is required for op=write".to_string());
    }

    match body.op.as_str() {
        "read" => {
            match (body.start_line, body.end_line) {
                (Some(start), Some(end)) => {
                    if start == 0 || end < start {
                        return Err("invalid line range".to_string());
                    }
                }
                (Some(_), None) => {
                    return Err(
                        "end_line is required when start_line is set for op=read".to_string()
                    );
                }
                (None, Some(_)) => {
                    return Err(
                        "start_line is required when end_line is set for op=read".to_string()
                    );
                }
                (None, None) => {}
            }
            if body.line.is_some() {
                return Err("line is only allowed for op=insert_at_line".to_string());
            }
        }
        "replace_line_range" => {
            let start = body
                .start_line
                .ok_or_else(|| "start_line is required for op=replace_line_range".to_string())?;
            let end = body
                .end_line
                .ok_or_else(|| "end_line is required for op=replace_line_range".to_string())?;
            if start == 0 || end < start {
                return Err("invalid line range".to_string());
            }
            if body.line.is_some() {
                return Err("line is only allowed for op=insert_at_line".to_string());
            }
            if body.content.is_none() {
                return Err("content is required for op=replace_line_range".to_string());
            }
        }
        "delete_line_range" => {
            let start = body
                .start_line
                .ok_or_else(|| "start_line is required for op=delete_line_range".to_string())?;
            let end = body
                .end_line
                .ok_or_else(|| "end_line is required for op=delete_line_range".to_string())?;
            if start == 0 || end < start {
                return Err("invalid line range".to_string());
            }
            if body.line.is_some() || body.content.is_some() {
                return Err("delete_line_range only accepts start_line/end_line guards".to_string());
            }
        }
        "insert_at_line" => {
            let line = body
                .line
                .ok_or_else(|| "line is required for op=insert_at_line".to_string())?;
            if line == 0 {
                return Err("line out of range".to_string());
            }
            if body.start_line.is_some() || body.end_line.is_some() {
                return Err(
                    "start_line/end_line are only allowed for range line edit ops".to_string(),
                );
            }
            if body.content.is_none() {
                return Err("content is required for op=insert_at_line".to_string());
            }
        }
        "replace_exact_block" => {
            if body.old_text.as_deref().unwrap_or_default().is_empty() {
                return Err("old_text is required for op=replace_exact_block".to_string());
            }
            if body.content.is_none() {
                return Err("content is required for op=replace_exact_block".to_string());
            }
            if body.pattern.is_some()
                || body.expected_prefix.is_some()
                || body.start_line.is_some()
                || body.end_line.is_some()
                || body.line.is_some()
            {
                return Err(
                    "replace_exact_block only accepts old_text/content/expected_sha256 guards"
                        .to_string(),
                );
            }
        }
        "insert_before_pattern" | "insert_after_pattern" => {
            if body.pattern.as_deref().unwrap_or_default().is_empty() {
                return Err("pattern is required for insert pattern ops".to_string());
            }
            if body.content.as_deref().unwrap_or_default().is_empty() {
                return Err("content is required for insert pattern ops".to_string());
            }
            if body.old_text.is_some()
                || body.expected_sha256.is_some()
                || body.expected_prefix.is_some()
                || body.start_line.is_some()
                || body.end_line.is_some()
                || body.line.is_some()
            {
                return Err("insert pattern ops only accept pattern/content".to_string());
            }
        }
        "replace_in_file" | "write_project_file" => {
            if body.content.is_none() {
                return Err(format!("content is required for op={}", body.op));
            }
            if body.old_text.is_some()
                || body.pattern.is_some()
                || body.expected_sha256.is_some()
                || body.expected_prefix.is_some()
                || body.start_line.is_some()
                || body.end_line.is_some()
                || body.line.is_some()
                || body.max_bytes.is_some()
                || body.create_dirs
            {
                return Err(format!("{} only accepts path/content", body.op));
            }
        }
        "save_project_artifact"
        | "read_project_artifact_metadata"
        | "read_project_artifact"
        | "artifact_upload_begin"
        | "artifact_upload_chunk"
        | "artifact_upload_finish"
        | "artifact_upload_abort" => {
            if body.content.is_none() {
                return Err(format!("content is required for op={}", body.op));
            }
            if body.old_text.is_some()
                || body.pattern.is_some()
                || body.expected_sha256.is_some()
                || body.expected_prefix.is_some()
                || body.start_line.is_some()
                || body.end_line.is_some()
                || body.line.is_some()
                || body.max_bytes.is_some()
                || body.create_dirs
            {
                return Err(format!("{} only accepts path/content", body.op));
            }
        }
        "checkpoint_create" | "checkpoint_restore" => {
            if body.content.is_none() {
                return Err(format!("content is required for op={}", body.op));
            }
            if body.old_text.is_some()
                || body.pattern.is_some()
                || body.expected_sha256.is_some()
                || body.expected_prefix.is_some()
                || body.start_line.is_some()
                || body.end_line.is_some()
                || body.line.is_some()
                || body.max_bytes.is_some()
                || body.create_dirs
            {
                return Err("checkpoint ops only accept path/cwd/content".to_string());
            }
        }
        _ => {
            if body.expected_prefix.is_some()
                || body.start_line.is_some()
                || body.end_line.is_some()
                || body.line.is_some()
            {
                return Err("line edit fields are only allowed for line edit ops".to_string());
            }
            if body.old_text.is_some() || body.pattern.is_some() {
                return Err("anchor edit fields are only allowed for anchor edit ops".to_string());
            }
        }
    }
    if body.wait_timeout_secs > MAX_SYNC_WAIT_SECS {
        return Err(format!(
            "wait_timeout_secs must be <= {} for shellFileOp",
            MAX_SYNC_WAIT_SECS
        ));
    }
    Ok(())
}

pub(super) fn validate_run_request(body: &ShellRunRequest) -> Result<(), String> {
    validate_id(&body.client_id, "client_id")?;
    let command = body.command.trim();
    if command.is_empty() {
        return Err("command cannot be empty".to_string());
    }
    if body.command.len() > MAX_COMMAND_LEN {
        return Err(format!(
            "command is too long; maximum is {} bytes",
            MAX_COMMAND_LEN
        ));
    }
    if body.command.contains('\0') {
        return Err("command cannot contain NUL bytes".to_string());
    }
    if let Some(stdin) = &body.stdin {
        if stdin.len() > MAX_RUN_STDIN_BYTES {
            return Err(format!(
                "stdin is too large; maximum is {} bytes",
                MAX_RUN_STDIN_BYTES
            ));
        }
        if stdin.contains('\0') {
            return Err("stdin cannot contain NUL bytes".to_string());
        }
    }
    if let Some(cwd) = &body.cwd {
        if cwd.len() > MAX_CWD_LEN {
            return Err(format!("cwd is too long; maximum is {} bytes", MAX_CWD_LEN));
        }
        if cwd.contains('\0') {
            return Err("cwd cannot contain NUL bytes".to_string());
        }
    }
    if body.timeout_secs == 0 || body.timeout_secs > MAX_COMMAND_TIMEOUT_SECS {
        return Err(format!(
            "timeout_secs must be between 1 and {}",
            MAX_COMMAND_TIMEOUT_SECS
        ));
    }
    if body.wait_timeout_secs > MAX_SYNC_WAIT_SECS {
        return Err(format!(
            "wait_timeout_secs must be <= {} for synchronous runShell",
            MAX_SYNC_WAIT_SECS
        ));
    }
    Ok(())
}

pub(super) fn trim_string(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub(super) fn normalize_project_summaries(
    projects: Option<Vec<ShellAgentProjectSummary>>,
) -> Vec<ShellAgentProjectSummary> {
    let mut projects = projects.unwrap_or_default();
    projects.sort_by(|a, b| a.id.cmp(&b.id));
    projects.dedup_by(|a, b| a.id == b.id);
    projects
}

pub(super) fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn validate_sha256(value: &Option<String>) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.len() != 64 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("expected_sha256 must be 64 hex characters".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod provider_status_tests {
    use super::*;
    use crate::shell_protocol::ClaudeCodeProviderStatus;
    use std::collections::BTreeMap;

    fn provider_status() -> ToolProvidersStatus {
        ToolProvidersStatus {
            strategy: "claude_code_then_native".to_string(),
            claude_code: ClaudeCodeProviderStatus {
                enabled: true,
                version: Some("2.1.217".to_string()),
                available: true,
                process_state: "running".to_string(),
                discovered_tool_names: (0..100).map(|index| format!("Tool_{index}")).collect(),
                capabilities: BTreeMap::from([
                    ("edit_file".to_string(), "available".to_string()),
                    ("search_project_text".to_string(), "unmapped".to_string()),
                ]),
                last_error_code: None,
                last_call: Some(ProviderCallSummary {
                    capability: "edit_file".to_string(),
                    selected_provider: "claude_code".to_string(),
                    fallback_used: false,
                    result: "success".to_string(),
                    write_state: Some("confirmed".to_string()),
                    duration_ms: u64::MAX,
                    error_code: None,
                }),
            },
        }
    }

    #[test]
    fn provider_status_is_bounded_and_rejects_path_like_version() {
        let mut status = provider_status();
        status.claude_code.version = Some("/tmp/private/project".to_string());
        status
            .claude_code
            .discovered_tool_names
            .push("/tmp/private/Edit".to_string());
        let status = normalize_tool_providers(Some(status)).unwrap();
        assert_eq!(status.claude_code.version, None);
        assert_eq!(status.claude_code.discovered_tool_names.len(), 64);
        assert!(status
            .claude_code
            .discovered_tool_names
            .iter()
            .all(|name| name.chars().count() <= MAX_PROVIDER_TEXT_CHARS));
        assert_eq!(
            status.claude_code.last_call.as_ref().unwrap().duration_ms,
            24 * 60 * 60 * 1000
        );
        let serialized = serde_json::to_string(&status).unwrap();
        for forbidden in ["/tmp/private", "stderr", "environment", "token", "cookie"] {
            assert!(!serialized.contains(forbidden));
        }
    }

    #[test]
    fn unknown_provider_state_is_ignored_without_error() {
        let mut status = provider_status();
        status.claude_code.process_state = "raw stderr follows".to_string();
        assert!(normalize_tool_providers(Some(status)).is_none());
    }
}
