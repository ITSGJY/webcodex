use crate::auth::{AuthContext, AuthKind};
use crate::tool_runtime::activity::ActivityVisibility;
use crate::tool_runtime::ToolRuntime;
use crate::Database;
use salvo::prelude::*;
use serde_json::{json, Value};
use std::sync::Arc;

pub(crate) const ADMIN_ROUTES: &[&str] = &["/api/admin/dashboard"];

pub(crate) fn routes() -> Router {
    Router::with_path("admin").push(Router::with_path("dashboard").post(dashboard))
}

fn error(res: &mut Response, status: StatusCode, message: &str) {
    res.status_code(status);
    res.render(Json(json!({"error": {"message": message}})));
}

fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

fn compatibility_for_protocol(protocol: Option<&str>, global: &str) -> String {
    match protocol {
        Some("polling-v1" | "websocket-v1" | "quic-v1") => global.to_string(),
        Some(_) => "incompatible".to_string(),
        None => "unknown".to_string(),
    }
}

#[handler]
async fn dashboard(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    if let Err((status, _, message)) = crate::auth::require_json_same_origin(req) {
        return error(
            res,
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            &message,
        );
    }
    let Ok(auth) = depot.obtain::<AuthContext>().cloned() else {
        return error(res, StatusCode::UNAUTHORIZED, "authentication required");
    };
    if !auth.is_admin()
        || matches!(
            auth.kind,
            AuthKind::AgentToken
                | AuthKind::ProjectCredential
                | AuthKind::SharedKey
                | AuthKind::OpenAnonymous
                | AuthKind::AccountCredential
        )
    {
        return error(
            res,
            StatusCode::FORBIDDEN,
            "bootstrap or admin-scoped token required",
        );
    }
    let Ok(runtime) = depot.obtain::<Arc<ToolRuntime>>().cloned() else {
        return error(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "runtime unavailable",
        );
    };
    let Ok(db) = depot.obtain::<Arc<Database>>().cloned() else {
        return error(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "database unavailable",
        );
    };

    let status = runtime.runtime_status(Some(&auth)).await.output;
    let agents = runtime.list_agents(Some(&auth)).await.output;
    let projects = runtime.list_projects(Some(&auth)).await.output;
    let global_compat = status
        .pointer("/version_compatibility/status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    let mut device_rows = agents.get("agents").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().map(|agent| {
        let protocol = agent.get("agent_protocol_version").and_then(Value::as_str);
        json!({
            "display_name": text(agent.get("display_name")),
            "client_id": text(agent.get("client_id")),
            "status": text(agent.get("status")).unwrap_or_else(|| if agent.get("connected").and_then(Value::as_bool).unwrap_or(false) {"online".into()} else {"offline".into()}),
            "transport": text(agent.get("transport")),
            "hostname": text(agent.get("hostname")),
            "last_seen": agent.get("last_seen").cloned().unwrap_or(Value::Null),
            "capabilities": agent.get("capabilities").cloned().unwrap_or_else(|| json!([])),
            "project_count": agent.get("projects_count").cloned().unwrap_or_else(|| json!(0)),
            "active_jobs": agent.get("active_jobs").cloned().unwrap_or_else(|| json!(0)),
            "runner_protocol": protocol,
            "compatibility": compatibility_for_protocol(protocol, global_compat),
        })
    }).collect::<Vec<_>>();
    device_rows.sort_by(|a, b| {
        a["client_id"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["client_id"].as_str().unwrap_or_default())
    });

    let bootstrap = auth.is_bootstrap();
    let mut project_rows = projects.get("projects").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().map(|project| {
        let connected = project.get("connected").and_then(Value::as_bool).unwrap_or(false);
        let capabilities = project.get("capabilities").cloned().unwrap_or_else(|| json!({}));
        json!({
            "id": text(project.get("id")),
            "name": text(project.get("name")),
            "description": Value::Null,
            "client_id": text(project.get("client_id")),
            "path": if bootstrap { project.get("path").cloned().unwrap_or(Value::Null) } else { json!("hidden for non-bootstrap admin") },
            "readiness": if connected {"online"} else {"offline"},
            "git_available": capabilities.get("git_available").cloned().unwrap_or(Value::Null),
            "allow_patch": project.get("allow_patch").cloned().unwrap_or(Value::Null),
            "shell_profile_status": project.get("shell_profile_status").cloned().unwrap_or(Value::Null),
            "compatibility": global_compat,
            "console_hint": "Use /console with that project's credential; credentials never belong in URLs.",
        })
    }).collect::<Vec<_>>();
    project_rows.sort_by(|a, b| {
        a["id"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["id"].as_str().unwrap_or_default())
    });

    let (activity, activity_error) = match db.list_workspace_activity_for_clients(50, None, ActivityVisibility::Global, &[]) {
        Ok(rows) => (rows.into_iter().map(|row| json!({"created_at":row.created_at,"kind":row.tool,"project_id":row.project,"status":if row.success{"ok"}else{"failed"}})).collect::<Vec<_>>(), Value::Null),
        Err(_) => (Vec::new(), json!("activity unavailable")),
    };

    res.render(Json(json!({
        "overview": {
            "version": status.get("version").cloned().unwrap_or(Value::Null),
            "build_commit": status.pointer("/build/git_commit").cloned().unwrap_or(Value::Null),
            "authority_mode": status.pointer("/authority/mode").cloned().unwrap_or(Value::Null),
            "agents_total": status.pointer("/agents/count").cloned().unwrap_or_else(|| json!(0)),
            "agents_online": status.pointer("/agents/online_count").cloned().unwrap_or_else(|| json!(0)),
            "projects_total": status.pointer("/projects/agent_registered/count").cloned().unwrap_or_else(|| json!(0)),
            "projects_online": status.pointer("/projects/agent_registered/online_count").cloned().unwrap_or_else(|| json!(0)),
            "active_jobs": status.pointer("/jobs/active_count").cloned().unwrap_or_else(|| json!(0)),
            "version_compatibility": global_compat,
        },
        "devices": device_rows,
        "projects": project_rows,
        "diagnostics": {
            "runner_process": status.pointer("/connection_layers/runner_process").cloned().unwrap_or(Value::Null),
            "server_transport": status.pointer("/connection_layers/server_transport").cloned().unwrap_or(Value::Null),
            "server_registration": status.pointer("/connection_layers/server_registration").cloned().unwrap_or(Value::Null),
            "project_registry": status.pointer("/connection_layers/project_registry").cloned().unwrap_or(Value::Null),
            "connector_endpoint": status.pointer("/connection_layers/connector_endpoint").cloned().unwrap_or(Value::Null),
            "version_compatibility": status.get("version_compatibility").cloned().unwrap_or(Value::Null),
            "activity_error": activity_error,
        },
        "activity": activity,
        "limits": {"activity": 50}
    })));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::scopes::SCOPE_ADMIN;
    use salvo::test::{ResponseExt, TestClient};
    use salvo::Service;

    fn service(auth: Option<AuthContext>) -> Service {
        let config = crate::test_support::test_config(None);
        let (_tmp, db) = crate::test_support::test_db();
        let runtime = Arc::new(ToolRuntime::new(
            Arc::new(crate::ShellClientRegistry::default()),
            Arc::new(config.codex.clone()),
            Arc::new(crate::tool_runtime::RuntimeInfo::default()),
        ));
        let mut router = Router::new()
            .hoop(affix_state::inject(db))
            .hoop(affix_state::inject(runtime));
        if let Some(auth) = auth {
            router = router.hoop(affix_state::inject(auth));
        }
        Service::new(router.push(routes()))
    }

    async fn call(auth: Option<AuthContext>) -> (StatusCode, Value) {
        let mut response = TestClient::post("http://127.0.0.1/admin/dashboard")
            .add_header("host", "127.0.0.1", true)
            .add_header("origin", "http://127.0.0.1", true)
            .add_header("content-type", "application/json", true)
            .body("{}")
            .send(&service(auth))
            .await;
        let status = response.status_code.unwrap();
        let body = response.take_json::<Value>().await.unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn admin_dashboard_rejects_missing_and_project_scoped_credentials() {
        assert_eq!(call(None).await.0, StatusCode::UNAUTHORIZED);
        for kind in [AuthKind::ProjectCredential, AuthKind::AgentToken] {
            assert_eq!(
                call(Some(AuthContext::new(kind))).await.0,
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            call(Some(AuthContext::new(AuthKind::ApiToken))).await.0,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn admin_dashboard_accepts_bootstrap_and_admin_pat_without_secrets() {
        let mut bootstrap = AuthContext::new(AuthKind::Bootstrap);
        bootstrap.is_bootstrap = true;
        let (status, body) = call(Some(bootstrap)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let serialized = serde_json::to_string(&body).unwrap().to_ascii_lowercase();
        for forbidden in [
            "bootstrap_token",
            "project_credential",
            "agent_token",
            "secret_env",
            "authorization",
        ] {
            assert!(!serialized.contains(forbidden));
        }
        assert_eq!(body["limits"]["activity"], 50);

        let mut admin = AuthContext::new(AuthKind::ApiToken);
        admin.scopes.push(SCOPE_ADMIN.to_string());
        assert_eq!(call(Some(admin)).await.0, StatusCode::OK);
    }

    #[test]
    fn admin_routes_are_separate_from_console_routes() {
        assert!(ADMIN_ROUTES
            .iter()
            .all(|route| route.starts_with("/api/admin/")));
        assert!(ADMIN_ROUTES
            .iter()
            .all(|route| !crate::host_console_http::CONSOLE_ROUTES.contains(route)));
    }
}
