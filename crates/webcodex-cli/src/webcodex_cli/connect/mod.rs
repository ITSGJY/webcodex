mod output;
mod probe;
mod process;
mod profile;

pub(crate) use process::{
    local_runner_profile_marker, local_runner_state_summary, run_local_runner_logs,
    run_local_runner_service, LocalRunnerServiceAction,
};
pub(crate) use profile::ConnectOptions;

use super::connections::{canonical_server_url, ensure_real_directory_tree};
use super::login::validate_client_id;
use super::profiles::{
    client_output_dir_for_profile, client_state_dir_for_profile, default_client_base_dir,
    default_client_state_base_dir, validate_client_profile,
};
use super::system::discover_internal_binary;

use self::output::render_connect_output;
use self::probe::{preflight_shared_key, wait_for_connection};
use self::process::{
    ensure_runner_unlocked, load_runner_state, local_runner_log_path, process_matches,
    stop_runner_unlocked, RunnerStart,
};
use self::profile::{
    atomic_write, derived_profile, ensure_private_directory, generated_client_id,
    read_existing_agent_config, render_agent_document, render_project_file, resolve_key,
    resolve_project, validate_existing_profile, ProfileLock,
};

const DEFAULT_CONNECT_WAIT_MS: u64 = 15_000;

pub(crate) async fn run_connect(opts: ConnectOptions) -> Result<String, String> {
    let canonical_server = canonical_server_url(&opts.server_url)?;
    let canonical_project = opts.project.canonicalize().map_err(|error| {
        format!(
            "project path {} does not exist or cannot be resolved: {error}",
            opts.project.display()
        )
    })?;
    if !canonical_project.is_dir() {
        return Err(format!(
            "project path {} is not a directory",
            canonical_project.display()
        ));
    }
    let explicit_profile = opts
        .profile
        .as_deref()
        .map(validate_client_profile)
        .transpose()?;
    let config_base = opts
        .config_base
        .clone()
        .unwrap_or_else(default_client_base_dir);
    let state_base = opts
        .state_base
        .clone()
        .unwrap_or_else(default_client_state_base_dir);
    let resolved_key = resolve_key(
        &opts,
        &config_base,
        &canonical_server.url,
        &canonical_project,
    )?;
    let profile = explicit_profile
        .or(resolved_key.recovered_profile.clone())
        .unwrap_or_else(|| derived_profile(&canonical_server.url, &resolved_key.value));
    let profile = validate_client_profile(&profile)?;
    let config_base = ensure_real_directory_tree(&config_base)?;
    let state_base = ensure_real_directory_tree(&state_base)?;
    let profile_dir =
        ensure_private_directory(&client_output_dir_for_profile(&config_base, &profile))?;
    let state_dir = ensure_private_directory(&client_state_dir_for_profile(&state_base, &profile))?;
    let _lock = ProfileLock::acquire(&state_dir)?;

    let config_path = profile_dir.join("agent.toml");
    let projects_dir = ensure_private_directory(&profile_dir.join("projects.d"))?;
    let log_path = local_runner_log_path(&state_dir);
    let existing_config = read_existing_agent_config(&config_path)?;
    validate_existing_profile(
        existing_config.as_ref(),
        &canonical_server.url,
        &resolved_key.value,
    )?;
    let existing_summary = local_runner_state_summary(&state_dir)?;
    let client_id = match (&opts.client_id, existing_config.as_ref()) {
        (Some(requested), Some(existing)) => {
            let requested = validate_client_id(requested)?;
            if requested != existing.client_id && existing_summary.running {
                return Err(
                    "--client-id differs from the active profile; stop that Runner before changing its identity"
                        .to_string(),
                );
            }
            requested
        }
        (Some(requested), None) => validate_client_id(requested)?,
        (None, Some(existing)) => validate_client_id(&existing.client_id)?,
        (None, None) => generated_client_id(&canonical_server.url),
    };
    let (project_path, project, already_registered) = resolve_project(
        &projects_dir,
        &canonical_project,
        opts.project_id.as_deref(),
    )?;
    let runtime_project_id = format!("agent:{client_id}:{}", project.id);
    let runner_bin = opts
        .runner_bin
        .clone()
        .or_else(|| discover_internal_binary("webcodex-runner"))
        .ok_or_else(|| {
            "webcodex-runner was not found beside webcodex or in an absolute PATH entry".to_string()
        })?;

    // Fail before replacing a healthy profile when the destination cannot
    // authenticate this direct shared key at all.
    preflight_shared_key(&canonical_server.url, &resolved_key.value).await?;

    let project_changed = if already_registered {
        false
    } else {
        let project_content = render_project_file(&project)?;
        atomic_write(&project_path, project_content.as_bytes(), false)?
    };
    let agent_content = render_agent_document(
        &config_path,
        &canonical_server.url,
        &resolved_key.value,
        &client_id,
        &projects_dir,
        &canonical_project,
    )?;
    atomic_write(&config_path, agent_content.as_bytes(), true)?;
    atomic_write(
        &local_runner_profile_marker(&state_dir),
        format!("profile = {profile:?}\n").as_bytes(),
        false,
    )?;

    if project_changed
        && load_runner_state(&state_dir)?
            .as_ref()
            .is_some_and(process_matches)
    {
        stop_runner_unlocked(&state_dir)?;
    }
    let start = ensure_runner_unlocked(&runner_bin, &config_path, &state_dir).map_err(|error| {
        format!(
            "{error}. Runner logs: {}",
            local_runner_log_path(&state_dir).display()
        )
    })?;
    if let Err(error) = wait_for_connection(
        &canonical_server.url,
        &resolved_key.value,
        &client_id,
        &runtime_project_id,
        &state_dir,
        if opts.wait_timeout_ms == 0 {
            DEFAULT_CONNECT_WAIT_MS
        } else {
            opts.wait_timeout_ms
        },
    )
    .await
    {
        if start == RunnerStart::Started {
            let _ = stop_runner_unlocked(&state_dir);
        }
        return Err(format!("{error}. Runner logs: {}", log_path.display()));
    }
    if resolved_key.generated {
        atomic_write(
            &profile_dir.join(profile::KEY_DISCLOSED_FILE),
            b"disclosed = true\n",
            false,
        )?;
    }

    Ok(render_connect_output(
        &canonical_server.url,
        &profile,
        &client_id,
        &runtime_project_id,
        &config_path,
        &log_path,
        &resolved_key,
    ))
}
