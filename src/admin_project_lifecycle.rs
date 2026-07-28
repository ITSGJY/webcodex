use crate::auth::AuthContext;
use crate::db::AdminProjectAudit;
use crate::shell_protocol::ShellAgentProjectSummary;
use crate::tool_runtime::{ToolResult, ToolRuntime, ACTIVE_JOB_STATUSES};
use crate::Database;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex;

const WAIT_SECS: u64 = 30;
pub(crate) const IDEMPOTENCY_KEY_MAX: usize = 128;
type IdempotencyLocks = Mutex<HashMap<String, Arc<Mutex<()>>>>;
static IDEMPOTENCY_LOCKS: OnceLock<IdempotencyLocks> = OnceLock::new();

fn idempotency_locks() -> &'static IdempotencyLocks {
    IDEMPOTENCY_LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegisterProjectRequest {
    pub client_id: String,
    pub project_id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub path: String,
    #[serde(default = "default_true")]
    pub allow_patch: bool,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateProjectRequest {
    pub client_id: String,
    pub project_id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub path: String,
    #[serde(default = "default_true")]
    pub allow_patch: bool,
    #[serde(default)]
    pub git_init: bool,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub allow_existing_empty: bool,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectMutationRequest {
    pub project: String,
    pub expected_revision: String,
    pub idempotency_key: String,
    pub confirm: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub(crate) struct ServiceResponse {
    pub status: u16,
    pub body: Value,
}

#[derive(Clone)]
pub(crate) struct AdminProjectLifecycleService {
    runtime: Arc<ToolRuntime>,
    db: Arc<Database>,
}

impl AdminProjectLifecycleService {
    pub(crate) fn new(runtime: Arc<ToolRuntime>, db: Arc<Database>) -> Self {
        Self { runtime, db }
    }

    pub(crate) async fn register(
        &self,
        auth: &AuthContext,
        request: RegisterProjectRequest,
    ) -> ServiceResponse {
        let target = format!("agent:{}:{}", request.client_id, request.project_id);
        self.idempotent(
            auth,
            "register",
            &target,
            &request,
            &request.idempotency_key,
            || async {
                validate_common(
                    &request.client_id,
                    &request.project_id,
                    &request.name,
                    request.description.as_deref(),
                    &request.path,
                )?;
                require_online_client(&self.runtime, auth, &request.client_id).await?;
                let result = self
                    .runtime
                    .register_project(
                        request.client_id.clone(),
                        request.project_id.clone(),
                        request.name.clone(),
                        request.path.clone(),
                        request.description.clone(),
                        request.allow_patch,
                        false,
                        Some(auth),
                    )
                    .await;
                map_create_result("register", "registered", &target, result)
            },
        )
        .await
    }

    pub(crate) async fn create(
        &self,
        auth: &AuthContext,
        request: CreateProjectRequest,
    ) -> ServiceResponse {
        let target = format!("agent:{}:{}", request.client_id, request.project_id);
        self.idempotent(
            auth,
            "create",
            &target,
            &request,
            &request.idempotency_key,
            || async {
                validate_common(
                    &request.client_id,
                    &request.project_id,
                    &request.name,
                    request.description.as_deref(),
                    &request.path,
                )?;
                require_online_client(&self.runtime, auth, &request.client_id).await?;
                if let Some(template) = request.template.as_deref() {
                    if template.len() > 32 {
                        return Err(api_error(400, "invalid_request"));
                    }
                }
                let result = self
                    .runtime
                    .create_project(
                        request.client_id.clone(),
                        request.project_id.clone(),
                        request.name.clone(),
                        request.path.clone(),
                        request.description.clone(),
                        request.allow_patch,
                        request.template.clone(),
                        request.git_init,
                        request.allow_existing_empty,
                        false,
                        Some(auth),
                    )
                    .await;
                map_create_result("create", "created", &target, result)
            },
        )
        .await
    }

    pub(crate) async fn mutate(
        &self,
        auth: &AuthContext,
        action: &'static str,
        request: ProjectMutationRequest,
    ) -> ServiceResponse {
        let target = request.project.clone();
        self.idempotent(auth, action, &target, &request, &request.idempotency_key, || async {
            if !request.confirm { return Err(api_error(400, "invalid_request")); }
            validate_revision(&request.expected_revision)?;
            let (client_id, project_id) = parse_runtime_project(&request.project)?;
            let client = self.runtime.shell_clients.get_client_view_for_auth(&client_id, Some(auth)).await
                .ok_or_else(|| api_error(503, "agent_unavailable"))?;
            if !client.connected || client.status != "online" {
                return Err(api_error(503, "agent_unavailable"));
            }
            if !client.capabilities.project_lifecycle {
                return Err(api_error(409, "unsupported_runner_version"));
            }
            let project = client.projects.iter().find(|p| p.id == project_id);
            if action != "unregister" && project.is_none() {
                return Err(api_error(404, "project_not_found"));
            }
            let active_jobs = self.runtime.shell_clients.list_jobs_for_auth(Some(auth), Some(100)).await
                .into_iter()
                .filter(|job| job.project_id.as_deref() == Some(request.project.as_str())
                    && ACTIVE_JOB_STATUSES.contains(&job.status.as_str()))
                .count();
            if action == "unregister" && active_jobs > 0 {
                return Err(ServiceResponse { status: 409, body: json!({"error":{"code":"active_jobs_conflict"},"active_jobs":active_jobs}) });
            }
            let payload = serde_json::to_string(&json!({
                "project_id": project_id,
                "expected_revision": request.expected_revision,
            })).map_err(|_| api_error(500, "operation_failed"))?;
            let kind = format!("project_lifecycle_{action}");
            let (request_id, receiver) = self.runtime.shell_clients.enqueue_project_op(
                client_id.clone(), &kind, payload, "admin_project_lifecycle".to_string(),
            ).await.map_err(|_| api_error(503, "agent_unavailable"))?;
            let response = match tokio::time::timeout(Duration::from_secs(WAIT_SECS), receiver).await {
                Ok(Ok(value)) => value,
                _ => {
                    self.runtime.shell_clients.cancel_request(&request_id).await;
                    return Err(api_error(503, "agent_unavailable"));
                }
            };
            if let Some(error) = response.error.as_deref() {
                return Err(map_agent_error(error));
            }
            let output: Value = serde_json::from_str(response.stdout.as_deref().unwrap_or(""))
                .map_err(|_| api_error(502, "operation_failed"))?;
            let outcome = output.get("outcome").and_then(Value::as_str).ok_or_else(|| api_error(502, "operation_failed"))?;
            let changed = output.get("changed").and_then(Value::as_bool).unwrap_or(false);
            let revision = output.get("revision").cloned().unwrap_or(Value::Null);
            if action == "unregister" && matches!(outcome, "unregistered" | "already_unregistered") {
                let _ = self.runtime.shell_clients.remove_client_project(&client_id, &project_id).await;
            } else if let Some(summary) = lifecycle_summary(&output, &project_id) {
                let _ = self.runtime.shell_clients.upsert_client_project(&client_id, summary).await;
            }
            let warnings = if active_jobs > 0 {
                json!([{"code":"active_jobs_present","active_jobs":active_jobs}])
            } else { json!([]) };
            Ok(ServiceResponse { status: 200, body: json!({
                "operation": action, "project": target, "outcome": outcome,
                "changed": changed, "revision": revision, "active_jobs": active_jobs,
                "warnings": warnings
            }) })
        }).await
    }

    async fn idempotent<T, F, Fut>(
        &self,
        auth: &AuthContext,
        action: &str,
        target: &str,
        request: &T,
        key: &str,
        operation: F,
    ) -> ServiceResponse
    where
        T: Serialize,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<ServiceResponse, ServiceResponse>>,
    {
        if !valid_idempotency_key(key) {
            return api_error(400, "invalid_request");
        }
        let subject = subject_id(auth);
        let key_hash = digest(key.as_bytes());
        let request_hash = digest(&serde_json::to_vec(request).unwrap_or_default());
        let lock_scope = format!("{subject}\u{1f}{action}\u{1f}{target}\u{1f}{key_hash}");
        let operation_lock = {
            let mut locks = idempotency_locks().lock().await;
            if locks.len() > 2_048 {
                locks.retain(|_, lock| Arc::strong_count(lock) > 1);
            }
            locks
                .entry(lock_scope.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _operation_guard = operation_lock.lock().await;
        match self
            .db
            .get_admin_project_idempotency(&subject, action, target, &key_hash)
        {
            Ok(Some(stored)) if stored.request_hash == request_hash => {
                return ServiceResponse {
                    status: stored.http_status as u16,
                    body: serde_json::from_str(&stored.response_json)
                        .unwrap_or_else(|_| json!({"error":{"code":"operation_failed"}})),
                }
            }
            Ok(Some(_)) => return api_error(409, "idempotency_conflict"),
            Err(_) => return api_error(500, "operation_failed"),
            Ok(None) => {}
        }
        let response = match operation().await {
            Ok(v) | Err(v) => v,
        };
        let correlation_id = uuid::Uuid::new_v4().to_string();
        let outcome = response
            .body
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or("failed");
        let changed = response
            .body
            .get("changed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let reason_code = response.body.pointer("/error/code").and_then(Value::as_str);
        let client_id = parse_runtime_project(target).ok().map(|(client, _)| client);
        let subject_type = if auth.is_bootstrap() {
            "bootstrap"
        } else {
            "admin_pat"
        };
        let _ = self
            .db
            .insert_admin_project_lifecycle_audit(&AdminProjectAudit {
                correlation_id: &correlation_id,
                subject_type,
                subject_id: &subject,
                operation: action,
                project: target,
                client_id: client_id.as_deref(),
                outcome,
                changed,
                reason_code,
                idempotency_digest: &key_hash,
            });
        let response_json = serde_json::to_string(&response.body)
            .unwrap_or_else(|_| "{\"error\":{\"code\":\"operation_failed\"}}".to_string());
        match self.db.insert_admin_project_idempotency(
            &subject,
            action,
            target,
            &key_hash,
            &request_hash,
            response.status as i64,
            &response_json,
        ) {
            Ok(true) => response,
            Ok(false) => match self
                .db
                .get_admin_project_idempotency(&subject, action, target, &key_hash)
            {
                Ok(Some(stored)) if stored.request_hash == request_hash => ServiceResponse {
                    status: stored.http_status as u16,
                    body: serde_json::from_str(&stored.response_json).unwrap_or(response.body),
                },
                _ => api_error(409, "idempotency_conflict"),
            },
            Err(_) => api_error(500, "operation_failed"),
        }
    }
}

async fn require_online_client(
    runtime: &ToolRuntime,
    auth: &AuthContext,
    client_id: &str,
) -> Result<(), ServiceResponse> {
    let client = runtime
        .shell_clients
        .get_client_view_for_auth(client_id, Some(auth))
        .await
        .ok_or_else(|| api_error(503, "agent_unavailable"))?;
    if !client.connected || client.status != "online" {
        return Err(api_error(503, "agent_unavailable"));
    }
    Ok(())
}

fn map_create_result(
    operation: &str,
    outcome: &str,
    project: &str,
    result: ToolResult,
) -> Result<ServiceResponse, ServiceResponse> {
    if !result.success {
        return Err(map_agent_error(
            result.error.as_deref().unwrap_or("operation_failed"),
        ));
    }
    Ok(ServiceResponse {
        status: 200,
        body: json!({
            "operation": operation, "project": project, "outcome": outcome,
            "changed": true, "revision": result.output.get("revision").cloned().unwrap_or(Value::Null),
            "warnings": []
        }),
    })
}

fn lifecycle_summary(output: &Value, id: &str) -> Option<ShellAgentProjectSummary> {
    Some(ShellAgentProjectSummary {
        id: id.to_string(),
        name: output
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| Some(id.to_string())),
        path: output.get("path")?.as_str()?.to_string(),
        allow_patch: output
            .get("allow_patch")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        kind: None,
        description: output
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        hooks: Vec::new(),
        disabled: output
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        revision: output
            .get("revision")
            .and_then(Value::as_str)
            .map(str::to_string),
        git_branch: None,
        git_head: None,
        git_dirty: None,
        updated_at: chrono::Utc::now().timestamp(),
        shell_profile: None,
    })
}

fn validate_common(
    client: &str,
    project: &str,
    name: &str,
    description: Option<&str>,
    path: &str,
) -> Result<(), ServiceResponse> {
    if client.is_empty()
        || client.len() > 128
        || !client
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(api_error(400, "invalid_request"));
    }
    if project.is_empty()
        || project.len() > 64
        || !project
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(api_error(400, "invalid_request"));
    }
    if name.trim().is_empty()
        || name.len() > 120
        || path.is_empty()
        || path.len() > 4096
        || !path.starts_with('/')
        || description.is_some_and(|v| v.len() > 500)
    {
        return Err(api_error(400, "invalid_request"));
    }
    Ok(())
}

fn validate_revision(value: &str) -> Result<(), ServiceResponse> {
    if value.len() != 71
        || !value.starts_with("sha256:")
        || !value[7..].chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(api_error(400, "invalid_request"));
    }
    Ok(())
}

fn parse_runtime_project(value: &str) -> Result<(String, String), ServiceResponse> {
    let rest = value
        .strip_prefix("agent:")
        .ok_or_else(|| api_error(400, "invalid_request"))?;
    let (client, project) = rest
        .split_once(':')
        .ok_or_else(|| api_error(400, "invalid_request"))?;
    if client.is_empty() || project.is_empty() || project.contains(':') {
        return Err(api_error(400, "invalid_request"));
    }
    Ok((client.to_string(), project.to_string()))
}

fn valid_idempotency_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= IDEMPOTENCY_KEY_MAX
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}
fn subject_id(auth: &AuthContext) -> String {
    if auth.is_bootstrap() {
        "bootstrap".to_string()
    } else {
        auth.api_key_id
            .clone()
            .or_else(|| auth.user_id.clone())
            .unwrap_or_else(|| "admin".to_string())
    }
}
fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
fn api_error(status: u16, code: &str) -> ServiceResponse {
    ServiceResponse {
        status,
        body: json!({"error":{"code":code}}),
    }
}
fn map_agent_error(error: &str) -> ServiceResponse {
    let lower = error.to_ascii_lowercase();
    let (status, code) = if lower.contains("revision_conflict") {
        (409, "revision_conflict")
    } else if lower.contains("already exists") {
        (409, "project_already_exists")
    } else if lower.contains("outside allowed_roots")
        || lower.contains("path_outside_allowed_roots")
    {
        (400, "path_outside_allowed_roots")
    } else if lower.contains("not empty") {
        (409, "path_not_empty")
    } else if lower.contains("project_not_found") {
        (404, "project_not_found")
    } else if lower.contains("offline")
        || lower.contains("unknown shell client")
        || lower.contains("unknown client")
        || lower.contains("not connected")
        || lower.contains("unsupported")
    {
        (503, "agent_unavailable")
    } else {
        (500, "operation_failed")
    };
    api_error(status, code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_lifecycle_error_mapping_is_stable_and_safe() {
        assert_eq!(map_agent_error("unknown client_id: smoke").status, 503);
        assert_eq!(map_agent_error("revision_conflict").status, 409);
        assert_eq!(map_agent_error("secret internal backtrace").status, 500);
        assert_eq!(
            map_agent_error("secret internal backtrace").body["error"]["code"],
            "operation_failed"
        );
    }

    #[test]
    fn project_lifecycle_idempotency_keys_are_bounded() {
        assert!(valid_idempotency_key("req-1:retry_2"));
        assert!(!valid_idempotency_key(""));
        assert!(!valid_idempotency_key("contains space"));
        assert!(!valid_idempotency_key(&"a".repeat(IDEMPOTENCY_KEY_MAX + 1)));
    }
}
