//! GPT Action response wrapper for the shared coding startup brief.
//!
//! Tool execution constructs the model-facing projection once. This transport
//! layer may wrap that exact value, but must never rebuild, trim, or rename its
//! core fields.

use crate::tool_runtime::startup_brief::startup_brief_from_output;
use crate::tool_runtime::ToolResult;
use serde_json::{json, Value};

const STARTUP_RESULT_METADATA_FIELDS: &[&str] = &[
    "session_recorded",
    "session_id",
    "session_event_id",
    "session_hint",
    "permission",
];

/// Optionally wrap a successful GPT Action startup result.
///
/// Errors and all non-startup tools remain byte-for-byte equivalent at the
/// `ToolResult` value level.
pub(crate) fn compact_action_tool_result(tool: &str, result: ToolResult) -> ToolResult {
    if !result.success || tool != "start_coding_task" {
        return result;
    }
    ToolResult {
        success: true,
        output: compact_start_coding_task_output(&result.output),
        error: None,
    }
}

/// Carry the already-built core brief behind the one Action-only wrapper.
pub(crate) fn compact_start_coding_task_output(output: &Value) -> Value {
    let mut startup_brief = startup_brief_from_output(output)
        .cloned()
        .unwrap_or_else(|| json!({}));
    if output.get("detail").and_then(Value::as_str) != Some("full") {
        if let Some(object) = startup_brief.as_object_mut() {
            for field in STARTUP_RESULT_METADATA_FIELDS {
                object.remove(*field);
            }
        }
    }

    let mut compact = json!({
        "compact": true,
        "startup_brief": startup_brief,
    });
    if let Some(object) = compact.as_object_mut() {
        for field in STARTUP_RESULT_METADATA_FIELDS {
            if let Some(value) = output.get(*field) {
                object.insert((*field).to_string(), value.clone());
            }
        }
    }
    compact
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_brief() -> Value {
        json!({
            "detail": "standard",
            "session": {
                "session_id": "wc_sess_test123",
                "mode": "normal",
                "continuation": "continued",
                "reused": true,
                "resume_requested": false,
                "current_binding": {"status": "bound", "reason_code": null},
                "explicit_session_id_required_for_continuity": false
            },
            "project": {
                "requested": "demo",
                "resolved_id": "agent:test:demo",
                "repository_identity": "repository:v1:abc",
                "canonical_repository_root_matches": true
            },
            "workspace": {
                "status": "clean",
                "git_available": true,
                "branch": "main",
                "head": "abc",
                "clean": true,
                "conflicts": 0,
                "modified": 0,
                "untracked": 0,
                "staged": 0,
                "ahead": null,
                "behind": null
            },
            "instructions": {
                "status": "reused",
                "sources": [{
                    "path": "AGENTS.md",
                    "fingerprint": "abc",
                    "truncated": false,
                    "headings": ["# Rules"],
                    "content": null,
                    "read_more": null
                }],
                "changed_sources": [],
                "content_included": false,
                "truncated": false,
                "total_chars": 10
            },
            "continuation": {
                "status": "available",
                "reason_code": null,
                "instruction": {"status": "available", "excerpt": "fix it", "truncated": false},
                "outcome": {"status": "in_progress", "reason_codes": []},
                "changed_paths": {"items": ["src/lib.rs"], "total": 1, "returned": 1, "truncated": false},
                "validation": {
                    "latest_status": "failed",
                    "open_failures": {"items": [], "total": 0, "returned": 0, "truncated": false},
                    "delta": {
                        "status": "unavailable",
                        "reason_code": "no_previous_validation",
                        "new_failures": {"items": [], "total": 0, "returned": 0, "truncated": false},
                        "resolved_failures": {"items": [], "total": 0, "returned": 0, "truncated": false},
                        "still_failing": {"items": [], "total": 0, "returned": 0, "truncated": false}
                    }
                },
                "jobs": {
                    "active_count": 0,
                    "blocking_active_count": 0,
                    "nonblocking_active_count": 0,
                    "recovering_count": 0,
                    "terminal_pending_count": 0,
                    "latest_status": "not_observed"
                },
                "open_guidance": {"count": 0, "risk_count": 0, "todo_count": 0, "latest_kind": null},
                "suggested_next_actions": {"items": ["fix failing test x"], "total": 1, "returned": 1, "truncated": false}
            },
            "semantic_navigation": {
                "status": "available",
                "available": true,
                "provider": "rust-analyzer",
                "capability": "lsp_read_only_navigation",
                "reason_code": null
            },
            "blockers": [],
            "warnings": [],
            "startup_verdict": {
                "status": "pass",
                "blocking": false,
                "suggested_next_actions": ["fix failing test x"]
            },
            "deterministic": true,
            "llm_summary": false
        })
    }

    #[test]
    fn compact_action_keeps_the_exact_standard_core() {
        let brief = sample_brief();
        let compact = compact_start_coding_task_output(&brief);
        assert_eq!(compact["compact"], true);
        assert_eq!(compact["startup_brief"], brief);
        assert_eq!(compact["startup_brief"]["instructions"]["status"], "reused");
        assert_eq!(
            compact["startup_brief"]["continuation"]["suggested_next_actions"]["items"][0],
            "fix failing test x"
        );
    }

    #[test]
    fn compact_action_extracts_the_same_core_from_full_diagnostics() {
        let brief = sample_brief();
        let full = json!({
            "detail": "full",
            "runtime_status": {"large": true},
            "connection_state": {"diagnostic": true},
            "startup_brief": brief,
        });
        let compact = compact_start_coding_task_output(&full);
        assert_eq!(compact["startup_brief"], sample_brief());
        assert!(compact.get("runtime_status").is_none());
        assert!(compact.get("connection_state").is_none());
    }

    #[test]
    fn compact_action_keeps_recorder_metadata_outside_the_shared_core() {
        let brief = sample_brief();
        let mut recorded = brief.clone();
        recorded["session_recorded"] = json!(true);
        recorded["session_id"] = json!("wc_sess_recorder");
        recorded["session_event_id"] = json!("evt_recorded");
        recorded["session_hint"] = json!({
            "has_open_messages": true,
            "open_counts": {"guidance": 1, "question": 0, "todo": 0, "risk": 0},
            "highest_priority": "normal",
            "suggested_next_tool": "session_discussion_summary"
        });

        let compact = compact_start_coding_task_output(&recorded);
        assert_eq!(compact["startup_brief"], brief);
        assert_eq!(compact["session_recorded"], true);
        assert_eq!(compact["session_id"], "wc_sess_recorder");
        assert_eq!(compact["session_event_id"], "evt_recorded");
        assert!(compact["session_hint"].is_object());
        assert!(compact["startup_brief"].get("session_recorded").is_none());
        assert!(compact["startup_brief"].get("session_hint").is_none());
    }

    #[test]
    fn compact_action_tool_result_preserves_errors_and_other_tools() {
        let error = ToolResult::err_with_output(
            "project not found",
            json!({"code": "unknown_project", "project": "missing"}),
        );
        let output = compact_action_tool_result("start_coding_task", error);
        assert!(!output.success);
        assert_eq!(output.error.as_deref(), Some("project not found"));
        assert_eq!(output.output["code"], "unknown_project");

        let other = ToolResult::ok(json!({"count": 2}));
        let output = compact_action_tool_result("list_tools", other);
        assert_eq!(output.output["count"], 2);
    }
}
