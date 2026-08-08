//! Small transport-agnostic helpers shared across the runner crate.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// A program name resolved to a concrete file, with the launch mode made
/// explicit.
///
/// On Windows, `.cmd`/`.bat` batch scripts cannot be launched like native PE
/// executables: `CreateProcess` refuses them directly and they must go
/// through `cmd.exe`. Rust's `Command` performs that delegation
/// automatically when the program has a `.cmd`/`.bat` extension, so callers
/// can still `Command::new(resolved.path())` — but resolution, availability
/// checks and spawn-error classification must know which kind was selected,
/// and an extensionless POSIX shim (npm-style) must never be selected in
/// place of a valid native program or batch script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedProgram {
    /// Native executable (`.exe`, `.com`, or an extensionless PE image).
    Native(PathBuf),
    /// Batch script (`.cmd` / `.bat`), launched via `cmd.exe`.
    Batch(PathBuf),
}

impl ResolvedProgram {
    pub(crate) fn path(&self) -> &Path {
        match self {
            ResolvedProgram::Native(path) | ResolvedProgram::Batch(path) => path,
        }
    }

    pub(crate) fn is_batch(&self) -> bool {
        matches!(self, ResolvedProgram::Batch(_))
    }
}

/// Resolve `name` against a `PATH`-style `OsStr` into a concrete program
/// file, honoring Windows executable semantics.
///
/// - Unix: unchanged historical behavior — first PATH directory containing an
///   executable file named `name`.
/// - Windows: `.exe`/`.com`/`.cmd`/`.bat` are found via `PATHEXT` (in
///   `PATHEXT` order, case-insensitively); an extensionless file is accepted
///   only when it is a real PE image (MZ header), so npm-style POSIX shims
///   are never selected and later fail with `ERROR_BAD_EXE_FORMAT`. `.cmd` /
///   `.bat` resolve to [`ResolvedProgram::Batch`].
pub(crate) fn resolve_program_in_path(name: &str, path: &OsStr) -> Option<ResolvedProgram> {
    #[cfg(windows)]
    {
        if Path::new(name).components().count() > 1 || Path::new(name).is_absolute() {
            // Path-qualified program: the user named this exact file. If it
            // exists it is used as-is (spawn surfaces real errors); PATHEXT
            // variants are not appended for explicit paths.
            return resolve_absolute_candidate(Path::new(name));
        }
        for directory in std::env::split_paths(path) {
            // PATHEXT candidates first: `foo.cmd` must win over an
            // extensionless `foo` shim in the same directory.
            for extension in pathext_extensions() {
                let candidate = directory.join(format!("{name}.{extension}"));
                if let Some(program) = classify_candidate(&candidate) {
                    return Some(program);
                }
            }
            // Extensionless file: valid only as a real PE image.
            let bare = directory.join(name);
            if is_executable_file(&bare) && is_pe_image(&bare) {
                return Some(ResolvedProgram::Native(bare));
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        find_executable_in_path(name, path).map(ResolvedProgram::Native)
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
    #[cfg(windows)]
    {
        resolve_program_in_path(name, path).map(|program| program.path().to_path_buf())
    }
    #[cfg(not(windows))]
    {
        for directory in std::env::split_paths(path) {
            let candidate = directory.join(name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
        None
    }
}

#[cfg(windows)]
fn resolve_absolute_candidate(name: &Path) -> Option<ResolvedProgram> {
    if is_executable_file(name) {
        classify_candidate(name)
    } else {
        None
    }
}

/// Classify an existing file by its extension. `.cmd`/`.bat` (case-
/// insensitive) are batch scripts; everything else that exists is treated as
/// a native program.
#[cfg(windows)]
fn classify_candidate(path: &Path) -> Option<ResolvedProgram> {
    if !is_executable_file(path) {
        return None;
    }
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("cmd") | Some("bat") => Some(ResolvedProgram::Batch(path.to_path_buf())),
        _ => Some(ResolvedProgram::Native(path.to_path_buf())),
    }
}

/// The `PATHEXT` extension list, lowercased, without leading dots, in
/// declared order. Missing or empty `PATHEXT` falls back to the Windows
/// default order (`com;exe;bat;cmd`).
#[cfg(windows)]
fn pathext_extensions() -> Vec<String> {
    let raw = std::env::var("PATHEXT").unwrap_or_default();
    let mut extensions: Vec<String> = raw
        .split(';')
        .map(|entry| entry.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|entry| !entry.is_empty())
        .collect();
    if extensions.is_empty() {
        extensions = ["com", "exe", "bat", "cmd"]
            .iter()
            .map(|entry| entry.to_string())
            .collect();
    }
    extensions
}

/// True when the file starts with the `MZ` DOS header, i.e. it is a PE
/// image that `CreateProcess` can execute directly. Extensionless POSIX shims
/// (npm-style) fail this check and are skipped during resolution.
#[cfg(windows)]
fn is_pe_image(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic).is_ok() && magic == *b"MZ"
}

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

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn temp_dir_with(name: &str, contents: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join(name);
        if !contents.is_empty() {
            std::fs::write(&file, contents).unwrap();
        }
        (temp, file)
    }

    fn pe_bytes() -> &'static [u8] {
        b"MZ\x90\x00fake"
    }

    #[test]
    fn resolves_pe_native_executable() {
        let (temp, executable) = temp_dir_with("foo.exe", pe_bytes());
        let path = std::env::join_paths([temp.path()]).unwrap();
        assert_eq!(
            resolve_program_in_path("foo", &path),
            Some(ResolvedProgram::Native(executable))
        );
    }

    #[test]
    fn resolves_batch_script_as_batch() {
        let (temp, script) = temp_dir_with("foo.cmd", b"@echo off\r\n");
        let path = std::env::join_paths([temp.path()]).unwrap();
        let resolved = resolve_program_in_path("foo", &path).unwrap();
        assert_eq!(resolved.path(), script);
        assert!(resolved.is_batch());
    }

    #[test]
    fn cmd_wins_over_extensionless_shim_in_same_directory() {
        let temp = tempfile::tempdir().unwrap();
        // npm-style extensionless POSIX shim next to the real .cmd shim.
        std::fs::write(temp.path().join("foo"), b"#!/bin/sh\nexec foo.cmd \"$@\"\n").unwrap();
        std::fs::write(temp.path().join("foo.cmd"), b"@echo off\r\n").unwrap();
        let path = std::env::join_paths([temp.path()]).unwrap();
        let resolved = resolve_program_in_path("foo", &path).unwrap();
        assert_eq!(
            resolved.path(),
            temp.path().join("foo.cmd"),
            "the .cmd shim must win over the extensionless shim"
        );
        assert!(resolved.is_batch());
    }

    #[test]
    fn extensionless_shim_alone_is_never_selected() {
        let (temp, shim) = temp_dir_with("foo", b"#!/bin/sh\nexit 0\n");
        let path = std::env::join_paths([temp.path()]).unwrap();
        assert_eq!(
            resolve_program_in_path("foo", &path),
            None,
            "a non-PE extensionless file must not resolve (it would fail with os error 193)"
        );
        let _ = shim;
    }

    #[test]
    fn extensionless_pe_file_is_accepted() {
        let (temp, program) = temp_dir_with("tool", pe_bytes());
        let path = std::env::join_paths([temp.path()]).unwrap();
        assert_eq!(
            resolve_program_in_path("tool", &path),
            Some(ResolvedProgram::Native(program))
        );
    }

    #[test]
    fn pathext_case_variations_are_insensitive() {
        let (temp, _script) = temp_dir_with("foo.CMD", b"@echo off\r\n");
        let path = std::env::join_paths([temp.path()]).unwrap();
        let resolved = resolve_program_in_path("foo", &path).unwrap();
        // The candidate name is built from the lowercased PATHEXT entry; on a
        // case-insensitive filesystem it resolves to the same `foo.CMD` file.
        assert_eq!(
            resolved.path().to_string_lossy().to_lowercase(),
            temp.path().join("foo.cmd").to_string_lossy().to_lowercase()
        );
        assert!(resolved.is_batch());
    }

    #[test]
    fn com_executable_is_native() {
        let (temp, program) = temp_dir_with("foo.com", pe_bytes());
        let path = std::env::join_paths([temp.path()]).unwrap();
        assert_eq!(
            resolve_program_in_path("foo", &path),
            Some(ResolvedProgram::Native(program))
        );
    }

    #[test]
    fn path_entries_with_spaces_work() {
        let temp = tempfile::tempdir().unwrap();
        let spaced = temp.path().join("dir with spaces");
        std::fs::create_dir(&spaced).unwrap();
        let executable = spaced.join("foo.exe");
        std::fs::write(&executable, pe_bytes()).unwrap();
        let path = std::env::join_paths([&spaced]).unwrap();
        assert_eq!(
            resolve_program_in_path("foo", &path),
            Some(ResolvedProgram::Native(executable))
        );
    }

    #[test]
    fn missing_program_resolves_to_none() {
        let temp = tempfile::tempdir().unwrap();
        let path = std::env::join_paths([temp.path()]).unwrap();
        assert_eq!(resolve_program_in_path("no-such-tool", &path), None);
    }

    #[test]
    fn absolute_path_with_spaces_resolves_directly() {
        let temp = tempfile::tempdir().unwrap();
        let spaced = temp.path().join("tool dir");
        std::fs::create_dir(&spaced).unwrap();
        let executable = spaced.join("probe.exe");
        std::fs::write(&executable, pe_bytes()).unwrap();
        assert_eq!(
            resolve_program_in_path(&executable.to_string_lossy(), &OsStr::new("")),
            Some(ResolvedProgram::Native(executable))
        );
    }

    #[test]
    fn absolute_batch_path_resolves_as_batch() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("run.cmd");
        std::fs::write(&script, b"@echo off\r\n").unwrap();
        assert_eq!(
            resolve_program_in_path(&script.to_string_lossy(), &OsStr::new("")),
            Some(ResolvedProgram::Batch(script))
        );
    }

    #[test]
    fn executable_lookup_matches_windows_exe_suffix_resolution() {
        let (temp, executable) = temp_dir_with("webcodex-path-probe.exe", pe_bytes());
        let path = std::env::join_paths([temp.path()]).unwrap();
        assert_eq!(
            find_executable_in_path("webcodex-path-probe", &path),
            Some(executable)
        );
    }
}
