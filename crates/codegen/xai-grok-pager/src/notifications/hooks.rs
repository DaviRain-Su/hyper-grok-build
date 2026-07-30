use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use crate::notifications::NotificationEvent;
use crate::notifications::config::NotificationHook;

fn execute_hook(
    command: &str,
    event_str: &str,
    message: &str,
    session_id: Option<&str>,
    timeout: Duration,
) {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .env("GROK_EVENT", event_str)
        .env("GROK_MESSAGE", message)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(sid) = session_id {
        cmd.env("GROK_SESSION_ID", sid);
    }

    // Create a new process group so we can kill the entire tree on timeout,
    // preventing orphaned subprocesses from accumulating.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe per POSIX and does not
        // allocate or take locks. Called between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().ok();
                Ok(())
            });
        }
    }

    #[allow(clippy::disallowed_methods)] // enrolled below, once the child exists
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            tracing::debug!(error = %e, command, "hook spawn failed");
            return;
        }
    };

    // Enrolled so a hook still running when the pager exits is reaped with it.
    let group = attach_to_global_scope(&child);

    use wait_timeout::ChildExt;
    let waited = child.wait_timeout(timeout);
    if !matches!(waited, Ok(Some(_))) {
        // The enrollment ends with this function, so kill rather than orphan.
        match &group {
            Some(group) => {
                let _ = group.kill();
            }
            None => {
                let _ = child.kill();
            }
        }
        let _ = child.wait();
    }
    // Reaped, so the pid is released and its pgid can be recycled: the group
    // handle must not outlive the child it names.
    drop(group);
    match waited {
        Ok(Some(_)) => {}
        Ok(None) => tracing::warn!("hook timed out"),
        Err(e) => tracing::debug!(error = %e, command, "hook wait failed"),
    }
}

/// `None` when the child could not be enrolled, which includes the scope
/// having already closed and killed it.
fn attach_to_global_scope(child: &std::process::Child) -> Option<Arc<xai_tty_utils::ProcessGroup>> {
    let mut group = xai_tty_utils::ProcessGroup::new()
        .inspect_err(|e| tracing::debug!(error = %e, "hook process group failed"))
        .ok()?;
    group
        .attach_std(child)
        .inspect_err(|e| tracing::debug!(error = %e, "hook process group attach failed"))
        .ok()?;
    let group = Arc::new(group);
    xai_tty_utils::global_process_scope()
        .register(&group)
        .then_some(group)
}

pub fn run_hook(hook: &NotificationHook, event: &NotificationEvent) {
    let command = hook.command.clone();
    let event_str: &'static str = event.kind.as_str();
    let message = event.body.clone();
    let session_id = event.session_id.clone();
    let timeout = Duration::from_secs(hook.timeout_secs.max(1));

    std::thread::spawn(move || {
        execute_hook(
            &command,
            event_str,
            &message,
            session_id.as_deref(),
            timeout,
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifications::config::NotificationEventKind;
    use std::time::Instant;

    fn test_event() -> NotificationEvent {
        NotificationEvent {
            kind: NotificationEventKind::TurnComplete,
            title: "Grok".into(),
            body: "test body payload".into(),
            session_id: Some("test-session-123".into()),
        }
    }

    #[test]
    fn sets_environment_variables() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("env.txt");
        let command = format!(
            "printf 'GROK_EVENT=%s\\nGROK_MESSAGE=%s\\nGROK_SESSION_ID=%s\\n' \
             \"$GROK_EVENT\" \"$GROK_MESSAGE\" \"$GROK_SESSION_ID\" > {}",
            out.display()
        );

        execute_hook(
            &command,
            "Turn complete",
            "hello world",
            Some("sess-42"),
            Duration::from_secs(5),
        );

        let content = std::fs::read_to_string(&out).unwrap();
        assert!(
            content.contains("GROK_EVENT=Turn complete"),
            "missing GROK_EVENT: {content}"
        );
        assert!(
            content.contains("GROK_MESSAGE=hello world"),
            "missing GROK_MESSAGE: {content}"
        );
        assert!(
            content.contains("GROK_SESSION_ID=sess-42"),
            "missing GROK_SESSION_ID: {content}"
        );
    }

    #[test]
    fn omits_session_id_when_none() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("env.txt");
        let command = format!("env > {}", out.display());

        execute_hook(
            &command,
            "Turn complete",
            "msg",
            None,
            Duration::from_secs(5),
        );

        let content = std::fs::read_to_string(&out).unwrap();
        assert!(
            !content.contains("GROK_SESSION_ID"),
            "GROK_SESSION_ID should not be set: {content}"
        );
    }

    #[test]
    fn kills_on_timeout() {
        let start = Instant::now();
        execute_hook(
            "sleep 100",
            "Turn complete",
            "msg",
            None,
            Duration::from_secs(1),
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "should return within timeout, took {elapsed:?}"
        );
    }

    #[test]
    fn handles_failed_shell_command_gracefully() {
        execute_hook(
            "/nonexistent/path/binary",
            "Turn complete",
            "msg",
            None,
            Duration::from_secs(1),
        );
    }

    #[test]
    fn handles_nonzero_exit_gracefully() {
        execute_hook(
            "exit 1",
            "Turn complete",
            "msg",
            None,
            Duration::from_secs(5),
        );
    }

    #[test]
    fn successful_command_completes_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("done");
        let command = format!("touch {}", marker.display());

        execute_hook(
            &command,
            "Turn complete",
            "msg",
            None,
            Duration::from_secs(5),
        );

        assert!(marker.exists());
    }

    #[test]
    fn run_hook_spawns_thread_without_panic() {
        let hook = NotificationHook {
            command: "true".into(),
            events: vec![],
            only_unfocused: false,
            timeout_secs: 5,
        };
        run_hook(&hook, &test_event());
        std::thread::sleep(Duration::from_millis(200));
    }

    #[test]
    fn timeout_clamped_to_minimum_one_second() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("done");
        let hook = NotificationHook {
            command: format!("sleep 100; touch {}", marker.display()),
            events: vec![],
            only_unfocused: false,
            timeout_secs: 0, // exercises the .max(1) clamp inside run_hook
        };
        let start = Instant::now();
        run_hook(&hook, &test_event());
        // Wait for the spawned thread to finish (clamp turns 0 -> 1s timeout)
        std::thread::sleep(Duration::from_millis(2500));
        let elapsed = start.elapsed();
        // The hook should have been killed by the 1s timeout, so the marker
        // file should NOT exist (sleep 100 never completes).
        assert!(
            !marker.exists(),
            "hook should have been killed by timeout before creating marker"
        );
        // Sanity: the whole thing completed well under 10s, confirming the
        // timeout was ~1s (clamped) not 0s (instant) or unbounded.
        assert!(
            elapsed < Duration::from_secs(5),
            "should complete within a few seconds, took {elapsed:?}"
        );
    }

    #[test]
    fn run_hook_passes_correct_env_via_thread() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("env.txt");
        let hook = NotificationHook {
            command: format!(
                "printf 'GROK_EVENT=%s\\nGROK_MESSAGE=%s\\nGROK_SESSION_ID=%s\\n' \
                 \"$GROK_EVENT\" \"$GROK_MESSAGE\" \"$GROK_SESSION_ID\" > {}",
                out.display()
            ),
            events: vec![],
            only_unfocused: false,
            timeout_secs: 5,
        };
        let event = test_event();
        run_hook(&hook, &event);

        // Poll for the output file instead of a fixed sleep — the spawned
        // thread + fork/exec may take variable time on loaded systems.
        let deadline = Instant::now() + Duration::from_secs(5);
        let content = loop {
            if let Ok(c) = std::fs::read_to_string(&out) {
                break c;
            }
            assert!(
                Instant::now() < deadline,
                "hook did not produce output file within 5s (sh or printf may not be available)"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(content.contains("GROK_EVENT=Turn complete"));
        assert!(content.contains("GROK_MESSAGE=test body payload"));
        assert!(content.contains("GROK_SESSION_ID=test-session-123"));
    }
}
