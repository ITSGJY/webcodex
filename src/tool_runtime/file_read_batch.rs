//! Bounded batch `read_file` input types and shared validation helpers.
//!
//! This module owns the single-file/batch input envelope so the JSON Schema, the
//! runtime parser (`ToolCall`), and the batch executor all agree on the mutual
//! exclusion rules and the item count bound. The executor itself lives in
//! `files.rs` (`ToolRuntime::read_file_batch`) and reuses the same single-range
//! helper as the single-file path.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Maximum number of ranges in one batch `read_file` call.
pub(crate) const MAX_BATCH_ITEMS: usize = 16;

/// Maximum concurrent in-flight single-range reads in a batch. Requests beyond
/// this are queued; output always returns in request order regardless of
/// completion order.
pub(crate) const BATCH_MAX_CONCURRENCY: usize = 4;

/// One item in a batch `read_file` request. `path` is required; `start_line`
/// and `limit` carry the same defaults/clamps as the single-file form.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ReadFileItem {
    pub path: String,
    #[serde(default)]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Validate a raw batch `items` array's shape and count before deserializing
/// into typed items. Mirrors the JSON Schema's `maxItems`/`minItems` so the
/// runtime never trusts the schema alone.
pub(crate) fn validate_batch_items_value(items: &Value) -> Result<(), String> {
    let Some(array) = items.as_array() else {
        return Err("read_file items must be an array".to_string());
    };
    if array.is_empty() {
        return Err("read_file items must contain at least one range".to_string());
    }
    if array.len() > MAX_BATCH_ITEMS {
        return Err(format!(
            "read_file items must contain at most {MAX_BATCH_ITEMS} ranges"
        ));
    }
    for (index, item) in array.iter().enumerate() {
        if !item
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|p| !p.is_empty())
        {
            return Err(format!(
                "read_file items[{index}] requires a non-empty path"
            ));
        }
    }
    Ok(())
}
