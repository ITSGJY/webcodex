use std::path::{Path, PathBuf};

use super::env::is_effective_root;
use crate::ServiceScope;

pub(crate) const CLIENT_PROFILE_ERROR: &str =
    "--profile must be a safe path component using only ASCII letters, digits, '.', '_' or '-'";

pub(crate) fn default_client_base_dir() -> PathBuf {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty())
    {
        return PathBuf::from(config_home).join("webcodex");
    }
    if is_effective_root() {
        PathBuf::from("/etc/webcodex")
    } else {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".config/webcodex")
    }
}

pub(crate) fn current_user_home() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is required to derive user service paths".to_string())?;
    if !home.is_absolute() {
        return Err("HOME must be an absolute path to derive user service paths".to_string());
    }
    Ok(home)
}

pub(crate) fn user_config_home() -> Result<PathBuf, String> {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty())
    {
        let config_home = PathBuf::from(config_home);
        if !config_home.is_absolute() {
            return Err(
                "XDG_CONFIG_HOME must be an absolute path to derive user service paths".to_string(),
            );
        }
        return Ok(config_home);
    }
    Ok(current_user_home()?.join(".config"))
}

pub(crate) fn client_base_dir_for_scope(scope: ServiceScope) -> Result<PathBuf, String> {
    match scope {
        ServiceScope::User => Ok(user_config_home()?.join("webcodex")),
        ServiceScope::System => Ok(PathBuf::from("/etc/webcodex")),
    }
}

pub(crate) fn agent_config_for_scope(
    scope: ServiceScope,
    profile: Option<&str>,
) -> Result<PathBuf, String> {
    let base = client_base_dir_for_scope(scope)?;
    Ok(match profile {
        Some(profile) => client_output_dir_for_profile(&base, profile).join("agent.toml"),
        None => base.join("agent.toml"),
    })
}

pub(crate) fn client_profile_user_token_file_for_scope(
    scope: ServiceScope,
    profile: &str,
) -> Result<PathBuf, String> {
    Ok(
        client_output_dir_for_profile(&client_base_dir_for_scope(scope)?, profile)
            .join("webcodex-user-token"),
    )
}

pub(crate) fn client_profile_agent_token_file_for_scope(
    scope: ServiceScope,
    profile: &str,
) -> Result<PathBuf, String> {
    Ok(
        client_output_dir_for_profile(&client_base_dir_for_scope(scope)?, profile)
            .join("webcodex-runner-token"),
    )
}

pub(crate) fn user_systemd_unit_dir() -> Result<PathBuf, String> {
    Ok(user_config_home()?.join("systemd/user"))
}

pub(crate) fn agent_service_file_for_scope(
    scope: ServiceScope,
    profile: Option<&str>,
) -> Result<PathBuf, String> {
    let name = match profile {
        Some(profile) => format!("webcodex-runner-{profile}.service"),
        None => "webcodex-runner.service".to_string(),
    };
    let directory = match scope {
        ServiceScope::User => user_systemd_unit_dir()?,
        ServiceScope::System => PathBuf::from("/etc/systemd/system"),
    };
    Ok(directory.join(name))
}

pub(crate) fn validate_service_file_scope(
    scope: ServiceScope,
    service_file: &Path,
) -> Result<(), String> {
    if !service_file.is_absolute() {
        return Err("--service-file must be an absolute path".to_string());
    }
    let components = service_file
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    if components.iter().any(|component| *component == "..") {
        return Err("--service-file cannot contain '..' path components".to_string());
    }
    let is_user_unit_path = components
        .windows(2)
        .any(|pair| pair == ["systemd", "user"]);
    let is_system_unit_path = service_file.starts_with("/etc")
        || service_file.starts_with("/usr/lib/systemd/system")
        || service_file.starts_with("/usr/local/lib/systemd/system")
        || service_file.starts_with("/lib/systemd/system")
        || service_file.starts_with("/run/systemd/system")
        || components
            .windows(2)
            .any(|pair| pair == ["systemd", "system"]);
    match scope {
        ServiceScope::User if is_system_unit_path => Err(format!(
            "user scope cannot write a system unit path: {}",
            service_file.display()
        )),
        ServiceScope::System if is_user_unit_path => Err(format!(
            "system scope cannot write a user unit path: {}",
            service_file.display()
        )),
        _ => Ok(()),
    }
}

pub(crate) fn default_client_state_base_dir() -> PathBuf {
    if let Some(state_home) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(state_home).join("webcodex");
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".local/state/webcodex"))
        .unwrap_or_else(|| std::env::temp_dir().join("webcodex"))
}

pub(crate) fn validate_client_profile(profile: &str) -> Result<String, String> {
    let trimmed = profile.trim();
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.len() > 80
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(CLIENT_PROFILE_ERROR.to_string());
    }
    Ok(trimmed.to_string())
}

pub(crate) fn client_output_dir_for_profile(base_dir: &Path, profile: &str) -> PathBuf {
    base_dir.join("clients").join(profile)
}

pub(crate) fn client_profile_dir(profile: &str) -> PathBuf {
    client_output_dir_for_profile(&default_client_base_dir(), profile)
}

pub(crate) fn client_state_dir_for_profile(base_dir: &Path, profile: &str) -> PathBuf {
    base_dir.join("clients").join(profile)
}

pub(crate) fn client_profile_state_dir(profile: &str) -> PathBuf {
    client_state_dir_for_profile(&default_client_state_base_dir(), profile)
}

pub(crate) fn default_client_output_dir_for_profile(profile: &str) -> PathBuf {
    client_profile_dir(profile)
}

pub(crate) fn client_profile_agent_config(profile: &str) -> PathBuf {
    client_profile_dir(profile).join("agent.toml")
}

pub(crate) fn client_profile_projects_dir(profile: &str) -> PathBuf {
    client_profile_dir(profile).join("projects.d")
}
