use super::*;
use crate::lsp_bridge::{AgentLspPayload, AgentLspRequest};
use crate::shell_protocol::AGENT_PROTOCOL_VERSION_QUIC_V1;

fn auth_context(username: Option<&str>, is_bootstrap: bool) -> crate::auth::AuthContext {
    let (role, scopes) = if is_bootstrap {
        ("admin".to_string(), vec!["admin".to_string()])
    } else {
        ("user".to_string(), Vec::new())
    };
    crate::auth::AuthContext {
        kind: if is_bootstrap {
            crate::auth::AuthKind::Bootstrap
        } else {
            crate::auth::AuthKind::ApiToken
        },
        user_id: username.map(|username| format!("user-{}", username)),
        username: username.map(str::to_string),
        api_key_id: username.map(|username| format!("key-{}", username)),
        api_key_name: username.map(|username| format!("{} key", username)),
        role: Some(role),
        scopes,
        is_bootstrap,
        token_kind: if is_bootstrap {
            None
        } else {
            Some("user".to_string())
        },
        allowed_client_id: None,
        shared_key_hash: None,
        project_grant_id: None,
    }
}

/// Phase 3 test helper: build an agent-token AuthContext bound to
/// `username` and `allowed_client_id`, carrying the given agent scopes.
fn agent_auth_context(
    username: &str,
    allowed_client_id: &str,
    scopes: Vec<&str>,
) -> crate::auth::AuthContext {
    crate::auth::AuthContext {
        kind: crate::auth::AuthKind::AgentToken,
        user_id: Some(format!("user-{}", username)),
        username: Some(username.to_string()),
        api_key_id: Some("key-agent".to_string()),
        api_key_name: Some("agent key".to_string()),
        role: Some("user".to_string()),
        scopes: scopes.into_iter().map(str::to_string).collect(),
        is_bootstrap: false,
        token_kind: Some("agent".to_string()),
        allowed_client_id: Some(allowed_client_id.to_string()),
        shared_key_hash: None,
        project_grant_id: None,
    }
}

fn open_auth_context() -> crate::auth::AuthContext {
    crate::auth::shared_key::open_anonymous_context()
}

fn oauth_bridge_auth_context(hash: &str, scopes: Vec<&str>) -> crate::auth::AuthContext {
    crate::auth::AuthContext {
        kind: crate::auth::AuthKind::OAuth2Token,
        user_id: None,
        username: None,
        api_key_id: Some("oauth-access-token".to_string()),
        api_key_name: None,
        role: Some("shared-key".to_string()),
        scopes: scopes.into_iter().map(str::to_string).collect(),
        is_bootstrap: false,
        token_kind: Some("oauth2_shared_key".to_string()),
        allowed_client_id: Some("oauth-client".to_string()),
        shared_key_hash: Some(hash.to_string()),
        project_grant_id: None,
    }
}

fn managed_oauth_auth_context(
    username: &str,
    shared_key_hash: Option<&str>,
) -> crate::auth::AuthContext {
    crate::auth::AuthContext {
        kind: crate::auth::AuthKind::OAuth2Token,
        user_id: Some(format!("user-{}", username)),
        username: Some(username.to_string()),
        api_key_id: Some("oauth-access-token".to_string()),
        api_key_name: None,
        role: Some("user".to_string()),
        scopes: Vec::new(),
        is_bootstrap: false,
        token_kind: Some("oauth2".to_string()),
        allowed_client_id: Some("oauth-client".to_string()),
        shared_key_hash: shared_key_hash.map(str::to_string),
        project_grant_id: None,
    }
}

fn project_summary(id: &str, path: &str) -> ShellAgentProjectSummary {
    ShellAgentProjectSummary {
        id: id.to_string(),
        name: Some(id.to_string()),
        path: path.to_string(),
        allow_patch: true,
        kind: Some("rust".to_string()),
        description: Some("test project".to_string()),
        hooks: vec!["doctor".to_string(), "precommit".to_string()],
        disabled: false,
        revision: None,
        git_branch: Some("codex".to_string()),
        git_head: Some("9a7d3ce".to_string()),
        git_dirty: Some(false),
        updated_at: 123456,
        shell_profile: None,
    }
}

fn async_job_capabilities() -> ShellClientCapabilities {
    let mut capabilities = ShellClientCapabilities::default();
    capabilities.async_jobs = true;
    capabilities.async_shell_jobs = true;
    capabilities.jobs = true;
    capabilities
}

fn file_request(op: &str) -> ShellFileOpRequest {
    ShellFileOpRequest {
        op: op.to_string(),
        client_id: "oe".to_string(),
        path: "src/auth/scopes.rs".to_string(),
        cwd: Some("/root/git/webcodex".to_string()),
        content: None,
        max_bytes: None,
        old_text: None,
        pattern: None,
        expected_sha256: None,
        expected_prefix: None,
        start_line: None,
        end_line: None,
        line: None,
        create_dirs: false,
        wait_timeout_secs: 0,
    }
}

#[test]
fn validate_file_request_allows_read_with_start_and_end_line() {
    let mut req = file_request("read");
    req.start_line = Some(10);
    req.end_line = Some(20);

    validate_file_request(&req).unwrap();
}

#[test]
fn validate_file_request_rejects_invalid_read_requests() {
    let cases: Vec<(&str, fn(&mut ShellFileOpRequest), &str)> = vec![
        (
            "only start_line",
            |req| req.start_line = Some(10),
            "end_line is required when start_line is set for op=read",
        ),
        (
            "only end_line",
            |req| req.end_line = Some(20),
            "start_line is required when end_line is set for op=read",
        ),
        (
            "inverted line range",
            |req| {
                req.start_line = Some(20);
                req.end_line = Some(10);
            },
            "invalid line range",
        ),
        (
            "zero start_line",
            |req| {
                req.start_line = Some(0);
                req.end_line = Some(10);
            },
            "invalid line range",
        ),
        (
            "line field on read",
            |req| req.line = Some(10),
            "line is only allowed for op=insert_at_line",
        ),
        (
            "expected_prefix on read",
            |req| req.expected_prefix = Some("pub fn".to_string()),
            "expected_prefix is only allowed for line edit ops",
        ),
    ];

    for (case, mutate, expected) in cases {
        let mut req = file_request("read");
        mutate(&mut req);
        let err = validate_file_request(&req).unwrap_err();
        assert_eq!(err, expected, "case: {case}");
    }
}

#[test]
fn validate_file_request_allows_structured_edit_payload_ops() {
    for op in ["replace_in_file", "write_project_file"] {
        let mut req = file_request(op);
        req.content = Some(r#"{"path":"src/lib.rs"}"#.to_string());

        validate_file_request(&req).unwrap();
    }
}

#[test]
fn validate_file_request_rejects_structured_edit_extra_fields() {
    let mut req = file_request("write_project_file");
    req.content = Some(r#"{"path":"src/lib.rs"}"#.to_string());
    req.expected_sha256 =
        Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string());

    let err = validate_file_request(&req).unwrap_err();
    assert!(err.contains("expected_sha256 is only allowed"), "{err}");
}

#[tokio::test]
async fn registry_filters_lightweight_clients_by_auth_group() {
    let registry = ShellClientRegistry::default();
    let shared_a = crate::auth::shared_key::shared_key_context("token-a");
    let shared_b = crate::auth::shared_key::shared_key_context("token-b");
    let shared_hash = crate::auth::shared_key::shared_key_hash_of("token-a");
    let bridge_a = oauth_bridge_auth_context(&shared_hash, vec![]);
    let managed_oauth = managed_oauth_auth_context("alice", Some("hash-a"));
    let open = open_auth_context();
    let bootstrap = auth_context(None, true);

    for (client_id, auth) in [
        ("shared-a", &shared_a),
        ("shared-b", &shared_b),
        ("open", &open),
    ] {
        registry
            .register_with_auth(
                ShellClientRegisterRequest {
                    process_started_at: None,
                    build: None,
                    job_inventory: None,
                    client_id: client_id.to_string(),
                    agent_instance_id: format!("inst-{}", client_id),
                    display_name: None,
                    owner: None,
                    hostname: None,
                    capabilities: Some(async_job_capabilities()),
                    projects: Some(vec![project_summary(client_id, "/tmp/project")]),
                    agent_protocol_version: None,
                    policy: None,
                },
                Some(auth),
            )
            .await
            .unwrap();
    }
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "managed".to_string(),
            agent_instance_id: "inst-managed".to_string(),
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            capabilities: Some(async_job_capabilities()),
            projects: Some(vec![project_summary("managed", "/tmp/managed")]),
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();

    let visible_to_a: Vec<String> = registry
        .list_clients_for_auth(Some(&shared_a))
        .await
        .into_iter()
        .map(|c| c.client_id)
        .collect();
    assert_eq!(visible_to_a, vec!["shared-a"]);
    let visible_to_bridge_a: Vec<String> = registry
        .list_clients_for_auth(Some(&bridge_a))
        .await
        .into_iter()
        .map(|c| c.client_id)
        .collect();
    assert_eq!(visible_to_bridge_a, vec!["shared-a"]);
    assert!(registry
        .assert_client_access(Some(&shared_a), "shared-a")
        .await
        .is_ok());
    assert!(registry
        .assert_client_access(Some(&bridge_a), "shared-a")
        .await
        .is_ok());
    assert!(registry
        .assert_client_access(Some(&shared_a), "shared-b")
        .await
        .unwrap_err()
        .contains("unknown shell client"));
    assert!(registry
        .assert_client_access(Some(&shared_a), "open")
        .await
        .unwrap_err()
        .contains("unknown shell client"));
    assert!(registry
        .assert_client_access(Some(&bridge_a), "shared-b")
        .await
        .unwrap_err()
        .contains("unknown shell client"));
    assert!(registry
        .assert_client_access(Some(&bridge_a), "open")
        .await
        .unwrap_err()
        .contains("unknown shell client"));

    let visible_to_open: Vec<String> = registry
        .list_clients_for_auth(Some(&open))
        .await
        .into_iter()
        .map(|c| c.client_id)
        .collect();
    assert_eq!(visible_to_open, vec!["open"]);
    assert_eq!(
        ShellClientAuthGroup::from_auth(&open),
        Some(ShellClientAuthGroup::OpenAnonymous)
    );
    assert_eq!(
        ShellClientAuthGroup::from_auth(&bridge_a),
        Some(ShellClientAuthGroup::SharedKey(shared_hash))
    );
    assert!(bridge_a.is_oauth_shared_key_subject());
    assert_eq!(ShellClientAuthGroup::from_auth(&managed_oauth), None);
    assert!(!managed_oauth.is_oauth_shared_key_subject());
    let visible_to_managed_oauth: Vec<String> = registry
        .list_clients_for_auth(Some(&managed_oauth))
        .await
        .into_iter()
        .map(|c| c.client_id)
        .collect();
    assert_eq!(visible_to_managed_oauth, vec!["managed"]);
    assert!(registry
        .assert_client_access(Some(&managed_oauth), "managed")
        .await
        .is_ok());
    assert!(registry
        .assert_client_access(Some(&managed_oauth), "shared-a")
        .await
        .unwrap_err()
        .contains("unknown shell client"));

    let visible_to_bootstrap: Vec<String> = registry
        .list_clients_for_auth(Some(&bootstrap))
        .await
        .into_iter()
        .map(|c| c.client_id)
        .collect();
    assert_eq!(
        visible_to_bootstrap,
        vec!["managed", "open", "shared-a", "shared-b"]
    );
}

#[tokio::test]
async fn same_client_id_in_different_project_grants_is_isolated() {
    // Expected pre-fix failure: reusing the same instance id currently
    // lets a second auth group replace the first group's global lease.
    let registry = ShellClientRegistry::default();
    let grant_a = crate::auth::shared_key::project_credential_context("wc_pgrant_aaaaaaaaaaaaaaaa");
    let grant_b = crate::auth::shared_key::project_credential_context("wc_pgrant_bbbbbbbbbbbbbbbb");
    let registration = |hostname: &str, project: &str| ShellClientRegisterRequest {
        process_started_at: None,
        build: None,
        job_inventory: None,
        client_id: "same-project-agent".to_string(),
        agent_instance_id: "same-instance-id".to_string(),
        display_name: None,
        owner: None,
        hostname: Some(hostname.to_string()),
        capabilities: Some(async_job_capabilities()),
        projects: Some(vec![project_summary(project, "/tmp/project")]),
        agent_protocol_version: None,
        policy: None,
    };
    registry
        .register_with_auth(
            registration("grant-a-host", "grant-a-project"),
            Some(&grant_a),
        )
        .await
        .unwrap();

    let error = registry
        .register_with_auth(
            registration("grant-b-host", "grant-b-project"),
            Some(&grant_b),
        )
        .await
        .unwrap_err();
    assert!(!error.contains("grant-a-host"));
    assert!(!error.contains("grant-a-project"));
    let original = registry
        .get_client_view_for_auth("same-project-agent", Some(&grant_a))
        .await
        .expect("the original grant must retain its lease");
    assert_eq!(original.hostname.as_deref(), Some("grant-a-host"));
    assert!(registry
        .get_client_view_for_auth("same-project-agent", Some(&grant_b))
        .await
        .is_none());
}

#[test]
fn requested_by_from_auth_uses_bootstrap_username_or_anonymous() {
    let bootstrap = auth_context(None, true);
    assert_eq!(requested_by_from_auth(Some(&bootstrap)), "bootstrap");

    let alice = auth_context(Some("alice"), false);
    assert_eq!(requested_by_from_auth(Some(&alice)), "alice");

    assert_eq!(requested_by_from_auth(None), "anonymous");
}

#[test]
fn assert_shell_client_owner_enforces_owner_boundary() {
    let bootstrap = auth_context(None, true);
    assert!(assert_shell_client_owner(Some(&bootstrap), "client-1", None).is_ok());

    let alice = auth_context(Some("alice"), false);
    assert!(assert_shell_client_owner(Some(&alice), "client-1", Some("alice")).is_ok());

    let mismatch = assert_shell_client_owner(Some(&alice), "client-1", Some("bob")).unwrap_err();
    assert!(mismatch.contains("owned by bob"));
    assert!(mismatch.contains("belongs to alice"));

    let missing = assert_shell_client_owner(Some(&alice), "client-1", None).unwrap_err();
    assert_eq!(missing, "agent client client-1 has no owner");

    let anonymous = assert_shell_client_owner(None, "client-1", Some("anonymous")).unwrap_err();
    assert!(anonymous.contains("belongs to anonymous"));
}

#[tokio::test]
async fn registry_registers_and_lists_client() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "xrh".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: Some("XRH".to_string()),
            owner: Some("yyjeqhc".to_string()),
            hostname: Some("fineserver".to_string()),
            capabilities: None,
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    let clients = registry.list_clients().await;
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].client_id, "xrh");
    assert!(clients[0].connected);
    assert_eq!(clients[0].pending_requests, 0);
}

#[tokio::test]
async fn registry_register_saves_projects() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            capabilities: None,
            projects: Some(vec![project_summary("webcodex", "/root/git/webcodex")]),
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    let clients = registry.list_clients().await;
    assert_eq!(clients[0].projects.len(), 1);
    assert_eq!(clients[0].projects[0].id, "webcodex");

    let projects = registry.list_client_projects("oe").await.unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].path, "/root/git/webcodex");
}

#[tokio::test]
async fn registry_poll_updates_projects() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            capabilities: None,
            projects: Some(vec![project_summary("one", "/tmp/one")]),
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    let polled = registry
        .poll(ShellAgentPollRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            projects: Some(vec![
                project_summary("one", "/tmp/one"),
                project_summary("two", "/tmp/two"),
            ]),
        })
        .await
        .unwrap();
    assert!(polled.is_none());

    let projects = registry.list_client_projects("oe").await.unwrap();
    assert_eq!(projects.len(), 2);
    assert_eq!(projects[0].id, "one");
    assert_eq!(projects[1].id, "two");
}

#[tokio::test]
async fn registry_project_owner_check_enforces_boundary() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "alice-client".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            capabilities: None,
            projects: Some(vec![project_summary("webcodex", "/root/git/webcodex")]),
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "bob-client".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: Some("bob".to_string()),
            hostname: None,
            capabilities: None,
            projects: Some(vec![project_summary("secret", "/tmp/secret")]),
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();

    let alice = auth_context(Some("alice"), false);
    assert!(
        assert_registry_client_owner(&registry, Some(&alice), "alice-client")
            .await
            .is_ok()
    );
    let projects = registry.list_client_projects("alice-client").await.unwrap();
    assert_eq!(projects.len(), 1);

    let mismatch = assert_registry_client_owner(&registry, Some(&alice), "bob-client")
        .await
        .unwrap_err();
    assert_eq!(mismatch.0, StatusCode::FORBIDDEN);
    assert!(mismatch.1.contains("owned by bob"));
}

#[test]
fn protocol_async_capability_defaults_false() {
    let capabilities = ShellClientCapabilities::default();
    assert!(!capabilities.async_jobs);
    assert!(!capabilities.async_shell_jobs);
    assert!(!capabilities.structured_validation_argv);

    let request: ShellClientRegisterRequest = serde_json::from_str(
        r#"{
            "client_id": "oe",
            "agent_instance_id": "inst-1",
            "capabilities": {"shell": true}
        }"#,
    )
    .unwrap();
    let capabilities = request.capabilities.unwrap();
    assert!(!capabilities.async_jobs);
    assert!(!capabilities.async_shell_jobs);
    assert!(!capabilities.structured_validation_argv);
}

#[test]
fn protocol_serde_keeps_old_register_compatible() {
    let request: ShellClientRegisterRequest = serde_json::from_str(
        r#"{
            "client_id": "oe",
            "agent_instance_id": "inst-1",
            "capabilities": {"shell": true, "file_read": true}
        }"#,
    )
    .unwrap();
    assert_eq!(request.client_id, "oe");
    assert!(request.projects.is_none());
    // Old agents omit agent_protocol_version; the field deserializes as None.
    assert!(request.agent_protocol_version.is_none());
}

#[test]
fn protocol_serde_parses_agent_protocol_version() {
    let request: ShellClientRegisterRequest = serde_json::from_str(
        r#"{
            "client_id": "oe",
            "agent_instance_id": "inst-1",
            "agent_protocol_version": "polling-v1"
        }"#,
    )
    .unwrap();
    assert_eq!(
        request.agent_protocol_version.as_deref(),
        Some("polling-v1")
    );
}

#[tokio::test]
async fn register_without_protocol_version_defaults_to_unknown() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: None,
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    let clients = registry.list_clients().await;
    assert_eq!(clients[0].agent_protocol_version, "unknown");
}

#[tokio::test]
async fn register_with_protocol_version_is_exposed_in_view() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "xrh".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            capabilities: None,
            projects: None,
            agent_protocol_version: Some("polling-v1".to_string()),
            policy: None,
        })
        .await
        .unwrap();
    let clients = registry.list_clients().await;
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].client_id, "xrh");
    assert_eq!(clients[0].agent_protocol_version, "polling-v1");
    let view = registry.get_client_view("xrh").await.unwrap();
    assert_eq!(view.agent_protocol_version, "polling-v1");
}

#[tokio::test]
async fn register_blank_protocol_version_falls_back_to_unknown() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: None,
            projects: None,
            agent_protocol_version: Some("   ".to_string()),
            policy: None,
        })
        .await
        .unwrap();
    let clients = registry.list_clients().await;
    assert_eq!(clients[0].agent_protocol_version, "unknown");
}

#[tokio::test]
async fn client_supports_reflects_registered_capabilities() {
    let registry = ShellClientRegistry::default();
    let mut caps = ShellClientCapabilities::default();
    caps.shell = true;
    caps.file_read = true;
    caps.async_shell_jobs = true;
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: Some(caps),
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    assert!(registry
        .client_supports("oe", SHELL_CLIENT_CAPABILITY_SHELL)
        .await
        .unwrap());
    assert!(registry
        .client_supports("oe", SHELL_CLIENT_CAPABILITY_FILE_READ)
        .await
        .unwrap());
    assert!(registry
        .client_supports("oe", SHELL_CLIENT_CAPABILITY_ASYNC_SHELL_JOBS)
        .await
        .unwrap());
    assert!(!registry
        .client_supports("oe", SHELL_CLIENT_CAPABILITY_GIT)
        .await
        .unwrap());
    // Unknown capability name is false, not an error.
    assert!(!registry.client_supports("oe", "teleport").await.unwrap());
    // Unknown client is a structured error.
    let err = registry
        .client_supports("ghost", SHELL_CLIENT_CAPABILITY_SHELL)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        ShellClientLookupError::UnknownClient {
            client_id: "ghost".to_string()
        }
    );
    let err = registry.get_client_capabilities("ghost").await.unwrap_err();
    assert_eq!(
        err,
        ShellClientLookupError::UnknownClient {
            client_id: "ghost".to_string()
        }
    );
}

#[tokio::test]
async fn client_supports_recognizes_all_protocol_capability_names() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: Some(crate::shell_protocol::ShellJobInventory {
                active_complete: true,
                jobs: Vec::new(),
            }),
            client_id: "all".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: Some(ShellClientCapabilities {
                shell: true,
                file_read: true,
                file_write: true,
                git: true,
                jobs: true,
                async_jobs: true,
                async_shell_jobs: true,
                ssh_shell: true,
                persistent_shell: true,
                ssh_persistent_shell: true,
                structured_validation_argv: true,
                lsp_read_only_navigation: true,
                sandbox_inspect_commands: true,
                project_lifecycle: true,
                job_state_reconciliation: true,
            }),
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();

    for capability in SHELL_CLIENT_CAPABILITY_NAMES {
        assert!(
            registry.client_supports("all", capability).await.unwrap(),
            "shell client matcher must recognize protocol capability {capability}"
        );
    }
}

#[tokio::test]
async fn registry_enqueues_polls_and_completes_shell_request() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "xrh".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: None,
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    let (request_id, rx) = registry
        .enqueue_run(
            ShellRunRequest {
                client_id: "xrh".to_string(),
                cwd: Some("/tmp".to_string()),
                command: "echo hello".to_string(),
                stdin: Some("hello stdin".to_string()),
                timeout_secs: 10,
                wait_timeout_secs: 1,
            },
            "test".to_string(),
        )
        .await
        .unwrap();
    let polled = registry
        .poll(ShellAgentPollRequest {
            client_id: "xrh".to_string(),
            agent_instance_id: "inst".to_string(),
            projects: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(polled.request_id, request_id);
    assert_eq!(polled.command, "echo hello");
    assert_eq!(polled.stdin.as_deref(), Some("hello stdin"));
    registry
        .complete(ShellAgentResultRequest {
            client_id: "xrh".to_string(),
            agent_instance_id: "inst".to_string(),
            request_id,
            exit_code: Some(0),
            stdout: Some("hello\n".to_string()),
            stderr: Some(String::new()),
            duration_ms: Some(12),
            error: None,
        })
        .await
        .unwrap();
    let response = rx.await.unwrap();
    assert!(response.success);
    assert_eq!(response.stdout.as_deref(), Some("hello\n"));
}

#[tokio::test]
async fn registry_allows_session_scoped_run_without_ssh_resource() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "xrh".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: None,
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();

    let (request_id, _rx) = registry
        .enqueue_run_with_sandbox_and_ssh(
            ShellRunRequest {
                client_id: "xrh".to_string(),
                cwd: None,
                command: "echo local".to_string(),
                stdin: None,
                timeout_secs: 10,
                wait_timeout_secs: 1,
            },
            "test".to_string(),
            None,
            None,
            Some("wc_sess_local".to_string()),
        )
        .await
        .unwrap();
    assert!(!registry.cancel_request(&request_id).await);

    let error = registry
        .enqueue_run_with_sandbox_and_ssh(
            ShellRunRequest {
                client_id: "xrh".to_string(),
                cwd: None,
                command: "echo remote".to_string(),
                stdin: None,
                timeout_secs: 10,
                wait_timeout_secs: 1,
            },
            "test".to_string(),
            None,
            Some("tmp".to_string()),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.contains("ssh_session_required"), "{error}");
}

#[tokio::test]
async fn registry_rejects_unknown_client_run() {
    let registry = ShellClientRegistry::default();
    let err = registry
        .enqueue_run(
            ShellRunRequest {
                client_id: "missing".to_string(),
                cwd: None,
                command: "pwd".to_string(),
                stdin: None,
                timeout_secs: 10,
                wait_timeout_secs: 1,
            },
            "test".to_string(),
        )
        .await
        .unwrap_err();
    assert!(err.contains("unknown shell client"));
}

fn lsp_status_payload() -> AgentLspPayload {
    AgentLspPayload {
        project_id: "demo".to_string(),
        request: AgentLspRequest::Status,
    }
}

async fn register_lsp_test_client(
    registry: &ShellClientRegistry,
    client_id: &str,
    lsp_capable: bool,
) {
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: client_id.to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: Some(ShellClientCapabilities {
                lsp_read_only_navigation: lsp_capable,
                ..Default::default()
            }),
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn enqueue_lsp_returns_structured_unknown_client_error() {
    let registry = ShellClientRegistry::default();
    let error = registry
        .enqueue_lsp(
            "missing".to_string(),
            lsp_status_payload(),
            "test".to_string(),
            5,
        )
        .await
        .unwrap_err();

    assert_eq!(
        error,
        EnqueueLspError::UnknownClient {
            client_id: "missing".to_string()
        }
    );
    assert_eq!(error.to_string(), "unknown shell client: missing");
}

#[tokio::test]
async fn enqueue_lsp_returns_structured_unsupported_capability_error() {
    let registry = ShellClientRegistry::default();
    register_lsp_test_client(&registry, "legacy", false).await;
    let error = registry
        .enqueue_lsp(
            "legacy".to_string(),
            lsp_status_payload(),
            "test".to_string(),
            5,
        )
        .await
        .unwrap_err();

    assert_eq!(
        error,
        EnqueueLspError::UnsupportedCapability {
            client_id: "legacy".to_string()
        }
    );
    assert_eq!(
        error.to_string(),
        "agent client legacy does not support lsp_read_only_navigation"
    );
}

#[tokio::test]
async fn enqueue_lsp_returns_structured_offline_client_error() {
    let registry = ShellClientRegistry::default();
    register_lsp_test_client(&registry, "stale-lsp", true).await;
    registry
        .set_last_seen_for_test("stale-lsp", now_ts() - CLIENT_ONLINE_WINDOW_SECS - 1)
        .await;
    let error = registry
        .enqueue_lsp(
            "stale-lsp".to_string(),
            lsp_status_payload(),
            "test".to_string(),
            5,
        )
        .await
        .unwrap_err();

    assert_eq!(
        error,
        EnqueueLspError::ClientOffline {
            client_id: "stale-lsp".to_string()
        }
    );
}

#[tokio::test]
async fn enqueue_lsp_returns_structured_queue_full_error() {
    let registry = ShellClientRegistry::default();
    register_lsp_test_client(&registry, "full-lsp", true).await;
    {
        let mut inner = registry.inner.lock().await;
        inner.queues_by_client.insert(
            "full-lsp".to_string(),
            (0..MAX_QUEUED_REQUESTS_PER_CLIENT)
                .map(|index| format!("queued-{index}"))
                .collect(),
        );
    }
    let error = registry
        .enqueue_lsp(
            "full-lsp".to_string(),
            lsp_status_payload(),
            "test".to_string(),
            5,
        )
        .await
        .unwrap_err();

    assert_eq!(
        error,
        EnqueueLspError::QueueFull {
            client_id: "full-lsp".to_string(),
            limit: MAX_QUEUED_REQUESTS_PER_CLIENT,
        }
    );
}

async fn register_quic_v1_client(registry: &ShellClientRegistry, client_id: &str) {
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: client_id.to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            capabilities: Some(async_job_capabilities()),
            projects: Some(vec![project_summary("webcodex", "/tmp/webcodex")]),
            agent_protocol_version: Some(AGENT_PROTOCOL_VERSION_QUIC_V1.to_string()),
            policy: None,
        })
        .await
        .unwrap();
    registry
        .set_transport(client_id, TRANSPORT_QUIC)
        .await
        .unwrap();
}

#[tokio::test]
async fn registry_allows_quic_v1_run_queueing() {
    let registry = ShellClientRegistry::default();
    register_quic_v1_client(&registry, "quic-run").await;

    let (_request_id, _rx) = registry
        .enqueue_run(
            ShellRunRequest {
                client_id: "quic-run".to_string(),
                cwd: None,
                command: "echo hi".to_string(),
                stdin: None,
                timeout_secs: 5,
                wait_timeout_secs: 0,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();
    let view = registry.get_client_view("quic-run").await.unwrap();
    assert_eq!(view.transport, TRANSPORT_QUIC);
    assert_eq!(view.agent_protocol_version, AGENT_PROTOCOL_VERSION_QUIC_V1);
    assert_eq!(view.pending_requests, 1);
    assert!(view.capabilities.shell);
    assert!(view.capabilities.async_shell_jobs);
}

#[tokio::test]
async fn enqueue_file_op_allows_read_with_line_range() {
    let registry = ShellClientRegistry::default();
    register_quic_v1_client(&registry, "oe").await;

    let mut req = file_request("read");
    req.start_line = Some(7);
    req.end_line = Some(12);
    let (request_id, _rx) = registry
        .enqueue_file_op(req, "tester".to_string())
        .await
        .unwrap();

    let polled = registry
        .poll(ShellAgentPollRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            projects: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(polled.request_id, request_id);
    assert_eq!(polled.kind, "file_read");
    assert_eq!(polled.path.as_deref(), Some("src/auth/scopes.rs"));
    assert_eq!(polled.start_line, Some(7));
    assert_eq!(polled.end_line, Some(12));
    assert_eq!(polled.line, None);
}

#[tokio::test]
async fn registry_allows_quic_v1_file_and_project_ops_queueing() {
    let registry = ShellClientRegistry::default();
    register_quic_v1_client(&registry, "quic-ops").await;

    let (_file_request_id, _file_rx) = registry
        .enqueue_file_op(
            ShellFileOpRequest {
                op: "read".to_string(),
                client_id: "quic-ops".to_string(),
                path: "README.md".to_string(),
                cwd: None,
                content: None,
                max_bytes: None,
                old_text: None,
                pattern: None,
                expected_sha256: None,
                expected_prefix: None,
                start_line: None,
                end_line: None,
                line: None,
                create_dirs: false,
                wait_timeout_secs: 0,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();

    let (_project_request_id, _project_rx) = registry
        .enqueue_project_op(
            "quic-ops".to_string(),
            "register_project",
            "{}".to_string(),
            "tester".to_string(),
        )
        .await
        .unwrap();

    let view = registry.get_client_view("quic-ops").await.unwrap();
    assert_eq!(view.pending_requests, 2);
}

#[tokio::test]
async fn registry_allows_quic_v1_start_job_queueing() {
    let registry = ShellClientRegistry::default();
    register_quic_v1_client(&registry, "quic-job").await;

    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("quic-job".to_string()),
                cwd: None,
                command: Some("sleep 1".to_string()),
                timeout_secs: Some(5),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();

    let view = registry.get_client_view("quic-job").await.unwrap();
    assert_eq!(view.pending_requests, 1);
    assert_eq!(job.status, "queued");
    assert_eq!(registry.list_jobs(Some(10)).await.len(), 1);
}

#[tokio::test]
async fn registry_allows_quic_v1_stop_job_delivery_queueing() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "quic-stop".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            capabilities: Some(async_job_capabilities()),
            projects: None,
            agent_protocol_version: Some(AGENT_PROTOCOL_VERSION_QUIC_V1.to_string()),
            policy: None,
        })
        .await
        .unwrap();
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("quic-stop".to_string()),
                cwd: None,
                command: Some("sleep 10".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();
    let _ = registry
        .poll(ShellAgentPollRequest {
            client_id: "quic-stop".to_string(),
            agent_instance_id: "inst".to_string(),
            projects: None,
        })
        .await
        .unwrap()
        .unwrap();
    registry
        .set_transport("quic-stop", TRANSPORT_QUIC)
        .await
        .unwrap();

    let stopped = registry
        .stop_job(&job.job_id, "tester".to_string())
        .await
        .unwrap();
    let view = registry.get_client_view("quic-stop").await.unwrap();
    assert_eq!(view.pending_requests, 1);
    assert_eq!(stopped.status, "stop_requested");
}

#[test]
fn validate_run_request_allows_bounded_stdin_beyond_command_limit() {
    let body = ShellRunRequest {
        client_id: "client-1".to_string(),
        cwd: None,
        command: "cat >/dev/null".to_string(),
        stdin: Some("x".repeat(MAX_COMMAND_LEN + 1024)),
        timeout_secs: 10,
        wait_timeout_secs: 1,
    };
    validate_run_request(&body).expect("stdin has its own larger bound");
}

#[test]
fn validate_run_request_rejects_oversized_stdin() {
    let body = ShellRunRequest {
        client_id: "client-1".to_string(),
        cwd: None,
        command: "cat >/dev/null".to_string(),
        stdin: Some("x".repeat(MAX_RUN_STDIN_BYTES + 1)),
        timeout_secs: 10,
        wait_timeout_secs: 1,
    };
    let err = validate_run_request(&body).unwrap_err();
    assert!(err.contains("stdin is too large"), "got: {}", err);
}

#[tokio::test]
async fn registry_shell_job_start_poll_complete_and_log() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: Some(async_job_capabilities()),
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: Some("/tmp".to_string()),
                command: Some("printf hello".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: Some(ShellJobCodexMetadata {
                    project: Some("demo".to_string()),
                    goal_id: Some("goal-1".to_string()),
                    client_request_id: Some("crid-1".to_string()),
                    command: Some("printf hello".to_string()),
                    kind: Some("command".to_string()),
                    suite: None,
                    script_path: None,
                    reason: Some("test job".to_string()),
                    max_runtime_secs: Some(10),
                }),
            },
            "test".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(job.status, "queued");
    assert_eq!(
        job.codex
            .as_ref()
            .and_then(|codex| codex.client_request_id.as_deref()),
        Some("crid-1")
    );
    let polled = registry
        .poll(ShellAgentPollRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            projects: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(polled.command, "printf hello");
    let running = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(running.status, "agent_queued");
    registry
        .complete(ShellAgentResultRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            request_id: polled.request_id,
            exit_code: Some(0),
            stdout: Some("hello\n".to_string()),
            stderr: Some(String::new()),
            duration_ms: Some(20),
            error: None,
        })
        .await
        .unwrap();
    let done = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(done.status, "completed");
    assert_eq!(done.exit_code, Some(0));
    assert_eq!(
        done.codex
            .as_ref()
            .and_then(|codex| codex.project.as_deref()),
        Some("demo")
    );
    let listed = registry.list_jobs(Some(10)).await;
    assert_eq!(
        listed
            .iter()
            .find(|listed| listed.job_id == job.job_id)
            .and_then(|listed| listed.codex.as_ref())
            .and_then(|codex| codex.goal_id.as_deref()),
        Some("goal-1")
    );
    let (_info, stdout, stderr, next_stdout, next_stderr) = registry
        .job_log(&job.job_id, Some(1), Some(1), None)
        .await
        .unwrap();
    assert_eq!(stdout.as_deref(), Some("hello\n"));
    assert_eq!(stderr.as_deref(), Some(""));
    assert_eq!(next_stdout, 2);
    assert_eq!(next_stderr, 1);
}

#[tokio::test]
async fn registry_shell_job_stop_cancels_queued_job() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: Some(async_job_capabilities()),
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: None,
                command: Some("sleep 10".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "test".to_string(),
        )
        .await
        .unwrap();
    let stopped = registry
        .stop_job(&job.job_id, "test".to_string())
        .await
        .unwrap();
    assert_eq!(stopped.status, "stopped");
    let polled = registry
        .poll(ShellAgentPollRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            projects: None,
        })
        .await
        .unwrap();
    assert!(polled.is_none());
}

#[tokio::test]
async fn registry_shell_job_stop_running_delivers_stop_to_client() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: Some(async_job_capabilities()),
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: None,
                command: Some("sleep 10".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "test".to_string(),
        )
        .await
        .unwrap();
    let started = registry
        .poll(ShellAgentPollRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            projects: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(started.kind, "start_job");

    let stop_requested = registry
        .stop_job(&job.job_id, "test".to_string())
        .await
        .unwrap();
    assert_eq!(stop_requested.status, "stop_requested");
    let stop = registry
        .poll(ShellAgentPollRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            projects: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stop.kind, "stop_job");
    assert_eq!(stop.job_id.as_deref(), Some(job.job_id.as_str()));
}

#[tokio::test]
async fn registry_marks_running_job_lost_when_client_stale() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: Some(async_job_capabilities()),
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: None,
                command: Some("sleep 10".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "test".to_string(),
        )
        .await
        .unwrap();
    let _ = registry
        .poll(ShellAgentPollRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            projects: None,
        })
        .await
        .unwrap()
        .unwrap();
    {
        let mut inner = registry.inner.lock().await;
        let client = inner.clients.get_mut("oe").unwrap();
        client.last_seen = now_ts() - CLIENT_ONLINE_WINDOW_SECS - 1;
    }
    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert!(lost.error.unwrap().contains("stale"));
}

#[tokio::test]
async fn touch_client_refreshes_stale_client_back_to_online() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: Some(async_job_capabilities()),
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();

    // Age the client past the online window so it reads as stale.
    registry
        .set_last_seen_for_test("oe", now_ts() - CLIENT_ONLINE_WINDOW_SECS - 1)
        .await;
    let stale = registry.get_client_view("oe").await.unwrap();
    assert!(!stale.connected);
    assert_eq!(stale.status, "stale");

    // A keepalive touch must bring it back online.
    registry.touch_client("oe", "inst").await.unwrap();
    let fresh = registry.get_client_view("oe").await.unwrap();
    assert!(fresh.connected);
    assert_eq!(fresh.status, "online");

    // Unknown client_id is a clear error and does not mutate state.
    assert!(registry.touch_client("nope", "inst").await.is_err());
}

#[tokio::test]
async fn touch_client_refreshes_websocket_transport_client() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "ws-1".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: Some(async_job_capabilities()),
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    registry
        .set_transport("ws-1", TRANSPORT_WEBSOCKET)
        .await
        .unwrap();

    registry
        .set_last_seen_for_test("ws-1", now_ts() - CLIENT_ONLINE_WINDOW_SECS - 1)
        .await;
    let stale = registry.get_client_view("ws-1").await.unwrap();
    assert_eq!(stale.transport, "websocket");
    assert!(!stale.connected);

    registry.touch_client("ws-1", "inst").await.unwrap();
    let fresh = registry.get_client_view("ws-1").await.unwrap();
    assert_eq!(fresh.transport, "websocket");
    assert!(fresh.connected);
    assert_eq!(fresh.status, "online");
}

#[tokio::test]
async fn touch_client_rejects_stale_instance_and_accepts_active() {
    // Regression: a stale/replaced instance must not refresh the active
    // lease's `last_seen` via Ping/Pong keepalive.
    let registry = ShellClientRegistry::default();
    // Instance A registers and is online.
    let view_a = register_with_instance(&registry, "oe", "inst-a").await;
    assert!(view_a.connected);

    // Age A out so a newer instance may take over the lease.
    registry
        .set_last_seen_for_test("oe", now_ts() - CLIENT_ONLINE_WINDOW_SECS - 1)
        .await;
    // Instance B replaces A.
    let view_b = register_with_instance(&registry, "oe", "inst-b").await;
    assert_eq!(view_b.agent_instance_id, "inst-b");
    assert!(view_b.connected);

    // Capture B's last_seen right after registration.
    let before = registry.get_client_view("oe").await.unwrap().last_seen;
    // Sleep a moment so a successful touch would observably advance
    // last_seen.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    // Stale instance A's keepalive must be rejected and must NOT advance
    // last_seen for B.
    let err = registry.touch_client("oe", "inst-a").await.unwrap_err();
    assert!(
        err.contains("no longer the active instance"),
        "error was: {err}"
    );
    let after_a = registry.get_client_view("oe").await.unwrap().last_seen;
    assert_eq!(
        after_a, before,
        "stale instance touch must not refresh active last_seen"
    );
    // A stale instance must not resurrect the client to online either.
    let view_after_a = registry.get_client_view("oe").await.unwrap();
    assert!(view_after_a.connected);

    // Active instance B's keepalive succeeds and refreshes last_seen.
    registry.touch_client("oe", "inst-b").await.unwrap();
    let after_b = registry.get_client_view("oe").await.unwrap().last_seen;
    assert!(
        after_b > before,
        "active instance touch must refresh last_seen"
    );
    assert!(registry.get_client_view("oe").await.unwrap().connected);

    // An empty agent_instance_id is rejected by validation.
    assert!(registry.touch_client("oe", "").await.is_err());
}

#[test]
fn enforce_register_owner_cases() {
    let bootstrap = auth_context(None, true);
    let user_alice = auth_context(Some("alice"), false);
    let agent_alice = agent_auth_context(
        "alice",
        "alice-laptop",
        vec![
            "agent:register",
            "agent:poll",
            "agent:result",
            "agent:job_update",
        ],
    );
    let agent_alice_register_only =
        agent_auth_context("alice", "alice-laptop", vec!["agent:register"]);

    // (case, auth, client_id, owner, Ok or Err(required error fragments)).
    let cases = vec![
        // No AuthMiddleware (unit tests): defer to the middleware, which in
        // production rejects anonymous requests before the handler runs.
        (
            "no auth skips with owner",
            None,
            "client-1",
            Some("anyone"),
            Ok(()),
        ),
        (
            "no auth skips without owner",
            None,
            "client-1",
            None,
            Ok(()),
        ),
        // Bootstrap may register any owner.
        (
            "bootstrap allows missing owner",
            Some(&bootstrap),
            "client-1",
            None,
            Ok(()),
        ),
        (
            "bootstrap allows any owner",
            Some(&bootstrap),
            "client-1",
            Some("bob"),
            Ok(()),
        ),
        // Phase 3: user tokens (Phase 2 personal API tokens) are no longer
        // allowed on agent transport endpoints. Only bootstrap or agent
        // tokens may register.
        (
            "user token is rejected",
            Some(&user_alice),
            "client-1",
            Some("alice"),
            Err(vec!["user tokens are not allowed"]),
        ),
        // Matching client_id + matching owner -> Ok.
        (
            "agent token matching client_id and owner",
            Some(&agent_alice),
            "alice-laptop",
            Some("alice"),
            Ok(()),
        ),
        // Matching client_id + missing owner -> Ok (owner filled in by the
        // caller via effective_register_owner).
        (
            "agent token matching client_id, missing owner",
            Some(&agent_alice),
            "alice-laptop",
            None,
            Ok(()),
        ),
        (
            "agent token wrong client_id rejected",
            Some(&agent_alice_register_only),
            "other-laptop",
            None,
            Err(vec!["not bound to client_id"]),
        ),
        (
            "agent token owner mismatch rejected",
            Some(&agent_alice_register_only),
            "alice-laptop",
            Some("bob"),
            Err(vec!["agent token owner is 'alice'", "bob"]),
        ),
    ];

    for (case, auth, client_id, owner, expected) in cases {
        let result = enforce_register_owner(auth, client_id, owner);
        match expected {
            Ok(()) => assert!(result.is_ok(), "case '{case}': got: {result:?}"),
            Err(fragments) => {
                let err = result.expect_err(&format!("case '{case}': expected an error"));
                for fragment in fragments {
                    assert!(
                        err.contains(fragment),
                        "case '{case}': missing '{fragment}' in error: {err}"
                    );
                }
            }
        }
    }
}

#[test]
fn effective_register_owner_agent_token_fills_username() {
    let alice = agent_auth_context("alice", "alice-laptop", vec!["agent:register"]);
    // Missing owner -> filled with the token's username.
    assert_eq!(
        effective_register_owner(Some(&alice), None),
        Some("alice".to_string())
    );
    // Matching owner preserved.
    assert_eq!(
        effective_register_owner(Some(&alice), Some("alice")),
        Some("alice".to_string())
    );
    // Bootstrap keeps the request owner.
    let bootstrap = auth_context(None, true);
    assert_eq!(
        effective_register_owner(Some(&bootstrap), Some("bob")),
        Some("bob".to_string())
    );
}

#[test]
fn enforce_agent_transport_rejects_user_token() {
    let alice = auth_context(Some("alice"), false);
    let err = enforce_agent_transport(Some(&alice), "client-1").unwrap_err();
    assert!(err.contains("user tokens are not allowed"), "got: {}", err);
}

#[test]
fn enforce_agent_transport_agent_token_matching_client_succeeds() {
    let alice = agent_auth_context("alice", "alice-laptop", vec!["agent:poll"]);
    assert!(enforce_agent_transport(Some(&alice), "alice-laptop").is_ok());
    let err = enforce_agent_transport(Some(&alice), "other").unwrap_err();
    assert!(err.contains("not bound"), "got: {}", err);
}

#[test]
fn enforce_agent_transport_bootstrap_succeeds() {
    let bootstrap = auth_context(None, true);
    assert!(enforce_agent_transport(Some(&bootstrap), "any-client").is_ok());
}

#[test]
fn require_agent_transport_scope_agent_token_with_scope_succeeds() {
    let alice = agent_auth_context("alice", "alice-laptop", vec!["agent:poll"]);
    assert!(require_agent_transport_scope(Some(&alice), "agent:poll").is_ok());
    assert!(require_agent_transport_scope(Some(&alice), "agent:register").is_err());
}

#[test]
fn require_agent_transport_scope_bootstrap_always_succeeds() {
    let bootstrap = auth_context(None, true);
    assert!(require_agent_transport_scope(Some(&bootstrap), "agent:register").is_ok());
}

#[test]
fn require_agent_transport_scope_user_token_rejected() {
    let alice = auth_context(Some("alice"), false);
    let err = require_agent_transport_scope(Some(&alice), "agent:register").unwrap_err();
    assert!(err.contains("missing required scope"), "got: {}", err);
}

#[test]
fn oauth_bridge_token_remains_blocked_from_agent_transport() {
    let bridge = oauth_bridge_auth_context(
        "hash-a",
        vec![
            "agent:register",
            "agent:poll",
            "agent:result",
            "agent:job_update",
        ],
    );
    assert!(!bridge.is_lightweight());
    assert!(enforce_agent_transport(Some(&bridge), "client-a")
        .unwrap_err()
        .contains("user tokens are not allowed"));
    assert!(
        require_agent_transport_scope(Some(&bridge), "agent:register")
            .unwrap_err()
            .contains("missing required scope")
    );
}

#[tokio::test]
async fn registry_rejects_enqueue_when_queue_full() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "full".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            capabilities: None,
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    // Fill the queue to the limit without any consumer draining it.
    for _ in 0..MAX_QUEUED_REQUESTS_PER_CLIENT {
        registry
            .enqueue_run(
                ShellRunRequest {
                    client_id: "full".to_string(),
                    cwd: None,
                    command: "echo hi".to_string(),
                    stdin: None,
                    timeout_secs: 5,
                    wait_timeout_secs: 0,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();
    }
    // The next enqueue must be rejected with a structured error instead
    // of growing the queue unboundedly.
    let err = registry
        .enqueue_run(
            ShellRunRequest {
                client_id: "full".to_string(),
                cwd: None,
                command: "echo hi".to_string(),
                stdin: None,
                timeout_secs: 5,
                wait_timeout_secs: 0,
            },
            "tester".to_string(),
        )
        .await
        .unwrap_err();
    assert!(err.contains("too many pending requests"));
    assert!(err.contains("full"));
    // The queue is exactly at the cap; memory is bounded.
    let view = registry.get_client_view("full").await.unwrap();
    assert_eq!(view.pending_requests, MAX_QUEUED_REQUESTS_PER_CLIENT);
}

#[tokio::test]
async fn registry_rejects_enqueue_when_client_offline() {
    // Registered-but-stale agents must fail fast at enqueue rather than
    // accepting work that can only time out (or fill the 256-deep queue).
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "stale".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: None,
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    registry
        .set_last_seen_for_test("stale", now_ts() - CLIENT_ONLINE_WINDOW_SECS - 1)
        .await;

    let err = registry
        .enqueue_run(
            ShellRunRequest {
                client_id: "stale".to_string(),
                cwd: None,
                command: "echo hi".to_string(),
                stdin: None,
                timeout_secs: 5,
                wait_timeout_secs: 0,
            },
            "tester".to_string(),
        )
        .await
        .unwrap_err();
    assert!(
        err.contains("offline"),
        "enqueue against a stale agent must fail fast as offline: {err}"
    );
    let view = registry.get_client_view("stale").await.unwrap();
    assert_eq!(view.pending_requests, 0);
    assert!(!view.connected);
}

#[tokio::test]
async fn reconcile_disconnect_marks_running_jobs_lost() {
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: Some(async_job_capabilities()),
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: None,
                command: Some("sleep 10".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "test".to_string(),
        )
        .await
        .unwrap();
    // Job is "queued" with its request sitting in the client's queue.
    let before = registry.get_client_view("oe").await.unwrap();
    assert_eq!(before.pending_requests, 1);
    // Transport disconnects (e.g. WebSocket dropped).
    registry.reconcile_disconnect("oe", "inst").await;
    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert!(lost.error.unwrap().contains("disconnected"));
    // Pending request was dropped: no dangling waiter / queue entry.
    let after = registry.get_client_view("oe").await.unwrap();
    assert_eq!(after.pending_requests, 0);
}

#[tokio::test]
async fn reconcile_disconnect_fails_pending_sync_requests_fast() {
    // Regression guard for the MCP "no reply" hang: a synchronous tool
    // request (run_shell/read_file/... with job_id: None) whose agent drops
    // mid-flight must be resolved immediately, not parked until the caller's
    // wait timeout.
    let registry = ShellClientRegistry::default();
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "oe".to_string(),
            agent_instance_id: "inst".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: None,
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap();
    let (_request_id, rx) = registry
        .enqueue_run(
            ShellRunRequest {
                client_id: "oe".to_string(),
                cwd: Some("/tmp".to_string()),
                command: "echo hi".to_string(),
                stdin: None,
                timeout_secs: 30,
                wait_timeout_secs: 30,
            },
            "test".to_string(),
        )
        .await
        .unwrap();
    let before = registry.get_client_view("oe").await.unwrap();
    assert_eq!(before.pending_requests, 1);

    // Agent transport drops before returning a result.
    registry.reconcile_disconnect("oe", "inst").await;

    // Waiter resolves promptly with a disconnect error rather than parking
    // for the full 30s wait timeout. The short timeout turns a regression
    // (unbounded park) into a fast test failure instead of a hang.
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
        .await
        .expect("waiter must resolve promptly, not park until the caller timeout")
        .expect("waiter must be resolved, not dropped");
    assert!(!response.success);
    let error = response.error.expect("disconnect must set an error");
    assert!(
        error.contains("offline"),
        "error should classify as agent_offline: {error}"
    );
    // No dangling waiter or queue entry remains.
    let after = registry.get_client_view("oe").await.unwrap();
    assert_eq!(after.pending_requests, 0);
}

#[tokio::test]
async fn reconcile_disconnect_releases_active_lease_immediately() {
    let registry = ShellClientRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;

    registry.reconcile_disconnect("oe", "inst-a").await;

    let offline = registry.get_client_view("oe").await.unwrap();
    assert!(
        !offline.connected,
        "active disconnect must immediately leave online window"
    );
    assert!(now_ts().saturating_sub(offline.last_seen) > CLIENT_ONLINE_WINDOW_SECS);

    let new_view = register_with_instance(&registry, "oe", "inst-b").await;
    assert_eq!(new_view.agent_instance_id, "inst-b");
    assert!(
        new_view.connected,
        "new instance should register without waiting 60 seconds"
    );
}

// ------------------------------------------------------------------------
// Agent instance identity / lease model (Phase 1)
// ------------------------------------------------------------------------

/// Helper: register a client with an explicit `agent_instance_id`.
async fn register_with_instance(
    registry: &ShellClientRegistry,
    client_id: &str,
    instance: &str,
) -> ShellClientView {
    registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: client_id.to_string(),
            agent_instance_id: instance.to_string(),
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            capabilities: Some(async_job_capabilities()),
            projects: None,
            agent_protocol_version: Some("polling-v1".to_string()),
            policy: None,
        })
        .await
        .unwrap()
}

/// Helper: register a long-lived-transport (WebSocket/QUIC) client bound to
/// a server-internal `connection_id`. Mirrors what `agent_ws`/`agent_quic`
/// do at register time. Returns the view along with the connection_id so a
/// test can drive the connection-scoped poll/touch/result/update APIs.
async fn register_with_connection(
    registry: &ShellClientRegistry,
    client_id: &str,
    instance: &str,
    connection_id: &str,
) -> ShellClientView {
    registry
        .register_with_auth_connection(
            ShellClientRegisterRequest {
                process_started_at: None,
                build: None,
                job_inventory: None,
                client_id: client_id.to_string(),
                agent_instance_id: instance.to_string(),
                display_name: None,
                owner: Some("alice".to_string()),
                hostname: None,
                capabilities: Some(async_job_capabilities()),
                projects: None,
                agent_protocol_version: Some("polling-v1".to_string()),
                policy: None,
            },
            None,
            Some(connection_id),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn lease_first_register_accepts_instance() {
    let registry = ShellClientRegistry::default();
    let view = register_with_instance(&registry, "oe", "inst-a").await;
    assert_eq!(view.agent_instance_id, "inst-a");
    assert!(view.connected);
    // The view/list path exposes the instance id.
    let clients = registry.list_clients().await;
    assert_eq!(clients[0].agent_instance_id, "inst-a");
}

#[tokio::test]
async fn lease_same_instance_reregister_accepts() {
    let registry = ShellClientRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    // Same client_id + same instance id is a reconnect/refresh: accepted.
    let _ = register_with_instance(&registry, "oe", "inst-a").await;
    let view = registry.get_client_view("oe").await.unwrap();
    assert_eq!(view.agent_instance_id, "inst-a");
    assert!(view.connected);
}

#[tokio::test]
async fn lease_different_online_instance_rejected() {
    let registry = ShellClientRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    // A second process with the same client_id but a different instance
    // must be rejected while the first is online.
    let err = registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "oe".to_string(),
            agent_instance_id: "inst-b".to_string(),
            display_name: None,
            owner: Some("alice".to_string()),
            hostname: None,
            capabilities: Some(async_job_capabilities()),
            projects: None,
            agent_protocol_version: Some("polling-v1".to_string()),
            policy: None,
        })
        .await
        .unwrap_err();
    assert!(err.contains("already online"), "error was: {err}");
    assert!(err.contains("different instance"), "error was: {err}");
    // The active instance is unchanged.
    let view = registry.get_client_view("oe").await.unwrap();
    assert_eq!(view.agent_instance_id, "inst-a");
}

#[tokio::test]
async fn lease_stale_replaced_by_different_instance_accepts() {
    let registry = ShellClientRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    // Age the first instance past the online window so it reads as stale.
    registry
        .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
        .await;
    // A different instance may now take over the lease.
    let _ = register_with_instance(&registry, "oe", "inst-b").await;
    let view = registry.get_client_view("oe").await.unwrap();
    assert_eq!(view.agent_instance_id, "inst-b");
    assert!(view.connected);
}

#[tokio::test]
async fn lease_stale_instance_poll_rejected() {
    let registry = ShellClientRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    // Replace with a newer instance after aging out.
    registry
        .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
        .await;
    register_with_instance(&registry, "oe", "inst-b").await;

    // The stale instance A can no longer poll.
    let err = registry
        .poll(ShellAgentPollRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst-a".to_string(),
            projects: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.contains("no longer the active instance"),
        "error was: {err}"
    );

    // The active instance B can still poll.
    registry
        .poll(ShellAgentPollRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst-b".to_string(),
            projects: None,
        })
        .await
        .expect("active instance must poll");
}

#[tokio::test]
async fn lease_stale_instance_result_rejected() {
    let registry = ShellClientRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    // Enqueue a request and let instance A poll it.
    let (request_id, _rx) = registry
        .enqueue_run(
            ShellRunRequest {
                client_id: "oe".to_string(),
                cwd: None,
                command: "echo hi".to_string(),
                stdin: None,
                timeout_secs: 5,
                wait_timeout_secs: 0,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();
    let _ = registry
        .poll(ShellAgentPollRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst-a".to_string(),
            projects: None,
        })
        .await
        .unwrap()
        .unwrap();

    // Replace instance A with B after aging out.
    registry
        .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
        .await;
    register_with_instance(&registry, "oe", "inst-b").await;

    // The stale instance A cannot submit the result.
    let err = registry
        .complete(ShellAgentResultRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst-a".to_string(),
            request_id: request_id.clone(),
            exit_code: Some(0),
            stdout: Some("hi".to_string()),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.contains("no longer the active instance"),
        "error was: {err}"
    );

    // The active instance B can submit the result.
    registry
        .complete(ShellAgentResultRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst-b".to_string(),
            request_id,
            exit_code: Some(0),
            stdout: Some("hi".to_string()),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .expect("active instance must submit result");
}

#[tokio::test]
async fn lease_stale_instance_job_update_rejected() {
    // A new `agent_instance_id` replacing the old instance terminates the
    // old instance's active/recovering jobs to `lost` with
    // `runner_instance_replaced` immediately at registration. The old
    // instance's late update is rejected, the new instance cannot inherit
    // or update the old instance's job, and the terminal state never
    // revives.
    let registry = ShellClientRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: None,
                command: Some("sleep 10".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();

    // Replace instance A with B after aging out. The replacement must
    // terminate A's job to `lost` at registration time.
    registry
        .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
        .await;
    register_with_instance(&registry, "oe", "inst-b").await;

    let lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(lost.status, "lost");
    assert_eq!(
        lost.recovery_reason_code.as_deref(),
        Some("runner_instance_replaced")
    );
    assert!(lost.ended_at.is_some(), "replaced job must record ended_at");
    assert_eq!(
        lost.recovery_state.as_deref(),
        Some("lost_after_reconcile"),
        "replaced job must record lost_after_reconcile"
    );

    // The stale instance A cannot update the job (lease check).
    let err = registry
        .update_job(ShellAgentJobUpdateRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst-a".to_string(),
            update_seq: None,
            job_id: job.job_id.clone(),
            request_id: None,
            status: "running".to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: None,
            duration_ms: None,
            error: None,
            validation_progress: None,
            finished: false,
        })
        .await
        .unwrap_err();
    assert!(
        err.contains("no longer the active instance"),
        "error was: {err}"
    );

    // The active instance B cannot inherit or update A's job: it belongs
    // to the replaced runner instance.
    let err = registry
        .update_job(ShellAgentJobUpdateRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst-b".to_string(),
            update_seq: None,
            job_id: job.job_id.clone(),
            request_id: None,
            status: "running".to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: None,
            duration_ms: None,
            error: None,
            validation_progress: None,
            finished: false,
        })
        .await
        .unwrap_err();
    assert!(
        err.contains("replaced runner instance"),
        "active instance must not inherit replaced job: {err}"
    );

    // The terminal state is stable: a second late update from A does not
    // revive the job or change the first `ended_at` / reason.
    let first_ended_at = lost.ended_at.unwrap();
    let _ = registry
        .update_job(ShellAgentJobUpdateRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst-a".to_string(),
            update_seq: None,
            job_id: job.job_id.clone(),
            request_id: None,
            status: "completed".to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: Some(0),
            duration_ms: Some(1),
            error: None,
            validation_progress: None,
            finished: true,
        })
        .await;
    let still_lost = registry.get_job(&job.job_id).await.unwrap();
    assert_eq!(still_lost.status, "lost");
    assert_eq!(still_lost.ended_at, Some(first_ended_at));
    assert_eq!(
        still_lost.recovery_reason_code.as_deref(),
        Some("runner_instance_replaced")
    );
}

#[tokio::test]
async fn lease_list_clients_exposes_instance_id() {
    let registry = ShellClientRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    let clients = registry.list_clients().await;
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].agent_instance_id, "inst-a");
    let view = registry.get_client_view("oe").await.unwrap();
    assert_eq!(view.agent_instance_id, "inst-a");
}

#[tokio::test]
async fn lease_reconcile_disconnect_stale_instance_is_noop() {
    // A delayed disconnect from a stale, replaced instance must not affect
    // the current active instance: it must not clear B's notifier, not mark
    // B's freshly-created job lost/recovering, and not change A's old job
    // which was already terminated to `lost` (`runner_instance_replaced`)
    // at replacement time. Only B's own disconnect reconciles B's job.
    let registry = ShellClientRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    // Install a notifier for instance A.
    let notify_a = Arc::new(Notify::new());
    registry
        .register_notifier("oe", "inst-a", notify_a.clone())
        .await
        .unwrap();
    // Start a job under instance A. It is terminated to `lost` when B
    // replaces A, before any disconnect runs.
    let old_job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: None,
                command: Some("sleep 10".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();

    // Age out A and let B take over. The replacement terminates A's job.
    registry
        .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
        .await;
    register_with_instance(&registry, "oe", "inst-b").await;
    // B installs its own notifier.
    let notify_b = Arc::new(Notify::new());
    registry
        .register_notifier("oe", "inst-b", notify_b.clone())
        .await
        .unwrap();

    // B starts a fresh job of its own after the replacement.
    let b_job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: None,
                command: Some("sleep 10".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();

    // Snapshot A's old job terminal state before the stale disconnect.
    let old_lost = registry.get_job(&old_job.job_id).await.unwrap();
    assert_eq!(old_lost.status, "lost");
    assert_eq!(
        old_lost.recovery_reason_code.as_deref(),
        Some("runner_instance_replaced")
    );
    let old_ended_at = old_lost.ended_at.unwrap();

    // A's transport finally disconnects. This must be a no-op: B stays the
    // current instance, B's notifier stays installed, B's job stays
    // active, and A's old job keeps its first `ended_at`/reason.
    registry.reconcile_disconnect("oe", "inst-a").await;

    let view = registry.get_client_view("oe").await.unwrap();
    assert_eq!(view.agent_instance_id, "inst-b");
    assert!(view.connected, "stale disconnect must not drop B's lease");

    // B's notifier remains installed (still addressable) and B's job is
    // untouched.
    let b_view = registry.get_job(&b_job.job_id).await.unwrap();
    assert_ne!(
        b_view.status, "lost",
        "stale disconnect must not mark B's active job lost"
    );
    assert_ne!(
        b_view.status, "recovering",
        "stale disconnect must not drive B's job into recovering"
    );

    let old_after = registry.get_job(&old_job.job_id).await.unwrap();
    assert_eq!(old_after.status, "lost");
    assert_eq!(old_after.ended_at, Some(old_ended_at));
    assert_eq!(
        old_after.recovery_reason_code.as_deref(),
        Some("runner_instance_replaced")
    );

    // B can still poll/update/complete its own job after A's stale
    // disconnect.
    let updated = registry
        .update_job(ShellAgentJobUpdateRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst-b".to_string(),
            update_seq: None,
            job_id: b_job.job_id.clone(),
            request_id: None,
            status: "running".to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: None,
            duration_ms: None,
            error: None,
            validation_progress: None,
            finished: false,
        })
        .await
        .expect("B must still update its own job after A's stale disconnect");
    assert_eq!(updated.status, "running");

    // Only B's own disconnect reconciles B's job. A non-reconciliation
    // client's active job becomes `lost` (legacy_runner_disconnected).
    registry.reconcile_disconnect("oe", "inst-b").await;
    let b_final = registry.get_job(&b_job.job_id).await.unwrap();
    assert_eq!(b_final.status, "lost");
    assert_eq!(
        b_final.recovery_reason_code.as_deref(),
        Some("legacy_runner_disconnected")
    );
    // A's old job is unaffected by B's disconnect.
    let old_final = registry.get_job(&old_job.job_id).await.unwrap();
    assert_eq!(old_final.status, "lost");
    assert_eq!(old_final.ended_at, Some(old_ended_at));
}

#[tokio::test]
async fn lease_register_notifier_rejects_stale_instance() {
    let registry = ShellClientRegistry::default();
    register_with_instance(&registry, "oe", "inst-a").await;
    // Replace A with B.
    registry
        .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
        .await;
    register_with_instance(&registry, "oe", "inst-b").await;
    // A's late notifier registration must be rejected so it cannot
    // overwrite B's notifier.
    let err = registry
        .register_notifier("oe", "inst-a", Arc::new(Notify::new()))
        .await
        .unwrap_err();
    assert!(
        err.contains("no longer the active instance"),
        "error was: {err}"
    );
    // B can still install its notifier.
    registry
        .register_notifier("oe", "inst-b", Arc::new(Notify::new()))
        .await
        .expect("active instance must install notifier");
}

#[tokio::test]
async fn lease_register_rejects_empty_instance_id() {
    let registry = ShellClientRegistry::default();
    let err = registry
        .register(ShellClientRegisterRequest {
            process_started_at: None,
            build: None,
            job_inventory: None,
            client_id: "oe".to_string(),
            agent_instance_id: "".to_string(),
            display_name: None,
            owner: None,
            hostname: None,
            capabilities: None,
            projects: None,
            agent_protocol_version: None,
            policy: None,
        })
        .await
        .unwrap_err();
    assert!(err.contains("agent_instance_id"), "error was: {err}");
}
#[tokio::test]
async fn project_active_job_query_is_not_truncated_and_unregister_fences_starts() {
    let registry = ShellClientRegistry::default();
    register_with_instance(&registry, "oe", "inst-jobs").await;
    let request = |command: &str| ShellJobOpRequest {
        op: "start".to_string(),
        client_id: Some("oe".to_string()),
        cwd: None,
        command: Some(command.to_string()),
        timeout_secs: Some(60),
        job_id: None,
        since_stdout_line: None,
        since_stderr_line: None,
        tail_lines: None,
        limit: None,
        codex: None,
    };
    let target = "agent:oe:target";
    let target_job = registry
        .start_job_with_metadata(
            request("sleep 60"),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(target.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    {
        let mut inner = registry.inner.lock().await;
        inner
            .jobs_by_id
            .get_mut(&target_job.job_id)
            .unwrap()
            .created_at = 0;
    }
    for index in 0..101 {
        registry
            .start_job_with_metadata(
                request(&format!("echo {index}")),
                "tester".to_string(),
                ShellJobStartMetadata {
                    project_id: Some(format!("agent:oe:other-{index}")),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    assert_eq!(registry.list_jobs(Some(100)).await.len(), 100);
    assert_eq!(
        registry.count_active_jobs_for_project(None, target).await,
        1
    );
    assert_eq!(
        registry
            .begin_project_unregister(None, target)
            .await
            .unwrap(),
        1
    );

    {
        let mut inner = registry.inner.lock().await;
        let job = inner.jobs_by_id.get_mut(&target_job.job_id).unwrap();
        job.status = "completed".to_string();
        job.ended_at = Some(now_ts());
    }
    assert_eq!(
        registry
            .begin_project_unregister(None, target)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        registry
            .begin_project_unregister(None, target)
            .await
            .unwrap(),
        0
    );
    registry.end_project_unregister(target).await;
    let blocked = registry
        .start_job_with_metadata(
            request("echo blocked"),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(target.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(blocked, "project_unregister_in_progress");
    registry.end_project_unregister(target).await;
    registry
        .start_job_with_metadata(
            request("echo allowed"),
            "tester".to_string(),
            ShellJobStartMetadata {
                project_id: Some(target.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

// ------------------------------------------------------------------------
// Connection-scoped lease: same-instance transport reconnect races.
// A replaced connection (same client_id + same agent_instance_id but a
// newer connection_id) must not let the older socket dequeue new
// requests, refresh liveness, or clobber the new connection's metadata.
// ------------------------------------------------------------------------

#[tokio::test]
async fn stale_connection_poll_cannot_steal_new_request() {
    // Same runner instance registers over connection A, a request is
    // queued, then the instance reconnects over connection B (new lease).
    // Connection A's connection-scoped poll must be rejected with a stale
    // connection error AND leave the request in the queue / undispatched /
    // job un-transitioned (atomic: not just a stale error string). B then
    // polls and is the only one to receive the request.
    let registry = ShellClientRegistry::default();
    register_with_connection(&registry, "oe", "inst-x", "conn-a").await;

    // Start an async job (queued -> agent_queued only on dispatch).
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: None,
                command: Some("sleep 1".to_string()),
                timeout_secs: Some(1),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();
    // The job starts queued with one pending request in the queue.
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "queued"
    );

    // Same instance reconnects over connection B; B takes the lease.
    register_with_connection(&registry, "oe", "inst-x", "conn-b").await;

    // A's connection-scoped poll is rejected with the stable stale error.
    let err = registry
        .poll_for_connection(
            ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-x".to_string(),
                projects: None,
            },
            "conn-a",
        )
        .await
        .unwrap_err();
    assert!(
        err.contains("transport connection is no longer active"),
        "error was: {err}"
    );

    // Atomicity: the request must still be queued, undispatched, and the
    // job must still be queued (no queued -> agent_queued transition).
    let pending_depth = registry
        .get_client_view("oe")
        .await
        .unwrap()
        .pending_requests;
    assert_eq!(pending_depth, 1, "stale poll must not dequeue the request");
    {
        let inner = registry.inner.lock().await;
        let request_id = inner
            .jobs_by_id
            .get(&job.job_id)
            .and_then(|j| j.request_id.clone());
        let request_id = request_id.expect("job has a request_id");
        let pending = inner
            .pending_by_id
            .get(&request_id)
            .expect("request still pending");
        assert!(
            !pending.dispatched,
            "stale poll must not mark request dispatched"
        );
        assert_eq!(
            inner.jobs_by_id.get(&job.job_id).unwrap().status,
            "queued",
            "stale poll must not transition the job"
        );
    }

    // B's connection-scoped poll receives the request (exactly once).
    let polled_b = registry
        .poll_for_connection(
            ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-x".to_string(),
                projects: None,
            },
            "conn-b",
        )
        .await
        .unwrap()
        .expect("current connection must receive the request");
    assert_eq!(polled_b.kind, "start_job");
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "agent_queued"
    );
    // The queue is now drained: a second poll by either connection gets None.
    let again_a = registry
        .poll_for_connection(
            ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-x".to_string(),
                projects: None,
            },
            "conn-a",
        )
        .await;
    // A is still stale, so this is an error (not a None success).
    assert!(again_a.is_err());
}

#[tokio::test]
async fn stale_connection_keepalive_does_not_refresh_new_lease() {
    // After a same-instance reconnect, a delayed Ping/Pong from the old
    // connection must not refresh the new connection's last_seen or revive
    // a disconnected client. The current connection's keepalive does
    // refresh.
    let registry = ShellClientRegistry::default();
    register_with_connection(&registry, "oe", "inst-x", "conn-a").await;
    register_with_connection(&registry, "oe", "inst-x", "conn-b").await;

    // Pin the current client's last_seen to a known stale value so a
    // successful touch would observably advance it.
    let pinned = chrono::Utc::now().timestamp() - 90;
    registry.set_last_seen_for_test("oe", pinned).await;

    // A's connection-scoped touch fails and leaves last_seen unchanged.
    let err = registry
        .touch_client_for_connection("oe", "inst-x", "conn-a")
        .await
        .unwrap_err();
    assert!(
        err.contains("transport connection is no longer active"),
        "error was: {err}"
    );
    assert_eq!(
        registry.get_client_view("oe").await.unwrap().last_seen,
        pinned,
        "stale connection touch must not refresh last_seen"
    );

    // B's connection-scoped touch succeeds and advances last_seen.
    registry
        .touch_client_for_connection("oe", "inst-x", "conn-b")
        .await
        .unwrap();
    assert!(
        registry.get_client_view("oe").await.unwrap().last_seen > pinned,
        "current connection touch must refresh last_seen"
    );

    // An even newer connection C supersedes B; B's touch now fails too.
    register_with_connection(&registry, "oe", "inst-x", "conn-c").await;
    let err = registry
        .touch_client_for_connection("oe", "inst-x", "conn-b")
        .await
        .unwrap_err();
    assert!(
        err.contains("transport connection is no longer active"),
        "superseded connection touch must be rejected, error was: {err}"
    );
}

#[tokio::test]
async fn stale_connection_runtime_metadata_does_not_overwrite_current() {
    // A stale same-instance connection must not overwrite the current
    // connection's provider metadata. The current connection can.
    let registry = ShellClientRegistry::default();
    let register_with_policy = async |connection_id: &str| {
        registry
            .register_with_auth_connection(
                ShellClientRegisterRequest {
                    process_started_at: None,
                    build: None,
                    job_inventory: None,
                    client_id: "oe".to_string(),
                    agent_instance_id: "inst-x".to_string(),
                    display_name: None,
                    owner: Some("alice".to_string()),
                    hostname: None,
                    capabilities: Some(async_job_capabilities()),
                    projects: None,
                    agent_protocol_version: Some("polling-v1".to_string()),
                    policy: Some(AgentPolicySummary::default()),
                },
                None,
                Some(connection_id),
            )
            .await
            .unwrap()
    };
    register_with_policy("conn-a").await;
    register_with_policy("conn-b").await;

    let provider_status = |strategy: &str| ToolProvidersStatus {
        strategy: strategy.to_string(),
        claude_code: ClaudeCodeProviderStatus {
            enabled: true,
            version: None,
            available: true,
            process_state: "running".to_string(),
            discovered_tool_names: Vec::new(),
            capabilities: std::collections::BTreeMap::new(),
            last_error_code: None,
            last_call: None,
        },
        config_reload: Default::default(),
    };

    // Current connection B reports a provider status.
    registry
        .update_tool_providers_for_connection(
            "oe",
            "inst-x",
            "conn-b",
            Some(provider_status("claude_code")),
        )
        .await
        .unwrap();
    {
        let inner = registry.inner.lock().await;
        let client = inner.clients.get("oe").unwrap();
        assert_eq!(
            client
                .policy
                .as_ref()
                .unwrap()
                .tool_providers
                .as_ref()
                .unwrap()
                .strategy,
            "claude_code"
        );
    }

    // Stale connection A tries to overwrite with a different valid
    // strategy; it must be rejected and must not change the recorded
    // strategy.
    let err = registry
        .update_tool_providers_for_connection(
            "oe",
            "inst-x",
            "conn-a",
            Some(provider_status("native")),
        )
        .await
        .unwrap_err();
    assert!(
        err.contains("transport connection is no longer active"),
        "{err}"
    );
    {
        let inner = registry.inner.lock().await;
        let client = inner.clients.get("oe").unwrap();
        assert_eq!(
            client
                .policy
                .as_ref()
                .unwrap()
                .tool_providers
                .as_ref()
                .unwrap()
                .strategy,
            "claude_code",
            "stale connection must not overwrite current metadata"
        );
    }
}

#[tokio::test]
async fn stale_connection_disconnect_cleanup_is_noop_for_current_lease() {
    // Same-instance reconnect: A's delayed disconnect cleanup must not
    // touch B's notifier/queue/liveness. Extends the existing same-instance
    // reconnect coverage to the connection lease.
    let registry = ShellClientRegistry::default();
    register_with_connection(&registry, "oe", "inst-x", "conn-a").await;
    let notify_a = Arc::new(Notify::new());
    registry
        .register_notifier_for_connection("oe", "inst-x", "conn-a", notify_a)
        .await
        .unwrap();
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: None,
                command: Some("sleep 10".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();

    // B reconnects (same instance) and installs its own notifier.
    register_with_connection(&registry, "oe", "inst-x", "conn-b").await;
    let notify_b = Arc::new(Notify::new());
    registry
        .register_notifier_for_connection("oe", "inst-x", "conn-b", notify_b)
        .await
        .unwrap();

    // A's delayed disconnect cleanup is a no-op: B's job is not lost.
    registry
        .reconcile_disconnect_for_connection("oe", "inst-x", "conn-a")
        .await;
    assert_ne!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "lost",
        "stale connection cleanup must not mark current job lost"
    );
    // B's notifier survives A's cleanup and B's own dispatch still works.
    let polled = registry
        .poll_for_connection(
            ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-x".to_string(),
                projects: None,
            },
            "conn-b",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(polled.kind, "start_job");

    // B's own disconnect does reconcile the job to lost.
    registry
        .reconcile_disconnect_for_connection("oe", "inst-x", "conn-b")
        .await;
    assert_eq!(registry.get_job(&job.job_id).await.unwrap().status, "lost");
}

#[tokio::test]
async fn late_result_on_stale_connection_is_accepted_without_refreshing_liveness() {
    // A request dispatched to A (same instance) before the reconnect must
    // still complete on a late result arriving over the stale connection
    // A — it belongs to the same instance — but must NOT refresh B's
    // liveness. A cannot then poll a new request that arrived after B's
    // register.
    let registry = ShellClientRegistry::default();
    register_with_connection(&registry, "oe", "inst-x", "conn-a").await;

    // Enqueue a sync request and let A poll it (still current lease).
    let (request_id, rx) = registry
        .enqueue_run(
            ShellRunRequest {
                client_id: "oe".to_string(),
                cwd: None,
                command: "echo hi".to_string(),
                stdin: None,
                timeout_secs: 5,
                wait_timeout_secs: 0,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();
    let polled_a = registry
        .poll_for_connection(
            ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-x".to_string(),
                projects: None,
            },
            "conn-a",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(polled_a.request_id, request_id);

    // Same instance reconnects over B; B is now the current lease.
    register_with_connection(&registry, "oe", "inst-x", "conn-b").await;
    // Pin B's last_seen to an online-but-observable value. A refresh by a
    // successful connection-scoped operation would advance it to `now`; the
    // stale connection must leave it at the pinned value. Staying inside the
    // 60s online window keeps the later enqueue path valid.
    let pinned = chrono::Utc::now().timestamp() - 30;
    registry.set_last_seen_for_test("oe", pinned).await;

    // The late result arrives over stale connection A. It is accepted
    // (same instance) and resolves the waiter.
    registry
        .complete_for_connection(
            ShellAgentResultRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-x".to_string(),
                request_id: request_id.clone(),
                exit_code: Some(0),
                stdout: Some("hi".to_string()),
                stderr: None,
                duration_ms: Some(1),
                error: None,
            },
            "conn-a",
        )
        .await
        .unwrap();
    let response = rx.await.unwrap();
    assert!(response.success);
    // But it did NOT refresh B's liveness.
    assert_eq!(
        registry.get_client_view("oe").await.unwrap().last_seen,
        pinned,
        "late result on stale connection must not refresh new lease liveness"
    );

    // A cannot now poll a request enqueued after B's register. Enqueue a
    // new request under B's lease and verify A's poll is rejected.
    let (_new_request_id, _new_rx) = registry
        .enqueue_run(
            ShellRunRequest {
                client_id: "oe".to_string(),
                cwd: None,
                command: "echo two".to_string(),
                stdin: None,
                timeout_secs: 5,
                wait_timeout_secs: 0,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();
    let err = registry
        .poll_for_connection(
            ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-x".to_string(),
                projects: None,
            },
            "conn-a",
        )
        .await
        .unwrap_err();
    assert!(
        err.contains("transport connection is no longer active"),
        "{err}"
    );

    // B receives the new request.
    let polled_b = registry
        .poll_for_connection(
            ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-x".to_string(),
                projects: None,
            },
            "conn-b",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(polled_b.command, "echo two");
}

#[tokio::test]
async fn late_job_update_on_stale_connection_is_accepted_without_refreshing_liveness() {
    // A job dispatched to A before the reconnect: its high-sequence job
    // update arriving over stale connection A is still applied (ownership
    // + update_seq), but does not refresh B's liveness. A replaced runner
    // instance is still rejected.
    let registry = ShellClientRegistry::default();
    register_with_connection(&registry, "oe", "inst-x", "conn-a").await;
    let job = registry
        .start_job(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("oe".to_string()),
                cwd: None,
                command: Some("sleep 10".to_string()),
                timeout_secs: Some(10),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "tester".to_string(),
        )
        .await
        .unwrap();
    // A polls/dispatches the job (still current lease).
    registry
        .poll_for_connection(
            ShellAgentPollRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-x".to_string(),
                projects: None,
            },
            "conn-a",
        )
        .await
        .unwrap()
        .unwrap();

    // Same instance reconnects over B.
    register_with_connection(&registry, "oe", "inst-x", "conn-b").await;
    // Pin to an online-but-observable value: a refresh would advance it to
    // `now`, but the stale connection must leave it pinned. Staying online
    // also prevents `get_job`'s status refresh from marking the active job
    // lost while we inspect it.
    let pinned = chrono::Utc::now().timestamp() - 30;
    registry.set_last_seen_for_test("oe", pinned).await;

    // Late job update over stale connection A is accepted and applied.
    registry
        .update_job_for_connection(
            ShellAgentJobUpdateRequest {
                client_id: "oe".to_string(),
                agent_instance_id: "inst-x".to_string(),
                update_seq: None,
                job_id: job.job_id.clone(),
                request_id: None,
                status: "running".to_string(),
                stdout_chunk: None,
                stderr_chunk: None,
                stdout_tail: None,
                stderr_tail: None,
                log_snapshot: None,
                exit_code: None,
                duration_ms: None,
                error: None,
                validation_progress: None,
                finished: false,
            },
            "conn-a",
        )
        .await
        .unwrap();
    assert_eq!(
        registry.get_job(&job.job_id).await.unwrap().status,
        "running"
    );
    // But B's liveness was not refreshed.
    assert_eq!(
        registry.get_client_view("oe").await.unwrap().last_seen,
        pinned,
        "late job update on stale connection must not refresh new lease liveness"
    );

    // A replaced runner instance is still rejected outright (a brand new
    // instance cannot submit updates for the old instance's job). Age the
    // old instance out so the replacement can take the lease.
    registry
        .set_last_seen_for_test("oe", chrono::Utc::now().timestamp() - 120)
        .await;
    register_with_instance(&registry, "oe", "inst-y").await;
    let err = registry
        .update_job(ShellAgentJobUpdateRequest {
            client_id: "oe".to_string(),
            agent_instance_id: "inst-x".to_string(),
            update_seq: None,
            job_id: job.job_id.clone(),
            request_id: None,
            status: "completed".to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: Some(0),
            duration_ms: Some(1),
            error: None,
            validation_progress: None,
            finished: true,
        })
        .await
        .unwrap_err();
    assert!(
        err.contains("no longer the active instance"),
        "replaced runner instance must be rejected, error was: {err}"
    );
}
