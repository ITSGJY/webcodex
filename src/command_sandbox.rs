//! Kernel command sandbox (Linux Landlock) for read_only tasks.
//!
//! Policy philosophy (docs/READ_ONLY_ENFORCEMENT_DESIGN.zh-CN.md): govern
//! WRITE access classes only. Read stays untouched, so the policy never needs
//! to enumerate readable paths — the failure mode that sank the competitor's
//! sandbox. The residual risk (a shell can read any file inside the checkout,
//! including committed secrets) is accepted and documented; `doctor` states
//! sandbox availability explicitly instead of degrading silently.
//!
//! Compiled into both the server (readiness probe) and the agent (probe at
//! registration, enforcement at spawn).

/// Probe whether the running kernel can enforce the write-denying ruleset.
/// Creating a ruleset file descriptor has no effect on the current process.
#[cfg(target_os = "linux")]
pub(crate) fn read_only_sandbox_available() -> Result<(), String> {
    use landlock::{AccessFs, Ruleset, RulesetAttr, ABI};
    Ruleset::default()
        .handle_access(AccessFs::from_write(ABI::V2))
        .map_err(|error| error.to_string())?
        .create()
        .map(|_| ())
        .map_err(|error| format!("Landlock unavailable: {error}"))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_only_sandbox_available() -> Result<(), String> {
    Err("command sandbox requires Linux with Landlock (kernel >= 5.13)".to_string())
}

/// Restrict the calling process (and every descendant, irrevocably) to the
/// write-denying ruleset, allowing writes only beneath `writable` paths.
/// Returns an error when the kernel would not fully enforce the policy —
/// callers must treat that as "do not run the command".
#[cfg(target_os = "linux")]
pub(crate) fn restrict_writes_to(writable: &[std::path::PathBuf]) -> Result<(), String> {
    use landlock::{
        path_beneath_rules, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, ABI,
    };
    let abi = ABI::V2;
    let status = Ruleset::default()
        .handle_access(AccessFs::from_write(abi))
        .map_err(|error| error.to_string())?
        .create()
        .map_err(|error| error.to_string())?
        .add_rules(path_beneath_rules(writable, AccessFs::from_write(abi)))
        .map_err(|error| error.to_string())?
        .restrict_self()
        .map_err(|error| error.to_string())?;
    match status.ruleset {
        RulesetStatus::NotEnforced => {
            Err("kernel did not enforce the Landlock ruleset".to_string())
        }
        _ => Ok(()),
    }
}

/// Arrange for `command` to run under the write-denying sandbox. Applied in
/// the child between fork and exec; a policy failure aborts the spawn instead
/// of running the command unsandboxed.
#[cfg(target_os = "linux")]
pub(crate) fn sandbox_command_read_only(
    command: &mut std::process::Command,
    writable: Vec<std::path::PathBuf>,
) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(move || restrict_writes_to(&writable).map_err(std::io::Error::other));
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn sandbox_command_read_only(
    _command: &mut std::process::Command,
    _writable: Vec<std::path::PathBuf>,
) {
    // Non-Linux hosts never advertise the capability, so this path is only
    // reachable by programming error; the request will fail at execution.
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn probe_reports_available_on_this_kernel() {
        assert_eq!(read_only_sandbox_available(), Ok(()));
    }

    #[test]
    fn sandboxed_command_cannot_write_the_project_but_reads_it_fine() {
        let project = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("tracked.txt"), "hello\n").unwrap();

        // Write into the project: the kernel must refuse.
        let mut denied = Command::new("sh");
        denied
            .arg("-c")
            .arg(format!("echo x > {}/evil.txt", project.path().display()))
            .current_dir(project.path());
        sandbox_command_read_only(&mut denied, vec![scratch.path().to_path_buf()]);
        let denied = denied.output().unwrap();
        assert!(
            !denied.status.success(),
            "project write must be denied: {:?}",
            denied
        );
        assert!(!project.path().join("evil.txt").exists());

        // Read the project and write into the allowed scratch dir: both fine.
        let mut allowed = Command::new("sh");
        allowed
            .arg("-c")
            .arg(format!(
                "cat {}/tracked.txt > {}/copy.txt",
                project.path().display(),
                scratch.path().display()
            ))
            .current_dir(project.path());
        sandbox_command_read_only(&mut allowed, vec![scratch.path().to_path_buf()]);
        let allowed = allowed.output().unwrap();
        assert!(allowed.status.success(), "{:?}", allowed);
        assert_eq!(
            std::fs::read_to_string(scratch.path().join("copy.txt")).unwrap(),
            "hello\n"
        );
    }
}
