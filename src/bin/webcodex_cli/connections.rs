//! Per-device connection storage: one directory per (server, user).
//!
//! A device logs into a server once, the way you log into any app. The local
//! layout mirrors that directly:
//!
//! ```text
//! ~/.config/webcodex/
//!   api.example.com/
//!     alice/
//!       server.toml               server_url, username, device, logged-in time
//!       agent.toml
//!       webcodex-user-token
//!       webcodex-agent-token
//!       projects.d/
//! ```
//!
//! The directory name is an index for humans reading `ls`; `server.toml` holds
//! the exact `server_url` the connection was created with. Matching an existing
//! connection goes through the file, never the directory name, so the slug
//! rules can change later without stranding a login.

use std::path::{Path, PathBuf};

use super::env::is_effective_root;

/// Where connections live when no explicit directory is given.
pub(crate) fn default_base_dir() -> PathBuf {
    if is_effective_root() {
        PathBuf::from("/etc/webcodex")
    } else {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".config/webcodex")
    }
}

/// Filesystem-safe directory name for a server URL.
///
/// Lowercased host, with a non-default port appended so that
/// `example.com` and `example.com:8443` stay distinct. IPv6 literals lose
/// their brackets and their colons become `-`, since `:` is awkward in a path
/// and illegal on some systems.
pub(crate) fn server_slug(server_url: &str) -> Result<String, String> {
    let parsed = url::Url::parse(server_url.trim())
        .map_err(|_| format!("not a valid server URL: {server_url}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "server URL must use http or https, got {}",
            parsed.scheme()
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("server URL has no host: {server_url}"))?
        .trim_end_matches('.')
        .to_lowercase();
    if host.is_empty() {
        return Err(format!("server URL has no host: {server_url}"));
    }
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .replace(':', "-");
    match parsed.port() {
        Some(port) => Ok(format!("{host}_{port}")),
        None => Ok(host),
    }
}

/// Validate a username for use as a directory component. The server already
/// constrains usernames; this is the local guard against a hostile or
/// surprising value reaching the filesystem.
pub(crate) fn user_slug(username: &str) -> Result<String, String> {
    let trimmed = username.trim();
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.len() > 80
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "username '{username}' cannot be used as a local directory name"
        ));
    }
    Ok(trimmed.to_lowercase())
}

/// Paths for one (server, user) connection on this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectionPaths {
    pub(crate) dir: PathBuf,
    pub(crate) descriptor: PathBuf,
    pub(crate) agent_config: PathBuf,
    pub(crate) projects_dir: PathBuf,
    pub(crate) user_token: PathBuf,
    pub(crate) agent_token: PathBuf,
}

impl ConnectionPaths {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            descriptor: dir.join("server.toml"),
            agent_config: dir.join("agent.toml"),
            projects_dir: dir.join("projects.d"),
            user_token: dir.join("webcodex-user-token"),
            agent_token: dir.join("webcodex-agent-token"),
            dir,
        }
    }

    pub(crate) fn resolve(base: &Path, server_url: &str, username: &str) -> Result<Self, String> {
        let server = server_slug(server_url)?;
        let user = user_slug(username)?;
        Ok(Self::new(base.join(server).join(user)))
    }
}

/// A connection as recorded on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Connection {
    pub(crate) server_url: String,
    pub(crate) username: String,
    pub(crate) device: String,
    pub(crate) logged_in_at: Option<String>,
    pub(crate) paths: ConnectionPaths,
}

pub(crate) fn descriptor_toml(
    server_url: &str,
    username: &str,
    device: &str,
    logged_in_at: &str,
) -> String {
    format!(
        "# Written by `webcodex-cli login`. The directory name is only an index;\n\
         # this file is the authoritative record of the connection.\n\
         server_url = {}\n\
         username = {}\n\
         device = {}\n\
         logged_in_at = {}\n",
        toml_string(server_url),
        toml_string(username),
        toml_string(device),
        toml_string(logged_in_at),
    )
}

fn toml_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn descriptor_field(content: &str, key: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let line = line.trim();
        let rest = line.strip_prefix(key)?;
        let rest = rest.trim_start().strip_prefix('=')?.trim();
        let rest = rest.strip_prefix('"')?;
        let rest = rest.strip_suffix('"')?;
        Some(rest.replace("\\\"", "\"").replace("\\\\", "\\"))
    })
}

fn read_connection(dir: PathBuf) -> Option<Connection> {
    let paths = ConnectionPaths::new(dir);
    let content = std::fs::read_to_string(&paths.descriptor).ok()?;
    Some(Connection {
        server_url: descriptor_field(&content, "server_url")?,
        username: descriptor_field(&content, "username")?,
        device: descriptor_field(&content, "device").unwrap_or_default(),
        logged_in_at: descriptor_field(&content, "logged_in_at"),
        paths,
    })
}

/// Every connection recorded under `base`, sorted by server then user.
pub(crate) fn list_connections(base: &Path) -> Vec<Connection> {
    let mut found = Vec::new();
    let Ok(servers) = std::fs::read_dir(base) else {
        return found;
    };
    let mut server_dirs: Vec<PathBuf> = servers
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    server_dirs.sort();
    for server_dir in server_dirs {
        let Ok(users) = std::fs::read_dir(&server_dir) else {
            continue;
        };
        let mut user_dirs: Vec<PathBuf> = users
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        user_dirs.sort();
        for user_dir in user_dirs {
            if let Some(connection) = read_connection(user_dir) {
                found.push(connection);
            }
        }
    }
    found
}

/// Connections for one server, matched on the recorded `server_url` rather
/// than on the directory name.
pub(crate) fn connections_for_server(base: &Path, server_url: &str) -> Vec<Connection> {
    let wanted = server_slug(server_url).ok();
    list_connections(base)
        .into_iter()
        .filter(|connection| {
            connection.server_url.trim_end_matches('/') == server_url.trim().trim_end_matches('/')
                || wanted
                    .as_deref()
                    .zip(server_slug(&connection.server_url).ok().as_deref())
                    .is_some_and(|(a, b)| a == b)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_slug_normalizes_host_and_keeps_ports_distinct() {
        assert_eq!(server_slug("https://api.example.com").unwrap(), "api.example.com");
        assert_eq!(server_slug("https://API.Example.COM/").unwrap(), "api.example.com");
        assert_eq!(server_slug("https://api.example.com.").unwrap(), "api.example.com");
        // A non-default port is a different server and must not collide.
        assert_eq!(
            server_slug("https://api.example.com:8443").unwrap(),
            "api.example.com_8443"
        );
        assert_ne!(
            server_slug("https://api.example.com").unwrap(),
            server_slug("https://api.example.com:8443").unwrap()
        );
        assert_eq!(server_slug("http://192.168.1.10:8080").unwrap(), "192.168.1.10_8080");
    }

    #[test]
    fn server_slug_makes_ipv6_literals_path_safe() {
        let slug = server_slug("http://[::1]:8443").unwrap();
        assert!(!slug.contains(':'), "slug still has a colon: {slug}");
        assert!(!slug.contains('['), "slug still has a bracket: {slug}");
        assert_eq!(slug, "--1_8443");
    }

    #[test]
    fn server_slug_rejects_unusable_urls() {
        assert!(server_slug("not a url").is_err());
        assert!(server_slug("ftp://example.com").is_err());
        assert!(server_slug("file:///tmp").is_err());
    }

    #[test]
    fn user_slug_rejects_path_escapes() {
        assert!(user_slug("alice").is_ok());
        assert!(user_slug("..").is_err());
        assert!(user_slug("a/b").is_err());
        assert!(user_slug("../../etc").is_err());
        assert!(user_slug("").is_err());
        assert_eq!(user_slug("Alice").unwrap(), "alice");
    }

    #[test]
    fn descriptor_round_trips_through_the_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        let paths = ConnectionPaths::resolve(base, "https://api.example.com:8443", "alice").unwrap();
        std::fs::create_dir_all(&paths.dir).unwrap();
        std::fs::write(
            &paths.descriptor,
            descriptor_toml(
                "https://api.example.com:8443",
                "alice",
                "laptop",
                "2026-07-26T10:00:00Z",
            ),
        )
        .unwrap();

        let found = list_connections(base);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].server_url, "https://api.example.com:8443");
        assert_eq!(found[0].username, "alice");
        assert_eq!(found[0].device, "laptop");
        assert_eq!(found[0].logged_in_at.as_deref(), Some("2026-07-26T10:00:00Z"));
    }

    #[test]
    fn lookup_matches_on_recorded_url_not_directory_name() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        for (url, user) in [
            ("https://api.example.com", "alice"),
            ("https://api.example.com:8443", "alice"),
            ("https://other.example.com", "bob"),
        ] {
            let paths = ConnectionPaths::resolve(base, url, user).unwrap();
            std::fs::create_dir_all(&paths.dir).unwrap();
            std::fs::write(&paths.descriptor, descriptor_toml(url, user, "laptop", "t")).unwrap();
        }

        // A trailing slash must still find the same connection.
        let hits = connections_for_server(base, "https://api.example.com/");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].server_url, "https://api.example.com");

        // The ported server is a separate connection.
        let ported = connections_for_server(base, "https://api.example.com:8443");
        assert_eq!(ported.len(), 1);
        assert_eq!(ported[0].server_url, "https://api.example.com:8443");

        assert_eq!(list_connections(base).len(), 3);
    }

    #[test]
    fn one_server_can_hold_several_users() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        for user in ["alice", "bob"] {
            let paths = ConnectionPaths::resolve(base, "https://api.example.com", user).unwrap();
            std::fs::create_dir_all(&paths.dir).unwrap();
            std::fs::write(
                &paths.descriptor,
                descriptor_toml("https://api.example.com", user, "laptop", "t"),
            )
            .unwrap();
        }
        let hits = connections_for_server(base, "https://api.example.com");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].username, "alice");
        assert_eq!(hits[1].username, "bob");
    }
}
