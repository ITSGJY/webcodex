//! Shared input types used by runtime tool calls.

use serde::{Deserialize, Serialize};

pub use webcodex_core::workflow_session_contract::{ExecutionShell, SessionMode};

/// Serde default helper: `true`. Used by `ToolCall` variants whose `allow_patch`
/// field defaults to true (matching the Runner-side Project TOML parser).
pub fn default_true() -> bool {
    true
}

/// Declared intent for a shell/job execution. This is evidence metadata, not
/// an authorization or command-selection policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPurpose {
    Validation,
    Test,
    Build,
    Format,
    Release,
    Diagnostic,
    Operation,
    #[default]
    Other,
}

impl ExecutionPurpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Validation => "validation",
            Self::Test => "test",
            Self::Build => "build",
            Self::Format => "format",
            Self::Release => "release",
            Self::Diagnostic => "diagnostic",
            Self::Operation => "operation",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupDetail {
    Minimal,
    #[default]
    Standard,
    Full,
}

impl StartupDetail {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Standard => "standard",
            Self::Full => "full",
        }
    }
}

#[cfg(feature = "workspace-checkpoints")]
pub use webcodex_core::runtime_contract::{
    CHECKPOINT_KIND_VALUES, CHECKPOINT_VALIDATION_STATUS_VALUES,
};

#[cfg(feature = "workspace-checkpoints")]
pub fn is_checkpoint_kind(value: &str) -> bool {
    CHECKPOINT_KIND_VALUES.contains(&value)
}

#[cfg(feature = "workspace-checkpoints")]
pub fn is_checkpoint_validation_status(value: &str) -> bool {
    CHECKPOINT_VALIDATION_STATUS_VALUES.contains(&value)
}

// Exact edit primitives and kinds remain shared with the Runner wire contract,
// but the model-facing file-change DTO intentionally differs: models carry a
// short read revision while ToolRuntime translates it back to the wire SHA.
pub use webcodex_core::apply_edits_shared::{
    ApplyFileChangeKind, ApplyTextEditInput, ApplyTextEditKind,
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyFileChangeInput {
    pub kind: ApplyFileChangeKind,
    pub path: String,
    #[serde(default)]
    pub to_path: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub edits: Vec<ApplyTextEditInput>,
    #[serde(default)]
    pub expected_read_revision: Option<u64>,
}

#[cfg(feature = "workspace-checkpoints")]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CheckpointValidationInput {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ListToolsOptions {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub features: Option<String>,
    #[serde(default)]
    pub summary_only: bool,
    #[serde(default)]
    pub limit: Option<usize>,
}
