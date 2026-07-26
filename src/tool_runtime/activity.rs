//! Workspace activity recording: a bounded, human-facing ledger of every
//! mutating tool execution, regardless of which client surface issued it.
//!
//! The runtime stays storage-agnostic: dispatch emits [`ActivityRecord`]s
//! through the injected [`ActivityRecorder`], and the host wires a durable
//! implementation (SQLite in the server binary). The default is a no-op so
//! embedded runtimes and tests pay nothing.

use serde_json::Value;

/// Who an activity row belonged to **when it was written**.
///
/// Activity used to be authorized by asking the live registry who owns a
/// `client_id` right now. That registry is in-memory, so a restart clears it
/// and a different project grant can register the same `client_id` and inherit
/// the previous grant's history — its command previews, paths, and errors.
/// Attribution has to be decided once, at write time, and then never
/// reinterpreted.
///
/// Deliberately narrower than [`ShellClientAuthGroup`](crate::shell_client):
/// that type identifies a shared-key group by the key's SHA-256, and a hash of
/// a credential does not belong in a durable table. Shared-key and anonymous
/// callers land in [`ActivityScope::Unscoped`] instead, visible only to
/// bootstrap/admin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityScope {
    /// A project credential or its agent token. `wc_pgrant_*` — stable and not
    /// secret.
    ProjectGrant(String),
    /// Bootstrap or admin: entitled to the whole host.
    HostGlobal,
    /// The caller's attribution could not be proven. Recorded rather than
    /// dropped so the ledger stays honest, but never shown to a project
    /// credential.
    Unscoped,
}

impl ActivityScope {
    /// Derive attribution from the authenticated caller.
    ///
    /// Only the `AuthContext` the runtime already verified is consulted: never
    /// a request parameter, and never a lookup keyed by client id.
    pub fn from_auth(auth: Option<&crate::auth::AuthContext>) -> Self {
        let Some(auth) = auth else {
            return Self::Unscoped;
        };
        if auth.is_bootstrap() || auth.is_admin() {
            return Self::HostGlobal;
        }
        match auth.project_grant_id.as_deref() {
            Some(grant) if !grant.is_empty() => Self::ProjectGrant(grant.to_string()),
            _ => Self::Unscoped,
        }
    }

    /// Stored `scope_kind`. One value per security meaning, so no single value
    /// has to be read two ways.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ProjectGrant(_) => "project_grant",
            Self::HostGlobal => "host_global",
            Self::Unscoped => "unscoped",
        }
    }

    /// Stored `scope_id`. Non-secret and only present for a project grant.
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::ProjectGrant(grant) => Some(grant.as_str()),
            Self::HostGlobal | Self::Unscoped => None,
        }
    }
}

/// Which rows a reader may see. An explicit choice rather than an `Option`, so
/// "no scope was computed" can never be mistaken for "allowed everywhere".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityVisibility<'a> {
    /// Bootstrap/admin, or a host-local operator with the state directory.
    Global,
    /// Sees only rows written by this grant.
    ProjectGrant(&'a str),
}

pub struct ActivityRecord<'a> {
    pub tool: &'a str,
    pub project: Option<&'a str>,
    /// Client surface that issued the call ("mcp" | "api").
    pub surface: &'a str,
    /// Executing device (agent client id) for agent-backed projects.
    pub client: Option<&'a str>,
    pub success: bool,
    pub session_id: Option<&'a str>,
    /// Raw command text for shell-like tools. Recorders must truncate it to a
    /// bounded preview and honor the operator's preview config switch.
    pub command: Option<&'a str>,
    /// Distinct project-relative paths named by the request (bounded).
    pub paths: Vec<String>,
    pub error_summary: Option<&'a str>,
    /// Attribution at write time. See [`ActivityScope`].
    pub scope: ActivityScope,
}

pub trait ActivityRecorder: Send + Sync {
    fn record(&self, record: ActivityRecord<'_>);
}

/// Default recorder: drops everything.
pub struct NoopActivityRecorder;

impl ActivityRecorder for NoopActivityRecorder {
    fn record(&self, _record: ActivityRecord<'_>) {}
}

/// Executing device for an agent-backed project id (`agent:<client>:<name>`).
/// Local projects have no device.
pub(crate) fn agent_client_from_project(project: &str) -> Option<&str> {
    project
        .strip_prefix("agent:")
        .and_then(|rest| rest.split_once(':'))
        .map(|(client, _)| client)
        .filter(|client| !client.is_empty())
}

/// Extract the paths a call names from its audit-sanitized argument summary
/// (`ToolCall::session_log_arguments`), so activity rows reuse the exact same
/// sanitization discipline as session events.
pub(crate) fn paths_from_sanitized_arguments(arguments: &Value, cap: usize) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    {
        let mut push = |value: &Value| {
            if let Some(path) = value.as_str() {
                if !path.is_empty() && paths.len() < cap && !paths.iter().any(|p| p == path) {
                    paths.push(path.to_string());
                }
            }
        };
        push(&arguments["path"]);
        for key in ["paths", "destination_paths"] {
            if let Some(list) = arguments[key].as_array() {
                for value in list {
                    push(value);
                }
            }
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths_extraction_dedupes_and_caps() {
        let arguments = json!({
            "path": "a.rs",
            "paths": ["a.rs", "b.rs", "", "c.rs"],
            "destination_paths": ["d.rs"]
        });
        assert_eq!(
            paths_from_sanitized_arguments(&arguments, 3),
            vec!["a.rs", "b.rs", "c.rs"]
        );
        assert_eq!(
            paths_from_sanitized_arguments(&arguments, 16),
            vec!["a.rs", "b.rs", "c.rs", "d.rs"]
        );
        assert!(paths_from_sanitized_arguments(&json!({}), 16).is_empty());
    }

    #[test]
    fn client_extraction_handles_agent_and_local_projects() {
        assert_eq!(
            agent_client_from_project("agent:laptop:webcodex"),
            Some("laptop")
        );
        assert_eq!(agent_client_from_project("agent::webcodex"), None);
        assert_eq!(agent_client_from_project("demo"), None);
        assert_eq!(agent_client_from_project("agent:solo"), None);
    }
}
