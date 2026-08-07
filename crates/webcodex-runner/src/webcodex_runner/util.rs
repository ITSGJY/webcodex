//! Small transport-agnostic helpers shared across the runner crate.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Return `true` if `haystack` contains any of `needles` as a substring.
///
/// Used by the error-classification helpers in [`crate::main`] (proxy/gateway
/// detection, connection-refused detection, TLS/auth failure detection) and
/// by the agent-transport error classifier in [`crate::webcodex_runner::transport`].
/// Both sites previously carried a byte-identical private copy of this one
/// liner; it has no behavioral coupling to either caller, so it lives here.
pub(crate) fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

/// True when `path` is a regular file that is executable.
///
/// On Unix this requires any execute bit (`& 0o111`); on other platforms any
/// regular file counts as executable (matching the platform's `Command`
/// semantics). The LSP supervisor (executable resolution + rustup-proxy
/// detection) and the validation executor (`resolve_executable`) previously
/// each carried a private copy that differed only in `path.metadata()` vs
/// `std::fs::metadata()` — `Path::metadata` is a thin wrapper over
/// `fs::metadata`, so the two were observationally identical.
pub(crate) fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Search `path` (a `PATH`-style `OsStr`) for the first directory containing an
/// executable named `name`, and return its full path.
///
/// Unifies the LSP supervisor's `find_executable_in_path` (used for both env
/// `PATH` lookup and profile-path resolution) with the validation executor's
/// `which_in_path`. Callers that need the ambient `PATH` should read
/// `std::env::var_os("PATH")` and pass it here.
pub(crate) fn find_executable_in_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    for directory in std::env::split_paths(path) {
        let candidate = directory.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        if Path::new(name).extension().is_none() {
            let candidate = directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn executable_lookup_matches_windows_exe_suffix_resolution() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("webcodex-path-probe.exe");
        std::fs::write(&executable, b"fixture").unwrap();
        let path = std::env::join_paths([temp.path()]).unwrap();

        assert_eq!(
            find_executable_in_path("webcodex-path-probe", &path),
            Some(executable)
        );
    }
}
