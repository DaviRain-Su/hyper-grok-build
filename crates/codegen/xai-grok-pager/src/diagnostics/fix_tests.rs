use super::*;
use crate::clipboard::{ClipboardDelivery, NativeClipboardPreflight, Osc52Capability};
use crate::diagnostics::{DiagnosticFinding, FindingDisposition, ManualRemediation};
use crate::host::DisplayServer;
use crate::terminal::{MultiplexerKind, TerminalName};

fn report() -> DiagnosticReport {
    let mut report = DiagnosticReport {
        facts: crate::diagnostics::DiagnosticFacts {
            terminal: TerminalName::Ghostty,
            xtversion: crate::diagnostics::RuntimeFact::Unavailable,
            multiplexer: MultiplexerKind::Undetected,
            byobu: None,
            ssh: false,
            color: crate::diagnostics::ColorFacts {
                level: crate::diagnostics::RuntimeFact::Unavailable,
                available_themes: Vec::new(),
                total_themes: crate::theme::ThemeKind::ALL.len(),
            },
            keyboard: None,
            newline: None,
            clipboard: crate::diagnostics::ClipboardFacts {
                native_route: false,
                native_tool: "none".to_owned(),
                native_preflight: NativeClipboardPreflight::Disabled,
                tmux_route: false,
                osc52_route: false,
                osc52_capability: Osc52Capability::Unknown,
                wrap_sink: false,
                display_server: DisplayServer::Unknown,
                container_no_display: false,
                data_control: crate::diagnostics::DataControlFact::NotApplicable,
                delivery: ClipboardDelivery::Failed,
                fix: None,
            },
            voice: None,
        },
        findings: Vec::new(),
        probe_notes: Vec::new(),
    };
    report.findings.push(DiagnosticFinding {
        id: SSH_WRAP_ID,
        disposition: FindingDisposition::Recommendation,
        message: "Use local SSH wrapping".to_owned(),
        remediation: Some(ManualRemediation {
            fix: SSH_WRAP_ONE_OFF.to_owned(),
            config_path: None,
        }),
        automatic_remediation: Some(ssh_wrap_automatic_remediation()),
        note: None,
    });
    report
}

fn terminal() -> TerminalContext {
    TerminalContext {
        brand: TerminalName::Ghostty,
        env_brand: TerminalName::Ghostty,
        multiplexer: MultiplexerKind::Undetected,
        byobu: None,
        embedded_editor: None,
        tmux_meta: Default::default(),
        is_ssh: false,
        is_official_vscode_remote: false,
        term_var: Some("xterm-256color".to_owned()),
        tmux_version: None,
        vte_version: None,
        tmux_extended_keys: None,
        term_program_version: None,
    }
}

fn request(home: &Path, shell: &str) -> FixRequest {
    FixRequest {
        id: SSH_WRAP_ID,
        home: home.to_path_buf(),
        shell: Some(PathBuf::from(shell)),
        validator: None,
    }
}

#[test]
fn canonical_and_short_ids_resolve_to_canonical_id() {
    assert_eq!(resolve_fix_id("terminal.ssh-wrap").unwrap(), SSH_WRAP_ID);
    let command = human_fix_command(SSH_WRAP_ID).expect("SSH fix command");
    assert_eq!(command, "grok doctor fix ssh-wrap");
    assert_eq!(
        resolve_fix_id(command.strip_prefix("grok doctor fix ").unwrap()).unwrap(),
        SSH_WRAP_ID
    );
    assert!(human_fix_command(DiagnosticId::new("terminal", "unknown")).is_none());
    assert!(matches!(
        resolve_fix_id("terminal.unknown"),
        Err(FixError::UnknownId(_))
    ));
}

#[test]
fn bash_zsh_and_fish_plans_use_exact_paths_and_aliases() {
    let temp = tempfile::tempdir().unwrap();
    for (shell, relative, alias) in [
        ("/bin/bash", ".bashrc", "alias ssh='grok wrap ssh'"),
        ("/bin/zsh", ".zshrc", "alias ssh='grok wrap ssh'"),
        (
            "/usr/local/bin/fish",
            ".config/fish/config.fish",
            "alias ssh 'grok wrap ssh'",
        ),
    ] {
        let plan = plan_fix(request(temp.path(), shell), &report(), &terminal()).unwrap();
        assert_eq!(plan.id, SSH_WRAP_ID);
        assert_eq!(plan.changes[0].requested_path, temp.path().join(relative));
        assert_eq!(
            plan.changes[0].block,
            format!(
                "# >>> grok doctor >>>\n# >>> terminal.ssh-wrap >>>\n{alias}\n# <<< terminal.ssh-wrap <<<\n# <<< grok doctor <<<"
            )
        );
        assert!(plan.caveats.iter().any(|line| line.contains("command ssh")));
        assert!(plan.caveats.iter().any(|line| line.contains("ssh -f")));
        assert!(
            plan.caveats
                .iter()
                .any(|line| line.contains("ControlPersist"))
        );
        assert!(plan.caveats.iter().any(|line| line.contains("~^Z")));
    }
}

#[test]
fn remote_vscode_and_unsupported_shell_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let mut remote = terminal();
    remote.is_ssh = true;
    assert!(matches!(
        plan_fix(request(temp.path(), "/bin/zsh"), &report(), &remote),
        Err(FixError::RemoteSession)
    ));

    let mut vscode = terminal();
    vscode.is_official_vscode_remote = true;
    assert!(matches!(
        plan_fix(request(temp.path(), "/bin/zsh"), &report(), &vscode),
        Err(FixError::NotApplicable)
    ));
    assert!(matches!(
        plan_fix(request(temp.path(), "/bin/tcsh"), &report(), &terminal()),
        Err(FixError::UnsupportedShell)
    ));
}

#[cfg(windows)]
#[test]
fn windows_is_manual_only_before_shell_selection() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = request(temp.path(), "C:\\Program Files\\Git\\bin\\bash.exe");
    request.shell = Some(PathBuf::from("bash"));
    assert!(matches!(
        plan_fix(request, &report(), &terminal()),
        Err(FixError::PlatformUnsupported)
    ));
}

#[test]
fn existing_alias_and_function_conflicts_are_preserved() {
    let cases = [
        ("/bin/bash", ".bashrc", "alias ssh='ssh -A'\n"),
        ("/bin/zsh", ".zshrc", "ssh() { command ssh -A \"$@\"; }\n"),
        (
            "/usr/bin/fish",
            ".config/fish/config.fish",
            "function ssh\n  command ssh -A $argv\nend\n",
        ),
    ];
    for (shell, relative, content) in cases {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        assert!(matches!(
            plan_fix(request(temp.path(), shell), &report(), &terminal()),
            Err(FixError::ExistingCustomization { .. })
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), content);
    }
}

#[test]
fn alias_and_fish_function_scanners_accept_shell_whitespace() {
    for declaration in [
        "alias  ssh='ssh -A'",
        "alias\tssh = 'ssh -A'",
        "alias \t ssh='ssh -A'",
    ] {
        assert!(
            detect_posix_ssh_customization(declaration).is_some(),
            "{declaration}"
        );
    }
    for declaration in [
        "alias  ssh 'ssh -A'",
        "alias\tssh='ssh -A'",
        "function  ssh",
        "function\tssh --description wrapped",
    ] {
        assert!(
            detect_fish_ssh_customization(declaration).is_some(),
            "{declaration}"
        );
    }
    for not_ssh in [
        "aliases ssh='ssh -A'",
        "alias ssh_wrap='ssh -A'",
        "alias sshuttle='ssh -A'",
    ] {
        assert!(
            detect_posix_ssh_customization(not_ssh).is_none(),
            "{not_ssh}"
        );
        assert!(
            detect_fish_ssh_customization(not_ssh).is_none(),
            "{not_ssh}"
        );
    }
}

#[test]
fn posix_function_scanner_requires_exact_ssh_name_boundary() {
    for declaration in [
        "function ssh { command ssh \"$@\"; }",
        "function ssh() { command ssh \"$@\"; }",
        "ssh() { command ssh \"$@\"; }",
        "ssh () { command ssh \"$@\"; }",
    ] {
        assert!(
            detect_posix_ssh_customization(declaration).is_some(),
            "{declaration}"
        );
    }
    for not_ssh in [
        "function ssh_wrap { :; }",
        "function sshuttle { :; }",
        "ssh_wrap() { :; }",
        "sshuttle () { :; }",
    ] {
        assert!(
            detect_posix_ssh_customization(not_ssh).is_none(),
            "{not_ssh}"
        );
    }
}

#[test]
fn conflict_scan_uses_the_exact_validated_source_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".bashrc");
    std::fs::write(&path, "export KEEP=1\n").unwrap();
    let plan = plan_fix(request(temp.path(), "/bin/bash"), &report(), &terminal()).unwrap();
    std::fs::write(&path, "alias ssh='ssh -A'\n").unwrap();
    assert!(matches!(
        apply_fix(plan),
        Err(FixError::Managed(
            xai_grok_config::managed_text::ManagedConfigError::StalePlan(_)
        ))
    ));
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "alias ssh='ssh -A'\n"
    );
}

#[test]
fn non_utf8_source_fails_closed_before_conflict_policy() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".zshrc");
    std::fs::write(&path, [0xff]).unwrap();
    assert!(matches!(
        plan_fix(request(temp.path(), "/bin/zsh"), &report(), &terminal()),
        Err(FixError::Managed(
            xai_grok_config::managed_text::ManagedConfigError::UnsafePath { .. }
        ))
    ));
}

#[cfg(unix)]
#[test]
fn validator_prefers_custom_executable_shell_and_uses_path_for_basename_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let shadow = temp.path().join("shadow");
    let valid = temp.path().join("valid");
    std::fs::create_dir(&shadow).unwrap();
    std::fs::create_dir(&valid).unwrap();
    std::fs::write(shadow.join("bash"), "not executable").unwrap();
    let real = valid.join("bash");
    std::fs::write(&real, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        find_on_path_in("bash", [&shadow, &valid]),
        Some(real.clone())
    );

    let custom = temp.path().join("custom/bash");
    std::fs::create_dir_all(custom.parent().unwrap()).unwrap();
    std::fs::write(&custom, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&custom, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(resolve_validator_program(&custom), Some(custom.clone()));

    std::fs::set_permissions(&custom, std::fs::Permissions::from_mode(0o644)).unwrap();
    // A non-executable explicit SHELL path is not silently substituted with a
    // different same-basename shell from PATH.
    assert_eq!(resolve_validator_program(&custom), None);

    assert_eq!(
        find_on_path_in("bash", [&shadow, &valid]),
        Some(real),
        "basename-only shell names may resolve through PATH"
    );
}

#[test]
fn comments_and_managed_alias_do_not_create_false_conflicts() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".zshrc");
    std::fs::write(
        &path,
        "# alias ssh='ssh -A'\n# >>> grok doctor >>>\n# >>> terminal.ssh-wrap >>>\nalias ssh='grok wrap ssh'\n# <<< terminal.ssh-wrap <<<\n# <<< grok doctor <<<\n",
    )
    .unwrap();
    let plan = plan_fix(request(temp.path(), "/bin/zsh"), &report(), &terminal()).unwrap();
    let outcome = apply_fix(plan).unwrap();
    assert_eq!(outcome.status, FixStatus::AlreadyConfigured);
    assert!(outcome.backup_path.is_none());
}

#[test]
fn managed_alias_with_later_unmanaged_conflict_is_not_configured() {
    let cases = [
        (
            ShellKind::Bash,
            "# >>> grok doctor >>>\n# >>> terminal.ssh-wrap >>>\nalias ssh='grok wrap ssh'\n# <<< terminal.ssh-wrap <<<\n# <<< grok doctor <<<\nalias ssh='ssh -A'\n",
        ),
        (
            ShellKind::Fish,
            "# >>> grok doctor >>>\n# >>> terminal.ssh-wrap >>>\nalias ssh 'grok wrap ssh'\n# <<< terminal.ssh-wrap <<<\n# <<< grok doctor <<<\nfunction ssh\n  command ssh -A $argv\nend\n",
        ),
    ];
    for (shell, content) in cases {
        let temp = tempfile::tempdir().unwrap();
        let path = shell.config_path(temp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        assert!(!managed_alias_configured(&path, shell));
    }
}

#[test]
fn stale_plan_is_rejected_and_apply_verifies_postcondition() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".bashrc");
    std::fs::write(&path, "export KEEP=1\n").unwrap();
    let plan = plan_fix(request(temp.path(), "/bin/bash"), &report(), &terminal()).unwrap();
    std::fs::write(&path, "export KEEP=2\n").unwrap();
    assert!(matches!(
        apply_fix(plan),
        Err(FixError::Managed(
            xai_grok_config::managed_text::ManagedConfigError::StalePlan(_)
        ))
    ));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "export KEEP=2\n");

    let plan = plan_fix(request(temp.path(), "/bin/bash"), &report(), &terminal()).unwrap();
    let outcome = apply_fix(plan).unwrap();
    assert_eq!(outcome.status, FixStatus::Applied);
    assert_eq!(outcome.id, SSH_WRAP_ID);
    assert!(managed_alias_configured(&path, ShellKind::Bash));
}

#[test]
fn configured_report_reaches_pass_state_only_for_exact_managed_alias() {
    let mut diagnostic = report();
    diagnostic = configured_report(diagnostic, false);
    assert!(
        diagnostic
            .findings
            .iter()
            .any(|finding| finding.id == SSH_WRAP_ID)
    );
    diagnostic = configured_report(diagnostic, true);
    assert!(
        !diagnostic
            .findings
            .iter()
            .any(|finding| finding.id == SSH_WRAP_ID)
    );

    let temp = tempfile::tempdir().unwrap();
    let mut healthy = report();
    healthy.findings.clear();
    let plan = plan_fix(request(temp.path(), "/bin/bash"), &healthy, &terminal()).unwrap();
    assert_eq!(
        plan.id, SSH_WRAP_ID,
        "healthy reports can plan idempotent setup"
    );
}

#[cfg(unix)]
#[test]
fn shell_aliases_expand_to_exact_argv_and_bypass_is_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let capture = temp.path().join("capture");
    let grok = temp.path().join("grok");
    std::fs::write(
        &grok,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
            capture.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&grok, std::fs::Permissions::from_mode(0o755)).unwrap();

    if let Some(bash) = find_on_path("bash") {
        let rc = temp.path().join("bashrc");
        std::fs::write(&rc, "alias ssh='grok wrap ssh'\n").unwrap();
        let command = format!(
            "source '{}'; source '{}'; eval 'ssh -p 2222 host'",
            rc.display(),
            rc.display()
        );
        let mut shell = std::process::Command::new(bash);
        shell
            .args(["-ic", &command])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    temp.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .envs(xai_tty_utils::pager_env());
        xai_tty_utils::detach_std_command(&mut shell);
        let status = shell.status().unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&capture).unwrap(),
            "wrap\nssh\n-p\n2222\nhost\n"
        );
    }
    if let Some(zsh) = find_on_path("zsh") {
        let rc = temp.path().join("zshrc");
        std::fs::write(&rc, "alias ssh='grok wrap ssh'\n").unwrap();
        let command = format!(
            "source '{}'; source '{}'; eval 'ssh -p 2222 host'",
            rc.display(),
            rc.display()
        );
        let mut shell = std::process::Command::new(zsh);
        shell
            .args(["-dfc", &command])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    temp.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .envs(xai_tty_utils::pager_env());
        xai_tty_utils::detach_std_command(&mut shell);
        let status = shell.status().unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&capture).unwrap(),
            "wrap\nssh\n-p\n2222\nhost\n"
        );
    }

    let fake_bin = temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin).unwrap();
    let fake_ssh = fake_bin.join("ssh");
    std::fs::write(&fake_ssh, "#!/bin/sh\nprintf bypass > \"$CAPTURE\"\n").unwrap();
    std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let Some(bash) = find_on_path("bash") else {
        return;
    };
    let mut shell = std::process::Command::new(bash);
    shell
        .args(["-ic", "alias ssh='grok wrap ssh'; command ssh host"])
        .env("CAPTURE", &capture)
        .env(
            "PATH",
            format!(
                "{}:{}:{}",
                fake_bin.display(),
                temp.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .envs(xai_tty_utils::pager_env());
    xai_tty_utils::detach_std_command(&mut shell);
    let status = shell.status().unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&capture).unwrap(), "bypass");

    if let Some(fish) = find_on_path("fish") {
        let fish_capture = temp.path().join("fish-capture");
        let fish_grok = temp.path().join("fish-grok");
        std::fs::write(
            &fish_grok,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                fish_capture.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fish_grok, std::fs::Permissions::from_mode(0o755)).unwrap();
        let rc = temp.path().join("config.fish");
        std::fs::write(&rc, "alias ssh 'fish-grok wrap ssh'\n").unwrap();
        let command = format!(
            "source '{}'; source '{}'; ssh -p 2222 host; env | string match -rq '^ssh='; and exit 9; or exit 0",
            rc.display(),
            rc.display()
        );
        let mut shell = std::process::Command::new(fish);
        shell
            .args(["-c", &command])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    temp.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .envs(xai_tty_utils::pager_env());
        xai_tty_utils::detach_std_command(&mut shell);
        assert!(shell.status().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(fish_capture).unwrap(),
            "wrap\nssh\n-p\n2222\nhost\n"
        );
    } else {
        eprintln!("fish unavailable; fish runtime alias test skipped explicitly");
    }
}
