use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use xai_grok_tools::util::ProcessGroup;

use crate::config::HookSpec;
use crate::event::HookEventEnvelope;
use crate::result::{HookDecision, StopHookOutcome};

use super::{
    GateHookJson, GateKind, HookRunnerResult, RunContext, StopHookJson, gate_json_to_decision,
    stop_json_to_outcome,
};

/// Maximum bytes to capture from hook stdout or stderr (64 KB).
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Exit code that a blocking hook uses to signal an explicit deny (PreToolUse)
/// or block (Stop/SubagentStop, with stderr as the feedback).
const GATE_EXIT_CODE: i32 = 2;

/// `None` when the group cannot be built, which only costs session reaping, so
/// the hook still runs.
fn hook_process_group(child: &tokio::process::Child) -> Option<Arc<ProcessGroup>> {
    let mut group = ProcessGroup::new()
        .inspect_err(
            |e| tracing::warn!(pid = child.id(), error = %e, "hook: no process group; not reaped on session close"),
        )
        .ok()?;
    group
        .attach(child)
        .inspect_err(
            |e| tracing::warn!(pid = child.id(), error = %e, "hook: process group attach failed; not reaped on session close"),
        )
        .ok()?;
    Some(Arc::new(group))
}

/// Run a single hook command.
///
/// Spawns the command as a child process, writes the envelope JSON on stdin,
/// reads stdout/stderr with buffer limits, enforces the timeout, and parses
/// the result.
pub async fn run_command_hook(
    spec: &HookSpec,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
    mode: GateKind,
) -> (HookRunnerResult, Duration) {
    let start = Instant::now();

    let Some(ref command) = spec.command else {
        return (
            HookRunnerResult::Failed("command hook has no 'command' field".into()),
            start.elapsed(),
        );
    };
    let command_str = command.to_string_lossy();

    let stdin_json = match serde_json::to_string(envelope) {
        Ok(j) => j,
        Err(e) => {
            let elapsed = start.elapsed();
            return (
                HookRunnerResult::Failed(format!("failed to serialize envelope: {e}")),
                elapsed,
            );
        }
    };

    let debug_payloads = std::env::var("GROK_HOOK_DEBUG").is_ok_and(|v| v == "1");
    if debug_payloads {
        tracing::trace!(
            hook_name = %spec.name,
            stdin_bytes = stdin_json.len(),
            "hook stdin payload"
        );
    }

    // Commands with shell metacharacters (spaces, pipes, &&, ||, redirects,
    // semicolons, env-var refs) or a leading `~` run through `sh -c` so shell
    // command strings from compatible configs work; everything else is a
    // direct executable path resolved from the hook file's directory.
    let is_shell_command = command_str.contains(' ')
        || command_str.contains('|')
        || command_str.contains('&')
        || command_str.contains(';')
        || command_str.contains('>')
        || command_str.contains('<')
        || command_str.contains('$')
        || command_str.starts_with('~');

    let mut cmd = if is_shell_command {
        // Fail fast on env vars we can't resolve (runner vars, per-hook
        // extra_env, or process env). Letting sh expand them to empty yields a
        // broken command that exits 127 with an opaque reason; surface a clear
        // error instead.
        let unresolved = find_unresolved_env_vars(&command_str, &spec.extra_env);
        if !unresolved.is_empty() {
            let elapsed = start.elapsed();
            let list = unresolved
                .iter()
                .map(|v| format!("${{{v}}}"))
                .collect::<Vec<_>>()
                .join(", ");
            return (
                HookRunnerResult::Failed(format!(
                    "hook not executed: required env var(s) not set: {list}"
                )),
                elapsed,
            );
        }
        #[cfg(unix)]
        {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(command_str.as_ref());
            c
        }
        #[cfg(not(unix))]
        {
            let inv = xai_grok_config::shell::shell_command_argv(&command_str);
            let mut c = tokio::process::Command::new(&inv.program);
            c.args(&inv.args).envs(inv.env);
            c
        }
    } else {
        let command_path = if command.is_absolute() {
            command.clone()
        } else {
            spec.source_dir.join(command)
        };
        if !command_path.exists() {
            let elapsed = start.elapsed();
            return (
                HookRunnerResult::Failed(format!("command not found: {}", command_path.display())),
                elapsed,
            );
        }
        tokio::process::Command::new(command_path)
    };

    // Detach from the controlling terminal so children (e.g. GPG pinentry)
    // can't open /dev/tty and corrupt the TUI display.
    xai_grok_tools::util::detach_command(&mut cmd);

    // Spawn the child process.
    //
    // SECURITY: env-var precedence at spawn time. `Command::envs(&map)` runs
    // AFTER any preceding `.env(...)` calls and silently overrides them, so
    // the order matters: we MUST apply user/plugin `extra_env` FIRST and
    // the runner-injected vars LAST. Otherwise a user JSON hook (or a
    // plugin) can spoof `GROK_HOOK_EVENT`, `GROK_HOOK_NAME`, `GROK_SESSION_ID`,
    // `GROK_WORKSPACE_ROOT`, or `CLAUDE_PROJECT_DIR`, which are the
    // identity/event signals a hook script consumes for policy and audit.
    // See the `runner_injected_vars_override_extra_env_at_spawn`
    // regression test in `tests/integration.rs` and the rustdoc on
    // `HookSpec::extra_env`.
    #[allow(clippy::disallowed_methods)] // enrolled in the session scope below
    let mut child = match cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .current_dir(ctx.workspace_root)
        // 1. user/plugin extra_env first (lowest precedence).
        .envs(&spec.extra_env)
        // 2. runner-injected vars last (highest precedence, always win).
        .env("GROK_HOOK_EVENT", envelope.hook_event_name.to_string())
        .env("GROK_HOOK_NAME", &spec.name)
        .env("GROK_SESSION_ID", ctx.session_id)
        .env("GROK_WORKSPACE_ROOT", ctx.workspace_root)
        // Compatibility alias for external hooks that read this env name.
        // Same value as `GROK_WORKSPACE_ROOT`; native `.grok` hooks should use
        // `GROK_WORKSPACE_ROOT`.
        .env("CLAUDE_PROJECT_DIR", ctx.workspace_root)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let elapsed = start.elapsed();
            return (
                HookRunnerResult::Failed(format!("failed to spawn command: {e}")),
                elapsed,
            );
        }
    };

    let mut hook_group = None;
    if let Some(scope) = ctx.process_scope.as_ref()
        && let Some(group) = hook_process_group(&child)
    {
        // A closed scope means the session is gone and `register` already killed
        // the child, so stop rather than write stdin to a corpse.
        if !scope.register(&group) {
            return (
                HookRunnerResult::Failed("session closed before the hook ran".to_string()),
                start.elapsed(),
            );
        }
        hook_group = Some(group);
    }

    // Write stdin concurrently with draining output, under the timeout: a hook
    // that never reads stdin would otherwise block `write_all` on a full pipe
    // buffer, outside the deadline.
    let stdin = child.stdin.take();
    let timeout = Duration::from_millis(spec.timeout_ms);
    let result = tokio::time::timeout(timeout, async move {
        let write = async {
            if let Some(mut stdin) = stdin {
                let _ = stdin.write_all(stdin_json.as_bytes()).await;
            }
        };
        let (_, output) = tokio::join!(write, child.wait_with_output());
        output
    })
    .await;

    let elapsed = start.elapsed();

    // killpg takes grandchildren that kill_on_drop would miss.
    if !matches!(result, Ok(Ok(_)))
        && let Some(group) = &hook_group
    {
        let _ = group.kill();
    }

    match result {
        Err(_) => (
            HookRunnerResult::Failed(format!("timed out after {}ms", spec.timeout_ms)),
            elapsed,
        ),
        Ok(Err(e)) => (
            HookRunnerResult::Failed(format!("command execution failed: {e}")),
            elapsed,
        ),
        Ok(Ok(output)) => {
            let exit_code = output.status.code().unwrap_or(-1);

            let stdout = truncate_output(&output.stdout);
            let stderr = truncate_output(&output.stderr);

            if !stderr.is_empty() {
                tracing::debug!(
                    hook_name = %spec.name,
                    stderr_bytes = stderr.len(),
                    "hook stderr output captured"
                );
            }

            if debug_payloads {
                tracing::trace!(
                    hook_name = %spec.name,
                    stdout_bytes = stdout.len(),
                    "hook stdout payload"
                );
            }

            tracing::debug!(
                hook_name = %spec.name,
                exit_code,
                stdout_bytes = stdout.len(),
                stderr_bytes = stderr.len(),
                elapsed_ms = elapsed.as_millis() as u64,
                "hook command completed"
            );

            match mode {
                GateKind::Observe => {
                    if exit_code == 0 {
                        return (HookRunnerResult::Success, elapsed);
                    }
                    (
                        HookRunnerResult::Failed(format!("exit code {exit_code}")),
                        elapsed,
                    )
                }
                GateKind::Tool => parse_blocking_result(&stdout, exit_code, &spec.name, elapsed),
                GateKind::Stop => {
                    parse_stop_result(&stdout, &stderr, exit_code, &spec.name, elapsed)
                }
            }
        }
    }
}

/// Env vars the runner sets unconditionally on every spawned hook.
///
/// Used by:
///
/// * [`find_unresolved_env_vars`] to avoid flagging vars that *are* set
///   by the runner itself,
/// * [`crate::config::parse_hook_file`] and the plugin adapter to strip
///   user-supplied attempts to override these keys via the JSON `env`
///   map (those attempts would be silently ignored by the spawn-time
///   precedence ordering anyway, but stripping them at load time gives
///   users a clear "ignored, reserved key" warning).
pub(crate) const RUNNER_ALWAYS_SET_ENV: &[&str] = &[
    "GROK_HOOK_EVENT",
    "GROK_HOOK_NAME",
    "GROK_SESSION_ID",
    "GROK_WORKSPACE_ROOT",
    "CLAUDE_PROJECT_DIR",
];

/// Parse `command_str` for `${VAR}` and `$VAR` references and return the
/// names that aren't resolvable from any of:
///
/// * the runner's always-set env vars (see [`RUNNER_ALWAYS_SET_ENV`]),
/// * the per-hook `extra_env` map (set by the plugin adapter for plugin
///   hooks),
/// * the Grok process's own environment (which is inherited by the child),
/// * local shell assignments inside the command itself (e.g. an
///   `INPUT=$(cat)` earlier in the string defines `INPUT` for the rest of
///   the command).
///
/// Names that appear inside a parameter-expansion form with a default,
/// fallback, or substitution modifier (`${VAR:-x}`, `${VAR-x}`, `${VAR:=x}`,
/// `${VAR:?msg}`, `${VAR:+x}`, `${VAR%pat}`, `${VAR#pat}`, `${VAR/pat/repl}`,
/// `${VAR:offset}`) are deliberately NOT flagged: the user has explicitly
/// handled the unset case in the shell expression, so the runner shouldn't
/// second-guess them.
///
/// The returned list is sorted and de-duplicated. Names are bare (no `$` or
/// `{}`).
fn find_unresolved_env_vars(
    command_str: &str,
    extra_env: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    let locally_assigned = find_local_shell_assignments(command_str);
    let mut out: Vec<String> = Vec::new();
    for r in crate::env_expand::iter_env_var_references(command_str) {
        if r.name.is_empty() || r.has_modifier {
            continue;
        }
        if RUNNER_ALWAYS_SET_ENV.contains(&r.name) {
            continue;
        }
        if extra_env.contains_key(r.name) {
            continue;
        }
        if std::env::var_os(r.name).is_some() {
            continue;
        }
        if locally_assigned.contains(r.name) {
            continue;
        }
        out.push(r.name.to_string());
    }
    out.sort();
    out.dedup();
    out
}

/// Find shell variable assignments within `command_str` so that subsequent
/// `${VAR}` references to those names aren't flagged as undefined.
///
/// Detects two patterns common in inline hook commands:
///
/// * Plain assignments at the start of a command position: `VAR=value`,
///   `VAR=$(cmd)`, `VAR="..."`. The identifier must follow either the
///   start of the string, whitespace, or a statement separator (`;`, `&`,
///   `|`, `\n`).
/// * `read VAR1 VAR2 ...` statements (very common pattern for consuming
///   stdin in hooks).
///
/// This is a deliberately small heuristic, not a full shell parser. It
/// errs on the side of treating an identifier as locally set; the
/// consequence of a false negative here is a false positive in
/// [`find_unresolved_env_vars`] (which is precisely what we're trying to
/// avoid). Callers who need to be sure can always use the parameter-
/// expansion default form (`${VAR:-}`).
fn find_local_shell_assignments(command_str: &str) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let bytes = command_str.as_bytes();
    let mut i = 0;
    let is_statement_start = |idx: usize| -> bool {
        if idx == 0 {
            return true;
        }
        let mut j = idx;
        while j > 0 {
            let c = bytes[j - 1];
            if c == b' ' || c == b'\t' {
                j -= 1;
                continue;
            }
            return matches!(c, b';' | b'&' | b'|' | b'\n' | b'(' | b'{');
        }
        true
    };
    while i < bytes.len() {
        let c = bytes[i];
        if !(c.is_ascii_alphabetic() || c == b'_') {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        let ident = std::str::from_utf8(&bytes[start..i]).unwrap_or("");
        if ident.is_empty() {
            continue;
        }
        if i < bytes.len() && bytes[i] == b'=' && is_statement_start(start) {
            names.insert(ident.to_string());
            continue;
        }
        if ident == "read" && is_statement_start(start) {
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            while i < bytes.len() {
                let c2 = bytes[i];
                if matches!(c2, b';' | b'&' | b'|' | b'\n' | b'<' | b'>') {
                    break;
                }
                if c2 == b' ' || c2 == b'\t' {
                    i += 1;
                    continue;
                }
                if c2 == b'-' {
                    // `read -r VAR` etc.: skip the option flag.
                    while i < bytes.len() && bytes[i] != b' ' && bytes[i] != b'\t' {
                        i += 1;
                    }
                    continue;
                }
                if !(c2.is_ascii_alphabetic() || c2 == b'_') {
                    break;
                }
                let s = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let read_ident = std::str::from_utf8(&bytes[s..i]).unwrap_or("");
                if !read_ident.is_empty() {
                    names.insert(read_ident.to_string());
                }
            }
        }
    }
    names
}

/// Parse the result of a blocking hook from stdout and exit code.
fn parse_blocking_result(
    stdout: &str,
    exit_code: i32,
    hook_name: &str,
    elapsed: Duration,
) -> (HookRunnerResult, Duration) {
    let json_decision = if !stdout.trim().is_empty() {
        serde_json::from_str::<GateHookJson>(stdout.trim()).ok()
    } else {
        None
    };

    if let Some(output) = json_decision {
        match gate_json_to_decision(output, hook_name) {
            Ok(HookDecision::Deny { reason, hook_name }) => {
                // A JSON deny is honored on any exit code (fail-safe).
                if exit_code != GATE_EXIT_CODE && exit_code != 0 {
                    tracing::warn!(
                        hook_name,
                        exit_code,
                        "JSON decision is 'deny' but exit code is not 0 or 2 — using JSON decision"
                    );
                }
                return (
                    HookRunnerResult::Decision(HookDecision::Deny { reason, hook_name }),
                    elapsed,
                );
            }
            Ok(HookDecision::Allow) => {
                if exit_code == GATE_EXIT_CODE {
                    // Exit 2 wins over a JSON allow (stdout is not
                    // processed on exit 2); the exit-code ladder below
                    // denies.
                    tracing::warn!(
                        hook_name,
                        "JSON decision is 'allow' but exit code is 2 — denying (stdout is ignored on exit 2)"
                    );
                } else {
                    return (HookRunnerResult::Decision(HookDecision::Allow), elapsed);
                }
            }
            // Unknown decision value: failure so typos surface.
            Err(err) => return (HookRunnerResult::Failed(err), elapsed),
        }
    }

    match exit_code {
        0 => (HookRunnerResult::Decision(HookDecision::Allow), elapsed),
        GATE_EXIT_CODE => (
            HookRunnerResult::Decision(HookDecision::Deny {
                reason: format!("denied by hook '{hook_name}' (exit code {GATE_EXIT_CODE})"),
                hook_name: hook_name.to_string(),
            }),
            elapsed,
        ),
        _ => (
            HookRunnerResult::Failed(format!(
                "hook '{hook_name}' failed with exit code {exit_code}"
            )),
            elapsed,
        ),
    }
}

/// Parse the result of a `Stop`/`SubagentStop` gate hook from stdout, stderr,
/// and exit code:
///
/// A valid decision JSON on stdout wins over the exit code. The exit code
/// decides only when stdout carries no usable JSON.
///
/// * **JSON stdout (any exit code)**: parsed as [`StopHookJson`]:
///   `decision: "block"` (+ `reason`), `continue: false` (+ `stopReason`), and
///   `hookSpecificOutput.additionalContext`.
/// * **no JSON + exit 0**: plain allow-stop.
/// * **no JSON + exit 2**: block, with stderr as the feedback fed to the model.
/// * **no JSON + any other exit code**: failure (callers fail open: the agent
///   stops normally).
fn parse_stop_result(
    stdout: &str,
    stderr: &str,
    exit_code: i32,
    hook_name: &str,
    elapsed: Duration,
) -> (HookRunnerResult, Duration) {
    let trimmed = stdout.trim();
    if !trimmed.is_empty() {
        match serde_json::from_str::<StopHookJson>(trimmed) {
            Ok(json) => {
                return match stop_json_to_outcome(json, hook_name) {
                    Ok(outcome) => (HookRunnerResult::Stop(outcome), elapsed),
                    Err(err) => (HookRunnerResult::Failed(err), elapsed),
                };
            }
            Err(err) => {
                // JSON-looking output that fails to parse is likely a broken
                // decision; warn and fall back to the exit code.
                if trimmed.starts_with('{') {
                    tracing::warn!(
                        hook_name,
                        error = %err,
                        "stop hook stdout looks like JSON but failed to parse; falling back to the exit code"
                    );
                }
            }
        }
    }
    match exit_code {
        0 => (HookRunnerResult::Stop(StopHookOutcome::default()), elapsed),
        GATE_EXIT_CODE => {
            let feedback = stderr.trim();
            let block_reason = if feedback.is_empty() {
                format!("Blocked by stop hook '{hook_name}' (exit code {GATE_EXIT_CODE})")
            } else {
                feedback.to_string()
            };
            (
                HookRunnerResult::Stop(StopHookOutcome {
                    block_reason: Some(block_reason),
                    ..Default::default()
                }),
                elapsed,
            )
        }
        _ => (
            HookRunnerResult::Failed(format!(
                "hook '{hook_name}' failed with exit code {exit_code}"
            )),
            elapsed,
        ),
    }
}

/// Truncate output bytes to MAX_OUTPUT_BYTES and convert to a lossy UTF-8 string.
fn truncate_output(bytes: &[u8]) -> String {
    if bytes.len() <= MAX_OUTPUT_BYTES {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        let mut truncated = String::from_utf8_lossy(&bytes[..MAX_OUTPUT_BYTES]).into_owned();
        truncated.push_str(" [truncated]");
        tracing::warn!(
            total_bytes = bytes.len(),
            max_bytes = MAX_OUTPUT_BYTES,
            "hook output truncated"
        );
        truncated
    }
}

/// Resolve the absolute command path for a hook spec.
///
/// Returns `None` for non-command handler types.
pub fn resolve_command_path(spec: &HookSpec) -> Option<std::path::PathBuf> {
    let command = spec.command.as_ref()?;
    if command.is_absolute() {
        Some(command.clone())
    } else {
        Some(spec.source_dir.join(command))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_decision() {
        let (allow, _) =
            parse_blocking_result(r#"{"decision":"allow"}"#, 0, "test", Duration::ZERO);
        assert!(matches!(
            allow,
            HookRunnerResult::Decision(HookDecision::Allow)
        ));

        let (deny, _) = parse_blocking_result(
            r#"{"decision":"deny","reason":"bad command"}"#,
            2,
            "test",
            Duration::ZERO,
        );
        match deny {
            HookRunnerResult::Decision(HookDecision::Deny { reason, .. }) => {
                assert_eq!(reason, "bad command");
            }
            other => panic!("expected Deny, got {other:?}"),
        }

        let (deny_no_reason, _) =
            parse_blocking_result(r#"{"decision":"deny"}"#, 2, "my-hook", Duration::ZERO);
        match deny_no_reason {
            HookRunnerResult::Decision(HookDecision::Deny { reason, .. }) => {
                assert!(reason.contains("my-hook"));
            }
            other => panic!("expected Deny, got {other:?}"),
        }

        let (unknown, _) =
            parse_blocking_result(r#"{"decision":"maybe"}"#, 0, "test", Duration::ZERO);
        assert!(matches!(unknown, HookRunnerResult::Failed(_)));
    }

    #[test]
    fn fallback_to_exit_code() {
        for (stdout, code, expect_allow) in
            [("", 0, true), ("not json at all", 0, true), ("", 2, false)]
        {
            let (result, _) = parse_blocking_result(stdout, code, "test", Duration::ZERO);
            if expect_allow {
                assert!(matches!(
                    result,
                    HookRunnerResult::Decision(HookDecision::Allow)
                ));
            } else {
                assert!(matches!(
                    result,
                    HookRunnerResult::Decision(HookDecision::Deny { .. })
                ));
            }
        }
        let (fail, _) = parse_blocking_result("", 1, "test", Duration::ZERO);
        assert!(matches!(fail, HookRunnerResult::Failed(_)));
    }

    #[test]
    fn json_decision_vs_exit_code() {
        let (deny, _) = parse_blocking_result(
            r#"{"decision":"deny","reason":"nope"}"#,
            0,
            "test",
            Duration::ZERO,
        );
        assert!(matches!(
            deny,
            HookRunnerResult::Decision(HookDecision::Deny { .. })
        ));

        let (blocked, _) =
            parse_blocking_result(r#"{"decision":"allow"}"#, 2, "test", Duration::ZERO);
        assert!(matches!(
            blocked,
            HookRunnerResult::Decision(HookDecision::Deny { .. })
        ));
    }

    fn stop_outcome(result: HookRunnerResult) -> StopHookOutcome {
        match result {
            HookRunnerResult::Stop(outcome) => outcome,
            other => panic!("expected Stop outcome, got {other:?}"),
        }
    }

    #[test]
    fn stop_block_decision_with_reason() {
        let (result, _) = parse_stop_result(
            r#"{"decision":"block","reason":"tests are failing"}"#,
            "",
            0,
            "my-stop",
            Duration::ZERO,
        );
        let outcome = stop_outcome(result);
        assert_eq!(
            outcome,
            StopHookOutcome {
                block_reason: Some("tests are failing".into()),
                ..Default::default()
            }
        );

        let (result, _) =
            parse_stop_result(r#"{"decision":"block"}"#, "", 0, "my-stop", Duration::ZERO);
        assert_eq!(
            stop_outcome(result).block_reason.as_deref(),
            Some("Blocked by stop hook 'my-stop'")
        );
    }

    #[test]
    fn stop_exit_2_blocks_with_stderr() {
        let (result, _) =
            parse_stop_result("", "run the test suite first\n", 2, "s", Duration::ZERO);
        assert_eq!(
            stop_outcome(result).block_reason.as_deref(),
            Some("run the test suite first")
        );

        let (result, _) = parse_stop_result("", "", 2, "s", Duration::ZERO);
        assert_eq!(
            stop_outcome(result).block_reason.as_deref(),
            Some("Blocked by stop hook 's' (exit code 2)")
        );
    }

    #[test]
    fn stop_stdout_json_wins_over_exit_2() {
        let (result, _) = parse_stop_result(
            r#"{"continue":false,"stopReason":"enough","hookSpecificOutput":{"additionalContext":"ctx"}}"#,
            "log noise\n",
            2,
            "s",
            Duration::ZERO,
        );
        let outcome = stop_outcome(result);
        assert_eq!(
            outcome
                .force_stop
                .as_ref()
                .and_then(|f| f.reason.as_deref()),
            Some("enough")
        );
        assert_eq!(outcome.additional_context.as_deref(), Some("ctx"));

        let (result, _) = parse_stop_result("log noise\n", "blocked", 2, "s", Duration::ZERO);
        assert_eq!(
            stop_outcome(result).block_reason.as_deref(),
            Some("blocked")
        );
    }

    #[test]
    fn stop_continue_false_prevents_continuation() {
        let (result, _) = parse_stop_result(
            r#"{"continue":false,"stopReason":"budget exhausted"}"#,
            "",
            0,
            "s",
            Duration::ZERO,
        );
        let outcome = stop_outcome(result);
        assert_eq!(
            outcome,
            StopHookOutcome {
                force_stop: Some(crate::result::StopOverride {
                    reason: Some("budget exhausted".into()),
                }),
                ..Default::default()
            }
        );
        let (result, _) = parse_stop_result(r#"{"continue":true}"#, "", 0, "s", Duration::ZERO);
        assert!(stop_outcome(result).is_empty());
    }

    #[test]
    fn stop_additional_context_captured() {
        let (result, _) = parse_stop_result(
            r#"{"hookSpecificOutput":{"hookEventName":"Stop","additionalContext":"run the test suite before finishing"}}"#,
            "",
            0,
            "s",
            Duration::ZERO,
        );
        let outcome = stop_outcome(result);
        assert_eq!(
            outcome,
            StopHookOutcome {
                additional_context: Some("run the test suite before finishing".into()),
                ..Default::default()
            }
        );
    }

    #[test]
    fn stop_allow_failure_and_unknown_decision() {
        let (result, _) = parse_stop_result("", "", 0, "s", Duration::ZERO);
        assert!(stop_outcome(result).is_empty());

        let (result, _) = parse_stop_result("all done!", "", 0, "s", Duration::ZERO);
        assert!(stop_outcome(result).is_empty());

        let (result, _) = parse_stop_result("", "boom", 1, "s", Duration::ZERO);
        assert!(matches!(result, HookRunnerResult::Failed(_)));

        let (result, _) = parse_stop_result(r#"{"decision":"deny"}"#, "", 0, "s", Duration::ZERO);
        assert!(matches!(result, HookRunnerResult::Failed(_)));

        // `approve` is accepted as a no-op (shared approve/block vocabulary).
        let (result, _) =
            parse_stop_result(r#"{"decision":"approve"}"#, "", 0, "s", Duration::ZERO);
        assert!(stop_outcome(result).is_empty());
    }

    #[test]
    fn stop_output_captures_all_combined_signals() {
        let (result, _) = parse_stop_result(
            r#"{"decision":"block","reason":"keep going","continue":false,"stopReason":"user said stop","hookSpecificOutput":{"additionalContext":"ctx"}}"#,
            "",
            0,
            "s",
            Duration::ZERO,
        );
        let outcome = stop_outcome(result);
        assert_eq!(
            outcome,
            StopHookOutcome {
                block_reason: Some("keep going".into()),
                additional_context: Some("ctx".into()),
                force_stop: Some(crate::result::StopOverride {
                    reason: Some("user said stop".into()),
                }),
            }
        );
    }

    #[test]
    fn truncate_output_respects_limit() {
        assert_eq!(truncate_output(b"hello world"), "hello world");

        let large = truncate_output(&vec![b'x'; MAX_OUTPUT_BYTES + 1000]);
        assert!(large.ends_with(" [truncated]"));
    }

    #[test]
    fn resolve_command_path_variants() {
        let spec =
            |handler: crate::config::HandlerType, command: Option<&str>, source: &str| HookSpec {
                name: "test".into(),
                event: crate::event::HookEventName::PreToolUse,
                handler_type: handler,
                configured_matcher: None,
                matcher: None,
                enabled: true,
                command: command.map(std::path::PathBuf::from),
                command_raw: command.map(str::to_string),
                url: None,
                url_raw: None,
                timeout_ms: 5000,
                source_dir: std::path::PathBuf::from(source),
                extra_env: std::collections::HashMap::new(),
                layer: crate::config::HookProvenance::File,
            };
        use crate::config::HandlerType;
        assert_eq!(
            resolve_command_path(&spec(
                HandlerType::Command,
                Some("/usr/bin/hook"),
                "/some/dir"
            )),
            Some(std::path::PathBuf::from("/usr/bin/hook"))
        );
        assert_eq!(
            resolve_command_path(&spec(
                HandlerType::Command,
                Some("bin/check.sh"),
                "/project/.grok/hooks"
            )),
            Some(std::path::PathBuf::from(
                "/project/.grok/hooks/bin/check.sh"
            ))
        );
        assert_eq!(
            resolve_command_path(&spec(HandlerType::Http, None, "/project")),
            None
        );
    }

    /// Helper to build a HookSpec that runs a shell command.
    fn make_shell_spec(command: &str) -> HookSpec {
        HookSpec {
            name: "test-hook".into(),
            event: crate::event::HookEventName::Stop,
            handler_type: crate::config::HandlerType::Command,
            configured_matcher: None,
            matcher: None,
            enabled: true,
            command: Some(command.into()),
            command_raw: Some(command.to_string()),
            url: None,
            url_raw: None,
            timeout_ms: 5000,
            source_dir: std::env::temp_dir(),
            extra_env: std::collections::HashMap::new(),
            layer: crate::config::HookProvenance::File,
        }
    }

    fn make_envelope() -> HookEventEnvelope {
        use crate::event::HookPayload;
        HookEventEnvelope {
            hook_event_name: crate::event::HookEventName::Stop,
            session_id: "test-session".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "2026-01-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            permission_mode: None,
            payload: HookPayload::Stop {
                reason: "test".into(),
                stop_hook_active: false,
                last_assistant_message: None,
                background_tasks: None,
                session_crons: None,
            },
        }
    }

    fn make_ctx() -> RunContext<'static> {
        RunContext {
            session_id: "test-session",
            workspace_root: "/tmp",
            process_scope: None,
        }
    }

    fn make_scoped_ctx(scope: xai_grok_tools::util::ProcessScope) -> RunContext<'static> {
        RunContext {
            process_scope: Some(scope),
            ..make_ctx()
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn hook_times_out() {
        let mut spec = make_shell_spec("sleep 5");
        spec.timeout_ms = 100;
        let envelope = make_envelope();
        let ctx = make_ctx();
        let (result, _) = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe).await;
        assert!(
            matches!(&result, HookRunnerResult::Failed(msg) if msg.contains("timed out")),
            "expected a timeout failure, got {result:?}"
        );
    }

    /// Regression: a hook that never reads stdin while writing large stdout must
    /// not deadlock, since stdin is written concurrently with draining output.
    #[tokio::test]
    #[cfg(unix)]
    async fn large_envelope_with_unreading_hook_does_not_deadlock() {
        use crate::event::HookPayload;
        let spec = make_shell_spec("head -c 200000 /dev/zero | tr '\\0' x");
        let mut envelope = make_envelope();
        envelope.payload = HookPayload::Stop {
            reason: "test".into(),
            stop_hook_active: false,
            // Larger than the OS pipe buffer (~64 KB) so the stdin write blocks
            // without concurrent draining.
            last_assistant_message: Some("x".repeat(256 * 1024)),
            background_tasks: None,
            session_crons: None,
        };
        let ctx = make_ctx();
        let run = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe);
        let (result, _) = tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("hook must not deadlock on a large envelope");
        assert!(matches!(result, HookRunnerResult::Success));
    }

    /// Verify that setsid() prevents hook child processes from opening
    /// `/dev/tty`. This is the core fix for GPG pinentry corruption.
    ///
    /// The hook tries `exec 3>/dev/tty` — if detached, this fails and the
    /// shell exits 1 (caught by `||`), making the overall command exit 0.
    /// If NOT detached, the open succeeds and the command exits 1.
    #[tokio::test]
    #[cfg(unix)]
    async fn test_hook_child_cannot_open_dev_tty() {
        // Skip in CI / environments without a controlling terminal —
        // setsid() gets EPERM when already a session leader and the
        // setpgid fallback doesn't detach /dev/tty.
        if std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/tty")
            .is_err()
        {
            eprintln!("skipping: no controlling terminal");
            return;
        }

        // exit 0 if /dev/tty is inaccessible (DETACHED), exit 1 if accessible
        let spec = make_shell_spec("exec 3>/dev/tty 2>/dev/null && exit 1 || exit 0");
        let envelope = make_envelope();
        let ctx = make_ctx();

        let (result, _duration) = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe).await;

        assert!(
            matches!(result, HookRunnerResult::Success),
            "hook child should not be able to open /dev/tty after setsid(), got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_hook_blocking_allow() {
        let spec = make_shell_spec(r#"echo '{"decision":"allow"}'"#);
        let envelope = make_envelope();
        let ctx = make_ctx();

        let (result, _duration) = run_command_hook(&spec, &envelope, &ctx, GateKind::Tool).await;

        assert!(
            matches!(result, HookRunnerResult::Decision(HookDecision::Allow)),
            "blocking hook should return Allow, got {:?}",
            result
        );
    }

    /// Regression: a hook command that uses `${VAR}` interpolation
    /// without any other shell metacharacters must still be invoked via
    /// `sh -c` so that the env var supplied via `extra_env` is expanded.
    /// Previously the runner treated `${...}` as part of a literal path
    /// and `command_path.exists()` failed; the hook silently never ran.
    /// Now the env-var pre-spawn check refuses with a clear reason when
    /// the var is unset (and the dispatcher fail-opens, so the tool call
    /// itself is not blocked).
    #[tokio::test]
    async fn test_env_var_interpolation_runs_via_shell() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("hook.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }

        let mut extra_env = std::collections::HashMap::new();
        extra_env.insert(
            "GB1183_PLUGIN_ROOT".to_string(),
            tmp.path().to_string_lossy().into_owned(),
        );

        let spec = HookSpec {
            name: "test-env-interp".into(),
            event: crate::event::HookEventName::Stop,
            handler_type: crate::config::HandlerType::Command,
            configured_matcher: None,
            matcher: None,
            enabled: true,
            command: Some(std::path::PathBuf::from("${GB1183_PLUGIN_ROOT}/hook.sh")),
            command_raw: Some("${GB1183_PLUGIN_ROOT}/hook.sh".to_string()),
            url: None,
            url_raw: None,
            timeout_ms: 5000,
            source_dir: tmp.path().to_path_buf(),
            extra_env,
            layer: crate::config::HookProvenance::File,
        };

        let envelope = make_envelope();
        let ctx = make_ctx();
        let (result, _) = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe).await;

        assert!(
            matches!(result, HookRunnerResult::Success),
            "hook with ${{VAR}} interpolation should be expanded via sh -c, got {:?}",
            result
        );
    }

    /// `CLAUDE_PROJECT_DIR` is part of the external hook contract: it points
    /// to the workspace/project root and is set for ALL hooks (not just
    /// plugin-scoped ones). Plugin hooks frequently reference it as
    /// `"$CLAUDE_PROJECT_DIR/.claude/hooks/foo.sh"`. The runner must export
    /// it on the spawned child so shell expansion via the `sh -c` branch
    /// resolves correctly; otherwise such hooks fail to find the
    /// command.
    #[tokio::test]
    async fn test_claude_project_dir_is_exported() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("hook.sh");
        // Exit 0 only if CLAUDE_PROJECT_DIR matches the workspace root.
        let workspace = tmp.path().to_string_lossy().into_owned();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ntest \"${{CLAUDE_PROJECT_DIR}}\" = \"{workspace}\"\n",
                workspace = workspace
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }

        let spec = HookSpec {
            name: "test-claude-project-dir".into(),
            event: crate::event::HookEventName::Stop,
            handler_type: crate::config::HandlerType::Command,
            configured_matcher: None,
            matcher: None,
            enabled: true,
            // Use ${CLAUDE_PROJECT_DIR} in the path itself so this also exercises
            // the `$` -> sh -c routing.
            command: Some(std::path::PathBuf::from("${CLAUDE_PROJECT_DIR}/hook.sh")),
            command_raw: Some("${CLAUDE_PROJECT_DIR}/hook.sh".to_string()),
            url: None,
            url_raw: None,
            timeout_ms: 5000,
            source_dir: tmp.path().to_path_buf(),
            extra_env: std::collections::HashMap::new(),
            layer: crate::config::HookProvenance::File,
        };

        let envelope = make_envelope();
        let ctx = RunContext {
            session_id: "test-session",
            workspace_root: &workspace,
            process_scope: None,
        };
        let (result, _) = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe).await;

        assert!(
            matches!(result, HookRunnerResult::Success),
            "hook should see CLAUDE_PROJECT_DIR set to the workspace root, got {:?}",
            result
        );
    }

    /// `extra_env` seeds what's "set" so the test does not depend on the
    /// process environment.
    #[test]
    fn find_unresolved_detects_and_dedups() {
        let mut env = std::collections::HashMap::new();
        env.insert("KNOWN".to_string(), "x".to_string());
        assert_eq!(
            find_unresolved_env_vars("${KNOWN}/${SOME_GB1183_UNSET_VAR}/foo", &env),
            vec!["SOME_GB1183_UNSET_VAR".to_string()]
        );
        assert_eq!(
            find_unresolved_env_vars("$SOME_GB1183_BARE_UNSET/foo", &env),
            vec!["SOME_GB1183_BARE_UNSET".to_string()]
        );
        assert_eq!(
            find_unresolved_env_vars(
                "${MISSING_GB1183_DUP} && ${MISSING_GB1183_DUP}/foo $MISSING_GB1183_DUP",
                &env,
            ),
            vec!["MISSING_GB1183_DUP".to_string()]
        );
    }

    #[test]
    fn find_unresolved_skips_resolvable_vars() {
        let mut env = std::collections::HashMap::new();
        env.insert("CLAUDE_PLUGIN_ROOT".to_string(), "/plugins/foo".to_string());
        let v = find_unresolved_env_vars(
            "${GROK_HOOK_EVENT}/${CLAUDE_PROJECT_DIR}/${GROK_SESSION_ID}/${CLAUDE_PLUGIN_ROOT}/foo",
            &env,
        );
        assert!(
            v.is_empty(),
            "resolvable vars should not be flagged, got {v:?}"
        );
    }

    #[test]
    fn find_unresolved_skips_non_var_dollars() {
        let env = std::collections::HashMap::new();
        // $1 (positional), $$ (pid), $(...) (cmd subst), $? (exit code), $#.
        let v = find_unresolved_env_vars("echo $1 $$ $? $# $(date)", &env);
        assert!(
            v.is_empty(),
            "shell special params should not be flagged, got {v:?}"
        );
    }

    #[test]
    fn find_unresolved_skips_local_assignments() {
        let env = std::collections::HashMap::new();
        for cmd in [
            r#"INPUT=$(cat); echo "$INPUT" | grep -q foo"#,
            "read -r LINE; echo $LINE",
            "echo first; X=hello && echo $X | cat",
        ] {
            let v = find_unresolved_env_vars(cmd, &env);
            assert!(v.is_empty(), "`{cmd}` should not flag any var, got {v:?}");
        }
    }

    #[test]
    fn find_unresolved_skips_parameter_expansion_modifiers() {
        let env = std::collections::HashMap::new();
        // All of these explicitly handle the unset case; the runner must
        // not flag them, otherwise we reject hooks that the user wrote
        // correctly.
        let cases = [
            "${MISSING_GB1183_MOD:-/default/path.sh}",
            "${MISSING_GB1183_MOD-/default/path.sh}",
            "${MISSING_GB1183_MOD:=/assigned/path.sh}",
            "${MISSING_GB1183_MOD:?msg here}",
            "${MISSING_GB1183_MOD:+/used/if/set.sh}",
            "${MISSING_GB1183_MOD%.sh}",
            "${MISSING_GB1183_MOD#prefix/}",
            "${MISSING_GB1183_MOD/foo/bar}",
            "${MISSING_GB1183_MOD:0:5}",
        ];
        for case in cases {
            let v = find_unresolved_env_vars(case, &env);
            assert!(
                v.is_empty(),
                "parameter-expansion form `{case}` should not be flagged, got {v:?}"
            );
        }
    }

    /// Regression follow-up: when a hook command references
    /// an env var that isn't set anywhere we know about, the runner must
    /// refuse to spawn entirely (no fork+exec, no opaque "exit code 127")
    /// and surface a clear failure reason naming the missing var(s).
    #[tokio::test]
    async fn test_undefined_env_var_refuses_to_spawn() {
        let mut extra_env = std::collections::HashMap::new();
        // Intentionally do NOT set NEVER_SET_GB1183 anywhere.
        extra_env.insert("UNRELATED_GB1183".to_string(), "/tmp".to_string());

        let spec = HookSpec {
            name: "test-undef".into(),
            event: crate::event::HookEventName::Stop,
            handler_type: crate::config::HandlerType::Command,
            configured_matcher: None,
            matcher: None,
            enabled: true,
            command: Some(std::path::PathBuf::from(
                "${NEVER_SET_GB1183}/does/not/exist.sh",
            )),
            command_raw: Some("${NEVER_SET_GB1183}/does/not/exist.sh".to_string()),
            url: None,
            url_raw: None,
            timeout_ms: 5000,
            source_dir: std::env::temp_dir(),
            extra_env,
            layer: crate::config::HookProvenance::File,
        };

        let envelope = make_envelope();
        let ctx = make_ctx();
        let (result, _) = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe).await;

        match result {
            HookRunnerResult::Failed(reason) => {
                assert!(
                    reason.contains("NEVER_SET_GB1183"),
                    "failure reason should name the undefined env var, got: {reason}"
                );
                assert!(
                    reason.contains("hook not executed"),
                    "failure reason should make clear the hook did not run, got: {reason}"
                );
                assert!(
                    !reason.contains("exit code"),
                    "failure reason should not reference an exit code (we never spawned), got: {reason}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// Regression: a hook command starting with `~` must be
    /// routed through `sh -c` so the shell expands `~` to `$HOME`.
    /// Previously `~/.claude/hook.sh` was treated as a relative path and
    /// joined to `source_dir`, producing a broken path.
    ///
    /// The test injects `HOME` via `extra_env` so it works in sandboxed
    /// CI environments where `HOME` is not set (e.g. hermetic remote exec).
    #[tokio::test]
    #[cfg(unix)]
    async fn test_tilde_expansion_runs_via_shell() {
        let tmp = tempfile::tempdir().unwrap();
        // Create the script at <tmp>/.grok-test-hooks-gb856/tilde-test.sh
        let hook_dir = tmp.path().join(".grok-test-hooks-gb856");
        std::fs::create_dir_all(&hook_dir).unwrap();
        let script = hook_dir.join("tilde-test.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }

        // Inject HOME via extra_env so `sh -c "~/.grok-test-hooks-gb856/..."`
        // expands `~` to the temp dir. This avoids depending on the system
        // HOME, which is absent in hermetic sandboxed test runners.
        let mut extra_env = std::collections::HashMap::new();
        extra_env.insert(
            "HOME".to_string(),
            tmp.path().to_string_lossy().into_owned(),
        );

        let spec = HookSpec {
            name: "test-tilde".into(),
            event: crate::event::HookEventName::Stop,
            handler_type: crate::config::HandlerType::Command,
            configured_matcher: None,
            matcher: None,
            enabled: true,
            command: Some(std::path::PathBuf::from(
                "~/.grok-test-hooks-gb856/tilde-test.sh",
            )),
            command_raw: Some("~/.grok-test-hooks-gb856/tilde-test.sh".to_string()),
            url: None,
            url_raw: None,
            timeout_ms: 5000,
            source_dir: std::env::temp_dir(),
            extra_env,
            layer: crate::config::HookProvenance::File,
        };

        let envelope = make_envelope();
        let ctx = make_ctx();

        // Freshly writing the script and exec'ing it via `sh -c` can transiently
        // fail with ETXTBSY ("Text file busy" -> exit 126) when a sibling test in
        // this multi-threaded binary forks while our write fd is still open and
        // its child inherits it. Retry ONLY that exact transient; a real tilde-
        // routing break surfaces as a different result (127/spawn error), so the
        // assertion below keeps its diagnostic power.
        let mut result = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe)
            .await
            .0;
        for _ in 0..8 {
            if !matches!(&result, HookRunnerResult::Failed(msg) if msg == "exit code 126") {
                break;
            }
            result = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe)
                .await
                .0;
        }

        assert!(
            matches!(result, HookRunnerResult::Success),
            "hook with ~/... path should be expanded via sh -c, got {:?}",
            result
        );
    }

    /// Hooks that explicitly handle the unset case via parameter expansion
    /// (e.g. `${VAR:-/some/default}`) must NOT be refused: the user has
    /// expressed intent for what should happen when the var is unset.
    #[tokio::test]
    async fn test_parameter_expansion_default_is_not_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("default.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }

        let spec = HookSpec {
            name: "test-default".into(),
            event: crate::event::HookEventName::Stop,
            handler_type: crate::config::HandlerType::Command,
            configured_matcher: None,
            matcher: None,
            enabled: true,
            // `MISSING_GB1183_DEFAULT` is intentionally unset; the `:-`
            // modifier supplies a fallback that points at the real script.
            command: Some(std::path::PathBuf::from(format!(
                "${{MISSING_GB1183_DEFAULT:-{}}}",
                script.display()
            ))),
            command_raw: Some(format!("${{MISSING_GB1183_DEFAULT:-{}}}", script.display())),
            url: None,
            url_raw: None,
            timeout_ms: 5000,
            source_dir: tmp.path().to_path_buf(),
            extra_env: std::collections::HashMap::new(),
            layer: crate::config::HookProvenance::File,
        };

        let envelope = make_envelope();
        let ctx = make_ctx();
        let (result, _) = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe).await;

        assert!(
            matches!(result, HookRunnerResult::Success),
            "hook with parameter-expansion default must run, got {:?}",
            result
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_hook_session_close_reaps_whole_group() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("grandchild_alive");
        // `& wait` keeps the leader alive while the grandchild outlives it, so
        // only a group kill stops the marker being written.
        let mut spec = make_shell_spec(&format!(
            "sh -c 'sleep 2 && echo alive > {}' & wait",
            marker.display()
        ));
        spec.timeout_ms = 60_000;
        let envelope = make_envelope();
        let scope = xai_grok_tools::util::ProcessScope::new();
        let hook_scope = scope.clone();
        let hook = tokio::spawn(async move {
            run_command_hook(
                &spec,
                &envelope,
                &make_scoped_ctx(hook_scope),
                GateKind::Observe,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(800)).await;
        scope.kill_all();

        tokio::time::timeout(Duration::from_secs(15), hook)
            .await
            .expect("kill_all must reap the enrolled hook, not leave it on its 60s timeout")
            .expect("hook task join");

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !marker.exists(),
            "grandchild outlived session close, so the group was not killpg'd"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_hook_fails_fast_when_scope_already_closed() {
        let scope = xai_grok_tools::util::ProcessScope::new();
        scope.kill_all();
        let mut spec = make_shell_spec("sleep 600");
        spec.timeout_ms = 60_000;

        let (result, _) = tokio::time::timeout(
            Duration::from_secs(15),
            run_command_hook(
                &spec,
                &make_envelope(),
                &make_scoped_ctx(scope),
                GateKind::Observe,
            ),
        )
        .await
        .expect("a closed scope must fail the hook immediately, not run to its 60s timeout");

        assert!(
            matches!(result, HookRunnerResult::Failed(_)),
            "got {result:?}"
        );
    }
}
