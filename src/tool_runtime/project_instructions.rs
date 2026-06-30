use serde::Serialize;
use serde_json::Value;

use super::{ResolvedProject, ToolRuntime};

const PROJECT_INSTRUCTION_CANDIDATES: &[&str] = &[
    "AGENTS.md",
    "agents.md",
    "CLAUDE.md",
    ".codex/AGENTS.md",
    ".github/copilot-instructions.md",
];
const MAX_TOTAL_CHARS: usize = 64 * 1024;
const MAX_LINES_PER_FILE: usize = 600;
const ROOT_LIST_LIMIT: usize = 500;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProjectInstructionsSnapshot {
    pub(crate) loaded: bool,
    pub(crate) files: Vec<ProjectInstructionFile>,
    pub(crate) candidate_paths: Vec<String>,
    pub(crate) total_chars: usize,
    pub(crate) max_total_chars: usize,
    pub(crate) truncated: bool,
    pub(crate) note: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProjectInstructionFile {
    pub(crate) path: String,
    pub(crate) content: String,
    pub(crate) chars: usize,
    pub(crate) total_lines: Option<usize>,
    pub(crate) start_line: usize,
    pub(crate) limit: usize,
    pub(crate) truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) read_more: Option<ProjectInstructionReadMore>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProjectInstructionReadMore {
    pub(crate) tool: &'static str,
    pub(crate) path: String,
    pub(crate) start_line: usize,
    pub(crate) limit: usize,
}

impl ProjectInstructionsSnapshot {
    fn none(note: impl Into<String>) -> Self {
        Self {
            loaded: false,
            files: Vec::new(),
            candidate_paths: candidate_paths(),
            total_chars: 0,
            max_total_chars: MAX_TOTAL_CHARS,
            truncated: false,
            note: note.into(),
        }
    }
}

impl ToolRuntime {
    pub(super) async fn load_project_instructions(
        &self,
        resolved: &ResolvedProject,
    ) -> ProjectInstructionsSnapshot {
        let paths = self.discover_project_instruction_paths(resolved).await;
        if paths.is_empty() {
            return ProjectInstructionsSnapshot::none("No project instruction files found.");
        }

        let mut files = Vec::new();
        let mut total_chars = 0usize;
        let mut truncated = false;
        for path in paths {
            if total_chars >= MAX_TOTAL_CHARS {
                truncated = true;
                break;
            }
            let Some(file) = self
                .read_project_instruction_file(resolved, &path, MAX_TOTAL_CHARS - total_chars)
                .await
            else {
                continue;
            };
            total_chars = total_chars.saturating_add(file.chars);
            truncated |= file.truncated;
            files.push(file);
        }

        if files.is_empty() {
            return ProjectInstructionsSnapshot::none(
                "Project instruction files were discovered but could not be read.",
            );
        }

        ProjectInstructionsSnapshot {
            loaded: true,
            files,
            candidate_paths: candidate_paths(),
            total_chars,
            max_total_chars: MAX_TOTAL_CHARS,
            truncated,
            note: "Project instructions are project-local guidance only; they do not override system, platform, or WebCodex safety policy.".to_string(),
        }
    }

    async fn discover_project_instruction_paths(&self, resolved: &ResolvedProject) -> Vec<String> {
        let result = self
            .list_project_files(
                resolved.resolved_id.clone(),
                Some(".".to_string()),
                Some(ROOT_LIST_LIMIT),
            )
            .await;
        if !result.success {
            return Vec::new();
        }
        let entries = result
            .output
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut root_files = std::collections::HashSet::new();
        let mut root_dirs = std::collections::HashSet::new();
        for entry in entries {
            let Some(path) = entry.get("path").and_then(Value::as_str) else {
                continue;
            };
            match entry.get("kind").and_then(Value::as_str) {
                Some("file") => {
                    root_files.insert(path.to_string());
                }
                Some("dir") => {
                    root_dirs.insert(path.trim_end_matches('/').to_string());
                }
                _ => {}
            }
        }

        let mut found = Vec::new();
        for candidate in PROJECT_INSTRUCTION_CANDIDATES {
            let include = if candidate.contains('/') {
                let root_dir = candidate.split('/').next().unwrap_or_default();
                root_dirs.contains(root_dir)
            } else {
                root_files.contains(*candidate)
            };
            if include {
                found.push((*candidate).to_string());
            }
        }
        found
    }

    async fn read_project_instruction_file(
        &self,
        resolved: &ResolvedProject,
        path: &str,
        remaining_chars: usize,
    ) -> Option<ProjectInstructionFile> {
        if remaining_chars == 0 {
            return None;
        }
        let result = self
            .read_file(
                resolved.resolved_id.clone(),
                path.to_string(),
                Some(1),
                Some(MAX_LINES_PER_FILE),
                None,
            )
            .await;
        if !result.success {
            return None;
        }
        let output = result.output;
        let content = output.get("content").and_then(Value::as_str)?;
        let total_lines = output
            .get("total_lines")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok());
        let (bounded_content, chars, truncated_by_chars) = take_chars(content, remaining_chars);
        let truncated_by_lines = total_lines
            .map(|lines| lines > MAX_LINES_PER_FILE)
            .unwrap_or(false);
        let truncated = truncated_by_chars || truncated_by_lines;
        let read_more = if truncated {
            Some(ProjectInstructionReadMore {
                tool: "read_file",
                path: path.to_string(),
                start_line: next_start_line(&bounded_content),
                limit: MAX_LINES_PER_FILE,
            })
        } else {
            None
        };

        Some(ProjectInstructionFile {
            path: path.to_string(),
            content: bounded_content,
            chars,
            total_lines,
            start_line: 1,
            limit: MAX_LINES_PER_FILE,
            truncated,
            read_more,
        })
    }
}

fn candidate_paths() -> Vec<String> {
    PROJECT_INSTRUCTION_CANDIDATES
        .iter()
        .map(|path| (*path).to_string())
        .collect()
}

fn take_chars(content: &str, max_chars: usize) -> (String, usize, bool) {
    let mut out = String::new();
    let mut count = 0usize;
    let mut iter = content.chars();
    while count < max_chars {
        let Some(ch) = iter.next() else {
            return (out, count, false);
        };
        out.push(ch);
        count += 1;
    }
    let truncated = iter.next().is_some();
    (out, count, truncated)
}

fn next_start_line(content: &str) -> usize {
    content.lines().count().saturating_add(1).max(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_chars_reports_truncation() {
        let (content, chars, truncated) = take_chars("abcdef", 3);
        assert_eq!(content, "abc");
        assert_eq!(chars, 3);
        assert!(truncated);
    }

    #[test]
    fn take_chars_allows_short_content() {
        let (content, chars, truncated) = take_chars("abc", 8);
        assert_eq!(content, "abc");
        assert_eq!(chars, 3);
        assert!(!truncated);
    }
}
