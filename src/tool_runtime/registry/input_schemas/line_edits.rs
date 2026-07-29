use serde_json::{json, Value};

use super::common::OPTIONAL_EXPLICIT_SESSION_ID_DESCRIPTION;

pub(crate) fn apply_text_edits_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "project": {
                "type": "string",
                "description": "Agent-registered project id."
            },
            "changes": {
                "type": "array",
                "minItems": 1,
                "maxItems": 16,
                "description": "Transactional list of 1..16 file changes. Existing files require expected_sha256; the whole batch is preflighted before mutation.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["edit", "create", "delete", "rename"],
                            "description": "File change kind."
                        },
                        "path": {
                            "type": "string",
                            "description": "Project-relative source or target path."
                        },
                        "to_path": {
                            "type": "string",
                            "description": "Project-relative destination path required by rename."
                        },
                        "content": {
                            "type": "string",
                            "description": "Complete UTF-8 content required by create."
                        },
                        "expected_sha256": {
                            "type": "string",
                            "pattern": "^[a-f0-9]{64}$",
                            "description": "Required current-file sha256 for edit, delete, and rename."
                        },
                        "edits": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 20,
                            "description": "Exact edits required by kind=edit.",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "kind": {
                                        "type": "string",
                                        "enum": ["replace_exact", "insert_after", "insert_before", "delete_exact"]
                                    },
                                    "old_text": { "type": "string" },
                                    "new_text": { "type": "string" },
                                    "anchor_text": { "type": "string" }
                                },
                                "required": ["kind"]
                            }
                        }
                    },
                    "required": ["kind", "path"]
                }
            },
            "dry_run": {
                "type": "boolean",
                "description": "If true, compute the plan without writing."
            },
            "session_id": {
                "type": "string",
                "description": OPTIONAL_EXPLICIT_SESSION_ID_DESCRIPTION
            }
        },
        "required": ["project", "changes"],
        "additionalProperties": false
    })
}
