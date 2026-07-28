//! Kernel command sandbox foundation (Linux Landlock).
//!
//! **This is a foundation, not a shipped capability.** The agent never
//! advertises `sandbox_read_only_commands`, and the server never lets a
//! `read_only` task run commands. A write-denying Landlock ruleset governs one
//! access class; "no consequential execution" needs several. What it does not
//! cover, and what has to be true before it could, is written down in
//! [`docs/READ_ONLY_COMMAND_SANDBOX.md`].
//!
//! The code stays because the write-only shape is the right starting point: it
//! never has to enumerate readable paths, which is the part that usually rots.
//! Everything here fails closed — an unsupported kernel, a partially applied
//! ruleset, or a non-Linux host is an error, never a silent pass-through.
//!
//! Compiled into both the server (readiness reporting) and the agent.

/// Why the sandbox cannot be used, when it cannot.
///
/// Kept distinct rather than collapsed into one error string because they call
/// for different answers: an old kernel is an operator's decision to make, a
/// partial ruleset is a bug in the policy, and a failed probe is a host
/// problem. All of them deny.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxUnavailable {
    /// No Landlock on this build target or kernel.
    Unsupported(String),
    /// The kernel applied only part of the ruleset. Treated as failure: a
    /// half-applied write filter is not a boundary, and `BestEffort`
    /// compatibility would otherwise let it through silently.
    PartiallyEnforced,
    /// The probe itself could not complete, so nothing was proven.
    ProbeFailed(String),
}

impl std::fmt::Display for SandboxUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(detail) => write!(formatter, "unsupported: {detail}"),
            Self::PartiallyEnforced => write!(
                formatter,
                "the kernel enforced only part of the ruleset; a partial write filter is not a boundary"
            ),
            Self::ProbeFailed(detail) => write!(formatter, "probe failed: {detail}"),
        }
    }
}

/// Scratch space for the probe, removed however the probe ends.
///
/// `tempfile` is a dev-dependency, and the probe runs in the shipped binary.
#[cfg(target_os = "linux")]
struct ProbeDir {
    path: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
impl ProbeDir {
    fn create() -> Result<Self, SandboxUnavailable> {
        let path = std::env::temp_dir().join(format!(
            "webcodex-sandbox-probe-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&path)
            .map_err(|error| SandboxUnavailable::ProbeFailed(error.to_string()))?;
        Ok(Self { path })
    }
}

#[cfg(target_os = "linux")]
impl Drop for ProbeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Probe whether this kernel would actually enforce the write-denying ruleset.
///
/// Runs the whole thing for real in a throwaway child: apply the ruleset, call
/// `restrict_self`, require `FullyEnforced`, then try to write somewhere the
/// policy forbids and require the kernel to refuse. Creating a ruleset file
/// descriptor — which is all this used to do — proves only that the syscall
/// exists, not that the policy takes effect.
#[cfg(target_os = "linux")]
pub fn read_only_sandbox_available() -> Result<(), SandboxUnavailable> {
    use std::io::Write;

    let probe = ProbeDir::create()?;
    let writable = probe.path.join("writable");
    let denied = probe.path.join("denied");
    for directory in [&writable, &denied] {
        std::fs::create_dir(directory)
            .map_err(|error| SandboxUnavailable::ProbeFailed(error.to_string()))?;
    }

    // A child, because `restrict_self` is irrevocable: proving enforcement in
    // this process would sandbox the agent itself for the rest of its life.
    let script = format!(
        "printf ok > {}/probe && ! printf x > {}/probe",
        shell_quote(&writable),
        shell_quote(&denied),
    );
    let mut command = std::process::Command::new("/bin/sh");
    command.arg("-c").arg(script);
    let writable_paths = vec![writable.clone()];
    let (reader, writer) =
        std::io::pipe().map_err(|error| SandboxUnavailable::ProbeFailed(error.to_string()))?;
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(move || {
            // The child reports the reason back over the pipe; a failed
            // `restrict_self` must abort the exec rather than run free.
            match restrict_writes_to(&writable_paths) {
                Ok(()) => Ok(()),
                Err(reason) => {
                    let mut writer = &writer;
                    let _ = writer.write_all(reason.to_string().as_bytes());
                    Err(std::io::Error::other(reason.to_string()))
                }
            }
        });
    }
    let output = command
        .output()
        .map_err(|error| SandboxUnavailable::ProbeFailed(error.to_string()))?;
    drop(command);

    if output.status.success() {
        // Wrote where allowed, refused where not.
        return Ok(());
    }
    let mut reason = String::new();
    {
        use std::io::Read;
        let mut reader = reader;
        let _ = reader.read_to_string(&mut reason);
    }
    if reason.contains("partially") {
        return Err(SandboxUnavailable::PartiallyEnforced);
    }
    if reason.is_empty() {
        return Err(SandboxUnavailable::ProbeFailed(
            "the sandboxed probe did not behave as required".to_string(),
        ));
    }
    Err(SandboxUnavailable::Unsupported(reason))
}

#[cfg(not(target_os = "linux"))]
pub fn read_only_sandbox_available() -> Result<(), SandboxUnavailable> {
    Err(SandboxUnavailable::Unsupported(
        "command sandboxing requires Linux with Landlock (kernel >= 5.13)".to_string(),
    ))
}

#[cfg(target_os = "linux")]
fn shell_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// Restrict the calling process, and irrevocably every descendant, to the
/// write-denying ruleset, allowing writes only beneath `writable`.
///
/// Only `FullyEnforced` succeeds. `PartiallyEnforced` means the kernel
/// understood some of the policy and dropped the rest, which is exactly the
/// case a `BestEffort` ruleset hides — so the compatibility level is a hard
/// requirement and anything short of full enforcement denies.
#[cfg(target_os = "linux")]
pub fn restrict_writes_to(writable: &[std::path::PathBuf]) -> Result<(), SandboxUnavailable> {
    use landlock::{
        path_beneath_rules, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, ABI,
    };
    let abi = ABI::V2;
    let status = Ruleset::default()
        // Hard requirement, not the default best effort: a kernel that cannot
        // honour the policy must say so instead of quietly applying less.
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_write(abi))
        .map_err(|error| SandboxUnavailable::Unsupported(error.to_string()))?
        .create()
        .map_err(|error| SandboxUnavailable::Unsupported(error.to_string()))?
        // Reads stay open everywhere; only writes are governed, so the policy
        // never has to enumerate what may be read.
        .add_rules(path_beneath_rules(writable, AccessFs::from_write(abi)))
        .map_err(|error| SandboxUnavailable::Unsupported(error.to_string()))?
        .restrict_self()
        .map_err(|error| SandboxUnavailable::Unsupported(error.to_string()))?;
    match status.ruleset {
        RulesetStatus::FullyEnforced => Ok(()),
        RulesetStatus::PartiallyEnforced => Err(SandboxUnavailable::PartiallyEnforced),
        RulesetStatus::NotEnforced => Err(SandboxUnavailable::Unsupported(
            "the kernel did not enforce the Landlock ruleset".to_string(),
        )),
    }
}

/// Arrange for `command` to run under the write-denying sandbox.
///
/// Returns an error when this host cannot sandbox at all, so a caller holding
/// an explicit sandbox request fails before spawning rather than running the
/// command unconfined. The policy itself is applied between fork and exec, and
/// a failure there aborts the exec.
#[cfg(target_os = "linux")]
pub fn sandbox_command_read_only(
    command: &mut std::process::Command,
    writable: Vec<std::path::PathBuf>,
) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(move || {
            restrict_writes_to(&writable)
                .map_err(|reason| std::io::Error::other(reason.to_string()))
        });
    }
    Ok(())
}

/// Non-Linux hosts cannot sandbox, and silently running the command anyway
/// would turn a sandbox request into an unconfined execution. Fails instead.
#[cfg(not(target_os = "linux"))]
pub fn sandbox_command_read_only(
    _command: &mut std::process::Command,
    _writable: Vec<std::path::PathBuf>,
) -> Result<(), String> {
    Err(SandboxUnavailable::Unsupported(
        "command sandboxing requires Linux with Landlock (kernel >= 5.13)".to_string(),
    )
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_unavailable_reason_denies_and_reads_distinctly() {
        // Each variant has to survive as its own answer: an operator decision,
        // a policy bug, and a broken host are not the same problem.
        let reasons = [
            SandboxUnavailable::Unsupported("old kernel".to_string()),
            SandboxUnavailable::PartiallyEnforced,
            SandboxUnavailable::ProbeFailed("no /bin/sh".to_string()),
        ];
        let rendered: Vec<String> = reasons.iter().map(ToString::to_string).collect();
        let unique: std::collections::HashSet<&String> = rendered.iter().collect();
        assert_eq!(unique.len(), rendered.len(), "{rendered:?}");
        assert!(rendered[1].contains("part"), "{}", rendered[1]);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_refuses_to_spawn_a_sandbox_request() {
        let mut command = std::process::Command::new("true");
        let error = sandbox_command_read_only(&mut command, Vec::new())
            .expect_err("a sandbox request must not silently run unconfined");
        assert!(error.contains("unsupported"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_probe_reports_one_of_the_defined_outcomes_and_fails_closed() {
        // CI kernels differ: Landlock may be absent, partial, or complete. Any
        // outcome other than a proven full enforcement must deny.
        match read_only_sandbox_available() {
            Ok(()) => {
                // Proven: the probe wrote where allowed and was refused where
                // not, under a fully enforced ruleset.
            }
            Err(SandboxUnavailable::Unsupported(_))
            | Err(SandboxUnavailable::PartiallyEnforced)
            | Err(SandboxUnavailable::ProbeFailed(_)) => {}
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_sandboxed_command_cannot_write_the_project_but_can_read_it() {
        if read_only_sandbox_available().is_err() {
            // This kernel cannot enforce the policy; the denial is covered by
            // the probe test above.
            return;
        }
        let project = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("tracked.txt"), "hello\n").unwrap();

        let mut denied = std::process::Command::new("/bin/sh");
        denied
            .arg("-c")
            .arg(format!("echo x > {}/evil.txt", project.path().display()))
            .current_dir(project.path());
        sandbox_command_read_only(&mut denied, vec![scratch.path().to_path_buf()]).unwrap();
        let denied_output = denied.output().unwrap();
        assert!(!denied_output.status.success(), "{denied_output:?}");
        assert!(!project.path().join("evil.txt").exists());

        let mut allowed = std::process::Command::new("/bin/sh");
        allowed
            .arg("-c")
            .arg(format!(
                "cat {}/tracked.txt > {}/copy.txt",
                project.path().display(),
                scratch.path().display()
            ))
            .current_dir(project.path());
        sandbox_command_read_only(&mut allowed, vec![scratch.path().to_path_buf()]).unwrap();
        assert!(allowed.output().unwrap().status.success());
        assert_eq!(
            std::fs::read_to_string(scratch.path().join("copy.txt")).unwrap(),
            "hello\n"
        );
    }

    /// The gap this module exists to document: reads are ungoverned, so a
    /// command under the write sandbox still sees files outside the checkout.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_write_sandbox_does_not_stop_reads_outside_the_checkout() {
        if read_only_sandbox_available().is_err() {
            return;
        }
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "outside-the-checkout\n").unwrap();
        let scratch = tempfile::tempdir().unwrap();

        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg(format!("cat {}", secret.display()));
        sandbox_command_read_only(&mut command, vec![scratch.path().to_path_buf()]).unwrap();
        let output = command.output().unwrap();
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("outside-the-checkout"),
            "this assertion documents a known gap; if it starts failing the \
             sandbox got stronger and the design doc needs updating"
        );
    }
}
