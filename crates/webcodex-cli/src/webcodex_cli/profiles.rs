use std::path::{Path, PathBuf};

use super::env::is_effective_root;

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

pub(crate) fn client_profile_user_token_file(profile: &str) -> PathBuf {
    client_profile_dir(profile).join("webcodex-user-token")
}

pub(crate) fn client_profile_agent_token_file(profile: &str) -> PathBuf {
    client_profile_dir(profile).join("webcodex-runner-token")
}

pub(crate) fn client_profile_service_file(profile: &str) -> PathBuf {
    PathBuf::from(format!(
        "/etc/systemd/system/webcodex-runner-{}.service",
        profile
    ))
}
