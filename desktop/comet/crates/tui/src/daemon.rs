//! Attach to a running engine, or start one that outlives us.
//!
//! This is the module that makes the terminal viewport *detachable*. The engine
//! already knows how to run without a UI (`comet headless`, ARCHITECTURE §1
//! "Headed / headless"), so the TUI never embeds one the way the gpui app can:
//! it is always a thin client over localhost RPC. That single decision buys the
//! herdr property — agents keep running, docs keep syncing, and the DeviceRoom
//! stays joined when you close the terminal, because the process doing that
//! work was never the process drawing frames.
//!
//! Two paths:
//!
//! - **Attach.** A daemon is already listening on the IPC port (started by
//!   `comet daemon install`, by a previous `comet-tui`, or by the headed app).
//!   We dial it and are done.
//! - **Spawn.** Nothing is listening, so we start `comet headless` in its own
//!   session ([`libc::setsid`]) with stdio redirected to the daemon log. A new
//!   session means it has no controlling terminal, so the SIGHUP the kernel
//!   sends when the terminal window closes never reaches it, and when we exit
//!   it is reparented to init rather than killed.
//!
//! We deliberately never `wait()` on the spawned child: reaping it is init's
//! job after we're gone. The engine's own instance lock (`instance_lock.rs`)
//! is what actually prevents two daemons, so a lost race on the port probe
//! costs a fast, clean exit in the loser, not a corrupt store.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use comet_rpc::{RpcClient, connect_ws};

/// How long a single TCP probe of the IPC port may take before we call it dead.
const PROBE_TIMEOUT: Duration = Duration::from_millis(750);
/// How long to wait for a freshly spawned daemon to start listening. Cold start
/// opens Loro stores and joins rooms; 20s is generous but bounded.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(20);
/// Port poll interval while waiting for the spawned daemon.
const SPAWN_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Engine data directory (`~/.comet-native`).
    pub data_dir: PathBuf,
    /// Localhost IPC port to probe and, if we spawn, to serve.
    pub ipc_port: u16,
    /// Explicit path to the `comet` binary hosting `headless`. `None` resolves
    /// it (see [`resolve_comet_bin`]).
    pub comet_bin: Option<PathBuf>,
    /// When false, a missing daemon is an error instead of something we start.
    /// (`--no-spawn`: for attaching to a service-managed engine only.)
    pub spawn: bool,
}

impl DaemonConfig {
    pub fn log_path(&self) -> PathBuf {
        self.data_dir.join("daemon.log")
    }
}

/// How we came to be connected — the status line tells the user which, because
/// it changes what exiting means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attachment {
    /// Something else already ran the engine; we are one of possibly several
    /// viewports.
    Attached,
    /// We started it. It keeps running after we quit.
    Spawned { pid: u32 },
}

pub struct Connection {
    pub client: RpcClient,
    pub attachment: Attachment,
    pub url: String,
}

fn ipc_url(port: u16) -> String {
    format!("ws://127.0.0.1:{port}")
}

/// Is something accepting connections on the IPC port right now?
pub async fn probe(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            PROBE_TIMEOUT,
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await,
        Ok(Ok(_))
    )
}

/// Connect to the engine, starting a detached daemon if none is listening.
pub async fn connect(config: &DaemonConfig) -> anyhow::Result<Connection> {
    let url = ipc_url(config.ipc_port);
    if probe(config.ipc_port).await {
        let client = connect_ws(&url)
            .await
            .with_context(|| format!("connecting to the engine at {url}"))?;
        return Ok(Connection {
            client,
            attachment: Attachment::Attached,
            url,
        });
    }

    if !config.spawn {
        bail!(
            "no engine listening on 127.0.0.1:{} — start one with `comet daemon start` \
             (or drop --no-spawn to have the TUI start it)",
            config.ipc_port
        );
    }

    let pid = spawn_detached(config)?;
    match wait_for_port(config.ipc_port, SPAWN_TIMEOUT).await {
        true => {
            let client = connect_ws(&url)
                .await
                .with_context(|| format!("connecting to the engine we started at {url}"))?;
            Ok(Connection {
                client,
                attachment: Attachment::Spawned { pid },
                url,
            })
        }
        false => Err(anyhow!(
            "the engine we started (pid {pid}) never opened 127.0.0.1:{}.\n{}",
            config.ipc_port,
            describe_daemon_failure(config)
        )),
    }
}

/// Reconnect after the link dropped: same as [`connect`], but a spawn here is
/// suspicious (the daemon we were talking to just died), so respawning is left
/// to the caller's discretion via `config.spawn`.
pub async fn reconnect(config: &DaemonConfig) -> anyhow::Result<Connection> {
    connect(config).await
}

/// The most useful thing we can say when a spawned daemon didn't come up. The
/// overwhelmingly common cause is "not signed in": a detached `comet headless`
/// has no TTY to run the paste-code flow on, so it exits immediately with that
/// message (`terminal_sign_in`'s non-TTY path).
fn describe_daemon_failure(config: &DaemonConfig) -> String {
    let log = config.log_path();
    match log_tail(&log, 12) {
        Some(tail) if tail.contains("comet login") => {
            format!(
                "The engine needs a session first — run `comet login`, then start me again.\n\nFrom {}:\n{tail}",
                log.display()
            )
        }
        Some(tail) => format!("Last lines of {}:\n{tail}", log.display()),
        None => format!(
            "No log at {} either — check that `comet headless` runs in the foreground.",
            log.display()
        ),
    }
}

/// Last `lines` lines of a log file, if it exists and is readable.
pub fn log_tail(path: &Path, lines: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    let tail: Vec<&str> = trimmed.lines().rev().take(lines).collect();
    Some(
        tail.into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
            .to_string(),
    )
}

/// Start `comet headless` in its own session, stdio to the daemon log.
fn spawn_detached(config: &DaemonConfig) -> anyhow::Result<u32> {
    let bin = match &config.comet_bin {
        Some(path) => path.clone(),
        None => resolve_comet_bin().context(
            "could not find the `comet` binary to run the engine — put it on PATH \
             or set COMET_BIN",
        )?,
    };
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("creating {}", config.data_dir.display()))?;
    let log_path = config.log_path();
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;

    let mut command = std::process::Command::new(&bin);
    command
        .arg("headless")
        .stdin(std::process::Stdio::null())
        .stdout(
            log.try_clone()
                .context("duplicating the log handle for stdout")?,
        )
        .stderr(log)
        // Pin the two settings that must agree with what this client resolved;
        // everything else (COMET_EDGE_URL, COMET_WORKOS_CLIENT_ID, PATH, …) is
        // inherited from our environment exactly as `comet daemon install`
        // captures it.
        .env("COMET_DATA_DIR", &config.data_dir)
        .env("COMET_IPC_PORT", config.ipc_port.to_string());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe and touches nothing but the
        // calling (already forked) child's session id. It cannot fail with
        // EPERM here because a fresh fork is never a process-group leader.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let child = command
        .spawn()
        .with_context(|| format!("spawning `{} headless`", bin.display()))?;
    // Intentionally not awaited or killed: this process is meant to outlive us.
    Ok(child.id())
}

/// Find the `comet` binary. Checked in order of "most likely to be the one the
/// user means": an explicit override, the binary sitting next to this one (a
/// cargo target dir or an installed `app/current/`), PATH, then the installer's
/// well-known location.
pub fn resolve_comet_bin() -> anyhow::Result<PathBuf> {
    let exe_name = if cfg!(windows) { "comet.exe" } else { "comet" };

    if let Some(explicit) = std::env::var_os("COMET_BIN").map(PathBuf::from) {
        if explicit.is_file() {
            return Ok(explicit);
        }
        bail!("COMET_BIN={} is not a file", explicit.display());
    }

    if let Ok(self_exe) = std::env::current_exe()
        && let Some(dir) = self_exe.parent()
    {
        let sibling = dir.join(exe_name);
        if sibling.is_file() {
            return Ok(sibling);
        }
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(exe_name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    if let Some(home) = std::env::var_os("HOME") {
        let installed = PathBuf::from(home)
            .join(".comet-native/app/current")
            .join(exe_name);
        if installed.is_file() {
            return Ok(installed);
        }
    }

    bail!("`{exe_name}` not found next to this binary, on PATH, or under ~/.comet-native/app")
}

/// Poll the IPC port until something answers, or the budget runs out.
async fn wait_for_port(port: u16, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return true;
        }
        if tokio::time::Instant::now() + SPAWN_POLL >= deadline {
            return false;
        }
        tokio::time::sleep(SPAWN_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_tail_reads_the_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("daemon.log");
        std::fs::write(&log, "one\ntwo\nthree\nfour\n").unwrap();
        assert_eq!(log_tail(&log, 2).unwrap(), "three\nfour");
        assert_eq!(log_tail(&log, 99).unwrap(), "one\ntwo\nthree\nfour");
        // Missing and empty logs are indistinguishable to the caller: nothing
        // useful to show.
        assert_eq!(log_tail(&dir.path().join("nope.log"), 2), None);
        std::fs::write(&log, "\n\n").unwrap();
        assert_eq!(log_tail(&log, 2), None);
    }

    #[test]
    fn explicit_comet_bin_must_exist() {
        // A bogus COMET_BIN is a hard error rather than a silent fallthrough to
        // PATH — otherwise a typo'd override starts the wrong engine.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("comet");
        let err = with_env("COMET_BIN", Some(missing.as_os_str()), resolve_comet_bin)
            .expect_err("a nonexistent COMET_BIN should fail");
        assert!(err.to_string().contains("is not a file"), "{err}");

        std::fs::write(&missing, b"#!/bin/sh\n").unwrap();
        let found =
            with_env("COMET_BIN", Some(missing.as_os_str()), resolve_comet_bin).expect("now valid");
        assert_eq!(found, missing);
    }

    #[tokio::test]
    async fn probe_sees_a_listener_and_misses_a_closed_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(probe(port).await, "an open listener must be detected");
        drop(listener);
        assert!(!probe(port).await, "a closed port must not be");
    }

    #[tokio::test]
    async fn missing_daemon_without_spawn_is_an_error_not_a_launch() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let dir = tempfile::tempdir().unwrap();
        let result = connect(&DaemonConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            comet_bin: None,
            spawn: false,
        })
        .await;
        let err = match result {
            Ok(_) => panic!("--no-spawn must refuse to start anything"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("no engine listening"), "{err}");
    }

    /// Set an env var for the duration of `f`, restoring it after. Tests that
    /// touch the environment must not leak into their neighbours.
    fn with_env<T>(key: &str, value: Option<&std::ffi::OsStr>, f: impl FnOnce() -> T) -> T {
        let previous = std::env::var_os(key);
        // SAFETY: single-threaded test body; the guard restores the prior value
        // before returning.
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        let out = f();
        unsafe {
            match previous {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        out
    }
}
