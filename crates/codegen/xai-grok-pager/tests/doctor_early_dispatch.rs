use std::collections::HashSet;
use std::process::{Command, Stdio};

fn pager_binary() -> Result<std::path::PathBuf, String> {
    for key in ["PAGER_BINARY", "CARGO_BIN_EXE_xai-grok-pager"] {
        if let Some(value) = std::env::var_os(key) {
            let path = std::path::PathBuf::from(value);
            if path.exists() {
                return Ok(path);
            }
        }
    }
    Err("PAGER_BINARY/CARGO_BIN_EXE_xai-grok-pager not set".to_owned())
}

#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn doctor_json_bypasses_unrelated_startup_state() {
    let binary = pager_binary().expect("real pager binary is required when this test is selected");
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let grok_home = temp.path().join("grok-home");
    std::fs::create_dir_all(&home).expect("create HOME");
    std::fs::create_dir_all(&grok_home).expect("create GROK_HOME");

    let version_path = grok_home.join("version.json");
    std::fs::write(
        &version_path,
        br#"{"stable":{"version":"999.0.0"},"checked_at":0}"#,
    )
    .expect("write valid hostile version state");

    let before = directory_entries(&grok_home);
    let output = run_pager(
        &binary,
        &home,
        &grok_home,
        "/bin/sh",
        &["doctor", "--json"],
        &[],
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "stderr must be clean: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON document");
    assert_eq!(json["schemaVersion"], "1");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Grok Doctor"));

    let after = directory_entries(&grok_home);
    assert_eq!(after, before, "doctor must not create startup artifacts");
    assert_eq!(
        std::fs::read(&version_path).unwrap(),
        br#"{"stable":{"version":"999.0.0"},"checked_at":0}"#
    );
    for absent in ["docs", "crash", "memtrace", "active_sessions.json"] {
        assert!(
            !grok_home.join(absent).exists(),
            "unexpected startup artifact: {absent}"
        );
    }
}

#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn doctor_fix_without_id_lists_only_applicable_automatic_fixes() {
    let binary = pager_binary().expect("real pager binary is required when selected");
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let grok_home = temp.path().join("qhome");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&grok_home).unwrap();

    let output = run_pager(
        &binary,
        &home,
        &grok_home,
        "/bin/bash",
        &["doctor", "fix"],
        &[("SSH_CONNECTION", "1 2 3 4")],
    );
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("On your local computer, run: grok doctor fix ssh-wrap"),
        "{stdout}"
    );
    assert!(!home.join(".bashrc").exists());

    let output = run_pager(
        &binary,
        &home,
        &grok_home,
        "/bin/bash",
        &["doctor", "fix", "terminal.ssh-wrap", "--yes"],
        &[],
    );
    assert!(output.status.success());
    let output = run_pager(
        &binary,
        &home,
        &grok_home,
        "/bin/bash",
        &["doctor", "fix"],
        &[],
    );
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "No automatic fixes are available here.\n"
    );
}

#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn doctor_fix_yes_writes_only_actual_home_shell_rc() {
    let binary = pager_binary().expect("real pager binary is required when this test is selected");
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let grok_home = temp.path().join("grok-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&grok_home).unwrap();

    let output = run_pager(
        &binary,
        &home,
        &grok_home,
        "/bin/bash",
        &["doctor", "fix", "terminal.ssh-wrap", "--yes"],
        &[],
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Fix: terminal.ssh-wrap"));
    assert!(stdout.contains("ssh -f"));
    assert!(stdout.contains("ControlPersist"));
    assert!(stdout.contains("~^Z"));
    assert!(stdout.contains("command ssh"));
    assert_eq!(
        std::fs::read_to_string(home.join(".bashrc")).unwrap(),
        "# >>> grok doctor >>>\n# >>> terminal.ssh-wrap >>>\nalias ssh='grok wrap ssh'\n# <<< terminal.ssh-wrap <<<\n# <<< grok doctor <<<"
    );
    assert!(!grok_home.join(".bashrc").exists());
}

#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn doctor_fix_safety_boundaries_are_process_isolated() {
    let binary = pager_binary().expect("real pager binary is required when selected");
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let grok_home = temp.path().join("qhome");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&grok_home).unwrap();

    let conflict = home.join(".zshrc");
    std::fs::write(&conflict, "alias ssh='ssh -A'\n").unwrap();
    let output = run_pager(
        &binary,
        &home,
        &grok_home,
        "/bin/zsh",
        &["doctor", "fix", "ssh-wrap", "--yes"],
        &[],
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Grok found an existing SSH alias or function")
            && stderr.contains(&conflict.display().to_string()),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(&conflict).unwrap(),
        "alias ssh='ssh -A'\n"
    );
    std::fs::remove_file(&conflict).unwrap();

    let output = run_pager(
        &binary,
        &home,
        &grok_home,
        "/bin/zsh",
        &["doctor", "fix", "ssh-wrap"],
        &[],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Fix: terminal.ssh-wrap"));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Cannot apply this fix without confirmation")
    );
    assert!(!conflict.exists());

    let output = run_pager(
        &binary,
        &home,
        &grok_home,
        "/bin/zsh",
        &["doctor", "fix", "ssh-wrap", "--yes"],
        &[("SSH_CONNECTION", "1 2 3 4")],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Run this fix on your local computer")
    );
    assert!(!conflict.exists());
}

#[cfg(unix)]
#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn restrictive_umask_still_preserves_exact_rc_mode() {
    use std::os::unix::fs::PermissionsExt as _;

    let binary = pager_binary().expect("real pager binary is required when selected");
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let grok_home = temp.path().join("qhome");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&grok_home).unwrap();
    let rc = home.join(".bashrc");
    std::fs::write(&rc, "export KEEP=1\n").unwrap();
    std::fs::set_permissions(&rc, std::fs::Permissions::from_mode(0o666)).unwrap();

    let mut command = base_pager_command(&binary, &home, &grok_home, "/bin/bash");
    command.args(["doctor", "fix", "terminal.ssh-wrap", "--yes"]);
    use std::os::unix::process::CommandExt as _;
    // SAFETY: umask is async-signal-safe and runs only in the isolated child.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let output = command.output().expect("run restrictive-umask pager");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::metadata(&rc).unwrap().permissions().mode() & 0o7777,
        0o666
    );
}

#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn wrap_non_tty_true_exec_preserves_argv_and_exit() {
    let binary = pager_binary().expect("real pager binary is required when selected");
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let grok_home = temp.path().join("qhome");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&grok_home).unwrap();
    let output = run_pager(
        &binary,
        &home,
        &grok_home,
        "/bin/sh",
        &[
            "wrap",
            "/bin/sh",
            "-c",
            "printf '%s' \"$1\"; exit 7",
            "sh",
            "argv-ok",
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"argv-ok");
}

fn run_pager(
    binary: &std::path::Path,
    home: &std::path::Path,
    grok_home: &std::path::Path,
    shell: &str,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let mut command = base_pager_command(binary, home, grok_home, shell);
    command.args(args).envs(extra_env.iter().copied());
    command.output().expect("run isolated pager binary")
}

fn base_pager_command(
    binary: &std::path::Path,
    home: &std::path::Path,
    grok_home: &std::path::Path,
    shell: &str,
) -> Command {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("HOME", home)
        .env("GROK_HOME", grok_home)
        .env("SHELL", shell)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(xai_tty_utils::pager_env());
    xai_tty_utils::detach_std_command(&mut command);
    command
}

fn directory_entries(path: &std::path::Path) -> HashSet<std::ffi::OsString> {
    std::fs::read_dir(path)
        .expect("read directory")
        .map(|entry| entry.expect("directory entry").file_name())
        .collect()
}
