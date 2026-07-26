//! `login` / `logout` / `status` — the everyday device commands.
//!
//! `client enroll` needs the server URL, a pairing code, a client id, and a
//! profile name, and the client id has to match what the server put in the
//! pairing record — so the device name is typed twice, once on each side.
//! `login` keeps the two values a person actually has (which server, which
//! code) and derives the rest: the device name from the hostname, and the
//! destination from the username the server returns.

use std::path::{Path, PathBuf};

use super::connections::{
    connections_for_server, default_base_dir, descriptor_toml, list_connections, Connection,
    ConnectionPaths,
};

/// Device name reported to the server. The hostname is what a person would
/// call this machine; `--device` overrides it.
pub(crate) fn default_device_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .map(|value| sanitize_device_name(&value))
        .unwrap_or_else(|| "device".to_string())
}

fn sanitize_device_name(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        "device".to_string()
    } else {
        cleaned.chars().take(80).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoginOptions {
    pub(crate) server_url: String,
    pub(crate) code: String,
    pub(crate) device: String,
    pub(crate) base_dir: PathBuf,
    pub(crate) transport: String,
    pub(crate) allowed_roots: Vec<PathBuf>,
    pub(crate) overwrite: bool,
    pub(crate) json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogoutOptions {
    pub(crate) server_url: String,
    pub(crate) username: Option<String>,
    pub(crate) base_dir: PathBuf,
    pub(crate) yes: bool,
    pub(crate) json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusOptions {
    pub(crate) base_dir: PathBuf,
    pub(crate) json: bool,
}

/// Where a login writes, once the server has told us who we are.
pub(crate) fn resolve_destination(
    base: &Path,
    server_url: &str,
    username: &str,
) -> Result<ConnectionPaths, String> {
    ConnectionPaths::resolve(base, server_url, username)
}

/// Refuse to clobber an existing login unless asked.
pub(crate) fn ensure_destination_available(
    paths: &ConnectionPaths,
    overwrite: bool,
) -> Result<(), String> {
    if overwrite {
        return Ok(());
    }
    for path in [&paths.user_token, &paths.agent_token, &paths.agent_config] {
        if path.exists() {
            return Err(format!(
                "already logged in at {}; pass --overwrite to replace it",
                paths.dir.display()
            ));
        }
    }
    Ok(())
}

pub(crate) fn write_descriptor(
    paths: &ConnectionPaths,
    server_url: &str,
    username: &str,
    device: &str,
    now: &str,
) -> Result<(), String> {
    std::fs::write(
        &paths.descriptor,
        descriptor_toml(server_url, username, device, now),
    )
    .map_err(|error| {
        format!(
            "failed to write {}: {error}",
            paths.descriptor.display()
        )
    })
}

pub(crate) fn render_login_result(
    paths: &ConnectionPaths,
    server_url: &str,
    username: &str,
    device: &str,
    json: bool,
) -> Result<String, String> {
    if json {
        let summary = serde_json::json!({
            "server_url": server_url,
            "username": username,
            "device": device,
            "dir": paths.dir.to_string_lossy(),
            "agent_config": paths.agent_config.to_string_lossy(),
        });
        return serde_json::to_string_pretty(&summary).map_err(|error| error.to_string());
    }
    Ok(format!(
        "Logged in to {server_url} as {username} ({device}).\n\n  \
         config: {}\n\nStart the agent:\n  webcodex-agent --config {}\n",
        paths.dir.display(),
        paths.agent_config.display(),
    ))
}

pub(crate) fn render_status(connections: &[Connection], json: bool) -> Result<String, String> {
    if json {
        let rows: Vec<_> = connections
            .iter()
            .map(|connection| {
                serde_json::json!({
                    "server_url": connection.server_url,
                    "username": connection.username,
                    "device": connection.device,
                    "logged_in_at": connection.logged_in_at,
                    "dir": connection.paths.dir.to_string_lossy(),
                })
            })
            .collect();
        return serde_json::to_string_pretty(&serde_json::json!({ "connections": rows }))
            .map_err(|error| error.to_string());
    }
    if connections.is_empty() {
        return Ok("Not logged in to any server.\n\nRun: webcodex-cli login <server-url> --code <pairing-code>\n"
            .to_string());
    }
    let width = connections
        .iter()
        .map(|connection| connection.server_url.len())
        .max()
        .unwrap_or(0)
        .max(6);
    let mut out = String::from("Logged in:\n\n");
    for connection in connections {
        out.push_str(&format!(
            "  {:width$}  {}  ({})\n",
            connection.server_url,
            connection.username,
            connection.device,
            width = width
        ));
    }
    Ok(out)
}

/// Connections a logout would remove. Without a username, every user on that
/// server matches; the caller confirms before anything is deleted.
pub(crate) fn logout_targets(opts: &LogoutOptions) -> Vec<Connection> {
    connections_for_server(&opts.base_dir, &opts.server_url)
        .into_iter()
        .filter(|connection| {
            opts.username
                .as_deref()
                .is_none_or(|username| connection.username.eq_ignore_ascii_case(username))
        })
        .collect()
}

pub(crate) fn remove_connection(connection: &Connection) -> Result<(), String> {
    std::fs::remove_dir_all(&connection.paths.dir).map_err(|error| {
        format!(
            "failed to remove {}: {error}",
            connection.paths.dir.display()
        )
    })?;
    // Drop the server directory too once its last user is gone, so `status`
    // and `ls` do not show an empty shell.
    if let Some(server_dir) = connection.paths.dir.parent() {
        if std::fs::read_dir(server_dir).is_ok_and(|mut entries| entries.next().is_none()) {
            let _ = std::fs::remove_dir(server_dir);
        }
    }
    Ok(())
}

pub(crate) fn all_connections(base: &Path) -> Vec<Connection> {
    list_connections(base)
}

pub(crate) fn base_dir_or_default(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(default_base_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(base: &Path, server: &str, user: &str) -> ConnectionPaths {
        let paths = ConnectionPaths::resolve(base, server, user).unwrap();
        std::fs::create_dir_all(&paths.dir).unwrap();
        write_descriptor(&paths, server, user, "laptop", "t").unwrap();
        paths
    }

    #[test]
    fn device_name_is_derived_and_path_safe() {
        assert_eq!(sanitize_device_name("Alice-Laptop"), "alice-laptop");
        assert_eq!(sanitize_device_name("  host.local  "), "host.local");
        assert_eq!(sanitize_device_name("weird/name:here"), "weird-name-here");
        assert_eq!(sanitize_device_name("---"), "device");
        assert_eq!(sanitize_device_name(""), "device");
        assert!(sanitize_device_name(&"x".repeat(200)).len() <= 80);
    }

    #[test]
    fn destination_is_server_then_user() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = resolve_destination(temp.path(), "https://api.example.com", "alice").unwrap();
        assert_eq!(paths.dir, temp.path().join("api.example.com").join("alice"));
        assert_eq!(paths.agent_config, paths.dir.join("agent.toml"));
    }

    #[test]
    fn existing_login_is_not_clobbered_without_overwrite() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = seed(temp.path(), "https://api.example.com", "alice");
        std::fs::write(&paths.agent_config, "x").unwrap();

        let error = ensure_destination_available(&paths, false).unwrap_err();
        assert!(error.contains("already logged in"), "{error}");
        ensure_destination_available(&paths, true).expect("--overwrite must allow replacing");
    }

    #[test]
    fn logout_without_username_targets_every_user_on_that_server() {
        let temp = tempfile::TempDir::new().unwrap();
        seed(temp.path(), "https://api.example.com", "alice");
        seed(temp.path(), "https://api.example.com", "bob");
        seed(temp.path(), "https://other.example.com", "alice");

        let opts = LogoutOptions {
            server_url: "https://api.example.com".to_string(),
            username: None,
            base_dir: temp.path().to_path_buf(),
            yes: true,
            json: false,
        };
        let targets = logout_targets(&opts);
        assert_eq!(targets.len(), 2, "{targets:?}");

        let scoped = LogoutOptions {
            username: Some("bob".to_string()),
            ..opts
        };
        let targets = logout_targets(&scoped);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].username, "bob");
    }

    #[test]
    fn removing_the_last_user_also_drops_the_server_directory() {
        let temp = tempfile::TempDir::new().unwrap();
        seed(temp.path(), "https://api.example.com", "alice");
        seed(temp.path(), "https://api.example.com", "bob");
        let server_dir = temp.path().join("api.example.com");

        let connections = all_connections(temp.path());
        remove_connection(&connections[0]).unwrap();
        assert!(server_dir.exists(), "server dir must stay while a user remains");

        let connections = all_connections(temp.path());
        remove_connection(&connections[0]).unwrap();
        assert!(!server_dir.exists(), "empty server dir should be cleaned up");
    }

    #[test]
    fn status_lists_every_connection_and_guides_when_empty() {
        let temp = tempfile::TempDir::new().unwrap();
        let empty = render_status(&all_connections(temp.path()), false).unwrap();
        assert!(empty.contains("Not logged in"), "{empty}");
        assert!(empty.contains("login"), "{empty}");

        seed(temp.path(), "https://api.example.com", "alice");
        seed(temp.path(), "https://other.example.com", "bob");
        let listed = render_status(&all_connections(temp.path()), false).unwrap();
        assert!(listed.contains("https://api.example.com"), "{listed}");
        assert!(listed.contains("alice"), "{listed}");
        assert!(listed.contains("bob"), "{listed}");

        let json = render_status(&all_connections(temp.path()), true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["connections"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn one_device_can_hold_the_same_user_on_several_servers() {
        let temp = tempfile::TempDir::new().unwrap();
        seed(temp.path(), "https://s1.example.com", "alice");
        seed(temp.path(), "https://s2.example.com", "alice");
        assert_eq!(all_connections(temp.path()).len(), 2);
        assert_eq!(
            connections_for_server(temp.path(), "https://s1.example.com").len(),
            1
        );
    }
}

/// Log this device into a server.
///
/// The destination is not known until the server answers, because it is keyed
/// by the username the pairing code belongs to. So the exchange happens first,
/// then the paths are resolved, then the files are written.
pub(crate) async fn run_login(opts: LoginOptions) -> Result<String, String> {
    let server_url = opts.server_url.trim().trim_end_matches('/').to_string();
    // Fail early on a URL we could not turn into a directory, rather than
    // after the one-time pairing code has already been spent.
    super::connections::server_slug(&server_url)?;

    let mut body = serde_json::json!({
        "pairing_code": opts.code,
        "client_id": opts.device,
        "transport": opts.transport,
        "allow_cwd_anywhere": false,
    });
    if !opts.allowed_roots.is_empty() {
        body["allowed_roots"] = serde_json::json!(opts
            .allowed_roots
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect::<Vec<_>>());
    }

    let value = super::http::post_json_unauthed(&server_url, "/api/pairing/enroll", body).await?;
    let user_token = value
        .get("user_token")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "enroll response missing user_token".to_string())?
        .to_string();
    let agent_token = value
        .get("agent_token")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "enroll response missing agent_token".to_string())?
        .to_string();
    let username = value
        .get("username")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "enroll response missing username".to_string())?
        .to_string();

    let paths = resolve_destination(&opts.base_dir, &server_url, &username)?;
    ensure_destination_available(&paths, opts.overwrite)?;
    std::fs::create_dir_all(&paths.projects_dir)
        .map_err(|error| format!("failed to create {}: {error}", paths.projects_dir.display()))?;

    super::system::write_text_file(
        &paths.user_token,
        &format!("{user_token}\n"),
        opts.overwrite,
        true,
    )?;
    super::system::write_text_file(
        &paths.agent_token,
        &format!("{agent_token}\n"),
        opts.overwrite,
        true,
    )?;

    crate::agent_init::run_agent_init(crate::agent_init::AgentInitOptions {
        server_url: server_url.clone(),
        token: Some(agent_token),
        token_file: None,
        client_id: opts.device.clone(),
        owner: username.clone(),
        display_name: None,
        transport: opts.transport.clone(),
        poll_interval_ms: crate::agent_init::DEFAULT_POLL_INTERVAL_MS,
        projects_dir: paths.projects_dir.clone(),
        output: paths.agent_config.clone(),
        allowed_roots: opts.allowed_roots.clone(),
        allow_cwd_anywhere: false,
        overwrite: opts.overwrite,
    })?;

    write_descriptor(
        &paths,
        &server_url,
        &username,
        &opts.device,
        &chrono::Utc::now().to_rfc3339(),
    )?;
    render_login_result(&paths, &server_url, &username, &opts.device, opts.json)
}

pub(crate) fn run_logout(opts: LogoutOptions) -> Result<String, String> {
    let targets = logout_targets(&opts);
    if targets.is_empty() {
        return Err(format!("not logged in to {}", opts.server_url));
    }
    if !opts.yes {
        let names: Vec<String> = targets
            .iter()
            .map(|connection| format!("{} as {}", connection.server_url, connection.username))
            .collect();
        return Err(format!(
            "this removes {} connection(s):\n  {}\n\nRe-run with --yes to confirm.",
            targets.len(),
            names.join("\n  ")
        ));
    }
    for connection in &targets {
        remove_connection(connection)?;
    }
    if opts.json {
        let rows: Vec<_> = targets
            .iter()
            .map(|connection| {
                serde_json::json!({
                    "server_url": connection.server_url,
                    "username": connection.username,
                })
            })
            .collect();
        return serde_json::to_string_pretty(&serde_json::json!({ "removed": rows }))
            .map_err(|error| error.to_string());
    }
    Ok(format!("Removed {} connection(s).\n", targets.len()))
}

pub(crate) fn run_status(opts: StatusOptions) -> Result<String, String> {
    render_status(&all_connections(&opts.base_dir), opts.json)
}
