//! Lightweight process-spawning utilities for TTY safety.
//!
//! When a TUI/pager/raw-mode terminal owns the parent process's controlling
//! TTY, every child process must be detached — otherwise it (or its
//! grandchildren: npm, git, pinentry, ssh-agent …) can open `/dev/tty`
//! directly and spew mouse escape codes, capability-probe replies, or
//! credential prompts onto the live screen.
//!
//! This crate provides the minimal surface needed to make subprocess spawning
//! safe. It is intentionally tiny so that **every** crate in the workspace
//! can depend on it without pulling in heavyweight libraries.
//!
//! # Usage
//!
//! ```rust,no_run
//! use xai_tty_utils::{detach_command, pager_env};
//! use std::process::Stdio;
//!
//! let mut cmd = tokio::process::Command::new("git");
//! cmd.args(["status", "--porcelain"]);
//! detach_command(&mut cmd);
//! cmd.stdin(Stdio::null());
//! cmd.envs(pager_env());
//! ```
//!
//! For `std::process::Command`:
//!
//! ```rust,no_run
//! use xai_tty_utils::{detach_std_command, pager_env};
//! use std::process::Stdio;
//!
//! let mut cmd = std::process::Command::new("git");
//! cmd.args(["log", "--oneline"]);
//! detach_std_command(&mut cmd);
//! cmd.stdin(Stdio::null());
//! cmd.envs(pager_env());
//! ```

use std::collections::HashMap;
use std::io;

mod process_scope;
pub use process_scope::{ProcessScope, global_process_scope};

// ---------------------------------------------------------------------------
// TTY detach — pre_exec building block
// ---------------------------------------------------------------------------

/// Detach from the controlling TTY by starting a new session.
///
/// This prevents spawned subprocesses from opening `/dev/tty` and competing
/// with the TUI for terminal input. `Stdio::null()` only redirects fd 0;
/// programs like `ssh`, `ssh-add`, and interactive shells (`zsh -i`) bypass
/// it by opening `/dev/tty` directly.
///
/// If `setsid()` fails with `EPERM` (the process is already a process group
/// leader), falls back to `setpgid(0, 0)` which still gives process-group
/// isolation even though the controlling terminal remains reachable.
///
/// # Safety
///
/// Must only be called inside a `pre_exec` hook (between `fork` and `exec`).
/// Both `setsid()` and `setpgid()` are async-signal-safe (POSIX).
#[cfg(unix)]
pub fn detach_from_tty() -> io::Result<()> {
    use nix::errno::Errno;
    use nix::unistd::{Pid, setpgid, setsid};

    match setsid() {
        Ok(_) => Ok(()),
        Err(Errno::EPERM) => {
            setpgid(Pid::from_raw(0), Pid::from_raw(0))
                .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
            Ok(())
        }
        Err(e) => Err(io::Error::from_raw_os_error(e as i32)),
    }
}

/// No-op on non-Unix platforms.
#[cfg(not(unix))]
pub fn detach_from_tty() -> io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// tokio::process::Command wrapper
// ---------------------------------------------------------------------------

/// Detach a `tokio::process::Command` from the parent's controlling TTY/console.
///
/// - Unix: `pre_exec` hook calling `setsid` (EPERM fallback: `setpgid`).
/// - Windows: `CREATE_NO_WINDOW`. Do NOT add `DETACHED_PROCESS` — it
///   breaks stdio pipe inheritance for grandchildren (`cmd.exe` → `node`).
pub fn detach_command(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        // SAFETY: detach_from_tty only calls setsid/setpgid, both POSIX
        // async-signal-safe. Satisfies the pre_exec contract.
        unsafe {
            cmd.pre_exec(detach_from_tty);
        }
    }
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::CREATE_NO_WINDOW;
        cmd.creation_flags(CREATE_NO_WINDOW.0);
    }
}

// ---------------------------------------------------------------------------
// std::process::Command wrapper
// ---------------------------------------------------------------------------

/// Detach a `std::process::Command` from the parent's controlling TTY/console.
///
/// This is the `std` counterpart of [`detach_command`] (which only works with
/// `tokio::process::Command`). Use this when you need to spawn via
/// `std::process::Command` — e.g. in synchronous code or `spawn_blocking`.
///
/// - Unix: `pre_exec` hook calling `setsid` (EPERM fallback: `setpgid`).
/// - Windows: `CREATE_NO_WINDOW`.
pub fn detach_std_command(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: detach_from_tty only calls setsid/setpgid, both POSIX
        // async-signal-safe. Satisfies the pre_exec contract.
        unsafe {
            cmd.pre_exec(detach_from_tty);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows::Win32::System::Threading::CREATE_NO_WINDOW;
        cmd.creation_flags(CREATE_NO_WINDOW.0);
    }
}

// ---------------------------------------------------------------------------
// Process group lifecycle
// ---------------------------------------------------------------------------

/// Configure a command so the spawned child becomes the leader of a new
/// process group.
pub fn new_process_group(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP.0);
    }
}

/// A Unix process-group id validated as safe to pass to `killpg`.
///
/// `killpg(pgid)` is `kill(-pgid)`, so a degenerate pgid escalates a scoped
/// group-kill into a broadcast: `0` signals the *caller's own* group, `1`
/// signals init, and the caller's own pgid would SIGKILL this very process and
/// its whole tree. [`ProcessGroupId::new`] rejects all three, so holding one is
/// a standing guarantee that `killpg` can only ever reach a real, foreign
/// group — the highest-blast-radius primitive in process teardown is validated
/// once, at enrollment, rather than re-checked at each call site.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessGroupId(u32);

#[cfg(unix)]
impl ProcessGroupId {
    /// Validate a group-leader pid. Errors for pid `0` (the caller's own
    /// group), pid `1` (init), or the caller's own process group (signalling it
    /// would kill this very process). A child spawned into its own group
    /// (`setpgid`/`setsid`, e.g. via [`new_process_group`] or a `detach_*`
    /// helper) always has a leader pid `> 1` distinct from the caller's pgid, so
    /// a well-formed enrollment never trips this — it only catches a child that
    /// was never grouped, which would otherwise broadcast the kill.
    pub fn new(pid: u32) -> io::Result<Self> {
        if pid <= 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing degenerate process-group id {pid} (0 = own group, 1 = init)"),
            ));
        }
        // killpg_unix casts `pid as i32`; values > i32::MAX wrap to negative,
        // and killpg with a negative pgid returns EINVAL on Linux/macOS. Reject
        // here so the invariant is safe-by-construction, not safe-by-OS-quirk.
        if pid > i32::MAX as u32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("process-group id {pid} exceeds i32::MAX; cannot be used with killpg"),
            ));
        }
        if i64::from(pid) == i64::from(nix::unistd::getpgrp().as_raw()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to killpg the caller's own process group ({pid})"),
            ));
        }
        Ok(Self(pid))
    }

    /// The validated raw process-group id.
    pub fn get(self) -> u32 {
        self.0
    }
}

/// Process-tree teardown handle.
///
/// - Unix: holds the validated group-leader id ([`ProcessGroupId`]); dispatches
///   to `killpg(pgid, signal)`.
/// - Windows: holds a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
///
/// **Drop semantics differ by platform.** On Windows, drop terminates every
/// process in the job. On Unix, drop is a no-op — call [`kill`](Self::kill)
/// or [`terminate`](Self::terminate) explicitly.
pub struct ProcessGroup {
    /// `None` until a child is enrolled via [`Self::attach_pid`]; then `Some`
    /// holds a killpg-safe id. The pid is set once at attach and never
    /// auto-cleared — PID-reuse safety comes from the *owner* dropping this
    /// group (its `Arc`) at reap, after which nothing can `kill` it, not from
    /// this field resetting itself.
    #[cfg(unix)]
    leader: Option<ProcessGroupId>,
    #[cfg(windows)]
    job: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
unsafe impl Send for ProcessGroup {}
#[cfg(windows)]
unsafe impl Sync for ProcessGroup {}

impl ProcessGroup {
    pub fn new() -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self { leader: None })
        }
        #[cfg(windows)]
        {
            use std::mem::{size_of, zeroed};
            use windows::Win32::Foundation::CloseHandle;
            use windows::Win32::System::JobObjects::{
                CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            };
            use windows::core::PCWSTR;

            let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
                .map_err(|e| io::Error::other(format!("CreateJobObjectW: {e}")))?;

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            let result = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if let Err(e) = result {
                let _ = unsafe { CloseHandle(job) };
                return Err(io::Error::other(format!("SetInformationJobObject: {e}")));
            }

            Ok(Self { job })
        }
    }

    pub fn attach(&mut self, child: &tokio::process::Child) -> io::Result<()> {
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("ProcessGroup::attach: child has already exited"))?;
        self.attach_pid(pid)
    }

    /// Attach a `std::process::Child` (rather than tokio's). The PID is read via
    /// `Child::id()`, which is valid until the child is reaped with `wait()`.
    pub fn attach_std(&mut self, child: &std::process::Child) -> io::Result<()> {
        self.attach_pid(child.id())
    }

    /// Attach an already-spawned process by raw PID. The process must be (or
    /// lead) its own group/job — e.g. spawned via [`new_process_group`] (Unix
    /// `setpgid`) or a `detach_*` helper (Unix `setsid`) — otherwise `kill`
    /// would signal the wrong group.
    pub fn attach_pid(&mut self, pid: u32) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.leader = Some(ProcessGroupId::new(pid)?);
            Ok(())
        }
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::CloseHandle;
            use windows::Win32::System::JobObjects::AssignProcessToJobObject;
            use windows::Win32::System::Threading::{
                OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
            };

            let process_handle =
                unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) }
                    .map_err(|e| io::Error::other(format!("OpenProcess({pid}): {e}")))?;

            let assign_result = unsafe { AssignProcessToJobObject(self.job, process_handle) };
            let _ = unsafe { CloseHandle(process_handle) };

            assign_result
                .map_err(|e| io::Error::other(format!("AssignProcessToJobObject({pid}): {e}")))
        }
    }

    pub fn terminate(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.killpg_unix(nix::sys::signal::Signal::SIGTERM)
        }
        #[cfg(windows)]
        {
            self.terminate_job(1)
        }
    }

    pub fn kill(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.killpg_unix(nix::sys::signal::Signal::SIGKILL)
        }
        #[cfg(windows)]
        {
            self.terminate_job(1)
        }
    }

    #[cfg(unix)]
    fn killpg_unix(&self, signal: nix::sys::signal::Signal) -> io::Result<()> {
        // `leader` is `None` until a child is enrolled, and a `ProcessGroupId`
        // is killpg-safe by construction (never 0/1/own-group), so this can only
        // ever signal a real, foreign group.
        let Some(leader) = self.leader else {
            return Ok(());
        };
        nix::sys::signal::killpg(nix::unistd::Pid::from_raw(leader.get() as i32), signal)
            .map_err(|e| io::Error::from_raw_os_error(e as i32))
    }

    #[cfg(windows)]
    fn terminate_job(&self, exit_code: u32) -> io::Result<()> {
        use windows::Win32::System::JobObjects::TerminateJobObject;
        unsafe { TerminateJobObject(self.job, exit_code) }
            .map_err(|e| io::Error::other(format!("TerminateJobObject: {e}")))
    }
}

#[cfg(windows)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.job) };
    }
}

// ---------------------------------------------------------------------------
// Environment variable helpers
// ---------------------------------------------------------------------------

/// Returns environment variables that prevent CLI tools from launching any
/// interactive program that would block waiting for user input — pagers,
/// editors, credential prompts.
pub fn pager_env() -> HashMap<String, String> {
    HashMap::from([
        ("PAGER".to_string(), passthrough_pager().to_string()),
        ("GIT_PAGER".to_string(), passthrough_pager().to_string()),
        ("GH_PAGER".to_string(), passthrough_pager().to_string()),
        ("MANPAGER".to_string(), passthrough_pager().to_string()),
        ("AWS_PAGER".to_string(), String::new()),
        ("SYSTEMD_PAGER".to_string(), passthrough_pager().to_string()),
        ("GIT_EDITOR".to_string(), noop_cmd().to_string()),
        ("GIT_SEQUENCE_EDITOR".to_string(), noop_cmd().to_string()),
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        // Empty (== unset for gpg) so gpg-agent/pinentry can't grab the TUI's tty; with setsid detach, signing fails cleanly instead of corrupting the screen.
        ("GPG_TTY".to_string(), String::new()),
    ])
}

/// Environment variables set on every git command to suppress interactive prompts.
pub const GIT_AUTH_SUPPRESSION_ENVS: [(&str, &str); 4] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_ASKPASS", ""),
    ("GIT_LFS_SKIP_SMUDGE", "1"),
    ("GIT_SSH_COMMAND", "ssh -o BatchMode=yes"),
];

/// Git command with auth/LFS/SSH prompt suppression and `--no-optional-locks`.
///
/// Respects `GIT_BIN_PATH` for hermetic git in Bazel test sandboxes.
pub fn git_command() -> std::process::Command {
    let mut hermetic_exec_path: Option<std::path::PathBuf> = None;
    let git = match std::env::var("GIT_BIN_PATH") {
        Ok(p) => {
            let p = std::path::PathBuf::from(p);
            let p = if p.is_relative() {
                std::env::current_dir().unwrap_or_default().join(&p)
            } else {
                p
            };
            // git-minimal spawns subcommands (`git stash` → `git
            // update-index`) through its exec path, which is baked to a
            // build-machine prefix. Helpers live next to the binary, so point
            // the exec path there. Skip the host-fallback wrapper: host git
            // must keep its own exec path.
            if p.file_name().is_some_and(|name| name == "git") {
                hermetic_exec_path = p.parent().map(std::path::Path::to_path_buf);
            }
            p.to_string_lossy().into_owned()
        }
        Err(_) => "git".to_string(),
    };
    let mut cmd = std::process::Command::new(&git);
    detach_std_command(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    cmd.envs(pager_env());
    for &(key, val) in &GIT_AUTH_SUPPRESSION_ENVS {
        cmd.env(key, val);
    }
    if let Some(exec_path) = hermetic_exec_path {
        cmd.env("GIT_EXEC_PATH", exec_path);
    }
    cmd.arg("--no-optional-locks");
    cmd
}

fn passthrough_pager() -> &'static str {
    #[cfg(unix)]
    {
        "cat"
    }
    #[cfg(not(unix))]
    {
        ""
    }
}

fn noop_cmd() -> &'static str {
    #[cfg(unix)]
    {
        "true"
    }
    #[cfg(not(unix))]
    {
        r"C:\Windows\System32\cmd.exe /c exit 0"
    }
}

// ---------------------------------------------------------------------------
// Stderr redirection — shield TUI output from C-library noise
// ---------------------------------------------------------------------------

/// The dup'd stderr fd that writes to the real terminal. Set once by
/// [`redirect_native_stderr`]. Stored as [`OwnedFd`](std::os::unix::io::OwnedFd)
/// for type safety; the [`OnceLock`](std::sync::OnceLock) keeps it alive for the
/// entire process lifetime.
#[cfg(unix)]
static TUI_STDERR_FD: std::sync::OnceLock<std::os::unix::io::OwnedFd> = std::sync::OnceLock::new();

/// Redirect native stderr (fd 2) to `/dev/null` so that messages written
/// directly to fd 2 by C libraries do not interleave with TUI escape
/// sequences.
///
/// On macOS, `libsystem_malloc` writes `MallocStackLogging` diagnostics
/// directly to fd 2 from C code (bypassing Rust's `stderr()` lock) when
/// a `fork()`'d child process cannot turn off the parent's stack logging.
/// The message originates from `turn_off_stack_logging()` in Apple's
/// open-source `libmalloc`:
/// <https://opensource.apple.com/source/libmalloc/>
///
/// After this call, all intentional terminal output should go through
/// the `File` obtained from [`dup_tui_stderr`].
///
/// # Platform
///
/// Unix-only. On non-Unix platforms this is a no-op.
///
/// Must be called **very early**, before any threads are spawned, to avoid
/// racing with other code writing to fd 2. The TUI's `spawn_writer_thread`
/// should be called after this.
pub fn redirect_native_stderr() {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

        // SAFETY: dup(2) is safe on fd 2 (stderr), which is always valid
        // at process start.
        let duped = unsafe { libc::dup(2) };
        if duped < 0 {
            return; // best-effort; don't crash
        }

        // Store the duped fd (as OwnedFd) before redirecting.
        // SAFETY: `duped` is a freshly dup'd, valid file descriptor.
        let owned = unsafe { OwnedFd::from_raw_fd(duped) };
        let _ = TUI_STDERR_FD.set(owned);

        // Flush any buffered stderr data before redirecting fd 2 to
        // /dev/null, so nothing is silently lost.
        let _ = std::io::stderr().flush();

        // Open /dev/null and dup2 it onto fd 2.
        let devnull = match std::fs::OpenOptions::new().write(true).open("/dev/null") {
            Ok(f) => f,
            Err(_) => return,
        };
        // SAFETY: devnull.as_raw_fd() is valid; fd 2 is a valid target.
        unsafe { libc::dup2(devnull.as_raw_fd(), 2) };
        // devnull File is dropped here; fd 2 now points at /dev/null.
    }
}

/// Create a new owned `File` that writes to the real terminal stderr.
///
/// This calls `dup(2)` on the saved fd to create an independently-owned
/// file descriptor. Each caller gets their own fd that they can wrap in
/// a `BufWriter`, pass to a thread, etc. Dropping the returned `File`
/// closes only that caller's dup'd copy — the underlying terminal fd
/// is never affected.
///
/// If [`redirect_native_stderr`] was not called, this dups normal fd 2.
pub fn dup_tui_stderr() -> io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::io::{AsRawFd, FromRawFd};
        let source_fd = TUI_STDERR_FD.get().map(|fd| fd.as_raw_fd()).unwrap_or(2);
        // SAFETY: source_fd is either the dup'd stderr (valid for the
        // process lifetime via OnceLock<OwnedFd>) or 2 (normal stderr).
        // dup() returns a new independently-owned fd.
        let new_fd = unsafe { libc::dup(source_fd) };
        if new_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: new_fd is a freshly dup'd, owned file descriptor.
        Ok(unsafe { std::fs::File::from_raw_fd(new_fd) })
    }
    #[cfg(not(unix))]
    {
        // On Windows, `redirect_native_stderr` is a no-op, so fd 2 is
        // always the real stderr. We use `try_clone()` on a temporarily
        // created File to get an independently-owned handle via
        // `DuplicateHandle` — avoiding the `from_raw_handle` footgun
        // where `File` would take ownership of the process stderr handle
        // and close it on drop.
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        let stderr_handle = unsafe {
            windows::Win32::System::Console::GetStdHandle(
                windows::Win32::System::Console::STD_ERROR_HANDLE,
            )
            .map_err(|e| io::Error::other(format!("GetStdHandle: {e}")))?
        };
        // Create a temporary File from the raw handle, clone it (which
        // calls DuplicateHandle internally), then forget the original so
        // it doesn't close the process stderr handle.
        let temp = unsafe { std::fs::File::from_raw_handle(stderr_handle.0 as _) };
        let cloned = temp.try_clone();
        // Prevent `temp` from closing the process stderr handle.
        std::mem::forget(temp);
        cloned
    }
}

/// Restore fd 2 to point to the real terminal stderr.
///
/// Call this before exiting so that any final messages (panic output,
/// tracing flushes) reach the user's terminal rather than `/dev/null`.
pub fn restore_native_stderr() {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        if let Some(fd) = TUI_STDERR_FD.get() {
            // SAFETY: fd is the dup'd stderr (valid for process lifetime
            // via OnceLock<OwnedFd>). dup2 atomically replaces fd 2 with
            // a copy of fd.
            unsafe { libc::dup2(fd.as_raw_fd(), 2) };
        }
    }
}

/// Returns `true` when running inside Windows Subsystem for Linux (WSL1/WSL2).
/// Cached for process lifetime. Detection: the `WSL_DISTRO_NAME` / `WSL_INTEROP`
/// env vars, with a `/proc/sys/kernel/osrelease` `microsoft`/`wsl` substring
/// fallback that also catches WSL1 and env-stripped shells.
pub fn is_wsl() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(detect_is_wsl)
}

fn detect_is_wsl() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let env: HashMap<String, String> = std::env::vars().collect();
    let osrelease = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
    is_wsl_from_inputs(&env, osrelease.as_deref())
}

/// Pure helper so tests can drive the env and `/proc` contents directly.
fn is_wsl_from_inputs(env: &HashMap<String, String>, osrelease: Option<&str>) -> bool {
    if env.contains_key("WSL_DISTRO_NAME") || env.contains_key("WSL_INTEROP") {
        return true;
    }
    osrelease.is_some_and(|s| {
        let lower = s.to_ascii_lowercase();
        lower.contains("microsoft") || lower.contains("wsl")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_command_does_not_panic() {
        let mut cmd = tokio::process::Command::new("echo");
        detach_command(&mut cmd);
    }

    #[test]
    fn detach_std_command_does_not_panic() {
        let mut cmd = std::process::Command::new("echo");
        detach_std_command(&mut cmd);
    }

    #[test]
    fn new_process_group_does_not_panic() {
        let mut cmd = tokio::process::Command::new("echo");
        new_process_group(&mut cmd);
    }

    fn wsl_env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn is_wsl_detects_wsl_distro_name() {
        assert!(is_wsl_from_inputs(
            &wsl_env(&[("WSL_DISTRO_NAME", "Ubuntu")]),
            None
        ));
    }

    #[test]
    fn is_wsl_detects_wsl_interop() {
        assert!(is_wsl_from_inputs(
            &wsl_env(&[("WSL_INTEROP", "/run/WSL/8_interop")]),
            None,
        ));
    }

    // osrelease fallback also catches env-stripped shells.
    #[test]
    fn is_wsl_detects_wsl2_osrelease() {
        assert!(is_wsl_from_inputs(
            &wsl_env(&[]),
            Some("5.15.167.4-microsoft-standard-WSL2\n"),
        ));
    }

    #[test]
    fn is_wsl_detects_wsl1_osrelease() {
        assert!(is_wsl_from_inputs(
            &wsl_env(&[]),
            Some("4.4.0-19041-Microsoft\n")
        ));
    }

    #[test]
    fn is_wsl_osrelease_match_is_case_insensitive() {
        assert!(is_wsl_from_inputs(
            &wsl_env(&[]),
            Some("WSLg-experimental\n")
        ));
        assert!(is_wsl_from_inputs(
            &wsl_env(&[]),
            Some("Foo-MICROSOFT-bar\n")
        ));
    }

    #[test]
    fn is_wsl_rejects_native_linux() {
        assert!(!is_wsl_from_inputs(
            &wsl_env(&[]),
            Some("6.5.0-21-generic\n")
        ));
    }

    #[test]
    fn is_wsl_rejects_when_osrelease_missing() {
        assert!(!is_wsl_from_inputs(&wsl_env(&[]), None));
    }

    #[test]
    fn is_wsl_env_var_match_is_exact_key_not_substring() {
        // A var whose name merely contains "WSL" must not trigger detection.
        assert!(!is_wsl_from_inputs(
            &wsl_env(&[("MY_WSLISH_VAR", "1"), ("OTHER", "wsl_in_value")]),
            Some("6.5.0-21-generic\n"),
        ));
    }

    #[test]
    fn pager_env_has_expected_keys() {
        let env = pager_env();
        assert!(env.contains_key("PAGER"));
        assert!(env.contains_key("GIT_PAGER"));
        assert!(env.contains_key("GH_PAGER"));
        assert!(env.contains_key("GIT_TERMINAL_PROMPT"));
        // Present and empty so gpg/pinentry cannot target the TUI tty.
        assert_eq!(env.get("GPG_TTY"), Some(&String::new()));
    }

    // ── stderr redirect integration tests ────────────────────────
    //
    // The redirect/dup/restore cycle mutates process-global state
    // (fd 2 and a `OnceLock`), so the full flow runs in a subprocess
    // to avoid polluting other tests.

    /// `dup_tui_stderr` returns a writable File even without prior redirect.
    #[test]
    fn dup_tui_stderr_without_redirect_returns_writable_file() {
        use std::io::Write;
        let mut f = dup_tui_stderr().expect("dup_tui_stderr should succeed");
        f.write_all(b"").expect("zero-length write should succeed");
    }

    /// The `File` returned by `dup_tui_stderr` can write multi-byte UTF-8
    /// without truncation or byte-level corruption.
    ///
    /// On Windows, a `File` wrapping a console handle uses `WriteFile`
    /// (byte-oriented, code-page-dependent) instead of `WriteConsoleW`
    /// (UTF-16, code-page-independent). This test verifies the bytes are
    /// at least accepted without error. Full Unicode correctness on Windows
    /// additionally requires `SetConsoleOutputCP(65001)` or using
    /// `std::io::stderr()` (which routes through `WriteConsoleW`).
    #[test]
    fn dup_tui_stderr_accepts_multibyte_utf8() {
        use std::io::Write;
        let mut f = dup_tui_stderr().expect("dup_tui_stderr should succeed");
        // Braille (3 bytes each), Powerline icon (3 bytes), emoji (4 bytes).
        let payload = "⣀⣾⠿⠛\u{e0a0}\u{1F600}";
        f.write_all(payload.as_bytes())
            .expect("multi-byte UTF-8 write should succeed");
        f.flush().expect("flush should succeed");
    }

    /// Spawn a subprocess that exercises redirect → dup → restore.
    #[cfg(unix)]
    #[test]
    fn stderr_redirect_roundtrip_subprocess() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("tests::stderr_redirect_roundtrip_body")
            .env("__XAI_STDERR_REDIRECT_SUBPROCESS", "1")
            .status()
            .expect("failed to spawn test subprocess");
        assert!(status.success(), "subprocess integration test failed");
    }

    /// The actual redirect/dup/restore test body (only runs as subprocess).
    #[cfg(unix)]
    #[test]
    fn stderr_redirect_roundtrip_body() {
        if std::env::var("__XAI_STDERR_REDIRECT_SUBPROCESS").is_err() {
            return; // skip when not invoked as subprocess
        }

        use std::io::Write;
        use std::os::unix::io::AsRawFd;

        // 1. After redirect, fd 2 points at /dev/null.
        redirect_native_stderr();
        let fd2_flags = unsafe { libc::fcntl(2, libc::F_GETFD) };
        assert!(fd2_flags >= 0, "fd 2 should be valid after redirect");

        // Verify fd 2 is /dev/null by checking that writes succeed
        // but reading the inode shows it's a different file.
        let mut stat_fd2: libc::stat = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::fstat(2, &mut stat_fd2) };
        assert_eq!(r, 0, "fstat(2) should succeed");

        // 2. dup_tui_stderr returns a working fd to the real terminal.
        let mut tui = dup_tui_stderr().expect("dup_tui_stderr after redirect");
        assert!(tui.as_raw_fd() > 2, "dup'd fd should be > 2");
        tui.write_all(b"").expect("writing to dup'd fd should work");

        // 3. A second call returns an independent fd.
        let tui2 = dup_tui_stderr().expect("second dup_tui_stderr");
        assert_ne!(
            tui.as_raw_fd(),
            tui2.as_raw_fd(),
            "each call should return a distinct fd"
        );
        drop(tui2);
        // First fd still works after second is dropped.
        tui.write_all(b"").expect("first fd survives second drop");

        // 4. restore_native_stderr brings fd 2 back.
        restore_native_stderr();
        let fd2_after = unsafe { libc::fcntl(2, libc::F_GETFD) };
        assert!(fd2_after >= 0, "fd 2 should be valid after restore");

        // Verify fd 2 is no longer /dev/null (inode changed).
        let mut stat_after: libc::stat = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::fstat(2, &mut stat_after) };
        assert_eq!(r, 0, "fstat(2) after restore should succeed");
        assert_ne!(
            stat_fd2.st_ino, stat_after.st_ino,
            "fd 2 inode should differ after restore (no longer /dev/null)"
        );
    }

    #[tokio::test]
    async fn process_group_kill_terminates_attached_child() {
        let mut cmd = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/C", "ping", "-n", "60", "127.0.0.1"]);
            c
        } else {
            let mut c = tokio::process::Command::new("sleep");
            c.arg("60");
            c
        };
        new_process_group(&mut cmd);

        let mut group = ProcessGroup::new().expect("create ProcessGroup");
        #[allow(clippy::disallowed_methods)] // test: exercises ProcessGroup directly
        let mut child = cmd.spawn().expect("spawn child");
        group.attach(&child).expect("attach child to group");

        group.kill().expect("kill ProcessGroup");

        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .expect("child should exit within 5s of group kill")
            .expect("wait returns ok");
        assert!(
            !status.success(),
            "killed child should not exit successfully"
        );
    }

    /// `kill()` must reap the WHOLE process group — including a GRANDCHILD the
    /// leader forks into the same group — not just the immediate leader. This is
    /// the `killpg` tree-kill property the LSP / MCP / terminal teardown relies
    /// on; the pre-fix code signalled only the direct child, orphaning
    /// grandchildren (e.g. a language server's own subprocesses).
    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_kill_reaps_grandchild_tree() {
        use tokio::io::{AsyncBufReadExt, BufReader};

        // Leader = sh; it backgrounds a grandchild in the SAME group (sh does
        // not `setsid` its children), prints the grandchild PID, then waits.
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "sleep 60 & echo $!; wait"]);
        cmd.stdout(std::process::Stdio::piped());
        new_process_group(&mut cmd);

        let mut group = ProcessGroup::new().expect("create ProcessGroup");
        #[allow(clippy::disallowed_methods)] // test: exercises ProcessGroup + grandchild kill
        let mut child = cmd.spawn().expect("spawn leader");
        group.attach(&child).expect("attach leader to group");

        // Grandchild PID from the leader's stdout.
        let stdout = child.stdout.take().expect("piped stdout");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .await
            .expect("read grandchild pid");
        let gc_pid: i32 = line.trim().parse().expect("parse grandchild pid");

        let alive =
            |pid: i32| nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
        assert!(
            alive(gc_pid),
            "grandchild should be running before the kill"
        );

        group.kill().expect("kill ProcessGroup");

        // Leader exits.
        tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .expect("leader should exit within 5s of group kill")
            .expect("wait ok");

        // The grandchild (same group) must ALSO be reaped by the killpg — poll
        // until gone (orphan → reparented to init → reaped → ESRCH).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while alive(gc_pid) && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !alive(gc_pid),
            "killpg must reap the WHOLE group incl. the grandchild (tree-kill), not just the \
             leader; grandchild pid {gc_pid} still alive after group kill"
        );
    }

    /// [`ProcessGroupId`] rejects degenerate pids (0, 1, own group) at
    /// construction so `killpg` can never broadcast outside a child's group.
    #[cfg(unix)]
    #[test]
    fn process_group_id_rejects_degenerate_and_own_group() {
        assert!(
            ProcessGroupId::new(0).is_err(),
            "pid 0 = caller's own group — must be refused"
        );
        assert!(
            ProcessGroupId::new(1).is_err(),
            "pid 1 = init — must be refused"
        );
        let own = nix::unistd::getpgrp().as_raw() as u32;
        assert!(
            ProcessGroupId::new(own).is_err(),
            "the caller's own process group ({own}) must be refused"
        );
        // Pid above i32::MAX wraps on cast — must be refused.
        assert!(
            ProcessGroupId::new(u32::MAX).is_err(),
            "pid > i32::MAX must be refused (wrapping cast in killpg)"
        );
        // A pid that is clearly foreign succeeds.
        let foreign = if own == 2 { 3 } else { 2 };
        let id = ProcessGroupId::new(foreign).expect("foreign pgid should be accepted");
        assert_eq!(id.get(), foreign);
    }
}
