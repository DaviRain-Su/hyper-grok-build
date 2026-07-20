//! Kimi Code device-code login (`grok login --kimi`).
//!
//! Two-phase flow matching kimi-cli / Kigi:
//! 1. request device authorization
//! 2. poll until approved, then persist under [`crate::auth::model::KIMI_CODE_OAUTH_SCOPE`]

use super::oauth::{
    DeviceAuthorization, DevicePollResult, poll_device_token, request_device_authorization,
};
use crate::auth::model::GrokAuth;
use crate::auth::storage::{read_kimi_code_auth, store_kimi_code_auth};

const SLOW_DOWN_INCREMENT_SECS: u64 = 5;

enum PollLoopOutcome {
    Done(Box<GrokAuth>),
    Restart,
}

/// Run interactive Kimi Code device login and persist the token set.
pub async fn run_kimi_code_login() -> anyhow::Result<GrokAuth> {
    let host = xai_grok_models::PlatformId::KimiCode
        .oauth_host()
        .ok_or_else(|| anyhow::anyhow!("Kimi Code OAuth host is not configured"))?;

    loop {
        let device_auth = request_device_authorization(&host).await?;
        prompt_on_stderr(&device_auth).await;

        match complete_device_code_login(&host, &device_auth).await? {
            PollLoopOutcome::Done(auth) => {
                let auth = *auth;
                store_kimi_code_auth(&xai_grok_config::grok_home(), &auth)?;
                eprintln!("✓ Signed in to Kimi Code");
                eprintln!(
                    "  Use model: kimi-code/kimi-for-coding  (or set [models] default)"
                );
                return Ok(auth);
            }
            PollLoopOutcome::Restart => {
                tracing::info!("auth: Kimi device code expired, restarting");
                eprintln!("Device code expired — requesting a new one...");
                continue;
            }
        }
    }
}

/// Load a usable Kimi Code access token: return cached if still valid,
/// otherwise refresh (when possible) and persist.
pub async fn ensure_kimi_code_access_token() -> Option<String> {
    let home = xai_grok_config::grok_home();
    let auth = read_kimi_code_auth(&home)?;
    if !crate::auth::is_expired(&auth) {
        return Some(auth.key);
    }
    let refresh = auth.refresh_token.as_deref()?;
    let host = xai_grok_models::PlatformId::KimiCode.oauth_host()?;
    match super::oauth::refresh_token(&host, refresh).await {
        Ok(new_auth) => {
            if let Err(e) = store_kimi_code_auth(&home, &new_auth) {
                tracing::warn!(error = %e, "auth: failed to persist refreshed Kimi token");
            }
            Some(new_auth.key)
        }
        Err(e) => {
            tracing::warn!(error = %e, "auth: Kimi token refresh failed");
            None
        }
    }
}

async fn complete_device_code_login(
    host: &str,
    device_auth: &DeviceAuthorization,
) -> anyhow::Result<PollLoopOutcome> {
    let mut poll_interval = std::time::Duration::from_secs(device_auth.interval.max(1) as u64);
    loop {
        tokio::time::sleep(poll_interval).await;
        match poll_device_token(host, &device_auth.device_code).await? {
            DevicePollResult::Success(auth) => return Ok(PollLoopOutcome::Done(auth)),
            DevicePollResult::Expired => return Ok(PollLoopOutcome::Restart),
            DevicePollResult::Pending { error, description } => {
                if error == "slow_down" {
                    poll_interval += std::time::Duration::from_secs(SLOW_DOWN_INCREMENT_SECS);
                } else {
                    tracing::debug!(
                        error = %error,
                        description = ?description,
                        "auth: Kimi device authorization pending"
                    );
                }
            }
        }
    }
}

async fn prompt_on_stderr(device_auth: &DeviceAuthorization) {
    let display_uri = &device_auth.verification_uri_complete;
    eprintln!();
    eprintln!("To sign in to Kimi Code, open this URL in your browser:");
    eprintln!();
    eprintln!("  {display_uri}");
    eprintln!();
    if !open_browser_detached(display_uri).await {
        eprintln!("  (Could not open browser automatically — open the URL above manually.)");
        eprintln!();
    }
    eprintln!("Confirm this code in your browser:");
    eprintln!();
    eprintln!("  {}", device_auth.user_code);
    eprintln!();
    eprintln!(
        "\x1b[90mOnly continue with a code you requested. \
         Don't share it with anyone.\x1b[0m"
    );
    eprintln!();
    eprintln!("Waiting for authorization...");
}

async fn open_browser_detached(url: &str) -> bool {
    if cfg!(test) {
        return false;
    }
    let url = url.to_owned();
    match tokio::task::spawn_blocking(move || webbrowser::open(&url)).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::info!(error = %e, "kimi auth: could not open browser");
            false
        }
        Err(e) => {
            tracing::info!(error = %e, "kimi auth: browser-open task failed");
            false
        }
    }
}
