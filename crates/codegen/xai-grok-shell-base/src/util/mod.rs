pub mod changelog;
pub mod event_id;
pub mod grok_home;
pub mod secure_file;
pub mod tips;
pub mod uname;
pub use xai_grok_shared::clipboard;
pub use xai_grok_shared::stderr::{stderr_lock, with_locked_stderr};
/// Generate a pseudo-random f64 in [0.0, 1.0).
///
/// Uses `RandomState::new()` which is OS-seeded (via `getrandom`) on each
/// instantiation, producing a unique hasher state per call. A fixed sentinel
/// is hashed to extract the random bits — the entropy comes entirely from
/// the OS-seeded `RandomState`, not from any clock source.
///
/// # Precision
/// The result uses all 53 bits of `f64` mantissa for a uniform distribution
/// over `[0.0, 1.0)`. We shift the 64-bit hash right by 11 bits to get a
/// 53-bit integer, then divide by `2^53`. This avoids the subtle bias that
/// occurs when casting a full `u64` to `f64` (which has only 52 bits of
/// mantissa, causing multiple `u64` values to map to the same `f64` for
/// values > 2^52).
///
/// Not cryptographically secure — suitable for sampling and feature
/// rollouts, not for security-sensitive randomness.
pub fn random_f64() -> f64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let random_state = RandomState::new();
    let mut hasher = random_state.build_hasher();
    hasher.write_u64(0x517cc1b727220a95);
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}
/// Probabilistic sampling. Returns `true` with probability `rate` (0.0–1.0).
pub fn probabilistic_sample(rate: f64) -> bool {
    random_f64() < rate
}
fn matches_trusted_base_url(candidate: &str, trusted_base: &str) -> bool {
    let Ok(candidate) = reqwest::Url::parse(candidate) else {
        return false;
    };
    let Ok(trusted) = reqwest::Url::parse(trusted_base) else {
        return false;
    };
    let trusted_path = trusted.path();
    let candidate_path = candidate.path();
    let path_matches = candidate_path == trusted_path
        || candidate_path
            .strip_prefix(trusted_path)
            .is_some_and(|suffix| suffix.starts_with('/'));
    candidate.scheme() == trusted.scheme()
        && candidate.host_str() == trusted.host_str()
        && candidate.port_or_known_default() == trusted.port_or_known_default()
        && path_matches
}
/// True for cli-chat-proxy URLs (production, plus local-dev hosts when the
/// optional non-production feature is enabled). When that feature is on,
/// runtime env overrides can extend this trust set. Loopback is always
/// accepted (unit tests and local mock servers on arbitrary ports).
pub fn is_cli_chat_proxy_url(url: &str) -> bool {
    if matches_trusted_base_url(url, crate::env::PROD_CLI_CHAT_PROXY_BASE_URL) {
        return true;
    }
    if let Ok(u) = reqwest::Url::parse(url)
        && let Some(h) = u.host_str()
        && (h == "localhost" || h == "127.0.0.1" || h == "::1")
    {
        return true;
    }
    false
}
/// True for xAI-operated endpoints (`*.x.ai`, cli-chat-proxy, and optional
/// non-production xAI hosts when that feature is enabled).
/// `disable_api_key_auth` refuses keys only for these; other hosts are BYOK and
/// exempt. Safe against invalid URLs and suffix attacks (`evil-x.ai.example`).
///
/// Scheme-agnostic so credential *refusal* fails closed. To decide where to
/// *attach* a credential, use [`is_xai_api_bearer_url`].
pub fn is_xai_api_url(url: &str) -> bool {
    is_xai_api_url_impl(url, false)
}
/// Like [`is_xai_api_url`], but requires `https` on every arm, so a
/// session bearer is never attached to a cleartext endpoint, including loopback
/// (a co-located process could otherwise read a token sent to `http://localhost`).
pub fn is_xai_api_bearer_url(url: &str) -> bool {
    is_xai_api_url_impl(url, true)
}
fn is_xai_api_url_impl(url: &str, require_https: bool) -> bool {
    if require_https {
        let Ok(parsed) = reqwest::Url::parse(url) else {
            return false;
        };
        if parsed.scheme() != "https" {
            return false;
        }
        if is_loopback_host(&parsed) {
            return false;
        }
    }
    if is_cli_chat_proxy_url(url) {
        return true;
    }
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .is_some_and(|host| host == "x.ai" || host.ends_with(".x.ai"))
}
fn is_loopback_host(parsed: &reqwest::Url) -> bool {
    match parsed.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}
/// Truncate a string to at most `max_chars` characters.
/// Slices at char boundaries so multi-byte UTF-8 never panics.
pub fn truncate(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        return s;
    }
    let end = s
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    &s[..end]
}
/// Check if a process is still alive.
///
/// - Unix: `kill(pid, 0)` via `nix`. True if the process exists (even
///   under a different UID); false only on ESRCH.
/// - Windows: `OpenProcess(SYNCHRONIZE)` + `WaitForSingleObject(0)`. True
///   while running; false on exit, absence, or open failure.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(_) => true,
    }
}
#[cfg(windows)]
pub fn is_process_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        return false;
    };
    let wait_result = unsafe { WaitForSingleObject(handle, 0) };
    let _ = unsafe { CloseHandle(handle) };
    wait_result == WAIT_TIMEOUT
}
/// Terminate a process by PID. Idempotent: already-dead is `Ok`.
///
/// - Unix: `SIGTERM` via `nix::sys::signal::kill`; ESRCH maps to `Ok`.
/// - Windows: `OpenProcess(PROCESS_TERMINATE)` + `TerminateProcess`;
///   ERROR_INVALID_PARAMETER (Windows' "no such process") maps to `Ok`.
pub fn kill_process_by_pid(pid: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use nix::errno::Errno;
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        match kill(Pid::from_raw(pid as i32), Signal::SIGTERM) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(e) => Err(std::io::Error::from_raw_os_error(e as i32)),
        }
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER};
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
        use windows::core::HRESULT;
        let no_such_process = HRESULT::from_win32(ERROR_INVALID_PARAMETER.0);
        let handle = match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
            Ok(h) => h,
            Err(e) if e.code() == no_such_process => return Ok(()),
            Err(e) => {
                return Err(std::io::Error::other(format!("OpenProcess({pid}): {e}")));
            }
        };
        let terminate = unsafe { TerminateProcess(handle, 0) };
        let _ = unsafe { CloseHandle(handle) };
        terminate.map_err(|e| std::io::Error::other(format!("TerminateProcess({pid}): {e}")))
    }
}
/// True if `pid` is a Grok Build / Hyper product process.
///
/// Pairs with [`kill_process_by_pid`] so leader cleanup does not treat a recycled
/// PID as "stale" and delete live lock/socket files. Recognizes:
/// - shipped binaries `grok` and `hyper` (exact basename)
/// - in-tree test/dev binaries named `xai-grok-*`
/// - managed install roots `~/.grok/bin/` and `~/.hyper/bin/`
///
/// Best-effort on macOS/BSD (liveness-only via `kill -0`), exact on Linux
/// (`/proc` cmdline) and Windows (image path).
pub fn is_grok_process(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let cmdline_path = format!("/proc/{pid}/cmdline");
        match std::fs::read(&cmdline_path) {
            Ok(data) => process_image_looks_like_product(&String::from_utf8_lossy(&data)),
            Err(_) => false,
        }
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        };
        use windows::core::PWSTR;
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            return false;
        };
        let mut buf: Vec<u16> = vec![0; 1024];
        let mut size: u32 = buf.len() as u32;
        let result = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut size,
            )
        };
        let _ = unsafe { CloseHandle(handle) };
        if result.is_err() {
            return false;
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]).to_ascii_lowercase();
        process_image_looks_like_product(&path)
    }
    #[cfg(all(not(target_os = "linux"), not(windows)))]
    {
        let mut cmd = std::process::Command::new("kill");
        cmd.args(["-0", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        xai_tty_utils::detach_std_command(&mut cmd);
        cmd.status().is_ok_and(|s| s.success())
    }
}

/// Whether a process image path / Linux cmdline identifies a product binary.
///
/// Linux `/proc/<pid>/cmdline` is NUL-separated argv; Windows paths use `\\`.
/// Matching is basename-first so random substrings (e.g. a workspace folder
/// named `hyper-grok-build`) do not count unless the executable basename itself
/// is a product name.
pub(crate) fn process_image_looks_like_product(image: &str) -> bool {
    let image = image.trim();
    if image.is_empty() {
        return false;
    }
    // Linux cmdline: argv0\0argv1\0…; also tolerate spaces for Windows-style dumps.
    for part in image.split(|c: char| c == '\0' || c.is_ascii_whitespace()) {
        if part.is_empty() {
            continue;
        }
        let base = part
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(part)
            .trim_end_matches(".exe");
        let base_lower = base.to_ascii_lowercase();
        if base_lower == "grok" || base_lower == "hyper" {
            return true;
        }
        // Cargo test / example binaries: package names use hyphens in Cargo.toml
        // (`xai-grok-shell-base`) but the built test artifact uses underscores
        // (`xai_grok_shell_base-<hash>`).
        if base_lower.starts_with("xai-grok-") || base_lower.starts_with("xai_grok_") {
            return true;
        }
    }
    // Managed install roots (path may appear when basename parsing is odd).
    let lower = image.to_ascii_lowercase();
    lower.contains("/.hyper/bin/")
        || lower.contains("/.grok/bin/")
        || lower.contains("\\.hyper\\bin\\")
        || lower.contains("\\.grok\\bin\\")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_is_cli_chat_proxy_url_accepts_proxy_subpath() {
        assert!(is_cli_chat_proxy_url(
            "https://cli-chat-proxy.grok.com/v1/chat/completions"
        ));
    }
    #[test]
    fn test_is_cli_chat_proxy_url_rejects_public_api() {
        assert!(!is_cli_chat_proxy_url("https://api.x.ai/v1"));
    }
    #[test]
    fn test_is_cli_chat_proxy_url_rejects_spoofed_hostname() {
        assert!(!is_cli_chat_proxy_url(
            "https://cli-chat-proxy.grok.com.evil.example/v1"
        ));
    }
    #[test]
    fn test_is_cli_chat_proxy_url_rejects_v11_prefix_confusion() {
        assert!(!is_cli_chat_proxy_url(
            "https://cli-chat-proxy.grok.com/v11/chat/completions"
        ));
    }
    #[test]
    fn test_is_xai_api_url() {
        assert!(is_xai_api_url("https://api.x.ai/v1"));
        assert!(is_xai_api_url("https://api.x.ai/v1/chat/completions"));
        assert!(is_xai_api_url("https://x.ai"));
        assert!(is_xai_api_url(
            "https://cli-chat-proxy.grok.com/v1/chat/completions"
        ));
        assert!(!is_xai_api_url("https://api.openai.com/v1"));
        assert!(!is_xai_api_url("https://api.anthropic.com/v1"));
        assert!(!is_xai_api_url("https://generativelanguage.googleapis.com"));
        assert!(!is_xai_api_url("https://api.x.ai.evil.example/v1"));
        assert!(!is_xai_api_url("https://evil-x.ai.attacker.com/v1"));
        assert!(!is_xai_api_url("https://prefixx.ai/v1"));
        assert!(!is_xai_api_url("not-a-url"));
        assert!(!is_xai_api_url(""));
        assert!(is_xai_api_url("http://api.x.ai/v1"));
        assert!(is_xai_api_url("http://localhost:11434/v1"));
    }
    #[test]
    fn test_is_xai_api_bearer_url() {
        assert!(is_xai_api_bearer_url("https://api.x.ai/v1"));
        assert!(!is_xai_api_bearer_url("http://api.x.ai/v1"));
        assert!(!is_xai_api_bearer_url("http://localhost:11434/v1"));
        {
            assert!(!is_xai_api_bearer_url("https://localhost:11434/v1"));
            assert!(!is_xai_api_bearer_url("https://127.0.0.2:11434/v1"));
            assert!(!is_xai_api_bearer_url("https://[::1]:11434/v1"));
        }
        assert!(is_xai_api_bearer_url("https://API.X.AI/v1"));
        assert!(!is_xai_api_bearer_url(
            "https://api.x.ai@attacker.example/v1"
        ));
        assert!(!is_xai_api_bearer_url("https://х.ai/v1"));
    }
    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 5), "hello");
        assert_eq!(truncate("abc🎉🎉def", 5), "abc🎉🎉");
    }
    #[test]
    fn is_process_alive_current_process() {
        assert!(is_process_alive(std::process::id()));
    }
    #[test]
    fn is_process_alive_dead_pid() {
        assert!(!is_process_alive(4_000_000_000));
    }
    #[cfg(unix)]
    #[test]
    fn is_process_alive_init_process() {
        assert!(is_process_alive(1));
    }
    #[test]
    fn kill_process_by_pid_already_dead_is_ok() {
        assert!(kill_process_by_pid(4_000_000_000).is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn kill_process_by_pid_terminates_live_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        kill_process_by_pid(pid).expect("kill should succeed");
        let status = child.wait().expect("wait child");
        assert!(
            !status.success(),
            "sleep was terminated, not exited cleanly"
        );
    }
    #[test]
    fn is_grok_process_self_true_impossible_pid_false() {
        assert!(is_grok_process(std::process::id()));
        assert!(!is_grok_process(u32::MAX));
    }

    #[test]
    fn process_image_recognizes_hyper_and_grok_basenames() {
        assert!(process_image_looks_like_product("/home/u/.hyper/bin/hyper"));
        assert!(process_image_looks_like_product(
            r"C:\Users\u\.hyper\bin\hyper.exe"
        ));
        assert!(process_image_looks_like_product("/home/u/.grok/bin/grok"));
        assert!(process_image_looks_like_product(
            "/tmp/xai-grok-shell-base-abc123"
        ));
        assert!(process_image_looks_like_product(
            "/tmp/xai_grok_shell_base-abc123"
        ));
        // Linux-style cmdline: path + NUL + args
        assert!(process_image_looks_like_product(
            "/home/u/.hyper/bin/hyper\0leader\0kill"
        ));
        // Workspace path alone must not match without a product basename
        assert!(!process_image_looks_like_product(
            "/home/u/hyper-grok-build-new-feature/target/debug/deps/some_other_test-xyz"
        ));
        assert!(!process_image_looks_like_product("/usr/bin/sleep"));
        assert!(!process_image_looks_like_product(""));
    }
}
