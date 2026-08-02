use serde_json::{json, Value};

use super::common::{
    array_schema, nullable_schema, permission_decision_schema, schema_type, search_match_schema,
    session_hint_schema, wrapped_output_schema,
};

pub(super) fn output_schema_for_tool(name: &str) -> Option<Value> {
    match name {
        "project_overview" => Some(wrapped_output_schema(vec![
            ("schema_version", schema_type("integer", "Overview schema version.")),
            ("project", schema_type("string", "Resolved runtime project id.")),
            ("path", schema_type("string", "Project-relative overview scope; empty means project root.")),
            ("deterministic", schema_type("boolean", "Always true; the overview uses deterministic path evidence only.")),
            ("project_types", array_schema(project_type_schema(), "Detected project types with project-relative evidence paths.")),
            ("manifests", array_schema(path_kind_schema("Detected build or package manifest."), "Detected manifests.")),
            ("key_files", array_schema(key_file_schema(), "Prioritized project entrypoints; metadata only.")),
            ("roots", roots_schema()),
            ("top_level", array_schema(top_level_entry_schema(), "Direct safe children of the requested path.")),
            ("suggested_next_reads", array_schema(suggested_read_schema(), "Bounded key-file subset recommended for later read_file calls.")),
            ("scan", scan_schema()),
            ("warnings", array_schema(schema_type("string", "Stable warning code."), "Bounded scan warning codes.")),
        ])),
        "list_project_files" => Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Resolved project id.")),
            (
                "path",
                schema_type("string", "Project-relative listed directory path."),
            ),
            (
                "entries",
                array_schema(
                    file_list_entry_schema(),
                    "Bounded project-relative file and directory entries.",
                ),
            ),
            (
                "truncated",
                schema_type(
                    "boolean",
                    "Whether more entries were available than returned.",
                ),
            ),
        ])),
        "list_project_tracked_files" => Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Resolved project id.")),
            (
                "path",
                schema_type("string", "Project-relative scope; empty means project root."),
            ),
            (
                "entries",
                array_schema(
                    tracked_list_entry_schema(),
                    "Tracked files, plus rolled-up directories carrying file_count.",
                ),
            ),
            ("returned", schema_type("integer", "Entries in this page.")),
            (
                "total_files",
                schema_type("integer", "Tracked files matching scope and globs, before rollup."),
            ),
            (
                "total_entries",
                schema_type("integer", "Entries after rollup, before paging."),
            ),
            (
                "depth",
                nullable_schema(
                    "integer",
                    "Effective rollup depth; null means every matching file is listed individually.",
                ),
            ),
            (
                "depth_auto",
                schema_type(
                    "boolean",
                    "True when depth was chosen automatically because the flat list exceeded limit.",
                ),
            ),
            (
                "truncated",
                schema_type("boolean", "Whether more entries remain on a later page."),
            ),
            (
                "next_offset",
                nullable_schema("integer", "Offset that continues the listing; null when complete."),
            ),
            (
                "list_truncated",
                schema_type(
                    "boolean",
                    "True when the raw index listing hit the transport cap, so total_files undercounts. Distinct from truncated, which is paging.",
                ),
            ),
            (
                "source",
                schema_type("string", "Listing source; git_index."),
            ),
            (
                "code",
                schema_type("string", "Stable structured error code on failure."),
            ),
            ("message", schema_type("string", "Structured failure message.")),
        ])),
        "read_file" => Some(read_file_output_schema()),
        "search_project_text" => {
            Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Resolved project id.")),
            ("pattern", schema_type("string", "Search pattern.")),
            (
                "path",
                schema_type("string", "Project-relative search root."),
            ),
            (
                "backend",
                nullable_schema(
                    "string",
                    "Search backend used: rg, grep, or native. Null/omitted when unknown (for example outer wait timeout before backend selection).",
                ),
            ),
            (
                "result_mode",
                json!({
                    "type": "string",
                    "enum": ["matches", "files_with_matches", "count"],
                    "description": "Effective result mode."
                }),
            ),
            (
                "effective_timeout_secs",
                schema_type("integer", "Effective clamped timeout in seconds."),
            ),
            (
                "matches",
                array_schema(
                    search_match_schema(),
                    "Bounded search matches; present in matches mode.",
                ),
            ),
            ("count", schema_type("integer", "Returned match count.")),
            (
                "files",
                array_schema(
                    search_file_result_schema(),
                    "Bounded file records for files_with_matches or count mode.",
                ),
            ),
            (
                "returned_file_count",
                schema_type("integer", "Number of returned file records."),
            ),
            (
                "returned_match_count",
                schema_type(
                    "integer",
                    "Sum of match_count values in returned count-mode file records.",
                ),
            ),
            (
                "count_complete",
                schema_type(
                    "boolean",
                    "True only when count mode completed without limit or transport truncation.",
                ),
            ),
            (
                "total_matches",
                nullable_schema(
                    "integer",
                    "Global matching-line total only when count_complete=true; otherwise null.",
                ),
            ),
            (
                "truncated",
                schema_type("boolean", "Whether more mode-specific records were available."),
            ),
            (
                "truncation_reason",
                json!({
                    "anyOf": [
                        {
                            "type": "string",
                            "enum": ["limit", "output_bytes", "timeout", "transport"],
                        },
                        { "type": "null" }
                    ],
                    "description": "Truncation reason: limit, output_bytes (the search byte budget cut the stream, complete records only), timeout (the effective timeout fired; records collected before it are complete), or transport; null when complete."
                }),
            ),
            (
                "exit_code",
                nullable_schema("integer", "Search command exit code, when available."),
            ),
            (
                "context_before",
                schema_type("integer", "Effective context lines before each match."),
            ),
            (
                "context_after",
                schema_type("integer", "Effective context lines after each match."),
            ),
            (
                "code",
                schema_type(
                    "string",
                    "Stable structured error code on validation, backend capability, execution, timeout, or request-drop failure.",
                ),
            ),
            (
                "field",
                schema_type(
                    "string",
                    "Input field name for invalid_search_request failures.",
                ),
            ),
            (
                "index",
                schema_type(
                    "integer",
                    "Optional zero-based index for invalid glob list entries.",
                ),
            ),
            (
                "reason",
                schema_type(
                    "string",
                    "Optional stable validation reason (empty, too_long, control_char, negated, protected_path, too_many, nul_byte, invalid_path).",
                ),
            ),
            (
                "requested_features",
                array_schema(
                    schema_type("string", "Requested advanced feature."),
                    "Advanced features that require ripgrep.",
                ),
            ),
            ("message", schema_type("string", "Structured failure message.")),
        ]))
        }
        _ => None,
    }
}

fn read_file_output_schema() -> Value {
    // The single-file and batch forms share the same success/failure property
    // shapes. The batch form adds a per-item envelope (index/path/success) on
    // top; Session/permission metadata is attached only to the outer output, so
    // item schemas intentionally omit it.
    let success_properties = json!({
        "text": schema_type("string", "The single primary text representation: plain content or numbered text according to format."),
        "format": {
            "type": "string",
            "enum": ["plain", "numbered"],
            "description": "Primary text format: plain or numbered."
        },
        "path": schema_type("string", "Project-relative path."),
        "sha256": {
            "type": "string",
            "pattern": "^[0-9a-f]{64}$",
            "description": "sha256 of the complete file, independent of the returned line range."
        },
        "start_line": {"type": "integer", "minimum": 1},
        "limit": {"type": "integer", "minimum": 1, "maximum": 2000},
        "total_lines": {"type": "integer", "minimum": 0},
        "returned_lines": {
            "type": "integer",
            "minimum": 0,
            "maximum": 2000,
            "description": "Returned source-line count. Runtime cursor construction guarantees this does not exceed limit."
        },
        "end_line": {
            "anyOf": [
                {"type": "integer", "minimum": 1},
                {"type": "null"}
            ]
        },
        "has_more": {"type": "boolean"},
        "next_start_line": {
            "anyOf": [
                {"type": "integer", "minimum": 1},
                {"type": "null"}
            ]
        },
        "session_recorded": schema_type("boolean", "True when this call was recorded in a provided session_id."),
        "session_id": schema_type("string", "Session id used for telemetry recording."),
        "session_event_id": schema_type("string", "Session event id for the recorded call."),
        "session_hint": session_hint_schema(),
        "permission": permission_decision_schema()
    });
    let failure_properties = json!({
        "error_kind": {"type": "string", "const": "read_file_failed"},
        "reason_code": {
            "type": "string",
            "enum": [
                "invalid_path", "sensitive_path", "not_found", "not_file",
                "permission_denied", "invalid_utf8", "range_too_large",
                "agent_unavailable", "timeout", "malformed_agent_response", "io_error"
            ]
        },
        "path": schema_type("string", "Project-relative input path."),
        "state_changed": {"type": "boolean", "const": false},
        "session_recorded": schema_type("boolean", "True when this call was recorded in a provided session_id."),
        "session_id": schema_type("string", "Session id used for telemetry recording."),
        "session_event_id": schema_type("string", "Session event id for the recorded call."),
        "session_hint": session_hint_schema(),
        "permission": permission_decision_schema()
    });
    let success_output = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": success_properties.clone(),
        "required": [
            "text", "format", "path", "sha256", "start_line", "limit",
            "total_lines", "returned_lines", "end_line", "has_more", "next_start_line"
        ]
    });
    let read_failure_output = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": failure_properties.clone(),
        "required": ["error_kind", "reason_code", "path", "state_changed"]
    });
    let failure_output = json!({
        "anyOf": [
            {"type": "null"},
            {
                "type": "object",
                "additionalProperties": true,
                "allOf": [
                    {
                        "if": {
                            "properties": {"error_kind": {"const": "read_file_failed"}},
                            "required": ["error_kind"]
                        },
                        "then": read_failure_output
                    }
                ]
            }
        ]
    });
    let batch_item_success = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "index": {"type": "integer", "minimum": 0},
            "path": schema_type("string", "Project-relative path."),
            "success": {"type": "boolean", "const": true},
            "output": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "text": schema_type("string", "The single primary text representation: plain content or numbered text according to format."),
                    "format": {"type": "string", "enum": ["plain", "numbered"]},
                    "path": schema_type("string", "Project-relative path."),
                    "sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 2000},
                    "total_lines": {"type": "integer", "minimum": 0},
                    "returned_lines": {"type": "integer", "minimum": 0, "maximum": 2000},
                    "end_line": {"anyOf": [{"type": "integer", "minimum": 1}, {"type": "null"}]},
                    "has_more": {"type": "boolean"},
                    "next_start_line": {"anyOf": [{"type": "integer", "minimum": 1}, {"type": "null"}]}
                },
                "required": ["text", "format", "path", "sha256", "start_line", "limit", "total_lines", "returned_lines", "end_line", "has_more", "next_start_line"]
            },
            "error": {"type": "null"}
        },
        "required": ["index", "path", "success", "output", "error"]
    });
    let batch_item_failure = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "index": {"type": "integer", "minimum": 0},
            "path": schema_type("string", "Project-relative path."),
            "success": {"type": "boolean", "const": false},
            "output": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "error_kind": {"type": "string", "const": "read_file_failed"},
                    "reason_code": {"type": "string", "enum": [
                        "invalid_path", "sensitive_path", "not_found", "not_file",
                        "permission_denied", "invalid_utf8", "range_too_large",
                        "agent_unavailable", "timeout", "malformed_agent_response", "io_error"
                    ]},
                    "path": schema_type("string", "Project-relative path."),
                    "state_changed": {"type": "boolean", "const": false}
                },
                "required": ["error_kind", "reason_code", "path", "state_changed"]
            },
            "error": schema_type("string", "Stable failure message.")
        },
        "required": ["index", "path", "success", "output", "error"]
    });
    let batch_output = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "mode": {"type": "string", "const": "batch"},
            "project": schema_type("string", "Resolved project id."),
            "requested_count": {"type": "integer", "minimum": 1},
            "returned_count": {"type": "integer", "minimum": 0},
            "succeeded_count": {"type": "integer", "minimum": 0},
            "failed_count": {"type": "integer", "minimum": 0},
            "items": {"type": "array", "items": {"anyOf": [batch_item_success, batch_item_failure]}},
            "output_truncated": {"type": "boolean"},
            "next_items": {
                "type": "array",
                "description": "Original requests that were not returned because the overall serialized batch budget was exhausted. Never partially serialized.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": schema_type("string", "Project-relative path."),
                        "start_line": {"anyOf": [{"type": "integer", "minimum": 1}, {"type": "null"}]},
                        "limit": {"anyOf": [{"type": "integer", "minimum": 1}, {"type": "null"}]}
                    },
                    "required": ["path"]
                }
            },
            "session_recorded": schema_type("boolean", "True when this call was recorded in a provided session_id."),
            "session_id": schema_type("string", "Session id used for telemetry recording."),
            "session_event_id": schema_type("string", "Session event id for the recorded call."),
            "session_hint": session_hint_schema(),
            "permission": permission_decision_schema()
        },
        "required": ["mode", "project", "requested_count", "returned_count", "succeeded_count", "failed_count", "items", "output_truncated", "next_items"]
    });
    // Superset property map for tooling that inspects `output` at the top
    // level (schema presence tests). The authoritative shape validation is the
    // `allOf` conditional below; this map only documents the union of possible
    // single-file success, batch, and failure fields.
    let mut discovery_properties = success_properties
        .as_object()
        .expect("read_file success properties")
        .clone();
    discovery_properties.extend(
        failure_properties
            .as_object()
            .expect("read_file failure properties")
            .clone(),
    );
    for batch_field in [
        "mode",
        "requested_count",
        "returned_count",
        "succeeded_count",
        "failed_count",
        "items",
        "output_truncated",
        "next_items",
    ] {
        discovery_properties.insert(batch_field.to_string(), json!({}));
    }
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "success": {"type": "boolean"},
            "output": {
                "properties": serde_json::Value::Object(discovery_properties),
                "anyOf": [
                    {"type": "object"},
                    {"type": "null"}
                ]
            },
            "error": {
                "anyOf": [
                    {"type": "string"},
                    {"type": "null"}
                ]
            }
        },
        "required": ["success", "output"],
        "allOf": [
            {
                "if": {
                    "properties": {"success": {"const": true}},
                    "required": ["success"]
                },
                "then": {
                    "properties": {
                        "output": {"anyOf": [success_output, batch_output]},
                        "error": {"type": "null"}
                    }
                },
                "else": {
                    "required": ["error"],
                    "properties": {
                        "output": failure_output,
                        "error": {"type": "string"}
                    }
                }
            }
        ]
    })
}

pub(super) fn project_type_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": schema_type("string", "Stable project type identifier."),
            "evidence": array_schema(schema_type("string", "Project-relative evidence path."), "Sorted evidence paths."),
            "evidence_total": schema_type("integer", "Real evidence path count before bounding."),
            "evidence_truncated": schema_type("boolean", "True when evidence was capped."),
        },
        "required": ["kind", "evidence"],
        "additionalProperties": false,
    })
}

pub(super) fn path_kind_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "path": schema_type("string", "Project-relative path."),
            "kind": schema_type("string", "Stable classification."),
        },
        "required": ["path", "kind"],
        "additionalProperties": false,
    })
}

pub(super) fn key_file_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": schema_type("string", "Project-relative key-file path."),
            "kind": schema_type("string", "Stable key-file classification."),
            "reason": schema_type("string", "Deterministic classification reason."),
        },
        "required": ["path", "kind", "reason"],
        "additionalProperties": false,
    })
}

pub(super) fn roots_schema() -> Value {
    let paths = || {
        array_schema(
            schema_type("string", "Project-relative conventional root."),
            "Sorted conventional roots.",
        )
    };
    json!({
        "type": "object",
        "properties": {
            "source": paths(),
            "tests": paths(),
            "docs": paths(),
            "examples": paths(),
            "scripts": paths(),
            "ci": paths(),
            "classification_basis": schema_type("string", "Classification basis; conventional_directory_name."),
        },
        "required": ["source", "tests", "docs", "examples", "scripts", "ci", "classification_basis"],
        "additionalProperties": false,
    })
}

pub(super) fn top_level_entry_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": schema_type("string", "Project-relative direct-child path."),
            "kind": {"type": "string", "enum": ["file", "directory"]},
        },
        "required": ["path", "kind"],
        "additionalProperties": false,
    })
}

pub(super) fn suggested_read_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": schema_type("string", "Project-relative path for a later read_file call."),
            "reason": schema_type("string", "Deterministic recommendation reason."),
        },
        "required": ["path", "reason"],
        "additionalProperties": false,
    })
}

pub(super) fn scan_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "max_depth": schema_type("integer", "Effective clamped maximum depth."),
            "limit": schema_type("integer", "Effective clamped entry limit."),
            "returned_entry_count": schema_type("integer", "Number of safe scanned entries used to construct the overview."),
            "truncated": schema_type("boolean", "Whether limit or depth bounded the scan."),
            "truncation_reason": nullable_schema("string", "limit, max_depth, limit_and_max_depth, or null."),
        },
        "required": ["max_depth", "limit", "returned_entry_count", "truncated", "truncation_reason"],
        "additionalProperties": false,
    })
}

fn search_file_result_schema() -> Value {
    json!({
        "type": "object",
        "description": "Unique project-relative matching file, with match_count in count mode.",
        "properties": {
            "path": schema_type("string", "Project-relative file path."),
            "match_count": schema_type("integer", "Matching-line count for this file in count mode."),
        },
        "required": ["path"],
        "additionalProperties": false,
    })
}

fn tracked_list_entry_schema() -> Value {
    json!({
        "type": "object",
        "description": "A tracked file, or a directory standing in for the files rolled up beneath it.",
        "properties": {
            "path": schema_type("string", "Project-relative path; rolled-up directories keep a trailing slash."),
            "kind": {
                "type": "string",
                "enum": ["file", "dir"],
                "description": "Entry kind."
            },
            "file_count": schema_type(
                "integer",
                "Tracked files beneath a rolled-up directory; absent for files.",
            ),
        },
        "required": ["path", "kind"],
        "additionalProperties": false
    })
}

fn file_list_entry_schema() -> Value {
    json!({
        "type": "object",
        "description": "One bounded file-list entry.",
        "properties": {
            "path": schema_type("string", "Project-relative file or directory path."),
            "kind": {
                "type": "string",
                "enum": ["file", "dir"],
                "description": "Entry kind."
            }
        },
        "required": ["path", "kind"],
        "additionalProperties": true
    })
}
