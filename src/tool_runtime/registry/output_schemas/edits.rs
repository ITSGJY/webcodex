use serde_json::Value;

use super::common::{
    array_schema, nullable_schema, open_object_schema, schema_type, wrapped_output_schema,
};

pub(super) fn output_schema_for_tool(name: &str) -> Option<Value> {
    match name {
        "apply_patch" | "apply_patch_checked" => Some(wrapped_output_schema(vec![
            (
                "exit_code",
                nullable_schema("integer", "Patch command exit code."),
            ),
            ("stdout", schema_type("string", "Patch command stdout.")),
            ("stderr", schema_type("string", "Patch command stderr.")),
            (
                "changed_files",
                array_schema(
                    open_object_schema("Changed file summary."),
                    "Changed files.",
                ),
            ),
            (
                "applied",
                schema_type("boolean", "Whether the patch was applied."),
            ),
            (
                "check",
                open_object_schema("Patch validation/check result."),
            ),
        ])),
        "validate_patch" => Some(wrapped_output_schema(vec![
            (
                "valid",
                schema_type("boolean", "Whether the patch passed validation."),
            ),
            (
                "applies",
                schema_type("boolean", "Whether git apply --check succeeded."),
            ),
            (
                "exit_code",
                nullable_schema("integer", "Validation command exit code."),
            ),
            ("stdout", schema_type("string", "Validation stdout.")),
            ("stderr", schema_type("string", "Validation stderr.")),
            (
                "diff_stat",
                schema_type("string", "Patch diff stat, when available."),
            ),
        ])),
        "write_project_file" => Some(wrapped_output_schema(vec![
            (
                "path",
                nullable_schema("string", "Project-relative path reported by the agent; null only when the agent could not parse the request payload."),
            ),
            (
                "created",
                schema_type("boolean", "True when the whole-file write created a new file."),
            ),
            (
                "overwritten",
                schema_type("boolean", "True when the whole-file write replaced an existing file."),
            ),
            (
                "bytes_written",
                schema_type("integer", "Bytes written to the final file. Result metadata only; does not include file content, is not a shell-execution interface, and does not expose environment, token, or secret values."),
            ),
            (
                "sha256",
                nullable_schema("string", "sha256 of the written file, current file on sha guard mismatch, or null when unavailable."),
            ),
            (
                "warning",
                nullable_schema("string", "Whole-file write safety warning, such as an unguarded overwrite warning; null otherwise."),
            ),
            (
                "error",
                schema_type("string", "Agent-side whole-file write rejection message, when unsuccessful."),
            ),
        ])),
        "apply_text_edits" => Some(wrapped_output_schema(vec![
            (
                "dry_run",
                schema_type("boolean", "Whether this was a dry-run (no write)."),
            ),
            (
                "applied_count",
                schema_type("integer", "Number of file changes applied in the batch."),
            ),
            (
                "changed",
                schema_type("boolean", "Whether the worktree was changed."),
            ),
            (
                "would_change",
                schema_type("boolean", "Whether the batch plan changes the worktree."),
            ),
            (
                "files",
                schema_type(
                    "array",
                    "Per-file summaries with kind, paths, changed state, and old/new sha256 values.",
                ),
            ),
            (
                "changed_paths",
                schema_type("array", "Paths touched by the edit batch."),
            ),
        ])),
        _ => None,
    }
}
