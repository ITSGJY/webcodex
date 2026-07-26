//! `login` / `logout` / `status` — the everyday device commands.
//!
//! `client enroll` needs the server URL, a pairing code, a client id, and a
//! profile name, and the client id has to match what the server put in the
//! pairing record — so the device name is typed twice, once on each side.
//! `login` keeps the two values a person actually has (which server, which
//! code) and derives the rest: the device name from the hostname, and the
//! destination from the username the server returns.
//!
//! # Why publishing is transactional
//!
//! Redeeming a pairing code is destructive and one-shot: the code is spent and
//! fresh tokens are minted the moment the server answers. Everything after that
//! point has to avoid two failure modes — losing the new credentials, and
//! damaging a working connection that was already there. So the whole result is
//! built in a staging directory first and only then moved into place; see
//! [`publish_connection`].

use std::path::{Path, PathBuf};

use super::connections::{
    canonical_server_url, connections_for_server, default_base_dir, descriptor_toml,
    list_connections, Connection, ConnectionPaths, INTERNAL_DIR_PREFIX,
};

/// Device name reported to the server. The hostname is what a person would call
/// this machine; `--device` overrides it.
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

/// The credentials a redeemed pairing code produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnrolledIdentity {
    pub(crate) username: String,
    pub(crate) user_token: String,
    pub(crate) agent_token: String,
}

/// Where a finished login ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PublishOutcome {
    /// The connection is live at its final path.
    Published,
    /// The final path was already taken and `--overwrite` was not given, so the
    /// new credentials were parked here instead of being thrown away.
    SavedForRecovery { path: PathBuf },
}

fn unique_internal_dir(parent: &Path, kind: &str) -> PathBuf {
    let token = uuid::Uuid::new_v4().simple().to_string();
    parent.join(format!("{INTERNAL_DIR_PREFIX}{kind}-{token}"))
}

#[cfg(unix)]
fn harden_dir(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("failed to secure {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn harden_dir(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn harden_secret_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("failed to secure {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn harden_secret_file(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Create a private staging directory next to where the connection will land.
///
/// It has to share a parent with the destination so that publishing is a
/// same-filesystem rename rather than a copy that could half-succeed.
pub(crate) fn create_staging_dir(final_dir: &Path) -> Result<PathBuf, String> {
    let parent = final_dir
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", final_dir.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let staging = unique_internal_dir(parent, "staging");
    std::fs::create_dir(&staging)
        .map_err(|error| format!("failed to create staging directory: {error}"))?;
    harden_dir(&staging)?;
    Ok(staging)
}

fn discard_staging(staging: &Path) {
    // Best effort: the caller is already reporting a failure, and a leftover
    // staging directory is skipped by `status` either way.
    let _ = std::fs::remove_dir_all(staging);
}

/// Move a fully-built staging directory into its final place.
///
/// * destination free — one rename, nothing else to undo.
/// * destination taken with `overwrite` — the old connection is moved aside
///   first and only deleted once the new one is in place; if the second rename
///   fails the old one goes back.
/// * destination taken without `overwrite` — the code has already been spent, so
///   the staged connection is kept under a recovery directory rather than
///   deleted, and the existing connection is left untouched.
pub(crate) fn publish_connection(
    staging: &Path,
    final_dir: &Path,
    overwrite: bool,
) -> Result<PublishOutcome, String> {
    let parent = final_dir
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", final_dir.display()))?;

    if !final_dir.exists() {
        return match std::fs::rename(staging, final_dir) {
            Ok(()) => Ok(PublishOutcome::Published),
            Err(error) => {
                discard_staging(staging);
                Err(format!(
                    "failed to publish the connection to {}: {error}",
                    final_dir.display()
                ))
            }
        };
    }

    if !overwrite {
        let recovery = unique_internal_dir(parent, "recovery");
        return match std::fs::rename(staging, &recovery) {
            Ok(()) => Ok(PublishOutcome::SavedForRecovery { path: recovery }),
            Err(error) => {
                discard_staging(staging);
                Err(format!("failed to save the new credentials: {error}"))
            }
        };
    }

    let backup = unique_internal_dir(parent, "backup");
    if let Err(error) = std::fs::rename(final_dir, &backup) {
        discard_staging(staging);
        return Err(format!(
            "failed to move the existing connection aside: {error}; {} is unchanged",
            final_dir.display()
        ));
    }

    if let Err(error) = std::fs::rename(staging, final_dir) {
        // Put the old connection back before reporting; a failed login must not
        // cost the user a working one.
        let restored = std::fs::rename(&backup, final_dir).is_ok();
        discard_staging(staging);
        return Err(if restored {
            format!(
                "failed to publish the connection: {error}; the previous connection at {} was restored",
                final_dir.display()
            )
        } else {
            format!(
                "failed to publish the connection: {error}; the previous connection is at {}",
                backup.display()
            )
        });
    }

    let _ = std::fs::remove_dir_all(&backup);
    Ok(PublishOutcome::Published)
}

pub(crate) fn resolve_destination(
    base: &Path,
    server_url: &str,
    username: &str,
) -> Result<ConnectionPaths, String> {
    ConnectionPaths::resolve(base, server_url, username)
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
    .map_err(|error| format!("failed to write {}: {error}", paths.descriptor.display()))
}

/// Build the whole connection inside `staging`.
///
/// The agent token is written only into `agent.toml`; there is deliberately no
/// second copy on disk for it to drift from.
pub(crate) fn stage_connection(
    staging: &Path,
    opts: &LoginOptions,
    server_url: &str,
    identity: &EnrolledIdentity,
    now: &str,
) -> Result<(), String> {
    let paths = ConnectionPaths::new(staging.to_path_buf());
    std::fs::create_dir_all(&paths.projects_dir)
        .map_err(|error| format!("failed to create {}: {error}", paths.projects_dir.display()))?;

    super::system::write_text_file(
        &paths.user_token,
        &format!("{}\n", identity.user_token),
        true,
        true,
    )?;

    crate::agent_init::run_agent_init(crate::agent_init::AgentInitOptions {
        server_url: server_url.to_string(),
        token: Some(identity.agent_token.clone()),
        token_file: None,
        client_id: opts.device.clone(),
        owner: identity.username.clone(),
        display_name: None,
        transport: opts.transport.clone(),
        poll_interval_ms: crate::agent_init::DEFAULT_POLL_INTERVAL_MS,
        projects_dir: paths.projects_dir.clone(),
        output: paths.agent_config.clone(),
        allowed_roots: opts.allowed_roots.clone(),
        allow_cwd_anywhere: false,
        overwrite: true,
    })?;
    harden_secret_file(&paths.agent_config)?;

    write_descriptor(&paths, server_url, &identity.username, &opts.device, now)
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

/// Message for the case where the code was spent but the destination was taken.
pub(crate) fn render_recovery_error(
    final_dir: &Path,
    recovery: &Path,
    server_url: &str,
    username: &str,
) -> String {
    format!(
        "Already logged in to {server_url} as {username}.\n\n\
         The pairing code was redeemed, so the new credentials are real and have\n\
         been saved rather than discarded. Nothing about the existing connection\n\
         was changed.\n\n  \
         existing:        {}\n  \
         new credentials: {}\n\n\
         To keep the new credentials, remove the existing connection and move the\n\
         saved directory into its place, or run `login` again with --overwrite\n\
         using a fresh code. Delete the saved directory once you are done.\n",
        final_dir.display(),
        recovery.display(),
    )
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
        return Ok(
            "Not logged in to any server.\n\nRun: webcodex-cli login <server-url> --code <pairing-code>\n"
                .to_string(),
        );
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

/// Connections a logout would remove.
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

/// Remove one connection directory.
///
/// The path is re-checked against the base directory immediately before the
/// delete, and the directory itself must still be a real directory rather than
/// a symlink, so a link swapped in after listing cannot redirect the removal.
pub(crate) fn remove_connection(base: &Path, connection: &Connection) -> Result<(), String> {
    let dir = &connection.paths.dir;
    let base = base
        .canonicalize()
        .map_err(|error| format!("failed to resolve {}: {error}", base.display()))?;

    let meta = std::fs::symlink_metadata(dir)
        .map_err(|error| format!("failed to inspect {}: {error}", dir.display()))?;
    if !meta.is_dir() {
        return Err(format!(
            "refusing to remove {}: not a directory",
            dir.display()
        ));
    }

    let resolved = dir
        .canonicalize()
        .map_err(|error| format!("failed to resolve {}: {error}", dir.display()))?;
    if !resolved.starts_with(&base) {
        return Err(format!(
            "refusing to remove {}: outside {}",
            resolved.display(),
            base.display()
        ));
    }
    // Two levels down from the base and no deeper: <base>/<server>/<user>.
    if resolved
        .strip_prefix(&base)
        .map(|rest| rest.components().count())
        != Ok(2)
    {
        return Err(format!(
            "refusing to remove {}: not a connection directory",
            resolved.display()
        ));
    }

    std::fs::remove_dir_all(&resolved)
        .map_err(|error| format!("failed to remove {}: {error}", resolved.display()))?;

    // Drop the server directory once its last user is gone, so `status` and
    // `ls` do not show an empty shell.
    if let Some(server_dir) = resolved.parent() {
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

/// Redeem a pairing code. Network only — writes nothing.
///
/// Errors deliberately do not carry the response body or the code: the body can
/// contain freshly minted tokens, and the code is a live credential until it is
/// spent.
pub(crate) async fn redeem_pairing_code(
    server_url: &str,
    opts: &LoginOptions,
) -> Result<EnrolledIdentity, String> {
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

    let value = super::http::post_json_unauthed(server_url, "/api/pairing/enroll", body).await?;
    let field = |name: &str| -> Result<String, String> {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("the server response did not include {name}"))
    };
    Ok(EnrolledIdentity {
        username: field("username")?,
        user_token: field("user_token")?,
        agent_token: field("agent_token")?,
    })
}

/// Log this device into a server.
pub(crate) async fn run_login(opts: LoginOptions) -> Result<String, String> {
    // Reject a URL we could not turn into an identity *before* spending the
    // one-time code on it.
    let canonical = canonical_server_url(&opts.server_url)?;
    let server_url = canonical.url;

    let identity = redeem_pairing_code(&server_url, &opts).await?;
    let paths = resolve_destination(&opts.base_dir, &server_url, &identity.username)?;

    let staging = create_staging_dir(&paths.dir)?;
    let now = chrono::Utc::now().to_rfc3339();
    if let Err(error) = stage_connection(&staging, &opts, &server_url, &identity, &now) {
        discard_staging(&staging);
        return Err(error);
    }

    match publish_connection(&staging, &paths.dir, opts.overwrite)? {
        PublishOutcome::Published => render_login_result(
            &paths,
            &server_url,
            &identity.username,
            &opts.device,
            opts.json,
        ),
        PublishOutcome::SavedForRecovery { path } => Err(render_recovery_error(
            &paths.dir,
            &path,
            &server_url,
            &identity.username,
        )),
    }
}

pub(crate) fn run_logout(opts: LogoutOptions) -> Result<String, String> {
    canonical_server_url(&opts.server_url)?;
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
        remove_connection(&opts.base_dir, connection)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = "wc_pair_supersecretcode";
    const USER_TOKEN: &str = "wc_pat_usersecret";
    const AGENT_TOKEN: &str = "wc_agent_agentsecret";

    fn login_opts(base: &Path, server_url: &str, overwrite: bool) -> LoginOptions {
        LoginOptions {
            server_url: server_url.to_string(),
            code: CODE.to_string(),
            device: "laptop".to_string(),
            base_dir: base.to_path_buf(),
            transport: "websocket".to_string(),
            allowed_roots: Vec::new(),
            overwrite,
            json: false,
        }
    }

    fn identity() -> EnrolledIdentity {
        EnrolledIdentity {
            username: "alice".to_string(),
            user_token: USER_TOKEN.to_string(),
            agent_token: AGENT_TOKEN.to_string(),
        }
    }

    /// The local half of a login, with the network exchange already done.
    fn publish_login(
        base: &Path,
        server_url: &str,
        overwrite: bool,
    ) -> Result<PublishOutcome, String> {
        let opts = login_opts(base, server_url, overwrite);
        let canonical = canonical_server_url(server_url).unwrap();
        let identity = identity();
        let paths = resolve_destination(base, &canonical.url, &identity.username).unwrap();
        let staging = create_staging_dir(&paths.dir)?;
        if let Err(error) = stage_connection(&staging, &opts, &canonical.url, &identity, "t") {
            discard_staging(&staging);
            return Err(error);
        }
        publish_connection(&staging, &paths.dir, overwrite)
    }

    fn seed_connection(base: &Path, server_url: &str, username: &str) -> ConnectionPaths {
        let canonical = canonical_server_url(server_url).unwrap();
        let paths = resolve_destination(base, &canonical.url, username).unwrap();
        std::fs::create_dir_all(&paths.dir).unwrap();
        write_descriptor(&paths, &canonical.url, username, "laptop", "t").unwrap();
        paths
    }

    fn assert_no_internal_residue(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name();
            assert!(
                !name.to_string_lossy().starts_with(INTERNAL_DIR_PREFIX),
                "leftover internal directory in {}: {name:?}",
                dir.display()
            );
        }
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
        assert_eq!(
            paths.dir,
            temp.path().join("https_api.example.com").join("alice")
        );
        assert_eq!(paths.agent_config, paths.dir.join("agent.toml"));
    }

    #[test]
    fn a_fresh_login_publishes_through_staging_and_leaves_nothing_behind() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        assert_eq!(
            publish_login(base, "https://api.example.com", false).unwrap(),
            PublishOutcome::Published
        );

        let listed = all_connections(base);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].username, "alice");
        assert_eq!(listed[0].server_url, "https://api.example.com");

        let paths = &listed[0].paths;
        assert!(paths.agent_config.is_file());
        assert!(paths.user_token.is_file());
        assert!(paths.projects_dir.is_dir());
        // The agent token has exactly one home.
        assert!(!paths.dir.join("webcodex-agent-token").exists());
        assert!(std::fs::read_to_string(&paths.agent_config)
            .unwrap()
            .contains(AGENT_TOKEN));

        assert_no_internal_residue(base);
        assert_no_internal_residue(paths.dir.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn published_secrets_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        publish_login(temp.path(), "https://api.example.com", false).unwrap();
        let paths = all_connections(temp.path())[0].paths.clone();
        for secret in [&paths.agent_config, &paths.user_token] {
            let mode = std::fs::metadata(secret).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} has mode {mode:o}", secret.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_staging_directory_is_private_while_it_exists() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        let paths = resolve_destination(temp.path(), "https://api.example.com", "alice").unwrap();
        let staging = create_staging_dir(&paths.dir).unwrap();
        let mode = std::fs::metadata(&staging).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "staging has mode {mode:o}");
        discard_staging(&staging);
        assert!(!staging.exists());
    }

    #[test]
    fn a_failure_while_staging_leaves_no_connection_and_no_residue() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        let opts = login_opts(base, "https://api.example.com", false);
        let identity = identity();
        let paths = resolve_destination(base, "https://api.example.com", "alice").unwrap();
        let staging = create_staging_dir(&paths.dir).unwrap();

        // Make agent.toml impossible to create by putting a directory there.
        std::fs::create_dir_all(staging.join("agent.toml")).unwrap();
        let result = stage_connection(&staging, &opts, "https://api.example.com", &identity, "t");
        assert!(result.is_err(), "staging should have failed");
        discard_staging(&staging);

        assert!(all_connections(base).is_empty());
        assert!(!paths.dir.exists(), "no connection may have been published");
        assert!(!staging.exists(), "staging must be cleaned up");
    }

    #[test]
    fn overwrite_replaces_the_connection_and_removes_the_backup() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        publish_login(base, "https://api.example.com", false).unwrap();
        let paths = all_connections(base)[0].paths.clone();
        std::fs::write(paths.dir.join("marker"), "old").unwrap();

        assert_eq!(
            publish_login(base, "https://api.example.com", true).unwrap(),
            PublishOutcome::Published
        );
        assert!(
            !paths.dir.join("marker").exists(),
            "the old connection should have been replaced"
        );
        assert_eq!(all_connections(base).len(), 1);
        assert_no_internal_residue(paths.dir.parent().unwrap());
    }

    #[test]
    fn a_failed_overwrite_restores_the_previous_connection() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        publish_login(base, "https://api.example.com", false).unwrap();
        let paths = all_connections(base)[0].paths.clone();
        std::fs::write(paths.dir.join("marker"), "old").unwrap();

        // A staging path that cannot be renamed into place.
        let missing = paths.dir.parent().unwrap().join(".staging-does-not-exist");
        let error = publish_connection(&missing, &paths.dir, true).unwrap_err();
        assert!(error.contains("restored"), "{error}");

        let listed = all_connections(base);
        assert_eq!(listed.len(), 1, "the old connection must survive");
        assert_eq!(
            std::fs::read_to_string(paths.dir.join("marker")).unwrap(),
            "old"
        );
    }

    #[test]
    fn without_overwrite_the_old_connection_stays_and_new_credentials_are_kept() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        publish_login(base, "https://api.example.com", false).unwrap();
        let paths = all_connections(base)[0].paths.clone();
        std::fs::write(paths.dir.join("marker"), "old").unwrap();

        let outcome = publish_login(base, "https://api.example.com", false).unwrap();
        let PublishOutcome::SavedForRecovery { path } = outcome else {
            panic!("expected the staged connection to be saved, got {outcome:?}");
        };

        // The existing connection is untouched...
        assert_eq!(
            std::fs::read_to_string(paths.dir.join("marker")).unwrap(),
            "old"
        );
        // ...the redeemed credentials still exist...
        assert!(path.join("agent.toml").is_file());
        assert!(std::fs::read_to_string(path.join("agent.toml"))
            .unwrap()
            .contains(AGENT_TOKEN));
        // ...and `status` shows one connection, not two.
        assert_eq!(all_connections(base).len(), 1);

        let message = render_recovery_error(&paths.dir, &path, "https://api.example.com", "alice");
        assert!(!message.contains(CODE), "message leaked the pairing code");
        assert!(!message.contains(AGENT_TOKEN), "message leaked a token");
        assert!(!message.contains(USER_TOKEN), "message leaked a token");
    }

    #[test]
    fn status_ignores_staging_backup_and_recovery_directories() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        publish_login(base, "https://api.example.com", false).unwrap();
        // A second login without --overwrite parks a full recovery directory.
        publish_login(base, "https://api.example.com", false).unwrap();

        let server_dir = base.join("https_api.example.com");
        let staging = create_staging_dir(&server_dir.join("alice")).unwrap();
        std::fs::write(
            staging.join("server.toml"),
            descriptor_toml("https://api.example.com", "alice", "laptop", "t"),
        )
        .unwrap();

        let listed = all_connections(base);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].paths.dir, server_dir.join("alice"));
    }

    #[test]
    fn logout_without_username_targets_every_user_on_that_server() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        seed_connection(base, "https://api.example.com", "alice");
        seed_connection(base, "https://api.example.com", "bob");
        seed_connection(base, "https://other.example.com", "alice");

        let opts = LogoutOptions {
            server_url: "https://api.example.com".to_string(),
            username: None,
            base_dir: base.to_path_buf(),
            yes: true,
            json: false,
        };
        assert_eq!(logout_targets(&opts).len(), 2);

        let scoped = LogoutOptions {
            username: Some("bob".to_string()),
            ..opts.clone()
        };
        let targets = logout_targets(&scoped);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].username, "bob");

        run_logout(opts).unwrap();
        let left = all_connections(base);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].server_url, "https://other.example.com");
    }

    #[test]
    fn logout_over_https_does_not_touch_the_http_connection() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        seed_connection(base, "http://api.example.com", "alice");
        seed_connection(base, "https://api.example.com", "alice");
        assert_eq!(all_connections(base).len(), 2);

        run_logout(LogoutOptions {
            server_url: "https://api.example.com".to_string(),
            username: None,
            base_dir: base.to_path_buf(),
            yes: true,
            json: false,
        })
        .unwrap();

        let left = all_connections(base);
        assert_eq!(left.len(), 1, "{left:?}");
        assert_eq!(left[0].server_url, "http://api.example.com");
    }

    #[test]
    fn logout_on_one_port_does_not_touch_another() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        seed_connection(base, "https://api.example.com", "alice");
        seed_connection(base, "https://api.example.com:8443", "alice");

        run_logout(LogoutOptions {
            server_url: "https://api.example.com:8443".to_string(),
            username: None,
            base_dir: base.to_path_buf(),
            yes: true,
            json: false,
        })
        .unwrap();

        let left = all_connections(base);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].server_url, "https://api.example.com");
    }

    #[cfg(unix)]
    #[test]
    fn logout_never_follows_a_symlinked_connection_directory() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path().join("config");
        let outside = temp.path().join("precious");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keepme"), "important").unwrap();

        let server_dir = base.join("https_api.example.com");
        std::fs::create_dir_all(&server_dir).unwrap();
        std::os::unix::fs::symlink(&outside, server_dir.join("alice")).unwrap();

        // A symlinked user directory is not a connection to begin with.
        assert!(all_connections(&base).is_empty());

        // Even handed a hand-built Connection pointing at it, removal refuses.
        let connection = Connection {
            server_url: "https://api.example.com".to_string(),
            username: "alice".to_string(),
            device: "laptop".to_string(),
            logged_in_at: None,
            paths: ConnectionPaths::new(server_dir.join("alice")),
        };
        let error = remove_connection(&base, &connection).unwrap_err();
        assert!(error.contains("refusing to remove"), "{error}");
        assert!(
            outside.join("keepme").exists(),
            "the symlink target was followed and deleted"
        );
    }

    #[test]
    fn removal_refuses_a_path_outside_the_base_directory() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path().join("config");
        std::fs::create_dir_all(&base).unwrap();
        let outside = temp.path().join("elsewhere/https_api.example.com/alice");
        std::fs::create_dir_all(&outside).unwrap();

        let connection = Connection {
            server_url: "https://api.example.com".to_string(),
            username: "alice".to_string(),
            device: "laptop".to_string(),
            logged_in_at: None,
            paths: ConnectionPaths::new(outside.clone()),
        };
        let error = remove_connection(&base, &connection).unwrap_err();
        assert!(error.contains("outside"), "{error}");
        assert!(outside.exists());
    }

    #[test]
    fn status_lists_every_connection_and_guides_when_empty() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        let empty = render_status(&all_connections(base), false).unwrap();
        assert!(empty.contains("Not logged in"), "{empty}");
        assert!(empty.contains("login"), "{empty}");

        publish_login(base, "https://api.example.com", false).unwrap();
        publish_login(base, "https://other.example.com", false).unwrap();
        let listed = render_status(&all_connections(base), false).unwrap();
        assert!(listed.contains("https://api.example.com"), "{listed}");
        assert!(listed.contains("https://other.example.com"), "{listed}");
        assert!(!listed.contains(AGENT_TOKEN), "status leaked a token");
        assert!(!listed.contains(USER_TOKEN), "status leaked a token");

        let json = render_status(&all_connections(base), true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["connections"].as_array().unwrap().len(), 2);
        assert!(!json.contains(AGENT_TOKEN), "status json leaked a token");
    }

    #[test]
    fn one_device_can_hold_the_same_user_on_several_servers() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        publish_login(base, "https://s1.example.com", false).unwrap();
        publish_login(base, "https://s2.example.com", false).unwrap();
        assert_eq!(all_connections(base).len(), 2);
        assert_eq!(
            connections_for_server(base, "https://s1.example.com").len(),
            1
        );
    }

    #[tokio::test]
    async fn login_rejects_an_unusable_server_url_before_spending_the_code() {
        let temp = tempfile::TempDir::new().unwrap();
        for bad in [
            "https://api.example.com/path",
            "ftp://api.example.com",
            "https://user:pw@api.example.com",
            "https://api.example.com/?a=b",
        ] {
            let error = run_login(login_opts(temp.path(), bad, false))
                .await
                .unwrap_err();
            assert!(!error.contains(CODE), "error leaked the pairing code");
            assert!(all_connections(temp.path()).is_empty());
        }
    }
}
