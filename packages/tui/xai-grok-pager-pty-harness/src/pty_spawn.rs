//! PTY child spawn: the Unix fork/exec session spawn (setsid, controlling
//! TTY, pdeathsig) and child-environment assembly; Windows keeps
//! portable-pty's spawn and shares the environment hygiene via [`EnvSink`].

use std::ffi::OsStr;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
use anyhow::{Context, Result};
use portable_pty::CommandBuilder;
#[cfg(any(unix, test))]
use xai_grok_test_support::TestSandbox;

use crate::pty::EnvOp;

const CLIPBOARD_SINK_ENV_VARS: &[&str] = &["GROK_OSC52_SINK", "LC_GROK_OSC52_SINK"];

/// Host / wrap appearance hints that would make `theme=auto` non-deterministic
/// in PTY tests (layout depends on the resolved palette).
const APPEARANCE_ENV_VARS: &[&str] = &[
    "GROK_APPEARANCE",
    "LC_GROK_APPEARANCE",
    "GROK_THEME",
    "LC_GROK_THEME",
    "COLORFGBG",
];

/// Host terminal identity markers stripped from the child environment.
///
/// The pager's terminal detection
/// (`xai-grok-pager-render/src/terminal/mod.rs`:
/// `detect_terminal_brand_from_env` / `detect_byobu_from_env` /
/// `detect_multiplexer_from_env` / `detect_tmux_meta_from_env`, plus
/// `embedded_editor.rs`'s `embedded_editor_from_env`) reads all of these,
/// so any one leaking from the harness's own host terminal reclassifies the
/// child: a dev running tests inside tmux leaks `TMUX` (every cell becomes
/// the remuxed profile), inside Cursor leaks `CURSOR_TRACE_ID` (checked
/// *before* `TERM_PROGRAM`, so it overrides even a test-injected brand),
/// inside nvim's `:terminal` leaks `NVIM` (clipboard OSC 52 wrapping).
/// Keep this list in sync with the detection source above.
const HOST_TERMINAL_ENV_VARS: &[&str] = &[
    // Brand chain (detect_terminal_brand_from_env), in detection order.
    "CURSOR_TRACE_ID",
    "VSCODE_GIT_ASKPASS_MAIN",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERMINAL_EMULATOR",
    "WEZTERM_VERSION",
    "ITERM_SESSION_ID",
    "ITERM_PROFILE",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "TERM_SESSION_ID",
    "KITTY_WINDOW_ID",
    "ALACRITTY_SOCKET",
    "TERMINATOR_UUID",
    "VTE_VERSION",
    "WT_SESSION",
    // Multiplexer / Byobu markers (detect_multiplexer_from_env,
    // detect_byobu_from_env, detect_tmux_meta_from_env).
    "TMUX",
    "TMUX_PANE",
    "ZELLIJ",
    "ZELLIJ_SESSION_NAME",
    "STY",
    "BYOBU_BACKEND",
    "BYOBU_CONFIG_DIR",
    "BYOBU_DISTRO",
    "CMUX_SOCKET_PATH",
    "CMUX_PANEL_ID",
    "CMUX_BUNDLE_ID",
    "HERDR_ENV",
    // Embedded editor markers (embedded_editor_from_env).
    "NVIM",
    "NVIM_LISTEN_ADDRESS",
    "VIM_TERMINAL",
    "INSIDE_EMACS",
];

/// Child-environment destination for [`apply_child_env`]: Windows'
/// `CommandBuilder` keeps its registry-merged base environment, Unix
/// assembles a plain map — one hygiene list, two containers.
pub(crate) trait EnvSink {
    fn set_var(&mut self, key: &OsStr, value: &OsStr);
    fn remove_var(&mut self, key: &OsStr);
}

impl EnvSink for CommandBuilder {
    fn set_var(&mut self, key: &OsStr, value: &OsStr) {
        self.env(key, value);
    }

    fn remove_var(&mut self, key: &OsStr) {
        self.env_remove(key);
    }
}

#[cfg(unix)]
impl EnvSink for std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString> {
    fn set_var(&mut self, key: &OsStr, value: &OsStr) {
        self.insert(key.to_owned(), value.to_owned());
    }

    fn remove_var(&mut self, key: &OsStr) {
        self.remove(key);
    }
}

/// Compute the full Unix child environment: the [`TestSandbox`] baseline when
/// provided (content-backed spawns), otherwise the inherited parent
/// environment (terminal-probe and grok-wrap fixtures), plus the hygiene pass
/// and caller overrides from [`apply_child_env`].
#[cfg(unix)]
pub(crate) fn compute_child_env(
    sandbox: Option<&TestSandbox>,
    env: &[EnvOp<'_>],
) -> std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString> {
    let mut map: std::collections::BTreeMap<_, _> = match sandbox {
        Some(sandbox) => sandbox.env().into_iter().collect(),
        None => std::env::vars_os().collect(),
    };
    // portable-pty parity: its spawn always seeded SHELL before the builder
    // env. The sandbox baseline sets one; for inherited-env spawns whose host
    // lacks SHELL (sanitized CI environments), pin the deterministic default
    // instead of portable-pty's passwd lookup.
    map.entry(std::ffi::OsString::from("SHELL"))
        .or_insert_with(|| std::ffi::OsString::from("/bin/sh"));
    apply_child_env(&mut map, env);
    map
}

/// Child working directory, portable-pty parity: the explicit cwd only when it
/// is a real directory, otherwise the child's own HOME (the sandbox home for
/// content-backed spawns).
///
/// Deviation (like the SHELL default in [`compute_child_env`]): portable-pty
/// uses `$HOME` unchecked — a bad HOME fails the spawn — then the passwd
/// home. This path checks HOME `is_dir` and falls back to the parent's cwd,
/// deliberately trading that spawn failure for test robustness.
#[cfg(unix)]
pub(crate) fn resolve_child_cwd(
    cwd: Option<&Path>,
    child_home: Option<&std::ffi::OsString>,
) -> Result<std::path::PathBuf> {
    if let Some(dir) = cwd
        && dir.is_dir()
    {
        return Ok(dir.to_path_buf());
    }
    if let Some(home) = child_home {
        let home = std::path::PathBuf::from(home);
        if home.is_dir() {
            return Ok(home);
        }
    }
    // The harness always provides a sandbox HOME or inherits the host's, so
    // this fallback is effectively unreachable.
    std::env::current_dir().context("failed to resolve a working directory for the PTY child")
}

/// Prepare the child environment hygiene + caller overrides. Callers seed the
/// sink first: sandboxed spawns from the [`TestSandbox`] baseline (inheritance
/// cleared), inherited-env spawns from the parent environment. Caller
/// overrides are always applied last.
pub(crate) fn apply_child_env<S: EnvSink>(cmd: &mut S, env: &[EnvOp<'_>]) {
    // Set TERM so the pager renders with full color support.
    cmd.set_var(OsStr::new("TERM"), OsStr::new("xterm-256color"));
    // Strip inherited color opt-outs/overrides for the same reason: a
    // leaked NO_COLOR (common in agent/CI shells) renders the pager
    // colorless, making style-sensitive assertions (e.g. the selection
    // highlight color swap) silently untestable on some hosts. Tests may
    // re-set these via the `env` list (applied after this).
    for color_var in ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE"] {
        cmd.remove_var(OsStr::new(color_var));
    }
    // Strip SSH vars inherited from the parent: the harness PTY is a
    // local terminal, but `SSH_CONNECTION`/`SSH_TTY` leaking through
    // makes the pager's terminal detector report SSH and disable the
    // drag-drop image classifier (see
    // `try_handle_dropped_paths_paste` in `agent_view.rs`).
    for ssh_var in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "SSH_AUTH_SOCK"] {
        cmd.remove_var(OsStr::new(ssh_var));
    }
    // A harness launched under `grok wrap` must not silently confirm clipboard
    // delivery for no-sink scenarios. Explicit sink tests re-inject a marker
    // through `env` after this hygiene pass.
    for sink_var in CLIPBOARD_SINK_ENV_VARS {
        cmd.remove_var(OsStr::new(sink_var));
    }
    for appearance_var in APPEARANCE_ENV_VARS {
        cmd.remove_var(OsStr::new(appearance_var));
    }
    // Neutralize parent-terminal identity bleed: agent hosts often export
    // TERM_PROGRAM=ghostty/iTerm/etc. (and mux/editor markers) which make
    // the child pager adopt that host's key/modifier/clipboard quirks even
    // though we only set TERM above.
    for term_var in HOST_TERMINAL_ENV_VARS {
        cmd.remove_var(OsStr::new(term_var));
    }
    for operation in env {
        match operation {
            EnvOp::Set(key, value) => cmd.set_var(key, value),
            EnvOp::Remove(key) => cmd.remove_var(key),
        }
    }
}

/// Spawn `cmd` as the session leader of `master`'s PTY.
///
/// Replaces portable-pty's `SlavePty::spawn_command` on Unix (its
/// `CommandBuilder` exposes no `pre_exec` hook) with an equivalent fork/exec:
/// default signal dispositions, fresh signal mask, `setsid`, the PTY slave as
/// controlling terminal on stdio, the leaked-fd sweep, and on Linux
/// `PR_SET_PDEATHSIG(SIGKILL)` so the kernel reaps the child when the
/// spawning process dies without userspace cleanup.
///
/// pdeathsig caveat: the kernel delivers it when the *spawning thread* dies,
/// not just the process. Harness spawns happen on the calling test thread
/// (libtest gives each test its own thread that outlives the test body), a
/// tokio multi-thread worker (alive until runtime shutdown at the end of the
/// test), or a `spawn_blocking` closure (the scroll-matrix runner's
/// `run_cell` `block_on`s the whole cell inside one blocking closure). All
/// three are safe only because the harness is created AND dropped — killing
/// the child — before that thread exits; the `spawn_blocking` case is the
/// sharpest edge, since tokio reaps idle blocking threads (~10s), so a
/// harness escaping its blocking closure gets its pager SIGKILLed mid-test
/// with nothing in the logs. Do not spawn a harness from a short-lived
/// helper thread (or blocking closure) that outlives its work.
#[cfg(unix)]
pub(crate) fn spawn_pty_session_child(
    mut cmd: std::process::Command,
    master: &dyn portable_pty::MasterPty,
) -> Result<std::process::Child> {
    use std::os::unix::ffi::OsStringExt as _;
    use std::os::unix::process::CommandExt as _;

    let tty = master
        .tty_name()
        .context("PTY master has no slave tty name")?;
    let tty = std::ffi::CString::new(tty.into_os_string().into_vec())
        .context("PTY slave tty name contains a NUL byte")?;

    // Parent-death safety net first: pre_exec hooks run in registration
    // order, so pdeathsig is armed as the child's first post-fork action,
    // minimizing the unprotected window. SIGKILL (not the helper's SIGTERM
    // default) so even a child that cannot service a catchable signal is
    // reaped. The binding survives the exec because test fixtures are
    // non-setuid with no file capabilities (the kernel clears PDEATHSIG
    // across a privileged exec).
    #[cfg(target_os = "linux")]
    xai_tty_utils::kill_on_parent_death_std_with(&mut cmd, libc::SIGKILL);

    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: the hook only performs fork-safe syscalls plus the same /dev/fd
    // sweep portable-pty itself runs between fork and exec.
    unsafe {
        cmd.pre_exec(move || pty_session_pre_exec(&tty));
    }
    // The child is enrolled post-spawn in a TestProcessTree (group kill on
    // teardown) and pdeathsig-guarded on Linux, which is exactly the enrollment
    // this lint asks for; ProcessScope is a production-pager concept.
    #[allow(clippy::disallowed_methods)]
    cmd.spawn().context("failed to spawn PTY child")
}

/// Child-side session/tty setup between fork and exec for
/// [`spawn_pty_session_child`], parity with portable-pty's own pre_exec
/// (which this spawn path replaces).
#[cfg(unix)]
fn pty_session_pre_exec(tty: &std::ffi::CStr) -> io::Result<()> {
    // SAFETY: runs in the forked child between fork and exec, so it calls only
    // async-signal-safe libc functions; error paths only wrap the raw errno
    // via `io::Error::last_os_error()` (no allocation). The one exception is
    // the final /dev/fd sweep, which portable-pty's own pre_exec performs
    // identically.
    unsafe {
        // Clear inherited signal dispositions and mask…
        for signo in [
            libc::SIGCHLD,
            libc::SIGHUP,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTERM,
            libc::SIGALRM,
        ] {
            libc::signal(signo, libc::SIG_DFL);
        }
        let mut empty_set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty_set);
        libc::sigprocmask(libc::SIG_SETMASK, &empty_set, std::ptr::null_mut());

        // …become a session (and thus process-group) leader, so the harness's
        // group kill takes the child's own descendants with it. Raw setsid
        // (not the shared detach helper) is deliberate: the helper's EPERM
        // fallback to setpgid would leave the child sessionless-but-grouped
        // and break the TIOCSCTTY acquisition below…
        if libc::setsid() == -1 {
            return Err(io::Error::last_os_error());
        }

        // …attach the PTY slave as the controlling terminal on stdio (required
        // for SIGWINCH delivery on resize)…
        let fd = libc::open(tty.as_ptr(), libc::O_RDWR);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // TIOCSCTTY's constant type is platform-dependent, hence `as _`.
        #[allow(clippy::cast_lossless)]
        if libc::ioctl(fd, libc::TIOCSCTTY as _, 0) == -1 {
            return Err(io::Error::last_os_error());
        }
        for stdio_fd in 0..=2 {
            if libc::dup2(fd, stdio_fd) == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        if fd > 2 {
            libc::close(fd);
        }

        // …and close host fds leaked past the stdio triple. This also closes
        // std's CLOEXEC exec-status pipe, so an exec failure (e.g. a bad
        // binary path) surfaces as a successful spawn plus an instant child
        // exit instead of a spawn error — parity with portable-pty's own
        // pre_exec sweep, which this path replaces.
        portable_pty::unix::close_random_fds();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    /// Every host-terminal marker the pager's detection chain reads must be
    /// stripped from the child env. The pollution is seeded directly into the
    /// sink (the same map inherited entries land in, so `remove_var` takes the
    /// identical path) rather than via process-global `set_var` (racy under
    /// parallel tests). Exercises the `CommandBuilder` sink (the Windows spawn
    /// path); `unix_child_env_strips_markers_and_keeps_sandbox_baseline`
    /// covers the map sink the Unix spawn feeds to `std::process::Command`.
    #[test]
    fn apply_child_env_strips_all_host_terminal_markers() {
        let mut cmd = CommandBuilder::new("true");
        for var in HOST_TERMINAL_ENV_VARS {
            cmd.env(var, "polluted");
        }
        for ssh_var in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "SSH_AUTH_SOCK"] {
            cmd.env(ssh_var, "polluted");
        }
        for color_var in ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE"] {
            cmd.env(color_var, "polluted");
        }
        for sink_var in CLIPBOARD_SINK_ENV_VARS {
            cmd.env(sink_var, "polluted");
        }
        for appearance_var in APPEARANCE_ENV_VARS {
            cmd.env(appearance_var, "polluted");
        }
        // Sandboxed launches remove unrelated inherited variables before
        // re-applying the baseline and explicit overrides.
        cmd.env("GROK_SCROLL_LOG", "/tmp/scroll.jsonl");
        let sandbox = TestSandbox::new();
        cmd.env_clear();
        sandbox.apply_to_command_builder(&mut cmd);

        apply_child_env(&mut cmd, &[]);

        for var in HOST_TERMINAL_ENV_VARS {
            assert!(
                cmd.get_env(var).is_none(),
                "host terminal marker {var} leaked into the child env"
            );
        }
        for ssh_var in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "SSH_AUTH_SOCK"] {
            assert!(
                cmd.get_env(ssh_var).is_none(),
                "SSH marker {ssh_var} leaked into the child env"
            );
        }
        for color_var in ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE"] {
            assert!(
                cmd.get_env(color_var).is_none(),
                "color override {color_var} leaked into the child env"
            );
        }
        for sink_var in CLIPBOARD_SINK_ENV_VARS {
            assert!(
                cmd.get_env(sink_var).is_none(),
                "clipboard sink marker {sink_var} leaked into the child env"
            );
        }
        for appearance_var in APPEARANCE_ENV_VARS {
            assert!(
                cmd.get_env(appearance_var).is_none(),
                "appearance hint {appearance_var} leaked into the child env"
            );
        }
        assert_eq!(
            cmd.get_env("TERM").and_then(|v| v.to_str()),
            Some("xterm-256color")
        );
        assert_eq!(
            cmd.get_env("GROK_SCROLL_LOG").and_then(|v| v.to_str()),
            None,
            "hermetic baseline must remove unrelated inherited vars"
        );
        assert_eq!(
            cmd.get_env("GROK_HOME").and_then(|v| v.to_str()),
            sandbox.grok_home().to_str()
        );
    }

    #[test]
    fn apply_child_env_uses_sandbox_baseline() {
        let sandbox = TestSandbox::new();
        let mut cmd = CommandBuilder::new("true");
        cmd.env_clear();
        sandbox.apply_to_command_builder(&mut cmd);

        apply_child_env(&mut cmd, &[]);

        assert_eq!(
            cmd.get_env("HOME").and_then(|v| v.to_str()),
            sandbox.home().to_str()
        );
        assert_eq!(
            cmd.get_env("GROK_HOME").and_then(|v| v.to_str()),
            sandbox.grok_home().to_str()
        );
        assert_eq!(cmd.get_env("GROK_LEADER_SOCKET"), None);
    }

    /// The Unix spawn path feeds a plain env map to `std::process::Command`.
    /// The sandbox baseline must win over nothing-inherited, the hygiene pass
    /// must strip host markers seeded into the map (the inherited-env mode
    /// stores parent vars in this same map), and caller ops apply last.
    #[cfg(unix)]
    #[test]
    fn unix_child_env_strips_markers_and_keeps_sandbox_baseline() {
        let sandbox = TestSandbox::new();
        let mut env = compute_child_env(Some(&sandbox), &[EnvOp::set("PTY_TEST_EXTRA", "1")]);
        assert_eq!(
            env.get(OsStr::new("HOME")).and_then(|v| v.to_str()),
            sandbox.home().to_str()
        );
        assert_eq!(
            env.get(OsStr::new("TERM")).and_then(|v| v.to_str()),
            Some("xterm-256color")
        );
        assert_eq!(
            env.get(OsStr::new("PTY_TEST_EXTRA"))
                .and_then(|v| v.to_str()),
            Some("1")
        );

        // Inherited-mode pollution lands in the same map the hygiene pass
        // edits; seed markers directly instead of process-global set_var.
        for var in HOST_TERMINAL_ENV_VARS {
            env.insert(OsString::from(var), OsString::from("polluted"));
        }
        apply_child_env(&mut env, &[]);
        for var in HOST_TERMINAL_ENV_VARS {
            assert!(
                !env.contains_key(OsStr::new(*var)),
                "host terminal marker {var} leaked into the unix child env"
            );
        }
    }

    /// Portable-pty parity: explicit cwd wins only when it's a directory;
    /// otherwise the child's HOME.
    #[cfg(unix)]
    #[test]
    fn unix_child_cwd_falls_back_to_child_home() {
        let sandbox = TestSandbox::new();
        let home = OsString::from(sandbox.home().as_os_str());

        let explicit = resolve_child_cwd(Some(sandbox.workspace()), Some(&home)).unwrap();
        assert_eq!(explicit, sandbox.workspace());

        let missing = sandbox.root().join("does-not-exist");
        let fallback = resolve_child_cwd(Some(&missing), Some(&home)).unwrap();
        assert_eq!(fallback, sandbox.home());

        let none = resolve_child_cwd(None, Some(&home)).unwrap();
        assert_eq!(none, sandbox.home());
    }

    #[test]
    fn apply_child_env_remove_deletes_sandbox_credential() {
        let sandbox = TestSandbox::builder()
            .mock_url("http://127.0.0.1:43123/v1")
            .build();
        let mut cmd = CommandBuilder::new("true");
        cmd.env_clear();
        sandbox.apply_to_command_builder(&mut cmd);

        apply_child_env(&mut cmd, &[EnvOp::remove("XAI_API_KEY")]);

        assert_eq!(cmd.get_env("XAI_API_KEY"), None);
        assert_eq!(
            cmd.get_env("GROK_XAI_API_BASE_URL")
                .and_then(|v| v.to_str()),
            Some("http://127.0.0.1:43123/v1")
        );
    }

    #[test]
    fn inherited_env_projection_preserves_unrelated_ambient_vars() {
        let mut cmd = CommandBuilder::new("true");
        cmd.env("AMBIENT_MARKER", "inherited");
        apply_child_env(&mut cmd, &[EnvOp::set("EXPLICIT_MARKER", "set")]);

        assert_eq!(
            cmd.get_env("AMBIENT_MARKER")
                .and_then(|value| value.to_str()),
            Some("inherited")
        );
        assert_eq!(
            cmd.get_env("EXPLICIT_MARKER")
                .and_then(|value| value.to_str()),
            Some("set")
        );
    }

    #[test]
    fn apply_child_env_caller_env_overrides_survive_strips() {
        let mut cmd = CommandBuilder::new("true");
        cmd.env("TMUX", "/tmp/host-tmux,999,0");
        cmd.env("CURSOR_TRACE_ID", "host-cursor");

        apply_child_env(
            &mut cmd,
            &[
                EnvOp::set("TERM_PROGRAM", "vscode"),
                EnvOp::set("NVIM", "/tmp/fake-nvim.sock"),
                EnvOp::set("TERM", "xterm-kitty"),
                EnvOp::set("GROK_OSC52_SINK", "1"),
            ],
        );

        // Host pollution is gone…
        assert!(cmd.get_env("TMUX").is_none());
        assert!(cmd.get_env("CURSOR_TRACE_ID").is_none());
        // …but caller-injected markers (applied after strips) survive,
        // including overriding the harness's own TERM default.
        assert_eq!(
            cmd.get_env("TERM_PROGRAM").and_then(|v| v.to_str()),
            Some("vscode")
        );
        assert_eq!(
            cmd.get_env("NVIM").and_then(|v| v.to_str()),
            Some("/tmp/fake-nvim.sock")
        );
        assert_eq!(
            cmd.get_env("TERM").and_then(|v| v.to_str()),
            Some("xterm-kitty")
        );
        assert_eq!(
            cmd.get_env("GROK_OSC52_SINK").and_then(|v| v.to_str()),
            Some("1"),
            "explicit sink scenarios must be able to re-inject the marker"
        );
    }
}
