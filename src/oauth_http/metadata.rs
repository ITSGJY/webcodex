use salvo::prelude::*;

use super::{authorization_response_issuer, oauth_discovery_scopes_supported};

/// Return protected resource metadata (RFC 9728 §3.1).
///
/// This is a **public** endpoint — no authentication required. Returns 404
/// when OAuth2 is disabled so discovery does not advertise capabilities that
/// are not active.
#[handler]
pub(crate) async fn oauth_metadata(depot: &mut Depot, res: &mut Response) {
    let Some(config) = crate::auth::get_config(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(serde_json::json!({"error": "no config"})));
        return;
    };

    if !config.oauth2.enabled {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Json(serde_json::json!({"error": "OAuth2 is not enabled"})));
        return;
    }

    let issuer = config
        .oauth2
        .issuer
        .as_deref()
        .unwrap_or("http://localhost");
    let resource = format!("{}/mcp", issuer.trim_end_matches('/'));

    let metadata = serde_json::json!({
        "resource": resource,
        "authorization_servers": [issuer],
        "bearer_methods_supported": ["header"],
        "resource_name": "WebCodex",
    });

    res.render(Json(metadata));
}

/// Return RFC 9728 metadata for one exact hosted MCP bridge resource.
///
/// For `https://host/mcp/bridge/{id}`, RFC 9728 inserts the well-known suffix
/// before the resource path, yielding
/// `https://host/.well-known/oauth-protected-resource/mcp/bridge/{id}`.
/// Metadata is emitted only while that opaque Runner/provider identity still
/// resolves; stale identities are never mapped to a replacement.
#[handler]
pub(crate) async fn oauth_hosted_bridge_metadata(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) {
    let Some(config) = crate::auth::get_config(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(serde_json::json!({"error": "no config"})));
        return;
    };
    if !config.oauth2.enabled {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Json(serde_json::json!({"error": "OAuth2 is not enabled"})));
        return;
    }

    let Some(resource) =
        crate::oauth_resource::hosted_bridge_resource_for_metadata_path(&config, req.uri().path())
    else {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Json(
            serde_json::json!({"error": "OAuth protected resource not found"}),
        ));
        return;
    };
    let Some(registry) = depot
        .obtain::<std::sync::Arc<crate::ShellClientRegistry>>()
        .ok()
        .cloned()
    else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(
            serde_json::json!({"error": "MCP bridge registry unavailable"}),
        ));
        return;
    };
    match crate::mcp_bridge_http::hosted_bridge_is_current(&registry, &resource.bridge_id).await {
        Ok(true) => {}
        Ok(false) => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(
                serde_json::json!({"error": "OAuth protected resource not found"}),
            ));
            return;
        }
        Err(_) => {
            res.status_code(StatusCode::SERVICE_UNAVAILABLE);
            res.render(Json(
                serde_json::json!({"error": "MCP bridge discovery unavailable"}),
            ));
            return;
        }
    }

    let Some(issuer) = config.oauth2.issuer.as_deref() else {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Json(
            serde_json::json!({"error": "OAuth protected resource not found"}),
        ));
        return;
    };
    res.render(Json(serde_json::json!({
        "resource": resource.uri,
        "authorization_servers": [issuer],
        "bearer_methods_supported": ["header"],
        "resource_name": "WebCodex MCP Bridge",
    })));
}

/// Return OAuth Authorization Server Metadata (RFC 8414).
///
/// This is a **public** endpoint — no authentication required. It advertises
/// only capabilities implemented by the current OAuth2 server.
#[handler]
pub(crate) async fn oauth_authorization_server_metadata(depot: &mut Depot, res: &mut Response) {
    let Some(config) = crate::auth::get_config(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(serde_json::json!({"error": "no config"})));
        return;
    };

    if !config.oauth2.enabled {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Json(serde_json::json!({"error": "OAuth2 is not enabled"})));
        return;
    }

    let issuer = config
        .oauth2
        .issuer
        .as_deref()
        .unwrap_or("http://localhost");
    let endpoint_base = issuer.trim_end_matches('/');

    let metadata = serde_json::json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{}/oauth/authorize", endpoint_base),
        "token_endpoint": format!("{}/oauth/token", endpoint_base),
        "revocation_endpoint": format!("{}/oauth/revoke", endpoint_base),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_post"],
        "authorization_response_iss_parameter_supported": authorization_response_issuer(&config).is_some(),
        "scopes_supported": oauth_discovery_scopes_supported(),
    });

    res.render(Json(metadata));
}
