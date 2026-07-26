use super::support::*;

#[test]
fn cli_help_and_version_exit_before_dispatch() {
    match cli_action(["--help"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("Usage: webcodex-cli"));
        }
        other => panic!("expected help exit, got {other:?}"),
    }
    match cli_action(["--version"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(stdout.starts_with(&format!(
                "webcodex-cli {} (commit ",
                env!("CARGO_PKG_VERSION")
            )));
            assert!(stdout.trim_end().ends_with(')'));
            assert_ne!(
                stdout,
                format!("webcodex-cli {}\n", env!("CARGO_PKG_VERSION"))
            );
        }
        other => panic!("expected version exit, got {other:?}"),
    }
}

#[test]
fn cli_version_output_includes_build_metadata() {
    match cli_action(["-V"]) {
        CliAction::Exit {
            code,
            stdout,
            stderr,
        } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("commit "));
            assert!(stdout.starts_with("webcodex-cli "));
            assert!(stderr.is_empty());
        }
        other => panic!("expected version exit, got {other:?}"),
    }
}

#[test]
fn removed_onboarding_and_doctor_commands_do_not_dispatch() {
    for command in ["connect", "doctor"] {
        match cli_action([command]) {
            CliAction::Exit {
                code: 2, stderr, ..
            } => assert!(stderr.contains("unknown command"), "{stderr}"),
            other => panic!("{command} unexpectedly dispatched: {other:?}"),
        }
    }
}

#[test]
fn webcodex_cli_help_mentions_management_commands() {
    match cli_action(["--help"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("pairing create"));
            assert!(stdout.contains("client enroll"));
            // The token actions are now listed once per group rather than one
            // line per action, but every action must still appear.
            for action in ["create-local", "generate", "register-hash", "list", "revoke"] {
                assert!(
                    stdout.contains(action),
                    "help no longer mentions token action {action}"
                );
            }
            assert!(stdout.contains("tokens create|"));
            assert!(stdout.contains("agent-tokens create|"));
            assert!(stdout.contains("agent init|install-service|status"));
        }
        other => panic!("expected help exit, got {other:?}"),
    }
}

#[test]
fn common_help_entrypoints_smoke() {
    let cases: &[(&[&str], &[&str])] = &[
        (
            &["--help"],
            &[
                "Usage: webcodex-cli <COMMAND>",
                "Commands:",
                "server up",
                "setup single-user",
            ],
        ),
        (
            &["server", "--help"],
            &[
                "Usage: webcodex-cli server <COMMAND>",
                "Commands:",
                "up",
                "init",
                "install-service",
                "status",
            ],
        ),
        (
            &["setup", "--help"],
            &[
                "Usage: webcodex-cli <COMMAND>",
                "Commands:",
                "setup single-user",
                "Common flags",
                "--server-url URL",
            ],
        ),
    ];

    for (args, expected) in cases {
        let out = cli_exit(args.iter().copied())
            .unwrap_or_else(|err| panic!("expected {args:?} help to exit successfully: {err}"));
        for needle in *expected {
            assert!(
                out.contains(needle),
                "help for {args:?} did not contain {needle:?}\n{out}"
            );
        }
    }
}

#[test]
fn webcodex_cli_agent_help_mentions_new_subcommands() {
    match cli_action(["agent", "--help"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("install-service"));
            assert!(stdout.contains("status"));
            assert!(stdout.contains("init"));
        }
        other => panic!("expected help exit, got {other:?}"),
    }
    match cli_action(["agent", "install-service", "--help"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("--config PATH"));
            assert!(stdout.contains("--bin PATH"));
            assert!(stdout.contains("Tokens are never inlined"));
        }
        other => panic!("expected help exit, got {other:?}"),
    }
    match cli_action(["agent", "status", "--help"]) {
        CliAction::Exit { code, stdout, .. } => {
            assert_eq!(code, 0);
            assert!(stdout.contains("--user-token-file PATH"));
            assert!(stdout.contains("--agent-token-file PATH"));
            assert!(stdout.contains("no tokens"));
        }
        other => panic!("expected help exit, got {other:?}"),
    }
}

#[test]
fn client_enroll_help_documents_profile_and_output_dir_precedence() {
    let help = client_enroll_usage();
    assert!(help.contains("--profile NAME"));
    assert!(help.contains("/etc/webcodex/clients/<profile>"));
    assert!(help.contains("~/.config/webcodex/clients/<profile>"));
    assert!(help.contains("Explicit --output-dir overrides"));
}

#[test]
fn singular_and_plural_token_groups_dispatch_identically() {
    // `tokens create-local` used to reach the admin parser, which has no such
    // action, so it failed with "unknown admin command" while the documented
    // `token create-local` worked. Both spellings now take the same path.
    for group in ["token", "tokens", "agent-token", "agent-tokens"] {
        match cli_action([group, "create-local", "--help"]) {
            CliAction::Exit { stdout, stderr, .. } => {
                let text = format!("{stdout}{stderr}");
                assert!(
                    text.contains("create-local"),
                    "{group} create-local was not recognized: {text}"
                );
                assert!(
                    !text.contains("unknown admin command"),
                    "{group} create-local still falls through to the admin parser: {text}"
                );
            }
            other => panic!("expected an exit for {group}, got {other:?}"),
        }
    }
}

#[test]
fn admin_token_actions_still_reach_the_admin_parser_under_both_spellings() {
    for group in ["token", "tokens"] {
        match cli_action([
            group,
            "list",
            "--server-url",
            "https://example.test",
            "--username",
            "alice",
        ]) {
            CliAction::Admin(_) => {}
            other => panic!("expected admin dispatch for {group} list, got {other:?}"),
        }
    }
}

#[test]
fn usage_lists_one_canonical_spelling_per_group() {
    // The old help text listed `user/users`, `token`, and `tokens` as separate
    // commands, which is what made the surface look twice its real size.
    match cli_action(["--help"]) {
        CliAction::Exit { stdout, .. } => {
            for canonical in ["users create|list", "tokens create|", "agent-tokens create|"] {
                assert!(
                    stdout.contains(canonical),
                    "help is missing {canonical}: {stdout}"
                );
            }
            assert!(
                !stdout.contains("user/users"),
                "help still advertises both spellings: {stdout}"
            );
        }
        other => panic!("expected help exit, got {other:?}"),
    }
}
