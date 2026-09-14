use serde_json::{json, Value};

use super::super::tool_result::ToolResult;
use super::super::ToolRuntime;
use super::shared::is_git_object_hex;

const DEFAULT_GIT_LOG_LIMIT: usize = 20;
const MAX_GIT_LOG_LIMIT: usize = 100;
const MAX_GIT_LOG_SKIP: usize = 10_000;
const GIT_LOG_RECORD_SEP: char = '\u{1e}';
const GIT_LOG_UNIT_SEP: char = '\u{1f}';

pub(crate) fn normalize_git_log_limit(limit: Option<usize>) -> usize {
    limit
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_GIT_LOG_LIMIT)
        .min(MAX_GIT_LOG_LIMIT)
}

pub(crate) fn normalize_git_log_skip(skip: Option<usize>) -> usize {
    skip.unwrap_or(0).min(MAX_GIT_LOG_SKIP)
}

pub(crate) fn git_log_next_skip(
    skip: usize,
    returned_count: usize,
    truncated: bool,
) -> Option<usize> {
    if !truncated || returned_count == 0 {
        return None;
    }
    skip.checked_add(returned_count)
        .filter(|next| *next > skip && *next <= MAX_GIT_LOG_SKIP)
}

pub(crate) fn git_log_command(limit: usize, skip: usize) -> String {
    let limit_plus_one = limit.saturating_add(1);
    format!(
        "git log --decorate=short --date=iso-strict --pretty=format:'%H%x1f%h%x1f%D%x1f%an%x1f%ae%x1f%aI%x1f%s%x1e' -n {limit_plus_one} --skip {skip}",
    )
}

fn parse_git_log_refs(decorations: &str) -> Vec<String> {
    decorations
        .split(',')
        .flat_map(|part| {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                Vec::new()
            } else if let Some((head, branch)) = trimmed.split_once(" -> ") {
                vec![head.trim().to_string(), branch.trim().to_string()]
            } else if let Some(tag) = trimmed.strip_prefix("tag: ") {
                vec![tag.trim().to_string()]
            } else {
                vec![trimmed.to_string()]
            }
        })
        .collect()
}

pub(crate) fn parse_git_log_commits(
    stdout: &str,
    limit: usize,
) -> Result<(Vec<Value>, bool), &'static str> {
    // Offset continuation counts source records, so silently skipping a partial
    // record (including a retained-tail prefix) would invent a page boundary.
    if !stdout.trim_end_matches(['\n', '\r']).is_empty()
        && !stdout
            .trim_end_matches(['\n', '\r'])
            .ends_with(GIT_LOG_RECORD_SEP)
    {
        return Err("git log source ended inside a record; retry with a smaller limit");
    }
    let mut commits = Vec::new();
    let mut truncated = false;
    for record in stdout.split(GIT_LOG_RECORD_SEP) {
        let record = record.trim_matches(['\n', '\r']);
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.splitn(7, GIT_LOG_UNIT_SEP).collect();
        if fields.len() != 7 || !is_git_object_hex(fields[0]) {
            return Err("git log source is incomplete or malformed; retry with a smaller limit");
        }
        if commits.len() >= limit {
            truncated = true;
            break;
        }
        commits.push(json!({
            "hash": fields[0],
            "short_hash": fields[1],
            "subject": fields[6],
            "author_name": fields[3],
            "author_email": fields[4],
            "author_date": fields[5],
            "refs": parse_git_log_refs(fields[2]),
        }));
    }
    Ok((commits, truncated))
}

fn git_log_empty_repo(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("does not have any commits") || lower.contains("no commits yet")
}

impl ToolRuntime {
    pub(crate) async fn git_log(
        &self,
        project: String,
        limit: Option<usize>,
        skip: Option<usize>,
    ) -> ToolResult {
        let limit = normalize_git_log_limit(limit);
        let skip = normalize_git_log_skip(skip);
        let command = git_log_command(limit, skip);
        let output = match self
            .run_project_command_capture(&project, command, 30, None)
            .await
        {
            Ok(output) => output,
            Err(e) => return ToolResult::err(e),
        };
        let (commits, truncated) = match parse_git_log_commits(&output.stdout, limit) {
            Ok(page) => page,
            Err(error) => {
                return ToolResult::err_with_output(
                    error,
                    json!({
                        "project": project,
                        "error_kind": "source_incomplete",
                        "state_changed": false,
                    }),
                );
            }
        };
        let next_skip = git_log_next_skip(skip, commits.len(), truncated);
        let payload = json!({
            "project": project,
            "limit": limit,
            "skip": skip,
            "count": commits.len(),
            "truncated": truncated,
            "next_skip": next_skip,
            "commits": commits,
        });
        if output.exit_code == Some(0) || git_log_empty_repo(&output.stderr) {
            ToolResult::ok(payload)
        } else {
            ToolResult {
                success: false,
                output: json!({
                    "project": payload["project"],
                    "limit": payload["limit"],
                    "skip": payload["skip"],
                    "count": payload["count"],
                    "truncated": payload["truncated"],
                    "next_skip": payload["next_skip"],
                    "commits": payload["commits"],
                    "exit_code": output.exit_code,
                    "stderr": output.stderr,
                }),
                error: Some("git log failed".to_string()),
            }
        }
    }
}
