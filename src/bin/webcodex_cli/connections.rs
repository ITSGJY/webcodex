//! Per-device connection storage: one directory per (server, user).
//!
//! A device logs into a server once, the way you log into any app. The local
//! layout mirrors that directly:
//!
//! ```text
//! ~/.config/webcodex/
//!   https_api.example.com/
//!     alice/
//!       server.toml               canonical server_url, username, device, time
//!       agent.toml                the agent token lives here, inline
//!       webcodex-user-token
//!       projects.d/
//! ```
//!
//! Server identity is the *canonical URL*, not the directory name. The slug is
//! lossy — it drops the `://` and cannot represent everything a URL can — so it
//! is only ever an index for a human reading `ls`. Every identity comparison
//! goes through [`canonical_server_url`], which is why `http://host` and
//! `https://host` are different connections rather than one that silently
//! overwrites the other.
//!
//! # Credential source of truth
//!
//! The agent token is stored **only** inline in `agent.toml`. `login` used to
//! also drop a `webcodex-agent-token` file, which left two copies that could
//! drift with nothing saying which one won. The user token keeps its own file
//! because a different consumer reads it (GPT Actions / MCP clients), not the
//! agent.

use std::path::{Path, PathBuf};

use super::env::is_effective_root;

/// Directory-name prefix reserved for in-progress and salvaged state.
///
/// [`user_slug`] rejects names starting with `.`, so a directory using this
/// prefix can never collide with a real user and [`list_connections`] can skip
/// it without guessing.
pub(crate) const INTERNAL_DIR_PREFIX: &str = ".";

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

/// A server URL reduced to the exact identity WebCodex uses for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CanonicalServerUrl {
    /// Canonical text form. Two inputs naming the same server produce the same
    /// string; this is what identity comparisons use.
    pub(crate) url: String,
    /// Filesystem-safe directory name. Lossy — never compare with this.
    pub(crate) slug: String,
}

/// Parse a server URL into its canonical identity.
///
/// Scheme is part of the identity: `http://host` and `https://host` are
/// different servers. Default ports normalize away (`https://host:443` is
/// `https://host`) but any other port is kept, so one host can run several
/// servers without them colliding.
///
/// Anything the WebCodex server URL has no meaning for is rejected rather than
/// silently dropped, because dropping it would make two different inputs look
/// like the same server: credentials, a query, a fragment, or a non-root path.
pub(crate) fn canonical_server_url(raw: &str) -> Result<CanonicalServerUrl, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("server URL cannot be empty".to_string());
    }
    let parsed =
        url::Url::parse(trimmed).map_err(|_| format!("not a valid server URL: {trimmed}"))?;

    let scheme = parsed.scheme();
    if !matches!(scheme, "http" | "https") {
        return Err(format!("server URL must use http or https, got {scheme}"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("server URL must not contain a username or password".to_string());
    }
    if parsed.query().is_some() {
        return Err("server URL must not contain a query string".to_string());
    }
    if parsed.fragment().is_some() {
        return Err("server URL must not contain a fragment".to_string());
    }
    if !matches!(parsed.path(), "" | "/") {
        return Err(format!(
            "server URL must point at the server root, got path {}",
            parsed.path()
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| format!("server URL has no host: {trimmed}"))?
        .trim_end_matches('.')
        .to_lowercase();
    if host.is_empty() {
        return Err(format!("server URL has no host: {trimmed}"));
    }

    let default_port = if scheme == "https" { 443 } else { 80 };
    let port = parsed.port().filter(|port| *port != default_port);

    let url = match port {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    };

    // The slug has to survive as one path component: IPv6 literals lose their
    // brackets and their colons, and the port joins with `_` rather than `:`.
    let host_slug = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .replace(':', "-");
    let slug = match port {
        Some(port) => format!("{scheme}_{host_slug}_{port}"),
        None => format!("{scheme}_{host_slug}"),
    };

    Ok(CanonicalServerUrl { url, slug })
}

/// Validate a username for use as a directory component.
///
/// A leading `.` is refused so that the reserved prefix used for staging,
/// backup, and recovery directories can never be produced by a real username.
pub(crate) fn user_slug(username: &str) -> Result<String, String> {
    let trimmed = username.trim();
    if trimmed.is_empty()
        || trimmed.starts_with(INTERNAL_DIR_PREFIX)
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
}

impl ConnectionPaths {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            descriptor: dir.join("server.toml"),
            agent_config: dir.join("agent.toml"),
            projects_dir: dir.join("projects.d"),
            user_token: dir.join("webcodex-user-token"),
            dir,
        }
    }

    pub(crate) fn resolve(base: &Path, server_url: &str, username: &str) -> Result<Self, String> {
        let server = canonical_server_url(server_url)?;
        let user = user_slug(username)?;
        Ok(Self::new(base.join(server.slug).join(user)))
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

/// True when `path` is a directory and not a symlink to one.
///
/// Symlinked server or user directories are ignored outright: following one
/// would let a link planted in the config directory steer a later `logout` at
/// an unrelated tree.
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

fn is_internal_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_none_or(|name| name.starts_with(INTERNAL_DIR_PREFIX))
}

/// Read one connection, refusing anything that is not the exact expected shape.
///
/// The descriptor's recorded `server_url` and `username` are treated as data,
/// never as a way to re-point the connection: `paths` always stays the
/// directory that was actually scanned.
fn read_connection(dir: PathBuf) -> Option<Connection> {
    if !is_real_dir(&dir) {
        return None;
    }
    let paths = ConnectionPaths::new(dir);
    if !is_regular_file(&paths.descriptor) {
        return None;
    }
    let content = std::fs::read_to_string(&paths.descriptor).ok()?;
    let server_url = descriptor_field(&content, "server_url")?;
    let username = descriptor_field(&content, "username")?;
    // A descriptor naming a server we cannot canonicalize is malformed; listing
    // it would put an entry in `status` that no `logout` could ever match.
    let canonical = canonical_server_url(&server_url).ok()?;
    user_slug(&username).ok()?;
    Some(Connection {
        server_url: canonical.url,
        username,
        device: descriptor_field(&content, "device").unwrap_or_default(),
        logged_in_at: descriptor_field(&content, "logged_in_at"),
        paths,
    })
}

fn real_subdirs(parent: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| !is_internal_name(&entry.file_name()))
        .map(|entry| entry.path())
        .filter(|path| is_real_dir(path))
        .collect();
    dirs.sort();
    dirs
}

/// Every connection recorded under `base`, sorted by server then user.
///
/// Staging, backup, and recovery directories are skipped, as are symlinked
/// directories and anything without a well-formed descriptor.
pub(crate) fn list_connections(base: &Path) -> Vec<Connection> {
    let mut found = Vec::new();
    for server_dir in real_subdirs(base) {
        for user_dir in real_subdirs(&server_dir) {
            if let Some(connection) = read_connection(user_dir) {
                found.push(connection);
            }
        }
    }
    found
}

/// Connections for one server, matched on canonical URL.
///
/// The slug is never used for this: it drops the scheme separator and so
/// cannot distinguish inputs that the canonical URL keeps apart.
pub(crate) fn connections_for_server(base: &Path, server_url: &str) -> Vec<Connection> {
    let Ok(wanted) = canonical_server_url(server_url) else {
        return Vec::new();
    };
    list_connections(base)
        .into_iter()
        .filter(|connection| connection.server_url == wanted.url)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(raw: &str) -> String {
        canonical_server_url(raw).unwrap().url
    }

    #[test]
    fn scheme_is_part_of_server_identity() {
        assert_ne!(
            canon("http://api.example.com"),
            canon("https://api.example.com")
        );
        assert_eq!(
            canonical_server_url("http://api.example.com").unwrap().slug,
            "http_api.example.com"
        );
        assert_eq!(
            canonical_server_url("https://api.example.com")
                .unwrap()
                .slug,
            "https_api.example.com"
        );
    }

    #[test]
    fn default_ports_normalize_and_other_ports_stay_distinct() {
        assert_eq!(
            canon("https://api.example.com:443"),
            canon("https://api.example.com")
        );
        assert_eq!(
            canon("http://api.example.com:80"),
            canon("http://api.example.com")
        );
        assert_ne!(
            canon("https://api.example.com:8443"),
            canon("https://api.example.com")
        );
        assert_ne!(
            canon("https://api.example.com:8443"),
            canon("https://api.example.com:9443")
        );
        assert_eq!(
            canonical_server_url("https://api.example.com:8443")
                .unwrap()
                .slug,
            "https_api.example.com_8443"
        );
        // A non-default port on one scheme is not the default port of another.
        assert_ne!(
            canon("http://api.example.com:443"),
            canon("https://api.example.com")
        );
    }

    #[test]
    fn host_spelling_variants_are_the_same_server() {
        let expected = canon("https://api.example.com");
        for variant in [
            "https://api.example.com/",
            "https://API.Example.COM",
            "https://api.example.com.",
            "  https://api.example.com  ",
            "https://api.example.com:443/",
        ] {
            assert_eq!(canon(variant), expected, "{variant}");
        }
    }

    #[test]
    fn urls_with_meaning_we_would_have_to_drop_are_rejected() {
        for raw in [
            "https://api.example.com/path-a",
            "https://api.example.com/path-b",
            "https://api.example.com/?a=b",
            "https://api.example.com/#frag",
            "https://user@api.example.com",
            "https://user:pw@api.example.com",
            "ftp://api.example.com",
            "file:///tmp",
            "not a url",
            "",
        ] {
            assert!(
                canonical_server_url(raw).is_err(),
                "should have been rejected: {raw:?}"
            );
        }
    }

    #[test]
    fn ipv6_literals_stay_path_safe_and_keep_their_identity() {
        let canonical = canonical_server_url("http://[::1]:8443").unwrap();
        assert_eq!(canonical.url, "http://[::1]:8443");
        assert!(!canonical.slug.contains(':'), "{}", canonical.slug);
        assert!(!canonical.slug.contains('['), "{}", canonical.slug);
        assert_eq!(canonical.slug, "http_--1_8443");
        assert_ne!(canon("http://[::1]:8443"), canon("https://[::1]:8443"));
    }

    #[test]
    fn user_slug_rejects_path_escapes_and_the_reserved_prefix() {
        assert_eq!(user_slug("Alice").unwrap(), "alice");
        for bad in [
            "",
            "..",
            ".",
            "a/b",
            "../../etc",
            ".staging-x",
            ".backup",
            "a b",
        ] {
            assert!(
                user_slug(bad).is_err(),
                "should have been rejected: {bad:?}"
            );
        }
    }

    fn seed(base: &Path, server: &str, user: &str) -> ConnectionPaths {
        let paths = ConnectionPaths::resolve(base, server, user).unwrap();
        std::fs::create_dir_all(&paths.dir).unwrap();
        let canonical = canonical_server_url(server).unwrap();
        std::fs::write(
            &paths.descriptor,
            descriptor_toml(&canonical.url, user, "laptop", "t"),
        )
        .unwrap();
        paths
    }

    #[test]
    fn http_and_https_are_separate_connections() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        seed(base, "http://api.example.com", "alice");
        seed(base, "https://api.example.com", "alice");
        assert_eq!(list_connections(base).len(), 2);

        let secure = connections_for_server(base, "https://api.example.com");
        assert_eq!(secure.len(), 1);
        assert_eq!(secure[0].server_url, "https://api.example.com");

        let plain = connections_for_server(base, "http://api.example.com");
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].server_url, "http://api.example.com");
    }

    #[test]
    fn lookup_matches_canonical_url_not_the_lossy_slug() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        seed(base, "https://api.example.com", "alice");
        seed(base, "https://api.example.com:8443", "alice");

        // Trailing slash, case, and the default port all reach the same one.
        for variant in [
            "https://api.example.com/",
            "https://API.EXAMPLE.COM",
            "https://api.example.com:443",
        ] {
            let hits = connections_for_server(base, variant);
            assert_eq!(hits.len(), 1, "{variant}: {hits:?}");
            assert_eq!(hits[0].server_url, "https://api.example.com");
        }

        let ported = connections_for_server(base, "https://api.example.com:8443");
        assert_eq!(ported.len(), 1);
        assert_eq!(ported[0].server_url, "https://api.example.com:8443");
    }

    #[test]
    fn one_server_can_hold_several_users() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        seed(base, "https://api.example.com", "alice");
        seed(base, "https://api.example.com", "bob");
        let hits = connections_for_server(base, "https://api.example.com");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].username, "alice");
        assert_eq!(hits[1].username, "bob");
    }

    #[test]
    fn internal_directories_are_never_listed_as_connections() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        let real = seed(base, "https://api.example.com", "alice");
        let server_dir = real.dir.parent().unwrap().to_path_buf();

        // A staging/backup/recovery directory holds a complete, valid
        // descriptor — only the reserved prefix keeps it out of the listing.
        for name in [
            ".staging-abc123",
            ".backup-abc123",
            ".recovery-alice-abc123",
        ] {
            let dir = server_dir.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("server.toml"),
                descriptor_toml("https://api.example.com", "alice", "laptop", "t"),
            )
            .unwrap();
        }
        // And one at the server level, in case a base-level scratch dir appears.
        std::fs::create_dir_all(base.join(".staging-server")).unwrap();

        let listed = list_connections(base);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].paths.dir, real.dir);
        assert_eq!(
            connections_for_server(base, "https://api.example.com").len(),
            1
        );
    }

    #[test]
    fn malformed_descriptors_are_ignored() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        let server_dir = base.join("https_api.example.com");

        for (user, body) in [
            ("no-username", "server_url = \"https://api.example.com\"\n"),
            ("no-server", "username = \"alice\"\n"),
            ("empty", ""),
            ("garbage", "}}}not toml{{{\n"),
            (
                "bad-server",
                "server_url = \"ftp://api.example.com\"\nusername = \"alice\"\n",
            ),
            (
                "bad-user",
                "server_url = \"https://api.example.com\"\nusername = \"../escape\"\n",
            ),
        ] {
            let dir = server_dir.join(user);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("server.toml"), body).unwrap();
        }
        // A user directory with no descriptor at all.
        std::fs::create_dir_all(server_dir.join("no-descriptor")).unwrap();

        assert!(
            list_connections(base).is_empty(),
            "{:?}",
            list_connections(base)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_and_descriptors_are_ignored() {
        let temp = tempfile::TempDir::new().unwrap();
        // `outside` must live beside the base, not under it, or it would be a
        // legitimate server directory rather than a symlink target.
        let base = &temp.path().join("config");
        std::fs::create_dir_all(base).unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(outside.join("alice")).unwrap();
        std::fs::write(
            outside.join("alice/server.toml"),
            descriptor_toml("https://api.example.com", "alice", "laptop", "t"),
        )
        .unwrap();

        // A symlinked server directory must not be walked into.
        std::os::unix::fs::symlink(&outside, base.join("https_evil.example.com")).unwrap();

        // Nor a symlinked user directory inside a real server directory.
        let server_dir = base.join("https_api.example.com");
        std::fs::create_dir_all(&server_dir).unwrap();
        std::os::unix::fs::symlink(outside.join("alice"), server_dir.join("alice")).unwrap();

        // Nor a real user directory whose descriptor is a symlink.
        let linked_descriptor = server_dir.join("bob");
        std::fs::create_dir_all(&linked_descriptor).unwrap();
        std::os::unix::fs::symlink(
            outside.join("alice/server.toml"),
            linked_descriptor.join("server.toml"),
        )
        .unwrap();

        assert!(
            list_connections(base).is_empty(),
            "{:?}",
            list_connections(base)
        );
    }

    #[test]
    fn descriptor_fields_cannot_redirect_the_connection_path() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        let paths = ConnectionPaths::resolve(base, "https://api.example.com", "alice").unwrap();
        std::fs::create_dir_all(&paths.dir).unwrap();
        // The descriptor claims a different user and server than its location.
        std::fs::write(
            &paths.descriptor,
            descriptor_toml("https://other.example.com", "mallory", "laptop", "t"),
        )
        .unwrap();

        let listed = list_connections(base);
        assert_eq!(listed.len(), 1);
        // Path stays where it was found, so removal can only ever touch here.
        assert_eq!(listed[0].paths.dir, paths.dir);
    }
}
