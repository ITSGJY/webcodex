use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use super::system::discover_named_binary_absolute;

pub(crate) const SERVER_SERVICE_FILE: &str = "/etc/systemd/system/webcodex.service";
pub(crate) const SERVER_SERVICE_UNIT: &str = "webcodex.service";
pub(crate) const AGENT_SERVICE_FILE: &str = "/etc/systemd/system/webcodex-runner.service";
pub(crate) const AGENT_SERVICE_UNIT: &str = "webcodex-runner.service";
pub(crate) const DEFAULT_LOG_LINES: u32 = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessInvocation {
    pub(crate) operation: String,
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) unit: Option<String>,
    pub(crate) inherit_stdio: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessOutput {
    pub(crate) success: bool,
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

pub(crate) trait ProcessExecutor {
    fn execute(&mut self, invocation: &ProcessInvocation) -> Result<ProcessOutput, String>;
}

pub(crate) struct RealProcessExecutor;

impl ProcessExecutor for RealProcessExecutor {
    fn execute(&mut self, invocation: &ProcessInvocation) -> Result<ProcessOutput, String> {
        let mut command = Command::new(&invocation.program);
        command.args(&invocation.args);
        if invocation.inherit_stdio {
            let status = command
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .map_err(|e| format!("failed to execute {}: {}", invocation.operation, e))?;
            return Ok(ProcessOutput {
                success: status.success(),
                code: status.code(),
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        let output = command
            .output()
            .map_err(|e| format!("failed to execute {}: {}", invocation.operation, e))?;
        Ok(ProcessOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SystemdStatus {
    pub(crate) active: String,
    pub(crate) enabled: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceControl {
    Start,
    Stop,
    Restart,
}

impl ServiceControl {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstallUnitResult {
    pub(crate) unit: String,
    pub(crate) started: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UninstallUnitResult {
    pub(crate) unit: String,
    pub(crate) removed: bool,
}

fn validate_systemd_value(field: &str, value: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err(format!("invalid systemd {field} value: contains NUL"));
    }
    if value.contains('\n') || value.contains('\r') {
        return Err(format!(
            "invalid systemd {field} value: contains a line break"
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(format!(
            "invalid systemd {field} value: contains a control character"
        ));
    }
    Ok(())
}

pub(crate) fn encode_exec_argument(field: &str, value: &str) -> Result<String, String> {
    validate_systemd_value(field, value)?;
    if value.is_empty() {
        return Err(format!("invalid systemd {field} value: cannot be empty"));
    }
    let encoded = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    Ok(format!("\"{encoded}\""))
}

fn utf8_absolute_path<'a>(field: &str, path: &'a Path) -> Result<&'a str, String> {
    if !path.is_absolute() {
        return Err(format!(
            "invalid systemd {field} value: path must be absolute"
        ));
    }
    let value = path
        .to_str()
        .ok_or_else(|| format!("invalid systemd {field} value: path is not valid UTF-8"))?;
    validate_systemd_value(field, value)?;
    if value.is_empty() {
        return Err(format!(
            "invalid systemd {field} value: path cannot be empty"
        ));
    }
    Ok(value)
}

pub(crate) fn encode_exec_program(field: &str, path: &Path) -> Result<String, String> {
    let value = utf8_absolute_path(field, path)?;
    if value.contains('"') {
        return Err(format!(
            "invalid systemd {field} value: executable path cannot contain a double quote"
        ));
    }
    if value.contains('\\') {
        return Err(format!(
            "invalid systemd {field} value: executable path cannot contain a backslash"
        ));
    }
    Ok(format!("\"{}\"", value.replace('%', "%%")))
}

pub(crate) fn encode_exec_path_argument(field: &str, path: &Path) -> Result<String, String> {
    let value = path
        .to_str()
        .ok_or_else(|| format!("invalid systemd {field} value: path is not valid UTF-8"))?;
    encode_exec_argument(field, value)
}

pub(crate) fn encode_unit_path_value(field: &str, path: &Path) -> Result<String, String> {
    let value = utf8_absolute_path(field, path)?;
    let mut encoded = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            ' ' => encoded.push_str("\\x20"),
            '"' => encoded.push_str("\\x22"),
            '\\' => encoded.push_str("\\x5c"),
            '%' => encoded.push_str("%%"),
            _ => encoded.push(ch),
        }
    }
    Ok(encoded)
}

pub(crate) fn validate_systemd_identity(field: &str, value: &str) -> Result<(), String> {
    validate_systemd_value(field, value)?;
    if value.is_empty() {
        return Err(format!("invalid systemd {field} value: cannot be empty"));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(format!(
            "invalid systemd {field} value: use only ASCII letters, digits, '_', '-' or '.'"
        ));
    }
    Ok(())
}

pub(crate) fn systemctl_path() -> Result<PathBuf, String> {
    if !cfg!(target_os = "linux") {
        return Err("systemd service management is supported only on Linux".to_string());
    }
    discover_named_binary_absolute("systemctl").ok_or_else(|| {
        "systemctl was not found in an absolute PATH entry; install systemd or use a rendering-only mode"
            .to_string()
    })
}

pub(crate) fn journalctl_path() -> Result<PathBuf, String> {
    if !cfg!(target_os = "linux") {
        return Err("systemd journal access is supported only on Linux".to_string());
    }
    discover_named_binary_absolute("journalctl")
        .ok_or_else(|| "journalctl was not found in an absolute PATH entry".to_string())
}

pub(crate) fn service_unit_name(service_file: &Path, default: &str) -> String {
    service_file
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(default)
        .to_string()
}

fn systemctl_invocation(
    systemctl: &Path,
    operation: &str,
    args: Vec<String>,
    unit: Option<&str>,
) -> ProcessInvocation {
    ProcessInvocation {
        operation: operation.to_string(),
        program: systemctl.to_path_buf(),
        args,
        unit: unit.map(str::to_string),
        inherit_stdio: false,
    }
}

pub(crate) fn plan_install(systemctl: &Path, unit: &str, no_start: bool) -> Vec<ProcessInvocation> {
    let mut enable_args = vec!["enable".to_string()];
    if !no_start {
        enable_args.push("--now".to_string());
    }
    enable_args.push(unit.to_string());
    let verify_action = if no_start { "is-enabled" } else { "is-active" };
    vec![
        systemctl_invocation(
            systemctl,
            "systemctl daemon-reload",
            vec!["daemon-reload".to_string()],
            Some(unit),
        ),
        systemctl_invocation(systemctl, "systemctl enable", enable_args, Some(unit)),
        systemctl_invocation(
            systemctl,
            &format!("systemctl {verify_action}"),
            vec![
                verify_action.to_string(),
                "--quiet".to_string(),
                unit.to_string(),
            ],
            Some(unit),
        ),
    ]
}

pub(crate) fn plan_control(
    systemctl: &Path,
    unit: &str,
    control: ServiceControl,
) -> Vec<ProcessInvocation> {
    let action = control.as_str();
    let mut plan = vec![systemctl_invocation(
        systemctl,
        &format!("systemctl {action}"),
        vec![action.to_string(), unit.to_string()],
        Some(unit),
    )];
    if matches!(control, ServiceControl::Start | ServiceControl::Restart) {
        plan.push(systemctl_invocation(
            systemctl,
            "systemctl is-active",
            vec![
                "is-active".to_string(),
                "--quiet".to_string(),
                unit.to_string(),
            ],
            Some(unit),
        ));
    }
    plan
}

pub(crate) fn plan_uninstall_before_remove(systemctl: &Path, unit: &str) -> Vec<ProcessInvocation> {
    vec![
        systemctl_invocation(
            systemctl,
            "systemctl stop",
            vec!["stop".to_string(), unit.to_string()],
            Some(unit),
        ),
        systemctl_invocation(
            systemctl,
            "systemctl disable",
            vec!["disable".to_string(), unit.to_string()],
            Some(unit),
        ),
    ]
}

pub(crate) fn plan_uninstall_after_remove(systemctl: &Path, unit: &str) -> Vec<ProcessInvocation> {
    vec![
        systemctl_invocation(
            systemctl,
            "systemctl daemon-reload",
            vec!["daemon-reload".to_string()],
            Some(unit),
        ),
        systemctl_invocation(
            systemctl,
            "systemctl reset-failed",
            vec!["reset-failed".to_string(), unit.to_string()],
            Some(unit),
        ),
    ]
}

pub(crate) fn journalctl_invocation(
    journalctl: &Path,
    unit: &str,
    lines: u32,
    since: Option<&str>,
    follow: bool,
) -> ProcessInvocation {
    let mut args = vec![
        "--unit".to_string(),
        unit.to_string(),
        "--lines".to_string(),
        lines.to_string(),
        "--no-pager".to_string(),
    ];
    if let Some(since) = since {
        args.push("--since".to_string());
        args.push(since.to_string());
    }
    if follow {
        args.push("--follow".to_string());
    }
    ProcessInvocation {
        operation: "journalctl logs".to_string(),
        program: journalctl.to_path_buf(),
        args,
        unit: Some(unit.to_string()),
        inherit_stdio: follow,
    }
}

fn failure_detail(output: &ProcessOutput) -> String {
    let detail = output
        .stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .or_else(|| output.stdout.lines().find(|line| !line.trim().is_empty()))
        .unwrap_or("command returned a non-zero status");
    let mut detail = detail.trim().to_string();
    if detail.len() > 300 {
        detail.truncate(300);
        detail.push_str("...");
    }
    detail
}

pub(crate) fn execute_required<E: ProcessExecutor>(
    executor: &mut E,
    invocation: &ProcessInvocation,
) -> Result<ProcessOutput, String> {
    let output = executor.execute(invocation)?;
    if output.success {
        return Ok(output);
    }
    let unit = invocation.unit.as_deref().unwrap_or("systemd manager");
    Err(format!(
        "{} failed for {}: {}",
        invocation.operation,
        unit,
        failure_detail(&output)
    ))
}

pub(crate) fn execute_plan<E: ProcessExecutor>(
    executor: &mut E,
    plan: &[ProcessInvocation],
) -> Result<(), String> {
    for invocation in plan {
        execute_required(executor, invocation)?;
    }
    Ok(())
}

fn write_text_file_atomic(path: &Path, content: &str, overwrite: bool) -> Result<(), String> {
    if path.exists() && !overwrite {
        return Err(format!(
            "{} already exists; pass --overwrite to replace it",
            path.display()
        ));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| format!("{} must have a parent directory", path.display()))?;
    let metadata = std::fs::metadata(parent)
        .map_err(|e| format!("failed to inspect {}: {}", parent.display(), e))?;
    if !metadata.is_dir() {
        return Err(format!("{} is not a directory", parent.display()));
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{} has an invalid file name", path.display()))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temporary = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce));
    let result = (|| {
        use std::io::Write;
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o644);
        let mut file = options
            .open(&temporary)
            .map_err(|e| format!("failed to create {}: {}", temporary.display(), e))?;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("failed to write {}: {}", temporary.display(), e))?;
        file.sync_all()
            .map_err(|e| format!("failed to sync {}: {}", temporary.display(), e))?;
        std::fs::rename(&temporary, path).map_err(|e| {
            format!(
                "failed to atomically replace {} from {}: {}",
                path.display(),
                temporary.display(),
                e
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingUnitKind {
    Absent,
    ManagedRegularFile,
}

fn preflight_unit_path(path: &Path, overwrite: bool) -> Result<ExistingUnitKind, String> {
    if !path.is_absolute() {
        return Err(format!(
            "systemd unit path must be absolute: {}",
            path.display()
        ));
    }
    let existing = match std::fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!("failed to inspect {}: {}", path.display(), error));
        }
    };
    if let Some(metadata) = existing.as_ref() {
        if !overwrite {
            return Err(format!(
                "{} already exists; pass --overwrite to replace it",
                path.display()
            ));
        }
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            let masked = std::fs::read_link(path)
                .map(|target| target == Path::new("/dev/null"))
                .unwrap_or(false);
            let kind = if masked {
                "masked systemd unit"
            } else {
                "systemd unit symlink"
            };
            return Err(format!(
                "cannot safely overwrite {kind}: {}; replace or unmask the unit explicitly before retrying",
                path.display()
            ));
        }
        if !file_type.is_file() {
            return Err(format!(
                "cannot safely overwrite non-regular systemd unit: {}; replace it explicitly before retrying",
                path.display()
            ));
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let metadata = std::fs::metadata(parent)
        .map_err(|e| format!("failed to inspect {}: {}", parent.display(), e))?;
    if !metadata.is_dir() {
        return Err(format!("{} is not a directory", parent.display()));
    }
    Ok(if existing.is_some() {
        ExistingUnitKind::ManagedRegularFile
    } else {
        ExistingUnitKind::Absent
    })
}

fn restore_unit_file(path: &Path, previous: Option<&str>) -> Result<(), String> {
    match previous {
        Some(content) => write_text_file_atomic(path, content, true),
        None if path.exists() => std::fs::remove_file(path)
            .map_err(|e| format!("failed to remove {}: {}", path.display(), e)),
        None => Ok(()),
    }
}

fn rollback_unit(path: &Path, previous: Option<&str>) {
    let _ = restore_unit_file(path, previous);
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UnitDiscovery {
    load_state: String,
    fragment_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstallSnapshot {
    existing_kind: ExistingUnitKind,
    previous_content: Option<String>,
    active: String,
    enabled: String,
}

fn set_discovery_field(field: &mut Option<String>, key: &str, value: &str) -> Result<(), String> {
    match field {
        Some(previous) if previous != value => {
            Err(format!("conflicting {key} values in systemctl show output"))
        }
        Some(_) => Ok(()),
        None => {
            *field = Some(value.to_string());
            Ok(())
        }
    }
}

fn parse_unit_discovery(output: &str) -> Result<UnitDiscovery, String> {
    const MAX_DISCOVERY_OUTPUT: usize = 4096;
    if output.len() > MAX_DISCOVERY_OUTPUT {
        return Err("systemctl show output exceeded the discovery limit".to_string());
    }
    let mut load_state = None;
    let mut fragment_path = None;
    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "LoadState" => set_discovery_field(&mut load_state, key, value)?,
            "FragmentPath" => set_discovery_field(&mut fragment_path, key, value)?,
            _ => {}
        }
    }
    Ok(UnitDiscovery {
        load_state: load_state
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "systemctl show output did not contain LoadState".to_string())?,
        fragment_path: fragment_path
            .ok_or_else(|| "systemctl show output did not contain FragmentPath".to_string())?,
    })
}

fn discover_existing_unit<E: ProcessExecutor>(
    executor: &mut E,
    systemctl: &Path,
    unit: &str,
) -> Result<UnitDiscovery, String> {
    let invocation = systemctl_invocation(
        systemctl,
        "systemctl show",
        vec![
            "show".to_string(),
            unit.to_string(),
            "--property=LoadState".to_string(),
            "--property=FragmentPath".to_string(),
            "--no-pager".to_string(),
        ],
        Some(unit),
    );
    let output = executor.execute(&invocation).map_err(|_| {
        format!("cannot determine whether systemd unit {unit} already exists; no changes were made")
    })?;
    if !output.success {
        return Err(format!(
            "cannot determine whether systemd unit {unit} already exists; no changes were made"
        ));
    }
    parse_unit_discovery(&output.stdout).map_err(|_| {
        format!("cannot determine whether systemd unit {unit} already exists; no changes were made")
    })
}

fn classify_existing_unit(
    unit: &str,
    service_file: &Path,
    target_kind: ExistingUnitKind,
    discovery: &UnitDiscovery,
) -> Result<ExistingUnitKind, String> {
    let fragment_matches_target =
        !discovery.fragment_path.is_empty() && Path::new(&discovery.fragment_path) == service_file;
    match target_kind {
        ExistingUnitKind::ManagedRegularFile => match discovery.load_state.as_str() {
            "loaded" if fragment_matches_target => Ok(ExistingUnitKind::ManagedRegularFile),
            "loaded" => Err(format!(
                "systemd unit {unit} resolves outside {}; refusing to overwrite while FragmentPath differs",
                service_file.display()
            )),
            "not-found" if discovery.fragment_path.is_empty() => {
                Ok(ExistingUnitKind::ManagedRegularFile)
            }
            _ => Err(format!(
                "cannot determine whether systemd unit {unit} can be overwritten safely; no changes were made"
            )),
        },
        ExistingUnitKind::Absent => {
            if discovery.load_state == "not-found" && discovery.fragment_path.is_empty() {
                return Ok(ExistingUnitKind::Absent);
            }
            if !discovery.fragment_path.is_empty() && !fragment_matches_target {
                let mut fragment = discovery.fragment_path.clone();
                if fragment.len() > 240 {
                    fragment.truncate(240);
                    fragment.push_str("...");
                }
                return Err(format!(
                    "systemd unit {unit} already exists outside {}; refusing to create a local override implicitly (FragmentPath={fragment}); use an explicit override option only after reviewing the existing unit",
                    service_file.display()
                ));
            }
            Err(format!(
                "cannot determine whether systemd unit {unit} already exists; no changes were made"
            ))
        }
    }
}

fn query_status_output<E: ProcessExecutor>(
    executor: &mut E,
    systemctl: &Path,
    unit: &str,
    action: &str,
) -> String {
    let invocation = systemctl_invocation(
        systemctl,
        &format!("systemctl {action}"),
        vec![action.to_string(), unit.to_string()],
        Some(unit),
    );
    match executor.execute(&invocation) {
        Ok(output) => {
            let value = output.stdout.trim();
            if value.is_empty() {
                "unknown".to_string()
            } else {
                value.to_string()
            }
        }
        Err(_) => "unknown".to_string(),
    }
}

fn capture_install_snapshot<E: ProcessExecutor>(
    executor: &mut E,
    systemctl: &Path,
    service_file: &Path,
    unit: &str,
    existing_kind: ExistingUnitKind,
) -> Result<InstallSnapshot, String> {
    let previous_content = match existing_kind {
        ExistingUnitKind::ManagedRegularFile => Some(
            std::fs::read_to_string(service_file)
                .map_err(|e| format!("failed to read {}: {}", service_file.display(), e))?,
        ),
        ExistingUnitKind::Absent => None,
    };
    let active = query_status_output(executor, systemctl, unit, "is-active");
    let enabled = query_status_output(executor, systemctl, unit, "is-enabled");
    if matches!(existing_kind, ExistingUnitKind::ManagedRegularFile)
        && !matches!(enabled.as_str(), "enabled" | "disabled")
    {
        return Err(format!(
            "cannot safely overwrite {unit} while systemctl is-enabled reports '{enabled}'; normalize the unit to enabled or disabled before retrying"
        ));
    }
    Ok(InstallSnapshot {
        existing_kind,
        previous_content,
        active,
        enabled,
    })
}

fn rollback_invocation(systemctl: &Path, operation: &str, unit: &str) -> ProcessInvocation {
    systemctl_invocation(
        systemctl,
        &format!("systemctl {operation}"),
        vec![operation.to_string(), unit.to_string()],
        Some(unit),
    )
}

fn push_rollback_error(errors: &mut Vec<String>, label: &str, error: String) {
    let mut summary = format!("{label}: {error}");
    if summary.len() > 180 {
        summary.truncate(180);
        summary.push_str("...");
    }
    errors.push(summary);
}

fn best_effort_execute<E: ProcessExecutor>(
    executor: &mut E,
    invocation: &ProcessInvocation,
    label: &str,
    errors: &mut Vec<String>,
) {
    if let Err(error) = execute_allow_missing(executor, invocation) {
        push_rollback_error(errors, label, error);
    }
}

fn rollback_failed_install<E: ProcessExecutor>(
    executor: &mut E,
    systemctl: &Path,
    service_file: &Path,
    unit: &str,
    snapshot: &InstallSnapshot,
) -> Vec<String> {
    let mut errors = Vec::new();
    best_effort_execute(
        executor,
        &rollback_invocation(systemctl, "stop", unit),
        "stop failed",
        &mut errors,
    );

    if matches!(snapshot.existing_kind, ExistingUnitKind::Absent) {
        best_effort_execute(
            executor,
            &rollback_invocation(systemctl, "disable", unit),
            "disable failed",
            &mut errors,
        );
    }

    if let Err(error) = restore_unit_file(service_file, snapshot.previous_content.as_deref()) {
        push_rollback_error(&mut errors, "unit restore failed", error);
    }

    best_effort_execute(
        executor,
        &systemctl_invocation(
            systemctl,
            "systemctl daemon-reload",
            vec!["daemon-reload".to_string()],
            Some(unit),
        ),
        "daemon-reload failed",
        &mut errors,
    );

    if matches!(snapshot.existing_kind, ExistingUnitKind::ManagedRegularFile) {
        match snapshot.enabled.as_str() {
            "enabled" => best_effort_execute(
                executor,
                &rollback_invocation(systemctl, "enable", unit),
                "enabled state restore failed",
                &mut errors,
            ),
            "disabled" => best_effort_execute(
                executor,
                &rollback_invocation(systemctl, "disable", unit),
                "disabled state restore failed",
                &mut errors,
            ),
            _ => {}
        }
        if snapshot.active == "active" {
            best_effort_execute(
                executor,
                &rollback_invocation(systemctl, "start", unit),
                "active state restore failed",
                &mut errors,
            );
        }
    } else {
        best_effort_execute(
            executor,
            &rollback_invocation(systemctl, "reset-failed", unit),
            "reset-failed failed",
            &mut errors,
        );
    }
    errors
}

fn install_error_with_rollback(unit: &str, error: String, rollback_errors: Vec<String>) -> String {
    let mut message = format!("installation failed for {unit}: {error}");
    if !rollback_errors.is_empty() {
        let mut summary = rollback_errors.join("; ");
        if summary.len() > 600 {
            summary.truncate(600);
            summary.push_str("...");
        }
        message.push_str("; rollback also encountered: ");
        message.push_str(&summary);
    }
    message
}

pub(crate) fn install_unit_with_executor<E: ProcessExecutor>(
    executor: &mut E,
    systemctl: &Path,
    service_file: &Path,
    unit: &str,
    content: &str,
    overwrite: bool,
    no_start: bool,
) -> Result<InstallUnitResult, String> {
    let target_kind = preflight_unit_path(service_file, overwrite)?;
    let discovery = discover_existing_unit(executor, systemctl, unit)?;
    let existing_kind = classify_existing_unit(unit, service_file, target_kind, &discovery)?;
    let snapshot =
        capture_install_snapshot(executor, systemctl, service_file, unit, existing_kind)?;
    write_text_file_atomic(service_file, content, overwrite)?;
    if let Err(error) = execute_plan(executor, &plan_install(systemctl, unit, no_start)) {
        let rollback_errors =
            rollback_failed_install(executor, systemctl, service_file, unit, &snapshot);
        return Err(install_error_with_rollback(unit, error, rollback_errors));
    }
    Ok(InstallUnitResult {
        unit: unit.to_string(),
        started: !no_start,
    })
}

pub(crate) fn control_service_with_executor<E: ProcessExecutor>(
    executor: &mut E,
    systemctl: &Path,
    unit: &str,
    control: ServiceControl,
) -> Result<(), String> {
    execute_plan(executor, &plan_control(systemctl, unit, control))
}

fn missing_unit_failure(output: &ProcessOutput) -> bool {
    let detail = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    [
        "not loaded",
        "not found",
        "does not exist",
        "could not be found",
    ]
    .iter()
    .any(|needle| detail.contains(needle))
}

fn execute_allow_missing<E: ProcessExecutor>(
    executor: &mut E,
    invocation: &ProcessInvocation,
) -> Result<(), String> {
    let output = executor.execute(invocation)?;
    if output.success || missing_unit_failure(&output) {
        return Ok(());
    }
    let unit = invocation.unit.as_deref().unwrap_or("systemd manager");
    Err(format!(
        "{} failed for {}: {}",
        invocation.operation,
        unit,
        failure_detail(&output)
    ))
}

pub(crate) fn uninstall_unit_with_executor<E: ProcessExecutor>(
    executor: &mut E,
    systemctl: &Path,
    service_file: &Path,
    unit: &str,
) -> Result<UninstallUnitResult, String> {
    if !service_file.exists() {
        return Ok(UninstallUnitResult {
            unit: unit.to_string(),
            removed: false,
        });
    }
    let previous = std::fs::read_to_string(service_file)
        .map_err(|e| format!("failed to read {}: {}", service_file.display(), e))?;
    for invocation in plan_uninstall_before_remove(systemctl, unit) {
        execute_allow_missing(executor, &invocation)?;
    }
    std::fs::remove_file(service_file)
        .map_err(|e| format!("failed to remove {}: {}", service_file.display(), e))?;
    let after = plan_uninstall_after_remove(systemctl, unit);
    if let Err(error) = execute_required(executor, &after[0]) {
        rollback_unit(service_file, Some(&previous));
        return Err(error);
    }
    let _ = execute_allow_missing(executor, &after[1]);
    Ok(UninstallUnitResult {
        unit: unit.to_string(),
        removed: true,
    })
}

pub(crate) fn run_logs_with_executor<E: ProcessExecutor>(
    executor: &mut E,
    journalctl: &Path,
    unit: &str,
    lines: u32,
    since: Option<&str>,
    follow: bool,
) -> Result<String, String> {
    let invocation = journalctl_invocation(journalctl, unit, lines, since, follow);
    let output = execute_required(executor, &invocation)?;
    Ok(output.stdout)
}

pub(crate) fn install_unit(
    service_file: &Path,
    unit: &str,
    content: &str,
    overwrite: bool,
    no_start: bool,
) -> Result<InstallUnitResult, String> {
    preflight_unit_path(service_file, overwrite)?;
    let systemctl = systemctl_path()?;
    let mut executor = RealProcessExecutor;
    install_unit_with_executor(
        &mut executor,
        &systemctl,
        service_file,
        unit,
        content,
        overwrite,
        no_start,
    )
}

pub(crate) fn control_service(unit: &str, control: ServiceControl) -> Result<(), String> {
    let systemctl = systemctl_path()?;
    let mut executor = RealProcessExecutor;
    control_service_with_executor(&mut executor, &systemctl, unit, control)
}

pub(crate) fn uninstall_unit(
    service_file: &Path,
    unit: &str,
) -> Result<UninstallUnitResult, String> {
    let systemctl = systemctl_path()?;
    let mut executor = RealProcessExecutor;
    uninstall_unit_with_executor(&mut executor, &systemctl, service_file, unit)
}

pub(crate) fn run_logs(
    unit: &str,
    lines: u32,
    since: Option<&str>,
    follow: bool,
) -> Result<String, String> {
    let journalctl = journalctl_path()?;
    let mut executor = RealProcessExecutor;
    run_logs_with_executor(&mut executor, &journalctl, unit, lines, since, follow)
}

pub(crate) fn run_internal_binary(path: &Path, args: &[String]) -> Result<i32, String> {
    let mut command = Command::new(path);
    command.args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        Err(format!("failed to execute {}: {}", path.display(), error))
    }
    #[cfg(not(unix))]
    {
        let status = command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| format!("failed to execute {}: {}", path.display(), e))?;
        Ok(status.code().unwrap_or(1))
    }
}

pub(crate) fn query_systemd_service_status(service_name: &str) -> SystemdStatus {
    let Ok(systemctl) = systemctl_path() else {
        return SystemdStatus {
            active: "unknown".to_string(),
            enabled: "unknown".to_string(),
        };
    };
    let mut executor = RealProcessExecutor;
    SystemdStatus {
        active: query_status_output(&mut executor, &systemctl, service_name, "is-active"),
        enabled: query_status_output(&mut executor, &systemctl, service_name, "is-enabled"),
    }
}

pub(crate) fn query_systemd_status() -> SystemdStatus {
    query_systemd_service_status(SERVER_SERVICE_UNIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_plan_has_stable_order_and_no_start_semantics() {
        let systemctl = Path::new("/usr/bin/systemctl");
        let start = plan_install(systemctl, SERVER_SERVICE_UNIT, false);
        assert_eq!(start.len(), 3);
        assert_eq!(start[0].args, ["daemon-reload"]);
        assert_eq!(start[1].args, ["enable", "--now", SERVER_SERVICE_UNIT]);
        assert_eq!(start[2].args, ["is-active", "--quiet", SERVER_SERVICE_UNIT]);

        let no_start = plan_install(systemctl, AGENT_SERVICE_UNIT, true);
        assert_eq!(no_start[1].args, ["enable", AGENT_SERVICE_UNIT]);
        assert_eq!(
            no_start[2].args,
            ["is-enabled", "--quiet", AGENT_SERVICE_UNIT]
        );
    }

    #[test]
    fn lifecycle_and_logs_are_argv_plans_without_shell_strings() {
        let systemctl = Path::new("/usr/bin/systemctl");
        let restart = plan_control(systemctl, SERVER_SERVICE_UNIT, ServiceControl::Restart);
        assert_eq!(restart[0].program, systemctl);
        assert_eq!(restart[0].args, ["restart", SERVER_SERVICE_UNIT]);
        assert_eq!(
            restart[1].args,
            ["is-active", "--quiet", SERVER_SERVICE_UNIT]
        );

        let logs = journalctl_invocation(
            Path::new("/usr/bin/journalctl"),
            "webcodex-runner-work.service",
            75,
            Some("yesterday 12:00"),
            true,
        );
        assert_eq!(
            logs.args,
            [
                "--unit",
                "webcodex-runner-work.service",
                "--lines",
                "75",
                "--no-pager",
                "--since",
                "yesterday 12:00",
                "--follow"
            ]
        );
        assert!(logs.inherit_stdio);
    }

    #[derive(Default)]
    struct FakeExecutor {
        outputs: std::collections::VecDeque<Result<ProcessOutput, String>>,
        calls: Vec<Vec<String>>,
    }

    impl FakeExecutor {
        fn with_outputs(outputs: Vec<Result<ProcessOutput, String>>) -> Self {
            Self {
                outputs: outputs.into(),
                calls: Vec::new(),
            }
        }
    }

    impl ProcessExecutor for FakeExecutor {
        fn execute(&mut self, invocation: &ProcessInvocation) -> Result<ProcessOutput, String> {
            self.calls.push(invocation.args.clone());
            self.outputs
                .pop_front()
                .unwrap_or_else(|| panic!("missing fake output for {:?}", invocation.args))
        }
    }

    fn output(success: bool, stdout: &str, stderr: &str) -> Result<ProcessOutput, String> {
        Ok(ProcessOutput {
            success,
            code: Some(if success { 0 } else { 1 }),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        })
    }

    fn ok() -> Result<ProcessOutput, String> {
        output(true, "", "")
    }

    fn status(value: &str) -> Result<ProcessOutput, String> {
        output(value == "active" || value == "enabled", value, "")
    }

    fn discovery(load_state: &str, fragment_path: &str) -> Result<ProcessOutput, String> {
        output(
            true,
            &format!("LoadState={load_state}\nFragmentPath={fragment_path}\n"),
            "",
        )
    }

    fn absent_discovery() -> Result<ProcessOutput, String> {
        discovery("not-found", "")
    }

    fn failed(message: &str) -> Result<ProcessOutput, String> {
        output(false, "", message)
    }

    #[test]
    fn new_install_daemon_reload_failure_removes_unit_and_reloads_again() {
        let tmp = tempfile::tempdir().unwrap();
        let service_file = tmp.path().join("webcodex.service");
        let mut executor = FakeExecutor::with_outputs(vec![
            absent_discovery(),
            status("inactive"),
            status("disabled"),
            failed("reload rejected"),
            ok(),
            ok(),
            ok(),
            ok(),
        ]);
        let error = install_unit_with_executor(
            &mut executor,
            Path::new("/usr/bin/systemctl"),
            &service_file,
            SERVER_SERVICE_UNIT,
            "new unit",
            false,
            false,
        )
        .unwrap_err();
        assert!(error.contains("installation failed for webcodex.service"));
        assert!(!service_file.exists());
        assert_eq!(
            executor.calls[0],
            [
                "show",
                SERVER_SERVICE_UNIT,
                "--property=LoadState",
                "--property=FragmentPath",
                "--no-pager"
            ]
        );
        assert_eq!(executor.calls[3], ["daemon-reload"]);
        assert_eq!(executor.calls[4], ["stop", SERVER_SERVICE_UNIT]);
        assert_eq!(executor.calls[5], ["disable", SERVER_SERVICE_UNIT]);
        assert_eq!(executor.calls[6], ["daemon-reload"]);
        assert_eq!(executor.calls[7], ["reset-failed", SERVER_SERVICE_UNIT]);
    }

    #[test]
    fn new_install_verification_failure_stops_disables_and_removes_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let service_file = tmp.path().join("webcodex.service");
        let mut executor = FakeExecutor::with_outputs(vec![
            absent_discovery(),
            status("inactive"),
            status("disabled"),
            ok(),
            ok(),
            failed("not active"),
            ok(),
            ok(),
            ok(),
            ok(),
        ]);
        install_unit_with_executor(
            &mut executor,
            Path::new("/usr/bin/systemctl"),
            &service_file,
            SERVER_SERVICE_UNIT,
            "new unit",
            false,
            false,
        )
        .unwrap_err();
        assert!(!service_file.exists());
        assert_eq!(executor.calls[6], ["stop", SERVER_SERVICE_UNIT]);
        assert_eq!(executor.calls[7], ["disable", SERVER_SERVICE_UNIT]);
        assert_eq!(executor.calls[8], ["daemon-reload"]);
    }

    #[test]
    fn no_start_verification_failure_disables_without_starting() {
        let tmp = tempfile::tempdir().unwrap();
        let service_file = tmp.path().join("webcodex.service");
        let mut executor = FakeExecutor::with_outputs(vec![
            absent_discovery(),
            status("inactive"),
            status("disabled"),
            ok(),
            ok(),
            failed("not enabled"),
            ok(),
            ok(),
            ok(),
            ok(),
        ]);
        install_unit_with_executor(
            &mut executor,
            Path::new("/usr/bin/systemctl"),
            &service_file,
            SERVER_SERVICE_UNIT,
            "new unit",
            false,
            true,
        )
        .unwrap_err();
        assert!(!service_file.exists());
        assert!(executor
            .calls
            .iter()
            .any(|args| args == &["disable", SERVER_SERVICE_UNIT]));
        assert!(!executor
            .calls
            .iter()
            .any(|args| args == &["start", SERVER_SERVICE_UNIT]));
    }

    #[test]
    fn overwrite_failure_restores_unit_and_prior_active_enabled_state() {
        let tmp = tempfile::tempdir().unwrap();
        let service_file = tmp.path().join("webcodex.service");
        std::fs::write(&service_file, "old unit").unwrap();
        let mut executor = FakeExecutor::with_outputs(vec![
            discovery("loaded", service_file.to_str().unwrap()),
            status("active"),
            status("enabled"),
            ok(),
            ok(),
            failed("verification failed"),
            ok(),
            ok(),
            ok(),
            ok(),
        ]);
        install_unit_with_executor(
            &mut executor,
            Path::new("/usr/bin/systemctl"),
            &service_file,
            SERVER_SERVICE_UNIT,
            "new unit",
            true,
            false,
        )
        .unwrap_err();
        assert_eq!(std::fs::read_to_string(&service_file).unwrap(), "old unit");
        assert_eq!(executor.calls[6], ["stop", SERVER_SERVICE_UNIT]);
        assert_eq!(executor.calls[7], ["daemon-reload"]);
        assert_eq!(executor.calls[8], ["enable", SERVER_SERVICE_UNIT]);
        assert_eq!(executor.calls[9], ["start", SERVER_SERVICE_UNIT]);
    }

    #[test]
    fn rollback_failures_are_bounded_and_do_not_replace_install_error() {
        let tmp = tempfile::tempdir().unwrap();
        let service_file = tmp.path().join("webcodex.service");
        let long = "x".repeat(500);
        let mut executor = FakeExecutor::with_outputs(vec![
            absent_discovery(),
            status("inactive"),
            status("disabled"),
            failed("primary install failure"),
            failed(&long),
            failed(&long),
            failed(&long),
            failed(&long),
        ]);
        let error = install_unit_with_executor(
            &mut executor,
            Path::new("/usr/bin/systemctl"),
            &service_file,
            SERVER_SERVICE_UNIT,
            "new unit",
            false,
            false,
        )
        .unwrap_err();
        assert!(error.contains("primary install failure"));
        assert!(error.contains("rollback also encountered"));
        assert!(error.len() < 1000, "{}", error.len());
    }

    #[test]
    fn systemd_encoders_keep_distinct_program_argument_and_path_rules() {
        assert_eq!(
            encode_exec_argument("ExecStart argument", "/opt/web codex/a\"b\\c%p").unwrap(),
            "\"/opt/web codex/a\\\"b\\\\c%%p\""
        );
        assert_eq!(
            encode_exec_program("ExecStart", Path::new("/opt/web codex/server%p")).unwrap(),
            "\"/opt/web codex/server%%p\""
        );
        assert!(encode_exec_program("ExecStart", Path::new("/opt/a\"b")).is_err());
        assert!(encode_exec_program("ExecStart", Path::new("/opt/a\\b")).is_err());
        assert_eq!(
            encode_unit_path_value("WorkingDirectory", Path::new("/srv/web codex/a\"b\\c%p"))
                .unwrap(),
            "/srv/web\\x20codex/a\\x22b\\x5cc%%p"
        );
        for value in ["bad\nvalue", "bad\rvalue", "bad\0value", "bad\tvalue"] {
            assert!(encode_exec_argument("ExecStart argument", value).is_err());
        }
        for value in ["webcodex", "web_codex-1.service", "group.name"] {
            validate_systemd_identity("User", value).unwrap();
        }
        for value in [
            "",
            "bad user",
            "bad/group",
            "bad\\group",
            "bad=group",
            "bad\"group",
        ] {
            assert!(validate_systemd_identity("User", value).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn overwrite_rejects_symlink_masked_and_non_regular_units_before_systemctl() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let systemctl = Path::new("/usr/bin/systemctl");
        let target = tmp.path().join("target.service");
        std::fs::write(&target, "old").unwrap();

        let linked = tmp.path().join("linked.service");
        symlink(&target, &linked).unwrap();
        let mut executor = FakeExecutor::default();
        let error = install_unit_with_executor(
            &mut executor,
            systemctl,
            &linked,
            SERVER_SERVICE_UNIT,
            "new",
            true,
            false,
        )
        .unwrap_err();
        assert!(error.contains("systemd unit symlink"));
        assert!(executor.calls.is_empty());

        let masked = tmp.path().join("masked.service");
        symlink("/dev/null", &masked).unwrap();
        let error = install_unit_with_executor(
            &mut executor,
            systemctl,
            &masked,
            SERVER_SERVICE_UNIT,
            "new",
            true,
            false,
        )
        .unwrap_err();
        assert!(error.contains("masked systemd unit"));
        assert!(executor.calls.is_empty());

        let directory = tmp.path().join("directory.service");
        std::fs::create_dir(&directory).unwrap();
        let error = install_unit_with_executor(
            &mut executor,
            systemctl,
            &directory,
            SERVER_SERVICE_UNIT,
            "new",
            true,
            false,
        )
        .unwrap_err();
        assert!(error.contains("non-regular systemd unit"));
        assert!(executor.calls.is_empty());
    }

    #[test]
    fn overwrite_rejects_special_enabled_states_before_writing() {
        for state in [
            "enabled-runtime",
            "linked",
            "linked-runtime",
            "alias",
            "masked",
            "masked-runtime",
            "unknown",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let service_file = tmp.path().join("webcodex.service");
            std::fs::write(&service_file, "old unit").unwrap();
            let mut executor = FakeExecutor::with_outputs(vec![
                discovery("loaded", service_file.to_str().unwrap()),
                status("inactive"),
                status(state),
            ]);
            let error = install_unit_with_executor(
                &mut executor,
                Path::new("/usr/bin/systemctl"),
                &service_file,
                SERVER_SERVICE_UNIT,
                "new unit",
                true,
                false,
            )
            .unwrap_err();
            assert!(
                error.contains("cannot safely overwrite"),
                "{state}: {error}"
            );
            assert_eq!(std::fs::read_to_string(&service_file).unwrap(), "old unit");
            assert_eq!(executor.calls.len(), 3);
        }
    }

    #[test]
    fn genuinely_absent_unit_installs_after_explicit_not_found_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let service_file = tmp.path().join("webcodex.service");
        let mut executor = FakeExecutor::with_outputs(vec![
            absent_discovery(),
            status("inactive"),
            status("disabled"),
            ok(),
            ok(),
            ok(),
        ]);
        let result = install_unit_with_executor(
            &mut executor,
            Path::new("/usr/bin/systemctl"),
            &service_file,
            SERVER_SERVICE_UNIT,
            "new unit",
            false,
            false,
        )
        .unwrap();
        assert!(result.started);
        assert_eq!(std::fs::read_to_string(&service_file).unwrap(), "new unit");
        assert_eq!(executor.calls.len(), 6);
    }

    #[test]
    fn external_vendor_runtime_and_generated_units_are_rejected_before_side_effects() {
        for (load_state, fragment) in [
            ("loaded", "/usr/lib/systemd/system/webcodex.service"),
            ("loaded", "/run/systemd/system/webcodex.service"),
            ("loaded", "/run/systemd/generator/webcodex.service"),
            ("generated", "/run/systemd/generator.late/webcodex.service"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let service_file = tmp.path().join("webcodex.service");
            let mut executor = FakeExecutor::with_outputs(vec![discovery(load_state, fragment)]);
            let error = install_unit_with_executor(
                &mut executor,
                Path::new("/usr/bin/systemctl"),
                &service_file,
                SERVER_SERVICE_UNIT,
                "new unit",
                false,
                false,
            )
            .unwrap_err();
            assert!(error.contains("already exists outside"), "{error}");
            assert!(
                error.contains("refusing to create a local override"),
                "{error}"
            );
            assert!(!service_file.exists());
            assert_eq!(executor.calls.len(), 1);
            assert_eq!(executor.calls[0][0], "show");
        }
    }

    #[test]
    fn failed_or_ambiguous_unit_discovery_is_conservatively_rejected() {
        let cases = vec![
            Err("systemctl unavailable".to_string()),
            output(false, "", "manager unavailable"),
            output(true, "", ""),
            output(true, "LoadState=not-found\n", ""),
            output(true, "FragmentPath=\n", ""),
            output(
                true,
                "LoadState=not-found\nLoadState=loaded\nFragmentPath=\n",
                "",
            ),
        ];
        for discovery_output in cases {
            let tmp = tempfile::tempdir().unwrap();
            let service_file = tmp.path().join("webcodex.service");
            let mut executor = FakeExecutor::with_outputs(vec![discovery_output]);
            let error = install_unit_with_executor(
                &mut executor,
                Path::new("/usr/bin/systemctl"),
                &service_file,
                SERVER_SERVICE_UNIT,
                "new unit",
                false,
                false,
            )
            .unwrap_err();
            assert!(
                error.contains("cannot determine whether systemd unit"),
                "{error}"
            );
            assert!(error.contains("no changes were made"), "{error}");
            assert!(!service_file.exists());
            assert_eq!(executor.calls.len(), 1);
        }
    }

    #[test]
    fn discovery_parser_ignores_unknown_fields_and_rejects_conflicting_keys() {
        assert_eq!(
            parse_unit_discovery(
                "Description=ignored\nLoadState=loaded\nFragmentPath=/usr/lib/systemd/system/a.service\n"
            )
            .unwrap(),
            UnitDiscovery {
                load_state: "loaded".to_string(),
                fragment_path: "/usr/lib/systemd/system/a.service".to_string(),
            }
        );
        assert!(
            parse_unit_discovery("LoadState=loaded\nFragmentPath=/a\nFragmentPath=/b\n").is_err()
        );
    }

    #[test]
    fn unit_name_comes_from_selected_service_file() {
        assert_eq!(
            service_unit_name(
                Path::new("/etc/systemd/system/webcodex-runner-special.service"),
                AGENT_SERVICE_UNIT
            ),
            "webcodex-runner-special.service"
        );
    }
}
