//! Subscription check for paywall gate lift.
//!
//! Provides `single_check()` which queries `GET /user?include=subscription`
//! for the live subscription tier from the backend, independent of the JWT.
//! If a qualifying tier is detected, does a best-effort JWT refresh and
//! settings re-fetch, then returns an `UnblockResult` so the agent can
//! lift the gate.
//!
//! The pager drives the polling via `x.ai/auth/check_subscription`: the 5s
//! paywall chain, the free-tier watch, the refocus check, and
//! verify-before-paywall gate deferral (see the pager's `app::subscription`
//! module).
use crate::auth::AuthManager;
use crate::auth::UserInfo;
use crate::auth::manager::RefreshReason;
use crate::auth::token_type::TokenType;
use std::sync::Arc;
use std::time::Duration;
/// Subscription tiers that qualify for Grok Build access.
/// Any active subscription qualifies -- the access gate in remote settings
/// controls which tiers are actually allowed.
const QUALIFYING_TIERS: &[&str] = &[
    "SuperGrokPro",
    "GrokPro",
    "SuperGrokLite",
    "XPremiumPlus",
    "XPremium",
    "XBasic",
];
/// Successful subscription check result: confirmed qualifying tier +
/// optionally refreshed settings.
pub(crate) struct UnblockResult {
    pub(crate) new_tier: String,
    pub(crate) settings: Option<crate::util::config::RemoteSettings>,
}
/// Fetch `/user?include=subscription` and return the parsed `UserInfo`.
async fn fetch_user_info(
    http_client: &reqwest::Client,
    url: &str,
    auth: &crate::auth::GrokAuth,
    auth_manager: &AuthManager,
    alpha_test_key: Option<&str>,
) -> Result<UserInfo, &'static str> {
    let request = http_client
        .get(url)
        .timeout(Duration::from_secs(10))
        .header("Authorization", format!("Bearer {}", auth.key))
        .header(
            "X-XAI-Token-Auth",
            auth_manager.grok_com_config().token_header.as_str(),
        )
        .header("x-grok-client-version", xai_grok_version::VERSION)
        .header(
            crate::http::CLIENT_MODE_HEADER,
            crate::http::process_client_mode(),
        );
    let _ = alpha_test_key;
    match request.send().await {
        Ok(resp) if resp.status().is_success() => {
            resp.json::<UserInfo>().await.map_err(|_| "parse")
        }
        Ok(_resp) => Err("http_status"),
        Err(e) if e.is_timeout() => Err("timeout"),
        Err(_) => Err("transport"),
    }
}
/// Single-shot subscription check. Called by the pager every 5s while
/// the paywall is shown (`x.ai/auth/check_subscription`).
///
/// Queries `/user?include=subscription` for the live tier. If a qualifying
/// tier is found, does a best-effort JWT refresh + settings re-fetch and
/// returns `Some(UnblockResult)`. Returns `None` if no qualifying
/// subscription exists or the request fails.
#[tracing::instrument(name = "paywall_check", skip_all, fields(user_id = %user_id))]
pub(crate) async fn single_check(
    auth_manager: Arc<AuthManager>,
    proxy_base_url: &str,
    alpha_test_key: Option<&str>,
    user_id: &str,
) -> Option<UnblockResult> {
    let user_url = format!("{}/user?include=subscription", proxy_base_url);
    let http_client = crate::http::shared_client();
    let auth = auth_manager.current()?;
    let user_info = match fetch_user_info(
        &http_client,
        &user_url,
        &auth,
        &auth_manager,
        alpha_test_key,
    )
    .await
    {
        Ok(ui) => ui,
        Err(kind) => {
            xai_grok_telemetry::unified_log::warn(
                "paywall_check_error",
                None,
                Some(serde_json::json!({ "user_id": user_id, "kind": kind })),
            );
            return None;
        }
    };
    xai_grok_telemetry::unified_log::info(
        "paywall_check_result",
        None,
        Some(serde_json::json!({
            "user_id": user_id,
            "subscription_tier": user_info.subscription_tier,
        })),
    );
    let new_tier = match &user_info.subscription_tier {
        Some(tier) if !tier.is_empty() => tier.clone(),
        _ => return None,
    };
    if !QUALIFYING_TIERS.contains(&new_tier.as_str()) {
        return None;
    }
    xai_grok_telemetry::unified_log::info(
        "paywall_check_subscription_detected",
        None,
        Some(serde_json::json!({
            "user_id": user_id,
            "new_tier": new_tier,
        })),
    );
    if let Err(e) = auth_manager
        .refresh_chain(TokenType::OidcSession, RefreshReason::ServerRejected)
        .await
    {
        xai_grok_telemetry::unified_log::warn(
            "paywall_check_error",
            None,
            Some(serde_json::json!({
                "user_id": user_id,
                "kind": "refresh_failed",
                "detail": e.to_string(),
            })),
        );
    }
    let settings = if crate::util::config::resolve_remote_fetch_enabled() {
        let base_url = proxy_base_url.to_string();
        let auth_for_settings = auth_manager.current().unwrap_or(auth);
        let atk = alpha_test_key.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            crate::remote::fetch_settings_blocking(&base_url, &auth_for_settings, atk.as_deref())
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    xai_grok_telemetry::unified_log::info(
        "paywall_check_unblocked",
        None,
        Some(serde_json::json!({ "user_id": user_id, "new_tier": new_tier })),
    );
    Some(UnblockResult { new_tier, settings })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qualifying_tiers_includes_all_paid_tiers() {
        for tier in &[
            "SuperGrokPro",
            "GrokPro",
            "SuperGrokLite",
            "XPremiumPlus",
            "XPremium",
            "XBasic",
        ] {
            assert!(
                QUALIFYING_TIERS.contains(tier),
                "{tier} must be in QUALIFYING_TIERS"
            );
        }
    }
    #[test]
    fn free_tier_is_not_qualifying() {
        assert!(!QUALIFYING_TIERS.contains(&"Free"));
    }
    #[test]
    fn empty_tier_is_not_qualifying() {
        assert!(!QUALIFYING_TIERS.contains(&""));
    }
    /// The subscription check only returns `Some` when `/user` reports a
    /// qualifying tier. Verify the tier matching is exact (no prefix match).
    #[test]
    fn partial_tier_name_is_not_qualifying() {
        assert!(!QUALIFYING_TIERS.contains(&"Super"));
        assert!(!QUALIFYING_TIERS.contains(&"Grok"));
        assert!(!QUALIFYING_TIERS.contains(&"XPremium+"));
    }
}
