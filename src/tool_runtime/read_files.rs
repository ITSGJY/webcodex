//! Bounded multi-file reads built from the canonical single-file read core.

use super::project_resolution::ResolvedProject;
use super::{ReadFilesItem, ToolResult, ToolRuntime};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::Instant;
use webcodex_workspace::file_read_normalize::MODEL_RESULT_ENVELOPE_RESERVE_BYTES;
use webcodex_workspace::file_read_range::MAX_SERIALIZED_OUTPUT_BYTES;

pub(crate) const MAX_READ_FILES_ITEMS: usize = 8;
pub(crate) const MAX_READ_FILES_CONCURRENCY: usize = 4;
pub(crate) const DEFAULT_READ_FILES_DEADLINE: Duration = Duration::from_secs(30);

fn batch_output(
    project: &str,
    requested_count: usize,
    items: Vec<Value>,
    output_truncated: bool,
    next_index: Option<usize>,
) -> Value {
    let succeeded_count = items
        .iter()
        .filter(|item| item["success"].as_bool() == Some(true))
        .count();
    let returned_count = items.len();
    json!({
        "project": project,
        "requested_count": requested_count,
        "returned_count": returned_count,
        "succeeded_count": succeeded_count,
        "failed_count": returned_count - succeeded_count,
        "items": items,
        "output_truncated": output_truncated,
        "next_index": next_index,
    })
}

fn serialized_batch_fits(output: &Value) -> bool {
    serde_json::to_vec(&ToolResult::ok(output.clone()))
        .map(|bytes| {
            bytes.len()
                <= MAX_SERIALIZED_OUTPUT_BYTES.saturating_sub(MODEL_RESULT_ENVELOPE_RESERVE_BYTES)
        })
        .unwrap_or(false)
}

fn apply_output_budget(project: &str, requested_count: usize, completed: Vec<Value>) -> Value {
    let mut returned = Vec::with_capacity(completed.len());
    let mut next_index = None;

    for item in completed {
        let index = item["index"].as_u64().unwrap_or(returned.len() as u64) as usize;
        let mut candidate_items = returned.clone();
        candidate_items.push(item.clone());
        // `false` plus `null` is the largest final flag representation, so a
        // candidate that fits this shape also fits a truncated final shape.
        let candidate = batch_output(project, requested_count, candidate_items, false, None);
        if !serialized_batch_fits(&candidate) {
            next_index = Some(index);
            break;
        }
        returned.push(item);
    }

    let output_truncated = next_index.is_some();
    batch_output(
        project,
        requested_count,
        returned,
        output_truncated,
        next_index,
    )
}

impl ToolRuntime {
    pub(crate) async fn read_files(
        &self,
        project: String,
        items: Vec<ReadFilesItem>,
        with_line_numbers: Option<bool>,
    ) -> ToolResult {
        let resolved = match self.resolve_project_input(&project).await {
            Ok(project) => project,
            Err(error) => return ToolResult::err(error),
        };
        self.read_files_resolved(&resolved, items, with_line_numbers)
            .await
    }

    pub(crate) async fn read_files_resolved(
        &self,
        resolved: &ResolvedProject,
        items: Vec<ReadFilesItem>,
        with_line_numbers: Option<bool>,
    ) -> ToolResult {
        if !(1..=MAX_READ_FILES_ITEMS).contains(&items.len())
            || items.iter().any(|item| item.path.trim().is_empty())
        {
            return ToolResult::err("read_files requires 1 to 8 items with non-empty paths");
        }

        let runtime_project_id = resolved.resolved_id.clone();
        let requested_count = items.len();
        let with_line_numbers = with_line_numbers.unwrap_or(false);
        let deadline = Instant::now() + self.read_files_deadline;

        // The concurrency slot covers validation, enqueue, and response wait.
        // No request can reach the Runner until its future is polled by
        // `buffer_unordered`, so at most four file reads are actually in flight.
        let mut completed: Vec<Value> =
            stream::iter(items.into_iter().enumerate().map(|(index, item)| {
                let project = &resolved.config;
                async move {
                    let path = item.path;
                    let result = self
                        .read_one_resolved_project_file(
                            project,
                            path.clone(),
                            item.start_line,
                            item.limit,
                            with_line_numbers,
                            deadline,
                        )
                        .await;
                    json!({
                        "index": index,
                        "path": path,
                        "success": result.success,
                        "output": result.output,
                        "error": result.error,
                    })
                }
            }))
            .buffer_unordered(MAX_READ_FILES_CONCURRENCY)
            .collect()
            .await;
        completed.sort_by_key(|item| item["index"].as_u64().unwrap_or(u64::MAX));

        ToolResult::ok(apply_output_budget(
            &runtime_project_id,
            requested_count,
            completed,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_budget_keeps_whole_items_and_points_at_first_omitted_index() {
        let item = |index, text: String| {
            json!({
                "index": index,
                "path": format!("src/{index}.rs"),
                "success": true,
                "output": {
                    "text": text,
                    "format": "plain",
                    "path": format!("src/{index}.rs"),
                    "sha256": "a".repeat(64),
                    "start_line": 1,
                    "limit": 1,
                    "total_lines": 1,
                    "returned_lines": 1,
                    "end_line": 1,
                    "has_more": false,
                    "next_start_line": null
                },
                "error": null
            })
        };
        let output = apply_output_budget(
            "agent:oe:demo",
            2,
            vec![
                item(0, "x".repeat(140 * 1024)),
                item(1, "y".repeat(140 * 1024)),
            ],
        );
        assert_eq!(output["returned_count"], 1);
        assert_eq!(output["output_truncated"], true);
        assert_eq!(output["next_index"], 1);
        assert_eq!(output["items"].as_array().unwrap().len(), 1);
        let serialized = serde_json::to_vec(&ToolResult::ok(output)).unwrap();
        assert!(serialized.len() <= MAX_SERIALIZED_OUTPUT_BYTES);
    }

    #[test]
    fn output_budget_reserves_space_for_outer_session_metadata() {
        let item = |index, text: String| {
            json!({
                "index": index,
                "path": format!("src/{index}.rs"),
                "success": true,
                "output": {
                    "text": text,
                    "format": "plain",
                    "path": format!("src/{index}.rs"),
                    "sha256": "b".repeat(64),
                    "start_line": 1,
                    "limit": 1,
                    "total_lines": 1,
                    "returned_lines": 1,
                    "end_line": 1,
                    "has_more": false,
                    "next_start_line": null
                },
                "error": null
            })
        };
        let output = apply_output_budget(
            "agent:oe:demo",
            3,
            vec![
                item(0, "x".repeat(120 * 1024)),
                item(1, "y".repeat(120 * 1024)),
                item(2, "z".repeat(120 * 1024)),
            ],
        );
        assert_eq!(output["returned_count"], 2);
        assert_eq!(output["next_index"], 2);

        let mut result = ToolResult::ok(output);
        result.output["session_recorded"] = json!(true);
        result.output["session_id"] = json!(format!("wc_sess_{}", "s".repeat(64)));
        result.output["session_event_id"] = json!(format!("evt_{}", "e".repeat(64)));
        result.output["session_hint"] = json!({
            "has_open_messages": true,
            "open_counts": {
                "guidance": u64::MAX,
                "question": u64::MAX,
                "todo": u64::MAX,
                "risk": u64::MAX
            },
            "highest_priority": "high",
            "suggested_next_tool": "session_discussion_summary"
        });
        assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_SERIALIZED_OUTPUT_BYTES);
    }
}
