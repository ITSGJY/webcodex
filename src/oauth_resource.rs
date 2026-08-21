//! Canonical OAuth resource identifiers shared by authorization, discovery,
//! and request-time audience enforcement.

use crate::mcp_bridge_http::is_valid_bridge_id;

const MCP_PATH: &str = "/mcp";
const MCP_BRIDGE_PATH_PREFIX: &str = "/mcp/bridge/";
const PROTECTED_RESOURCE_METADATA_PATH: &str = "/.well-known/oauth-protected-resource";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostedBridgeResource {
    pub(crate) uri: String,
    pub(crate) bridge_id: String,
}

fn normalize_http_resource(resource: &str) -> Option<String> {
    let resource = resource.trim();
    if resource.is_empty() {
        return None;
    }

    let parsed = url::Url::parse(resource).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }

    let mut normalized = parsed.origin().ascii_serialization();

    let mut path = parsed.path().to_string();
    if path == "/" {
        path.clear();
    } else {
        while path.ends_with('/') {
            path.pop();
        }
    }
    normalized.push_str(&path);
    Some(normalized)
}

pub(crate) fn canonical_issuer_resource(config: &crate::Config) -> Option<String> {
    normalize_http_resource(config.oauth2.issuer.as_deref()?)
}

pub(crate) fn canonical_mcp_resource(config: &crate::Config) -> Option<String> {
    Some(format!("{}{MCP_PATH}", canonical_issuer_resource(config)?))
}

pub(crate) fn canonical_hosted_bridge_resource(
    config: &crate::Config,
    bridge_id: &str,
) -> Option<String> {
    if !is_valid_bridge_id(bridge_id) {
        return None;
    }
    Some(format!(
        "{}{MCP_BRIDGE_PATH_PREFIX}{bridge_id}",
        canonical_issuer_resource(config)?
    ))
}

/// Normalize an RFC 8707 resource and accept only the issuer root, the
/// ordinary `/mcp` resource, or one syntactically exact hosted bridge.
///
/// A hosted bridge is only syntactically accepted here. Callers that issue or
/// rotate credentials must additionally prove that its opaque id currently
/// resolves through the Runner/provider registry.
pub(crate) fn validate_oauth_resource(config: &crate::Config, resource: &str) -> Option<String> {
    let normalized = normalize_http_resource(resource)?;
    let issuer = canonical_issuer_resource(config)?;
    if normalized == issuer
        || Some(normalized.as_str()) == canonical_mcp_resource(config).as_deref()
    {
        return Some(normalized);
    }

    let bridge_id = normalized.strip_prefix(&format!("{issuer}{MCP_BRIDGE_PATH_PREFIX}"))?;
    if !is_valid_bridge_id(bridge_id) {
        return None;
    }
    canonical_hosted_bridge_resource(config, bridge_id)
}

pub(crate) fn hosted_bridge_resource(
    config: &crate::Config,
    resource: &str,
) -> Option<HostedBridgeResource> {
    let normalized = validate_oauth_resource(config, resource)?;
    let issuer = canonical_issuer_resource(config)?;
    let bridge_id = normalized
        .strip_prefix(&format!("{issuer}{MCP_BRIDGE_PATH_PREFIX}"))?
        .to_string();
    Some(HostedBridgeResource {
        uri: normalized,
        bridge_id,
    })
}

pub(crate) fn hosted_bridge_resource_for_request_path(
    config: &crate::Config,
    path: &str,
) -> Option<HostedBridgeResource> {
    let bridge_id = hosted_bridge_id_for_request_path(path)?;
    Some(HostedBridgeResource {
        uri: canonical_hosted_bridge_resource(config, bridge_id)?,
        bridge_id: bridge_id.to_string(),
    })
}

pub(crate) fn hosted_bridge_id_for_request_path(path: &str) -> Option<&str> {
    let bridge_id = path.strip_prefix(MCP_BRIDGE_PATH_PREFIX)?;
    is_valid_bridge_id(bridge_id).then_some(bridge_id)
}

/// RFC 9728 §3 inserts the well-known suffix between the authority and the
/// protected resource path.
pub(crate) fn protected_resource_metadata_uri(resource: &str) -> Option<String> {
    let resource = normalize_http_resource(resource)?;
    let parsed = url::Url::parse(&resource).ok()?;
    let origin = parsed.origin().ascii_serialization();
    let resource_path = parsed.path().trim_start_matches('/');
    if resource_path.is_empty() {
        Some(format!("{origin}{PROTECTED_RESOURCE_METADATA_PATH}"))
    } else {
        Some(format!(
            "{origin}{PROTECTED_RESOURCE_METADATA_PATH}/{resource_path}"
        ))
    }
}

pub(crate) fn hosted_bridge_resource_for_metadata_path(
    config: &crate::Config,
    request_path: &str,
) -> Option<HostedBridgeResource> {
    let bridge_id = request_path.rsplit('/').next()?;
    let resource = HostedBridgeResource {
        uri: canonical_hosted_bridge_resource(config, bridge_id)?,
        bridge_id: bridge_id.to_string(),
    };
    let expected_path = url::Url::parse(&protected_resource_metadata_uri(&resource.uri)?)
        .ok()?
        .path()
        .to_string();
    (request_path == expected_path).then_some(resource)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(issuer: &str) -> crate::Config {
        crate::Config {
            addr: "127.0.0.1:0".to_string(),
            data_dir: std::path::PathBuf::from("./data"),
            token: None,
            max_text_size: 1024,
            max_file_size: 1024,
            codex: crate::CodexConfig::default(),
            oauth2: crate::OAuth2Config {
                issuer: Some(issuer.to_string()),
                ..crate::OAuth2Config::default()
            },
        }
    }

    fn bridge_id() -> String {
        format!("wc_mcpb_{}", "a".repeat(64))
    }

    #[test]
    fn exact_bridge_resource_is_canonical_and_not_prefix_widened() {
        let config = config("HTTPS://Example.TEST/root/");
        let bridge_id = bridge_id();
        let expected = format!("https://example.test/root/mcp/bridge/{bridge_id}");
        assert_eq!(
            validate_oauth_resource(&config, &expected).as_deref(),
            Some(expected.as_str())
        );
        assert!(validate_oauth_resource(
            &config,
            "https://example.test/root/mcp/bridge/not-an-opaque-id"
        )
        .is_none());
        assert!(
            validate_oauth_resource(&config, &format!("{expected}/tools")).is_none(),
            "subpaths must not widen an exact hosted bridge audience"
        );
    }

    #[test]
    fn rfc9728_metadata_uri_inserts_well_known_before_resource_path() {
        let config = config("https://example.test/root");
        let bridge_id = bridge_id();
        let resource = canonical_hosted_bridge_resource(&config, &bridge_id).unwrap();
        let metadata = protected_resource_metadata_uri(&resource).unwrap();
        assert_eq!(
            metadata,
            format!(
                "https://example.test/.well-known/oauth-protected-resource/root/mcp/bridge/{bridge_id}"
            )
        );
        assert_eq!(
            hosted_bridge_resource_for_metadata_path(
                &config,
                url::Url::parse(&metadata).unwrap().path()
            )
            .unwrap()
            .uri,
            resource
        );
    }
}
