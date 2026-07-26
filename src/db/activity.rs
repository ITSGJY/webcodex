//! Workspace activity ledger: one bounded row per mutating tool execution,
//! shared by the browser console and the offline CLI. Row count is capped at
//! insert time (oldest rows pruned) so long-running low-disk deployments never
//! grow without bound.

use super::Database;
use crate::tool_runtime::activity::{ActivityRecord, ActivityRecorder};
use rusqlite::params;
use serde::Serialize;
use std::sync::Arc;

/// Character budgets. Previews reuse the 120-char discipline of
/// `shell_client`'s command_preview; errors stay one short line.
const COMMAND_PREVIEW_MAX_CHARS: usize = 120;
const ERROR_SUMMARY_MAX_CHARS: usize = 200;

const DEFAULT_MAX_ROWS: i64 = 2_000;

#[derive(Debug, Serialize)]
pub struct WorkspaceActivityRow {
    pub id: i64,
    pub created_at: i64,
    pub project: Option<String>,
    pub tool: String,
    pub surface: String,
    pub client: Option<String>,
    pub success: bool,
    pub session_id: Option<String>,
    pub command_preview: Option<String>,
    pub paths: Vec<String>,
    pub error_summary: Option<String>,
}

impl Database {
    pub fn insert_workspace_activity(
        &self,
        created_at: i64,
        record: &ActivityRecord<'_>,
        command_preview: Option<&str>,
        max_rows: i64,
    ) -> anyhow::Result<()> {
        let paths_json = serde_json::to_string(&record.paths)?;
        let error_summary = record
            .error_summary
            .map(|error| truncate_chars(error, ERROR_SUMMARY_MAX_CHARS));
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO workspace_activity (
                created_at, project, tool, surface, client, success, session_id,
                command_preview, paths_json, error_summary
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                created_at,
                record.project,
                record.tool,
                record.surface,
                record.client,
                record.success,
                record.session_id,
                command_preview,
                paths_json,
                error_summary,
            ],
        )?;
        // Keep the ledger bounded: prune oldest rows beyond the cap in the
        // same connection so the table can never outgrow the operator budget.
        conn.execute(
            "DELETE FROM workspace_activity WHERE id NOT IN (
                SELECT id FROM workspace_activity ORDER BY id DESC LIMIT ?1
            )",
            params![max_rows.max(1)],
        )?;
        Ok(())
    }

    pub fn list_workspace_activity(
        &self,
        limit: usize,
        client: Option<&str>,
    ) -> anyhow::Result<Vec<WorkspaceActivityRow>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT id, created_at, project, tool, surface, client, success, session_id,
                    command_preview, paths_json, error_summary
             FROM workspace_activity
             WHERE ?2 IS NULL OR client = ?2
             ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit as i64, client], |row| {
            let paths_json: String = row.get(9)?;
            Ok(WorkspaceActivityRow {
                id: row.get(0)?,
                created_at: row.get(1)?,
                project: row.get(2)?,
                tool: row.get(3)?,
                surface: row.get(4)?,
                client: row.get(5)?,
                success: row.get(6)?,
                session_id: row.get(7)?,
                command_preview: row.get(8)?,
                paths: serde_json::from_str(&paths_json).unwrap_or_default(),
                error_summary: row.get(10)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

/// SQLite-backed [`ActivityRecorder`] wired into the server's `ToolRuntime`.
/// Env knobs (self-hosted operators own the privacy tradeoff):
/// - `WEBCODEX_ACTIVITY=0` disables recording entirely.
/// - `WEBCODEX_ACTIVITY_COMMAND_PREVIEW=0` drops command previews.
/// - `WEBCODEX_ACTIVITY_MAX_ROWS` bounds the ledger (default 2000).
pub struct WorkspaceActivityStore {
    db: Arc<Database>,
    preview_enabled: bool,
    max_rows: i64,
}

impl WorkspaceActivityStore {
    pub fn from_env(db: Arc<Database>) -> Option<Self> {
        if env_flag_disabled("WEBCODEX_ACTIVITY") {
            return None;
        }
        let max_rows = std::env::var("WEBCODEX_ACTIVITY_MAX_ROWS")
            .ok()
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_MAX_ROWS)
            .clamp(100, 100_000);
        Some(Self {
            db,
            preview_enabled: !env_flag_disabled("WEBCODEX_ACTIVITY_COMMAND_PREVIEW"),
            max_rows,
        })
    }
}

impl ActivityRecorder for WorkspaceActivityStore {
    fn record(&self, record: ActivityRecord<'_>) {
        let preview = record
            .command
            .filter(|_| self.preview_enabled)
            .map(|command| truncate_chars(command, COMMAND_PREVIEW_MAX_CHARS));
        if let Err(error) = self.db.insert_workspace_activity(
            chrono::Utc::now().timestamp(),
            &record,
            preview.as_deref(),
            self.max_rows,
        ) {
            tracing::warn!(error = %error, "workspace activity insert failed");
        }
    }
}

fn env_flag_disabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            value == "0" || value == "false" || value == "off"
        })
        .unwrap_or(false)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample<'a>(tool: &'a str, success: bool, error: Option<&'a str>) -> ActivityRecord<'a> {
        ActivityRecord {
            tool,
            project: Some("demo"),
            surface: "mcp",
            client: Some("laptop"),
            success,
            session_id: None,
            command: None,
            paths: vec!["a.rs".to_string()],
            error_summary: error,
        }
    }

    #[test]
    fn activity_roundtrip_orders_newest_first_and_prunes() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(&tmp.path().join("activity.db")).unwrap();
        for index in 0..5 {
            db.insert_workspace_activity(
                1_000 + index,
                &sample("run_shell", index % 2 == 0, None),
                Some("cargo test"),
                3,
            )
            .unwrap();
        }
        let rows = db.list_workspace_activity(10, None).unwrap();
        assert_eq!(rows.len(), 3, "prune keeps only max_rows newest rows");
        assert!(rows[0].id > rows[1].id && rows[1].id > rows[2].id);
        assert_eq!(rows[0].tool, "run_shell");
        assert_eq!(rows[0].surface, "mcp");
        assert_eq!(rows[0].paths, vec!["a.rs".to_string()]);
        assert_eq!(rows[0].command_preview.as_deref(), Some("cargo test"));
        assert_eq!(db.list_workspace_activity(2, None).unwrap().len(), 2);
    }

    #[test]
    fn activity_filter_by_client_matches_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(&tmp.path().join("activity.db")).unwrap();
        let mut other = sample("run_shell", true, None);
        other.client = Some("desktop");
        db.insert_workspace_activity(1, &sample("run_shell", true, None), None, 10)
            .unwrap();
        db.insert_workspace_activity(2, &other, None, 10).unwrap();
        assert_eq!(db.list_workspace_activity(10, None).unwrap().len(), 2);
        let filtered = db.list_workspace_activity(10, Some("laptop")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].client.as_deref(), Some("laptop"));
        assert!(db
            .list_workspace_activity(10, Some("nobody"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn activity_insert_truncates_long_error_summaries() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(&tmp.path().join("activity.db")).unwrap();
        let long_error = "e".repeat(500);
        db.insert_workspace_activity(
            1,
            &sample("write_project_file", false, Some(&long_error)),
            None,
            10,
        )
        .unwrap();
        let rows = db.list_workspace_activity(1, None).unwrap();
        let stored = rows[0].error_summary.as_deref().unwrap();
        assert!(stored.chars().count() <= ERROR_SUMMARY_MAX_CHARS + 1);
        assert!(stored.ends_with('…'));
        assert!(rows[0].command_preview.is_none());
    }
}
