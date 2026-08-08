//! Central platform directory policy shared by `webcodex-cli` and
//! `webcodex-runner`.
//!
//! Both binaries must agree on where per-user configuration, credentials,
//! runner state and logs live, and the rules differ per platform:
//!
//! - **Unix**: XDG-style layout rooted at `$HOME` (`~/.config/webcodex`,
//!   `~/.local/state/webcodex`), with `/etc/webcodex` for effective-root.
//!   Existing behavior is preserved exactly.
//! - **Windows**: a plain Windows environment has no `HOME`. Configuration and
//!   credentials live in `%APPDATA%\webcodex` (Roaming profile, follows the
//!   user), runner state/logs in `%LOCALAPPDATA%\webcodex` (machine-local).
//!   `USERPROFILE` is the home source and `HOME` is deliberately *not* used:
//!   on Windows `HOME` is either absent or a Git Bash/MSYS POSIX-style path
//!   like `/c/Users/...` that Windows APIs cannot consume.
//!
//! No derivation in this module ever falls back to the current working
//! directory; when no usable per-user directory exists the caller gets a
//! `Result` error instead of silently writing into a relative path.

use std::path::{Path, PathBuf};

/// Per-user home directory.
///
/// - Windows: `USERPROFILE` (set by the OS at logon; `HOME` is ignored).
/// - Unix: `HOME`.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.as_os_str().is_empty())
        .map(PathBuf::from)
        .filter(|path| !is_windows_style_home(path))
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.as_os_str().is_empty())
                .map(PathBuf::from)
        })
}

/// On Windows a `HOME` set by Git Bash / MSYS looks like `/c/Users/...`:
/// rooted but drive-relative and unusable by Windows APIs. It must never be
/// treated as the real home. On Unix there is no such concept.
fn is_windows_style_home(_path: &Path) -> bool {
    cfg!(windows) && {
        let text = _path.to_string_lossy();
        text.starts_with('/') || text.starts_with('\\')
    }
}

/// Effective-root detection. Only meaningful on Unix; on Windows always
/// `false` (there is no `/etc/webcodex` system scope).
pub fn is_effective_root() -> bool {
    #[cfg(unix)]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("Uid:") {
                    let mut parts = rest.split_whitespace();
                    let _real = parts.next();
                    if let Some(effective) = parts.next() {
                        return effective == "0";
                    }
                }
            }
        }
        std::env::var("USER").is_ok_and(|u| u == "root")
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Base directory for per-user WebCodex configuration and credentials
/// (client profiles, `agent.toml`, `projects.d`, token files).
///
/// - Unix (root): `/etc/webcodex`
/// - Unix (user): `$XDG_CONFIG_HOME/webcodex`, else `$HOME/.config/webcodex`.
///   When `HOME` is also missing the caller gets an error (never `.`).
/// - Windows: `%APPDATA%\webcodex`, else `%USERPROFILE%\.config\webcodex`.
pub fn default_client_config_base_dir() -> Result<PathBuf, String> {
    // An explicit XDG_CONFIG_HOME wins even for root, matching the historical
    // CLI behavior (see `omitted_scope_hosted_status_keeps_xdg_profile_paths_for_root`).
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(config_home).join("webcodex"));
    }
    if is_effective_root() {
        return Ok(PathBuf::from("/etc/webcodex"));
    }
    let home = home_dir()
        .ok_or_else(|| "cannot determine user home: set USERPROFILE (Windows) or HOME (Unix) to derive the WebCodex config directory".to_string())?;
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(appdata).join("webcodex"));
        }
    }
    Ok(home.join(".config/webcodex"))
}

/// Base directory for per-user WebCodex state: hosted Runner state
/// (`runner.toml`), Runner logs, checkpoints and recovery data.
///
/// - Unix: `$XDG_STATE_HOME/webcodex`, else `$HOME/.local/state/webcodex`,
///   else `$TMPDIR/webcodex` (existing behavior preserved).
/// - Windows: `%LOCALAPPDATA%\webcodex`, else
///   `%USERPROFILE%\.local\state\webcodex`.
pub fn default_client_state_base_dir() -> Result<PathBuf, String> {
    if let Some(state_home) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(state_home).join("webcodex"));
    }
    if let Some(home) = home_dir() {
        #[cfg(windows)]
        if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(local_appdata).join("webcodex"));
        }
        return Ok(home.join(".local/state/webcodex"));
    }
    // Existing Unix behavior: a volatile temp location is better than a
    // relative path for state that can be regenerated.
    Ok(std::env::temp_dir().join("webcodex"))
}

/// The per-user home as an absolute path, for deriving systemd user service
/// paths. Mirrors the historical `current_user_home` contract.
pub fn user_home() -> Result<PathBuf, String> {
    let home =
        home_dir().ok_or_else(|| "HOME is required to derive user service paths".to_string())?;
    if !home.is_absolute() {
        return Err("HOME must be an absolute path to derive user service paths".to_string());
    }
    Ok(home)
}

/// The per-user config root (`~/.config` equivalent), absolute.
pub fn user_config_home() -> Result<PathBuf, String> {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        let config_home = PathBuf::from(config_home);
        if !config_home.is_absolute() {
            return Err(
                "XDG_CONFIG_HOME must be an absolute path to derive user service paths".to_string(),
            );
        }
        return Ok(config_home);
    }
    Ok(user_home()?.join(".config"))
}

/// Compare two paths for equality under the filesystem's case rules.
/// Windows filesystems are case-insensitive; Unix comparisons are exact.
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        let a = normalize_path_identity(a);
        let b = normalize_path_identity(b);
        a == b
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

/// True when `path` equals `root` or lives underneath it, honoring Windows
/// case-insensitivity and `\\?\` extended-length prefixes.
///
/// The comparison is component-wise: `C:\Users\Alice2` is *not* under
/// `C:\Users\Alice`, even though the string starts with it.
pub fn path_is_within(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let path_components = normalized_components(path);
        let root_components = normalized_components(root);
        path_components.len() >= root_components.len()
            && path_components[..root_components.len()] == root_components[..]
    }
    #[cfg(not(windows))]
    {
        path == root || path.starts_with(root)
    }
}

#[cfg(windows)]
fn normalized_components(path: &Path) -> Vec<String> {
    let stripped = normalize_path_identity(path);
    Path::new(&stripped)
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

/// Stable filesystem-independent identity string for a path, used for project
/// id hashing and registry comparisons.
///
/// - Unix: the raw path bytes (unchanged historical behavior).
/// - Windows: strips `\\?\` / `\\?\UNC\` extended-length prefixes, normalizes
///   separators to `\`, and lowercases (Windows filesystems are
///   case-insensitive, so `C:\Foo` and `c:\foo` are the same directory).
pub fn normalize_path_identity(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        String::from_utf8_lossy(path.as_os_str().as_bytes()).into_owned()
    }
    #[cfg(not(unix))]
    {
        let mut text = path.to_string_lossy().replace('/', "\\");
        if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
            text = format!("\\\\{}", rest);
        } else if let Some(rest) = text.strip_prefix("\\\\?\\") {
            text = rest.to_string();
        }
        while text.len() > 1 && text.ends_with('\\') {
            text.pop();
        }
        text.to_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Single shared env-test lock for the whole crate: `lib.rs` tests and
    // `paths` tests both mutate process environment variables and must
    // serialize against each other.
    use crate::TEST_ENV_LOCK;

    /// RAII restore for environment variables: restores the previous value
    /// (or removes the variable) on drop, even if the test panics.
    struct EnvVarRestore {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarRestore {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(name);
            std::env::set_var(name, value);
            EnvVarRestore { name, previous }
        }

        fn remove(name: &'static str) -> Self {
            let previous = std::env::var_os(name);
            std::env::remove_var(name);
            EnvVarRestore { name, previous }
        }
    }

    impl Drop for EnvVarRestore {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

    #[test]
    fn home_dir_prefers_home_on_unix_and_ignores_msys_home_on_windows() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::set("HOME", "/home/alice");
        let _u = EnvVarRestore::remove("USERPROFILE");
        #[cfg(unix)]
        assert_eq!(home_dir(), Some(PathBuf::from("/home/alice")));
        #[cfg(windows)]
        assert_eq!(
            home_dir(),
            None,
            "MSYS-style HOME must not be used on Windows"
        );

        let _h2 = EnvVarRestore::set("HOME", "/c/Users/alice");
        let _u2 = EnvVarRestore::set("USERPROFILE", "C:\\Users\\alice");
        assert_eq!(home_dir(), Some(PathBuf::from("C:\\Users\\alice")));
    }

    #[test]
    fn home_dir_uses_userprofile_when_home_is_absent() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::remove("HOME");
        let _u = EnvVarRestore::set("USERPROFILE", "C:\\Users\\alice");
        #[cfg(windows)]
        assert_eq!(home_dir(), Some(PathBuf::from("C:\\Users\\alice")));
        #[cfg(not(windows))]
        assert_eq!(home_dir(), None);
    }

    #[test]
    fn config_base_never_falls_back_to_current_directory() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::remove("HOME");
        let _u = EnvVarRestore::remove("USERPROFILE");
        let _a = EnvVarRestore::remove("APPDATA");
        let _x = EnvVarRestore::remove("XDG_CONFIG_HOME");
        #[cfg(windows)]
        assert!(
            default_client_config_base_dir().is_err(),
            "no usable per-user directory must be an error, never CWD"
        );
        // On Unix without HOME the derivation must also fail closed; the
        // temporary-state fallback only applies to the state base.
        #[cfg(not(windows))]
        assert!(default_client_config_base_dir().is_err());
    }

    #[test]
    fn windows_config_base_uses_appdata_then_userprofile() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::remove("HOME");
        let _u = EnvVarRestore::set("USERPROFILE", "C:\\Users\\alice");
        let _x = EnvVarRestore::remove("XDG_CONFIG_HOME");
        #[cfg(windows)]
        {
            let _a = EnvVarRestore::set("APPDATA", "C:\\Users\\alice\\AppData\\Roaming");
            assert_eq!(
                default_client_config_base_dir().unwrap(),
                PathBuf::from("C:\\Users\\alice\\AppData\\Roaming\\webcodex")
            );
        }
        let _a2 = EnvVarRestore::remove("APPDATA");
        #[cfg(windows)]
        assert_eq!(
            default_client_config_base_dir().unwrap(),
            PathBuf::from("C:\\Users\\alice\\.config\\webcodex")
        );
    }

    #[test]
    fn windows_state_base_uses_localappdata_then_userprofile() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::remove("HOME");
        let _u = EnvVarRestore::set("USERPROFILE", "C:\\Users\\alice");
        let _x = EnvVarRestore::remove("XDG_STATE_HOME");
        #[cfg(windows)]
        {
            let _l = EnvVarRestore::set("LOCALAPPDATA", "C:\\Users\\alice\\AppData\\Local");
            assert_eq!(
                default_client_state_base_dir().unwrap(),
                PathBuf::from("C:\\Users\\alice\\AppData\\Local\\webcodex")
            );
        }
        let _l2 = EnvVarRestore::remove("LOCALAPPDATA");
        #[cfg(windows)]
        assert_eq!(
            default_client_state_base_dir().unwrap(),
            PathBuf::from("C:\\Users\\alice\\.local\\state\\webcodex")
        );
    }

    #[test]
    fn config_base_honors_xdg_and_platform_home() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::set("HOME", "/home/alice");
        let _u = EnvVarRestore::set("USERPROFILE", "C:\\Users\\alice");
        let _a = EnvVarRestore::set("APPDATA", "C:\\Users\\alice\\AppData\\Roaming");
        let _x = EnvVarRestore::set("XDG_CONFIG_HOME", "/tmp/cfg");
        assert_eq!(
            default_client_config_base_dir().unwrap(),
            PathBuf::from("/tmp/cfg/webcodex")
        );
        let _x2 = EnvVarRestore::remove("XDG_CONFIG_HOME");
        #[cfg(unix)]
        assert_eq!(
            default_client_config_base_dir().unwrap(),
            PathBuf::from("/home/alice/.config/webcodex")
        );
        #[cfg(windows)]
        assert_eq!(
            default_client_config_base_dir().unwrap(),
            PathBuf::from("C:\\Users\\alice\\AppData\\Roaming\\webcodex")
        );
    }

    #[test]
    fn state_base_honors_xdg_state_and_platform_home() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::set("HOME", "/home/alice");
        let _u = EnvVarRestore::set("USERPROFILE", "C:\\Users\\alice");
        let _l = EnvVarRestore::set("LOCALAPPDATA", "C:\\Users\\alice\\AppData\\Local");
        let _x = EnvVarRestore::set("XDG_STATE_HOME", "/tmp/state");
        assert_eq!(
            default_client_state_base_dir().unwrap(),
            PathBuf::from("/tmp/state/webcodex")
        );
        let _x2 = EnvVarRestore::remove("XDG_STATE_HOME");
        #[cfg(unix)]
        assert_eq!(
            default_client_state_base_dir().unwrap(),
            PathBuf::from("/home/alice/.local/state/webcodex")
        );
        #[cfg(windows)]
        assert_eq!(
            default_client_state_base_dir().unwrap(),
            PathBuf::from("C:\\Users\\alice\\AppData\\Local\\webcodex")
        );
    }

    #[test]
    fn state_base_falls_back_to_temp_only_when_no_home_anywhere() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::remove("HOME");
        let _x = EnvVarRestore::remove("XDG_STATE_HOME");
        let _u = EnvVarRestore::remove("USERPROFILE");
        let _l = EnvVarRestore::remove("LOCALAPPDATA");
        let _a = EnvVarRestore::remove("APPDATA");
        let base = default_client_state_base_dir().unwrap();
        assert!(base.is_absolute(), "state fallback must stay absolute");
        assert_eq!(base, std::env::temp_dir().join("webcodex"));
    }

    #[test]
    fn user_config_home_requires_absolute_home() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _x = EnvVarRestore::remove("XDG_CONFIG_HOME");
        let _u = EnvVarRestore::remove("USERPROFILE");
        let _h = EnvVarRestore::set("HOME", "/c/Users/alice");
        #[cfg(windows)]
        assert!(
            user_config_home().is_err(),
            "MSYS-style HOME is relative on Windows and must not be used"
        );
        #[cfg(unix)]
        assert_eq!(
            user_config_home().unwrap(),
            PathBuf::from("/c/Users/alice/.config")
        );
    }

    #[test]
    fn path_identity_is_case_insensitive_on_windows() {
        #[cfg(windows)]
        {
            assert_eq!(
                normalize_path_identity(Path::new(r"C:\Foo\Bar")),
                normalize_path_identity(Path::new(r"c:\foo\bar")),
            );
            assert_eq!(
                normalize_path_identity(Path::new(r"C:\Foo\Bar")),
                normalize_path_identity(Path::new(r"\\?\C:\Foo\Bar")),
            );
            assert_eq!(
                normalize_path_identity(Path::new(r"\\?\C:\Foo\Bar\")),
                normalize_path_identity(Path::new(r"C:\Foo\Bar")),
            );
            assert!(paths_equal(
                Path::new(r"C:\Foo\Bar"),
                Path::new(r"c:\foo\bar")
            ));
            assert!(path_is_within(
                Path::new(r"C:\Users\Alice\proj"),
                Path::new(r"c:\users\alice")
            ));
            assert!(!path_is_within(
                Path::new(r"C:\Users\Alice2\proj"),
                Path::new(r"c:\users\alice")
            ));
            assert_eq!(
                normalize_path_identity(Path::new(r"\\server\share\dir")),
                normalize_path_identity(Path::new(r"\\?\UNC\server\share\dir")),
            );
        }
        #[cfg(unix)]
        {
            assert_eq!(
                normalize_path_identity(Path::new("/home/alice/proj")),
                "/home/alice/proj"
            );
            assert!(paths_equal(
                Path::new("/home/alice/proj"),
                Path::new("/home/alice/proj")
            ));
            assert!(!paths_equal(
                Path::new("/home/alice/proj"),
                Path::new("/HOME/ALICE/PROJ")
            ));
        }
    }
}
