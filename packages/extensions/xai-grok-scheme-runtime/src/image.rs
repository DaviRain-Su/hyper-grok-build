//! Image process supervision: locate a Gambit/Gerbil runtime, spawn the
//! kernel with a scrubbed environment, run the self-identification handshake,
//! and provide a request/reply wire with per-call deadlines.
//!
//! Discovery order (first hit wins):
//! 1. `HYPER_SCHEME_IMAGE` env — explicit prebuilt image binary path.
//! 2. Configured prebuilt candidates (e.g. `~/.grok/bin/hyper-scheme-image`).
//! 3. `gxi` on PATH (Gerbil interpreter, runs the embedded kernel file).
//! 4. `gsi` on PATH (Gambit interpreter, same kernel file).
//!
//! Nothing found → the runtime degrades silently (one log line).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::frame::{read_frame, write_frame};
use crate::sexp::Sexp;

/// Protocol version; must match `kernel-protocol-version` in `kernel/runtime.ss`.
pub const PROTOCOL_VERSION: i64 = 1;

/// Embedded kernel source (written to a cache file for interpreted spawns;
/// compiled into the prebuilt image binary by the release pipeline).
pub const KERNEL_SOURCE: &str = include_str!("../kernel/runtime.ss");

/// Boot budget: spawn + handshake.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(5);
/// Graceful-quit budget before SIGKILL.
pub const QUIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Env allowlist for the image child. Ambient host environment (and any
/// credential material in it) is never inherited. `GERBIL_HOME` /
/// `LD_LIBRARY_PATH` are passed through so non-standard Gerbil/Gambit
/// installs can locate their runtime libraries.
const ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "TERM", "GERBIL_HOME", "LD_LIBRARY_PATH"];

/// Max bytes of one stderr diagnostic line we retain.
const MAX_STDERR_LINE: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("no scheme runtime available: {0}")]
    Unavailable(String),
    #[error("failed to spawn scheme image: {0}")]
    Spawn(std::io::Error),
    #[error("image handshake failed: {0}")]
    Handshake(String),
    #[error("image io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("image call timed out after {0:?}")]
    Timeout(Duration),
    #[error("image protocol error: {0}")]
    Protocol(String),
}

/// Resolved way to start the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageCommand {
    /// Prebuilt `gsc -exe` binary embedding the kernel.
    Binary(PathBuf),
    /// Interpreter (`gxi` / `gsi`) running the cached kernel source file.
    Interpreter { program: PathBuf, kernel_file: PathBuf },
}

impl ImageCommand {
    pub fn describe(&self) -> String {
        match self {
            Self::Binary(p) => format!("binary {}", p.display()),
            Self::Interpreter { program, .. } => format!("interpreter {}", program.display()),
        }
    }
}

/// Write the embedded kernel to `state_dir/kernel-v{PROTOCOL_VERSION}.ss`
/// when missing or stale. Returns the file path.
pub fn ensure_kernel_cache(state_dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(state_dir)?;
    let path = state_dir.join(format!("kernel-v{PROTOCOL_VERSION}.ss"));
    let stale = match std::fs::read_to_string(&path) {
        Ok(existing) => existing != KERNEL_SOURCE,
        Err(_) => true,
    };
    if stale {
        std::fs::write(&path, KERNEL_SOURCE)?;
    }
    Ok(path)
}

/// Resolve how to start the image, or `None` when no runtime is available.
pub fn resolve_image_command(
    prebuilt_candidates: &[PathBuf],
    state_dir: &Path,
) -> Option<ImageCommand> {
    if let Ok(explicit) = std::env::var("HYPER_SCHEME_IMAGE") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Some(ImageCommand::Binary(p));
        }
        tracing::warn!(
            path = %p.display(),
            "HYPER_SCHEME_IMAGE is set but not a file; falling back to discovery"
        );
    }
    for candidate in prebuilt_candidates {
        if candidate.is_file() {
            return Some(ImageCommand::Binary(candidate.clone()));
        }
    }
    for interp in ["gxi", "gsi"] {
        if let Ok(program) = which::which(interp) {
            match ensure_kernel_cache(state_dir) {
                Ok(kernel_file) => {
                    return Some(ImageCommand::Interpreter {
                        program,
                        kernel_file,
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to write scheme kernel cache");
                    return None;
                }
            }
        }
    }
    None
}

/// Request/reply over any framed byte stream. Generic so tests can run the
/// protocol over an in-process duplex instead of a child process.
pub struct Wire<R, W> {
    pub reader: R,
    pub writer: W,
}

impl<R, W> Wire<R, W>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    /// Send `req` and await one reply within `deadline`.
    ///
    /// A timeout or transport error leaves the stream desynchronized; the
    /// caller must discard the connection (kill the image).
    pub async fn request(&mut self, req: &Sexp, deadline: Duration) -> Result<Sexp, ImageError> {
        write_frame(&mut self.writer, &req.render()).await?;
        let reply = tokio::time::timeout(deadline, read_frame(&mut self.reader))
            .await
            .map_err(|_| ImageError::Timeout(deadline))??
            .ok_or_else(|| ImageError::Protocol("image closed the stream".into()))?;
        Sexp::parse(&reply).map_err(|e| ImageError::Protocol(format!("bad reply: {e}")))
    }
}

/// A live image child process with its protocol wire.
pub struct ImageHandle {
    child: Child,
    wire: Wire<BufReader<ChildStdout>, ChildStdin>,
    /// Kernel self-identification string from the handshake.
    pub kernel_version: String,
}

impl ImageHandle {
    /// Spawn + handshake. The child gets an allowlisted environment only.
    pub async fn spawn(command: &ImageCommand) -> Result<Self, ImageError> {
        let mut cmd = match command {
            ImageCommand::Binary(path) => Command::new(path),
            ImageCommand::Interpreter {
                program,
                kernel_file,
            } => {
                let mut c = Command::new(program);
                c.arg(kernel_file);
                c
            }
        };
        cmd.env_clear();
        for key in ENV_ALLOWLIST {
            if let Ok(v) = std::env::var(key) {
                cmd.env(key, v);
            }
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(ImageError::Spawn)?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_stderr(stderr));
        }

        let mut handle = ImageHandle {
            child,
            wire: Wire {
                reader: BufReader::new(stdout),
                writer: stdin,
            },
            kernel_version: String::new(),
        };

        let hello = Sexp::list(vec![Sexp::sym("hello"), Sexp::Int(PROTOCOL_VERSION)]);
        let reply = handle.wire.request(&hello, BOOT_TIMEOUT).await;
        match reply {
            Ok(r) if r.head_sym() == Some("hello-ok") => {
                handle.kernel_version = r
                    .arg(1)
                    .and_then(Sexp::as_str)
                    .unwrap_or_default()
                    .to_string();
                Ok(handle)
            }
            Ok(r) => {
                handle.kill().await;
                Err(ImageError::Handshake(format!(
                    "unexpected hello reply: {r}"
                )))
            }
            Err(e) => {
                handle.kill().await;
                Err(ImageError::Handshake(e.to_string()))
            }
        }
    }

    pub async fn request(&mut self, req: &Sexp, deadline: Duration) -> Result<Sexp, ImageError> {
        self.wire.request(req, deadline).await
    }

    /// Polite quit → deadline → SIGKILL. Never hangs.
    pub async fn shutdown(mut self) {
        let quit = Sexp::list(vec![Sexp::sym("quit")]);
        let _ = self.wire.request(&quit, QUIT_TIMEOUT).await;
        match tokio::time::timeout(QUIT_TIMEOUT, self.child.wait()).await {
            Ok(_) => {}
            Err(_) => self.kill().await,
        }
    }

    /// Hard kill + reap.
    pub async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(1), self.child.wait()).await;
    }
}

/// Forward bounded image stderr lines to tracing (never to the frame stream).
async fn drain_stderr(stderr: tokio::process::ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(mut line)) = lines.next_line().await {
        if line.len() > MAX_STDERR_LINE {
            line.truncate(MAX_STDERR_LINE);
        }
        tracing::debug!(target: "scheme_extension", "image stderr: {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wire_request_reply_and_timeout() {
        let (host_side, image_side) = tokio::io::duplex(64 * 1024);
        let (host_read, host_write) = tokio::io::split(host_side);
        let (mut img_read, mut img_write) = tokio::io::split(image_side);

        // Fake kernel: replies (hello-ok 1 "fake") to the first frame, then
        // goes silent so the second request times out.
        tokio::spawn(async move {
            let frame = read_frame(&mut img_read).await.unwrap().unwrap();
            assert_eq!(frame, "(hello 1)");
            write_frame(&mut img_write, "(hello-ok 1 \"fake\")")
                .await
                .unwrap();
            // Swallow the next frame without answering.
            let _ = read_frame(&mut img_read).await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        let mut wire = Wire {
            reader: host_read,
            writer: host_write,
        };
        let hello = Sexp::list(vec![Sexp::sym("hello"), Sexp::Int(1)]);
        let reply = wire.request(&hello, Duration::from_secs(2)).await.unwrap();
        assert_eq!(reply.head_sym(), Some("hello-ok"));
        assert_eq!(reply.arg(1).and_then(Sexp::as_str), Some("fake"));

        let err = wire
            .request(&Sexp::list(vec![Sexp::sym("inspect")]), Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, ImageError::Timeout(_)), "got {err:?}");
    }

    #[test]
    fn kernel_cache_write_and_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = ensure_kernel_cache(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), KERNEL_SOURCE);
        // Corrupt, then refresh.
        std::fs::write(&path, "stale").unwrap();
        let path2 = ensure_kernel_cache(dir.path()).unwrap();
        assert_eq!(path, path2);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), KERNEL_SOURCE);
    }

    #[test]
    fn resolve_prefers_prebuilt_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join("hyper-scheme-image");
        std::fs::write(&fake_bin, b"#!/bin/true\n").unwrap();
        let got = resolve_image_command(&[fake_bin.clone()], dir.path());
        assert_eq!(got, Some(ImageCommand::Binary(fake_bin)));
    }
}
