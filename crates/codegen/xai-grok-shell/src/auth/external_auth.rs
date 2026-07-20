use crate::auth::token_output::parse_token_output;
use crate::auth::{AuthMode, GrokAuth};

/// Parse stdout into a session-credential `GrokAuth`.
pub(crate) fn parse_output(output: &std::process::Output) -> anyhow::Result<GrokAuth> {
    let parsed = parse_token_output(output)?;
    Ok(GrokAuth {
        key: parsed.access_token,
        auth_mode: AuthMode::External,
        create_time: chrono::Utc::now(),
        user_id: String::new(),
        email: None,
        first_name: None,
        last_name: None,
        profile_image_asset_id: None,
        principal_type: None,
        principal_id: None,
        team_id: None,
        team_name: None,
        team_role: None,
        organization_id: None,
        organization_name: None,
        organization_role: None,
        user_blocked_reason: None,
        team_blocked_reasons: vec![],
        coding_data_retention_opt_out: crate::auth::default_coding_data_retention_opt_out(),
        has_grok_code_access: None,
        refresh_token: parsed.refresh_token,
        expires_at: parsed.expires_at,
        oidc_issuer: parsed.issuer,
        oidc_client_id: None,
    })
}

/// Sync version for mid-session refresh. 5s timeout for refresh, 60s for initial.
pub(crate) fn run_external_auth_sync(command: &str, is_refresh: bool) -> Option<GrokAuth> {
    let timeout_secs = if is_refresh { 5 } else { 60 };
    run_auth_command(command, timeout_secs, is_refresh)
}

/// Runs `command` via `sh -c`; `mark_expired` sets `GROK_AUTH_EXPIRED=1` so the
/// helper can distinguish re-mints from first runs.
fn run_auth_command(command: &str, timeout_secs: u64, mark_expired: bool) -> Option<GrokAuth> {
    use std::process::{Command, Stdio};

    tracing::info!(cmd = %command, mark_expired, timeout_secs, "auth: running external auth provider (sync)");

    let mut cmd = Command::new("sh");
    cmd.args(["-c", command])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Pipe stderr — inherit would corrupt the TUI alternate screen.
        .stderr(Stdio::piped());
    if mark_expired {
        cmd.env("GROK_AUTH_EXPIRED", "1");
    }
    xai_grok_tools::util::detach_std_command(&mut cmd);
    cmd.envs(xai_grok_tools::util::pager_env());
    let mut child = cmd.spawn()
        .map_err(|e| {
            tracing::warn!(error = %e, cmd = %command, "auth: failed to start external auth provider");
            e
        })
        .ok()?;

    let timeout = std::time::Duration::from_secs(timeout_secs);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    tracing::warn!(
                        cmd = %command,
                        timeout_secs,
                        "auth: external auth provider timed out (likely needs interactive auth), killing"
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                tracing::warn!(error = %e, "auth: error waiting for external auth provider");
                return None;
            }
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| {
            tracing::warn!(error = %e, "auth: failed to read external auth provider output");
            e
        })
        .ok()?;

    match parse_output(&output) {
        Ok(auth) => {
            tracing::info!("auth: external auth provider returned fresh token");
            Some(auth)
        }
        Err(e) => {
            tracing::warn!(error = %e, "auth: external auth provider failed");
            None
        }
    }
}

/// Run external auth provider, carrying forward `/user`-derived fields from previous auth.
pub(crate) fn refresh_with_command(command: &str, prev_auth: &GrokAuth) -> Option<GrokAuth> {
    let mut auth = run_external_auth_sync(command, true)?;
    auth.carry_user_profile_from(prev_auth);
    Some(auth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_output_nonzero_exit_is_err() {
        let output = std::process::Output {
            status: std::process::Command::new("false").status().unwrap(),
            stdout: b"token".to_vec(),
            stderr: vec![],
        };
        assert!(parse_output(&output).is_err());
    }

    #[test]
    fn parse_output_empty_stdout_is_err() {
        let output = std::process::Output {
            status: std::process::Command::new("true").status().unwrap(),
            stdout: b"  \n".to_vec(),
            stderr: vec![],
        };
        assert!(parse_output(&output).is_err());
    }

    #[test]
    fn parse_output_issuer_claim_enables_xai_auth() {
        let ok = |stdout: &str| std::process::Output {
            status: std::process::Command::new("true").status().unwrap(),
            stdout: stdout.as_bytes().to_vec(),
            stderr: vec![],
        };

        // x.ai issuer claim → first-party session (relay-eligible).
        let auth = parse_output(&ok(
            r#"{"access_token":"t","expires_in":900,"issuer":"https://auth.x.ai"}"#,
        ))
        .unwrap();
        assert_eq!(auth.oidc_issuer.as_deref(), Some("https://auth.x.ai"));
        assert!(auth.is_xai_auth());

        // Non-x.ai issuer is stored but stays third-party.
        let auth = parse_output(&ok(
            r#"{"access_token":"t","issuer":"https://idp.acme.example"}"#,
        ))
        .unwrap();
        assert_eq!(
            auth.oidc_issuer.as_deref(),
            Some("https://idp.acme.example")
        );
        assert!(!auth.is_xai_auth());

        // Missing / empty / whitespace issuer → None.
        let auth = parse_output(&ok(r#"{"access_token":"t"}"#)).unwrap();
        assert_eq!(auth.oidc_issuer, None);
        assert!(!auth.is_xai_auth());
        let auth = parse_output(&ok(r#"{"access_token":"t","issuer":"  "}"#)).unwrap();
        assert_eq!(auth.oidc_issuer, None);

        // Bare-token output never carries an issuer.
        let auth = parse_output(&ok("bare-token")).unwrap();
        assert_eq!(auth.oidc_issuer, None);
        assert!(!auth.is_xai_auth());
    }

    #[test]
    fn parse_output_json_shaped_but_invalid_is_err() {
        let output = std::process::Output {
            status: std::process::Command::new("true").status().unwrap(),
            stdout: b"{not valid json}".to_vec(),
            stderr: vec![],
        };
        assert!(parse_output(&output).is_err());
    }

    #[test]
    fn sync_spawn_failure_returns_none() {
        assert!(run_external_auth_sync("/nonexistent/binary", false).is_none());
    }

    #[test]
    fn sync_sets_grok_auth_expired_env_on_refresh() {
        let auth = run_external_auth_sync("echo $GROK_AUTH_EXPIRED", true).unwrap();
        assert_eq!(auth.key, "1");
    }

    #[test]
    fn refresh_carries_zdr_flags_forward() {
        let prev = GrokAuth {
            user_blocked_reason: Some("BLOCKED_REASON_OTHER".into()),
            team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
            coding_data_retention_opt_out: true,
            organization_id: Some("org-1".into()),
            ..GrokAuth::test_default()
        };
        let auth = refresh_with_command("echo fresh-token", &prev).unwrap();
        assert_eq!(auth.key, "fresh-token");
        assert!(auth.is_zdr_team(), "ZDR flag must survive refresh");
        assert!(auth.coding_data_retention_opt_out);
        assert_eq!(
            auth.user_blocked_reason.as_deref(),
            Some("BLOCKED_REASON_OTHER")
        );
        assert_eq!(auth.user_id, "test-user", "profile must survive refresh");
        assert_eq!(auth.organization_id.as_deref(), Some("org-1"));
    }

    #[test]
    fn sync_refresh_interactive_times_out() {
        // Binary writes link to stderr then blocks — 5s refresh timeout kills it.
        let cmd = r#"echo 'Visit http://example.com/auth' >&2; sleep 20; echo token"#;
        let start = std::time::Instant::now();
        let result = run_external_auth_sync(cmd, true);
        let elapsed = start.elapsed();
        assert!(result.is_none(), "should timeout and return None");
        assert!(
            elapsed.as_secs() < 10,
            "refresh should use 5s timeout, not 60s (took {}s)",
            elapsed.as_secs()
        );
    }
}
