use std::path::{Path, PathBuf};

pub(crate) fn write_text_file(
    path: &Path,
    content: &str,
    overwrite: bool,
    secret: bool,
) -> Result<(), String> {
    if path.exists() && !overwrite {
        return Err(format!(
            "{} already exists; pass --overwrite to replace it",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {}", parent.display(), e))?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut options = std::fs::OpenOptions::new();
        options.write(true);
        if overwrite {
            options.create(true).truncate(true);
        } else {
            options.create_new(true);
        }
        if secret {
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
        use std::io::Write;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
        if secret {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("failed to set permissions on {}: {}", path.display(), e))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = secret;
        if overwrite {
            std::fs::write(path, content)
                .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
        } else {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
            use std::io::Write;
            file.write_all(content.as_bytes())
                .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
        }
    }
    Ok(())
}

pub(crate) fn discover_internal_binary(name: &str) -> Option<PathBuf> {
    discover_sibling_binary(name).or_else(|| discover_named_binary_absolute(name))
}

fn discover_sibling_binary(name: &str) -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let directory = current.parent()?;
    let candidate = directory.join(name);
    if candidate.is_file() {
        return Some(candidate);
    }
    #[cfg(windows)]
    {
        let candidate = directory.join(format!("{name}.exe"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn discover_named_binary_absolute(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if !dir.is_absolute() {
            continue;
        }
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let candidate = dir.join(format!("{}.exe", name));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
pub(crate) fn system_user_home(user: &str) -> Option<PathBuf> {
    use std::ffi::{CStr, CString, OsString};
    use std::os::unix::ffi::OsStringExt;

    let user = CString::new(user).ok()?;
    let initial_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let size = if initial_size > 0 {
        usize::try_from(initial_size).ok()?.clamp(1024, 1024 * 1024)
    } else {
        16 * 1024
    };
    let mut buffer = vec![0_u8; size];
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwnam_r(
            user.as_ptr(),
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    let record = unsafe { record.assume_init() };
    if record.pw_dir.is_null() {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(record.pw_dir) }.to_bytes().to_vec();
    let path = PathBuf::from(OsString::from_vec(bytes));
    path.is_absolute().then_some(path)
}

#[cfg(unix)]
pub(crate) fn system_user_is_root(user: &str) -> bool {
    use std::ffi::CString;

    if user == "root" || user.parse::<u32>().is_ok_and(|uid| uid == 0) {
        return true;
    }
    let Ok(user) = CString::new(user) else {
        return false;
    };
    let initial_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let size = if initial_size > 0 {
        usize::try_from(initial_size)
            .unwrap_or(16 * 1024)
            .clamp(1024, 1024 * 1024)
    } else {
        16 * 1024
    };
    let mut buffer = vec![0_u8; size];
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwnam_r(
            user.as_ptr(),
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    status == 0 && !result.is_null() && unsafe { record.assume_init().pw_uid == 0 }
}

#[cfg(not(unix))]
pub(crate) fn system_user_home(_user: &str) -> Option<PathBuf> {
    None
}

#[cfg(not(unix))]
pub(crate) fn system_user_is_root(user: &str) -> bool {
    user == "root" || user.parse::<u32>().is_ok_and(|uid| uid == 0)
}

/// Write `content` to `path` with 0600 permissions on Unix, creating parent
/// directories as needed. Used for one-time plaintext token files.
pub(crate) fn write_secret_file(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {}", parent.display(), e))?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
        use std::io::Write;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("failed to set permissions on {}: {}", path.display(), e))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content)
            .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
    }
    Ok(())
}

pub(crate) fn read_optional_token(
    path: &Option<PathBuf>,
    label: &str,
) -> Result<Option<String>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    let token = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {} {}: {}", label, path.display(), e))?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(format!("{} {} is empty", label, path.display()));
    }
    Ok(Some(token))
}

pub(crate) fn validate_user_api_token(token: &str) -> Result<(), String> {
    if token.trim().starts_with("wc_agent_") {
        return Err(
            "This is an Agent transport token and cannot be used for project/runtime APIs. Use the generated webcodex-user-token instead."
                .to_string(),
        );
    }
    Ok(())
}

pub(crate) fn read_optional_user_api_token(
    path: &Option<PathBuf>,
    label: &str,
) -> Result<Option<String>, String> {
    let token = read_optional_token(path, label)?;
    if let Some(token) = token.as_deref() {
        validate_user_api_token(token)?;
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_api_token_validation_rejects_agent_tokens_without_echoing_them() {
        let token = "wc_agent_do_not_echo_0123456789";
        let error = validate_user_api_token(token).unwrap_err();
        assert!(error.contains("Agent transport token"));
        assert!(error.contains("webcodex-user-token"));
        assert!(!error.contains(token));
    }

    #[test]
    fn user_api_token_validation_accepts_user_tokens() {
        validate_user_api_token("wc_pat_user_api_token_0123456789").unwrap();
        validate_user_api_token("shared-key-without-managed-prefix").unwrap();
    }

    #[test]
    fn root_system_identities_include_numeric_zero() {
        assert!(system_user_is_root("root"));
        assert!(system_user_is_root("0"));
        assert!(system_user_is_root("000"));
    }
}
