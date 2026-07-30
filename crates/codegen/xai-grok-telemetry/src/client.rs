//! Core telemetry tracking — product events + Mixpanel.
//!
//! All calls route through [`track`]. Precedence: env > config > remote config > default.
//!
//! Extracted from `xai-grok-shell::agent::telemetry::track`. The HTTP client is
//! injected via [`init`]/[`init_if_needed`] so this crate avoids depending on
//! shell's `User-Agent` builder (which couples to the `permission` module).

use std::sync::{Arc, Mutex, OnceLock};

use chrono::{Local, SecondsFormat};
use serde_json::json;
use xai_mixpanel::Mixpanel;

use crate::config::{TelemetryConfig, TelemetryMode, deployment_id_from_key};
use crate::http::OriginClientInfo;
use crate::session_ctx::EmitterOrigin;

/// Event property map shared by all telemetry modules.
pub type Metadata = serde_json::Map<String, serde_json::Value>;

/// Derive the analytics `event_value` from the full wire `event_name` by stripping
/// whichever [`EmitterOrigin`] prefix it carries (`grok-shell-` /
/// `grok-workspace-`). Unprefixed names pass through unchanged. Kept in
/// lockstep with [`EmitterOrigin::event_prefix`] via [`EmitterOrigin::ALL`],
/// so shell events keep their historical stripped value and workspace events
/// collapse to the same bare suffix.
fn event_value(event_name: &str) -> &str {
    for origin in EmitterOrigin::ALL {
        if let Some(suffix) = event_name.strip_prefix(origin.event_prefix()) {
            return suffix;
        }
    }
    event_name
}

/// Product-analytics `$insert_id`: unique per emit, ≤36 bytes, `[A-Za-z0-9-]`.
///
/// Do not put the event name in this field. The analytics sink truncates to 36
/// chars and rejects most other characters; a name-prefixed id either collapses
/// to a constant (long names → per-user same-second dedup) or is dropped and
/// regenerated (shorter names with `:`). A bare UUID always validates.
fn product_analytics_insert_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[derive(Clone)]
pub struct TelemetryClient {
    mode: TelemetryMode,
    events_url: Option<String>,
    events_api_key: Option<String>,
    mixpanel: Option<Arc<Mixpanel>>,
    user_id: Option<String>,
    team_id: Option<String>,
    deployment_id: Option<String>,
    shell_version: String,
    client_type: Option<String>,
    client_version: Option<String>,
    subscription_tier: Option<String>,
    http_client: reqwest::Client,
}

impl std::fmt::Debug for TelemetryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryClient")
            .field("events_url", &self.events_url)
            .field(
                "events_api_key",
                &self.events_api_key.as_ref().map(|_| "***"),
            )
            .field("mixpanel", &self.mixpanel.as_ref().map(|_| "configured"))
            .finish()
    }
}

impl TelemetryClient {
    pub fn from_config(
        config: TelemetryConfig,
        mode: TelemetryMode,
        user_id: Option<String>,
        team_id: Option<String>,
        deployment_key: Option<String>,
        origin_client: Option<OriginClientInfo>,
        shell_version: String,
        subscription_tier: Option<String>,
        http_client: reqwest::Client,
    ) -> Self {
        let mixpanel = if config.mixpanel_enabled {
            config
                .mixpanel_token
                .as_ref()
                .map(|token| Arc::new(Mixpanel::new(token.as_str())))
        } else {
            None
        };
        let deployment_id = deployment_key
            .filter(|s| !s.is_empty())
            .map(|k| deployment_id_from_key(&k));
        let (client_type, client_version) = match origin_client {
            Some(o) => (Some(o.product), o.version),
            None => (None, None),
        };

        Self {
            mode,
            events_url: config.events_url,
            events_api_key: config.events_api_key,
            mixpanel,
            user_id,
            team_id,
            deployment_id,
            shell_version,
            client_type,
            client_version,
            subscription_tier: subscription_tier.map(|t| normalize_tier(&t)),
            http_client,
        }
    }
}

/// Normalize a subscription tier string to a consistent lowercase_underscore
/// format for Mixpanel. Handles both CCP display names ("SuperGrok Heavy")
/// and JWT-derived keys ("supergrok_heavy").
fn normalize_tier(tier: &str) -> String {
    match tier {
        "SuperGrok Heavy" | "supergrok_heavy" => "supergrok_heavy",
        "SuperGrok Plus" | "supergrok_plus" => "supergrok_plus",
        "SuperGrok" | "supergrok" => "supergrok",
        "SuperGrok Lite" | "supergrok_lite" => "supergrok_lite",
        "X Premium+" | "x_premium_plus" => "x_premium_plus",
        "X Premium" | "x_premium" => "x_premium",
        "X Basic" | "x_basic" => "x_basic",
        "Free" | "free" => "free",
        // Team / console API keys — dedicated Mixpanel segment, not free.
        "API Key" | "api_key" => "api_key",
        other => return other.to_ascii_lowercase().replace(' ', "_"),
    }
    .to_string()
}

static TELEMETRY_CLIENT: OnceLock<Mutex<Option<TelemetryClient>>> = OnceLock::new();

/// Returns `true` when telemetry mode is `Enabled`.
/// Used by `log_event` — product analytics events only fire in `Enabled` mode.
pub fn is_enabled() -> bool {
    TELEMETRY_CLIENT
        .get()
        .and_then(|m| m.lock().ok())
        .is_some_and(|g| g.as_ref().is_some_and(|c| c.mode.is_enabled()))
}

/// Returns `true` when telemetry mode is `Enabled` or `SessionMetrics`.
/// Used by `session_metrics` — lifecycle events fire in both modes.
pub fn is_session_metrics_enabled() -> bool {
    TELEMETRY_CLIENT
        .get()
        .and_then(|m| m.lock().ok())
        .is_some_and(|g| g.as_ref().is_some_and(|c| c.mode.session_metrics_enabled()))
}

pub struct UserContext {
    pub country: String,
    pub language: String,
    pub timestamp: String,
}

impl UserContext {
    pub fn collect() -> Self {
        let default_language = whoami::Language::En(whoami::Country::Any);
        let lang = whoami::langs()
            .ok()
            .and_then(|mut langs| langs.next())
            .unwrap_or(default_language);
        Self {
            country: lang.country().to_string(),
            language: lang.to_string(),
            timestamp: Local::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        }
    }
}

/// Core telemetry emitter. Routes to product events + Mixpanel.
pub async fn track(event_name: &str, request_id: &str, ctx: &UserContext, mut metadata: Metadata) {
    let lock = TELEMETRY_CLIENT.get_or_init(|| Mutex::new(None));
    let client = {
        let guard = lock.lock().unwrap_or_else(|err| err.into_inner());
        match guard.clone() {
            Some(c) => c,
            None => return,
        }
    };

    let agent_id = crate::id::agent_id();
    let user_id = client.user_id.as_deref().unwrap_or(&agent_id);
    metadata.insert("agent_id".into(), json!(agent_id));
    if let Some(ref team_id) = client.team_id {
        metadata.insert("team_id".into(), json!(team_id));
    }
    if let Some(ref deployment_id) = client.deployment_id {
        metadata.insert("deployment_id".into(), json!(deployment_id));
    }
    metadata.insert("shell_version".into(), json!(client.shell_version));
    if let Some(ref client_type) = client.client_type {
        metadata.insert("client_type".into(), json!(client_type));
    }
    if let Some(ref client_version) = client.client_version {
        metadata.insert("client_version".into(), json!(client_version));
    }
    if let Some(ref subscription_tier) = client.subscription_tier {
        metadata.insert("subscription_tier".into(), json!(subscription_tier));
    }

    // Product events path
    if let (Some(url), Some(api_key)) = (&client.events_url, &client.events_api_key) {
        let body = json!({
            "viewer_context": {
                "request_id": request_id,
                "user_attributes": {
                    "user_id": user_id,
                    "user_type": "LoggedIn",
                    "country": ctx.country,
                    "language": ctx.language,
                    "locale": "English",
                },
                "device_attributes": {
                    "app_name": "Grok Code",
                },
            },
            "api_key": api_key,
            "events": [{
                "event_name": event_name,
                "event_value": event_value(event_name),
                "event_metadata": metadata.clone(),
                "timestamp": ctx.timestamp,
            }]
        });
        let _ = client
            .http_client
            .post(url)
            .header("x-api-key", api_key.as_str())
            .timeout(std::time::Duration::from_secs(10))
            .json(&body)
            .send()
            .await;
    }

    // Mixpanel path
    if let Some(ref mixpanel) = client.mixpanel {
        let time_secs = chrono::Utc::now().timestamp();
        let insert_id = product_analytics_insert_id();

        // Convert serde_json::Map to HashMap for mixpanel
        let mut props: std::collections::HashMap<String, serde_json::Value> =
            metadata.into_iter().collect();
        props.insert("distinct_id".into(), json!(user_id));
        props.insert("time".into(), json!(time_secs));
        props.insert("$insert_id".into(), json!(insert_id));
        props.insert("app_name".into(), json!("Grok Code"));
        props.insert("user_type".into(), json!("LoggedIn"));
        props.insert("country".into(), json!(ctx.country));
        props.insert("language".into(), json!(ctx.language));
        props.insert("locale".into(), json!("English"));

        let _ = mixpanel.track(event_name, Some(props)).await;
    }
}

/// Sync the user's Mixpanel profile once per init. Fire-and-forget.
///
/// Only runs in [`TelemetryMode::Enabled`]. SessionMetrics mode may emit
/// lifecycle events via [`track`], but must not write Mixpanel people
/// profiles (`engage`).
pub fn sync_profile() {
    let lock = TELEMETRY_CLIENT.get_or_init(|| Mutex::new(None));
    let client = {
        let guard = lock.lock().unwrap_or_else(|err| err.into_inner());
        match guard.clone() {
            Some(c) => c,
            None => return,
        }
    };

    // The single profile-sync gate: reads the installed client's mode, so every
    // caller (and any init race) resolves against what was actually installed.
    if !client.mode.is_enabled() {
        return;
    }

    let Some(mixpanel) = client.mixpanel.clone() else {
        return;
    };

    let agent_id = crate::id::agent_id();
    let user_id = client.user_id.as_deref().unwrap_or(&agent_id).to_owned();

    tokio::spawn(async move {
        let mut props = std::collections::HashMap::new();
        props.insert("agent_id".into(), json!(agent_id));
        props.insert("shell_version".into(), json!(client.shell_version));
        props.insert("app_name".into(), json!("Grok Code"));
        if let Some(ref client_type) = client.client_type {
            props.insert("client_type".into(), json!(client_type));
        }
        if let Some(ref client_version) = client.client_version {
            props.insert("client_version".into(), json!(client_version));
        }
        if let Some(ref deployment_id) = client.deployment_id {
            props.insert("deployment_id".into(), json!(deployment_id));
        }
        if let Some(ref team_id) = client.team_id {
            props.insert("team_id".into(), json!(team_id));
        }
        if let Some(ref subscription_tier) = client.subscription_tier {
            props.insert("subscription_tier".into(), json!(subscription_tier));
        }
        let _ = mixpanel.engage(&user_id, props).await;
    });
}

/// Initialize telemetry client. Safe to call multiple times.
///
/// - `Disabled` → no client
/// - `SessionMetrics` → client active (only `session_metrics::*` events fire)
/// - `Enabled` → client active (all events fire)
///
/// `shell_version` is stamped into every event payload as `shell_version`
/// (legacy field name preserved for analytics continuity); shell passes its
/// own `CARGO_PKG_VERSION`. `http_client` is owned by the caller (typically
/// shell's `shared_client()`) so the shared TLS-warmed pool is reused for
/// telemetry posts.
pub fn init(
    config: TelemetryConfig,
    mode: TelemetryMode,
    user_id: Option<String>,
    team_id: Option<String>,
    deployment_key: Option<String>,
    origin_client: Option<OriginClientInfo>,
    shell_version: String,
    subscription_tier: Option<String>,
    http_client: reqwest::Client,
) {
    let lock = TELEMETRY_CLIENT.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().unwrap_or_else(|err| err.into_inner());
    *guard = if mode.is_disabled() {
        None
    } else {
        Some(TelemetryClient::from_config(
            config,
            mode,
            user_id,
            team_id,
            deployment_key,
            origin_client,
            shell_version,
            subscription_tier,
            http_client,
        ))
    };
    drop(guard);
    sync_profile();
}

/// Re-initialize the telemetry client if it was not created at startup
/// (e.g. because auth was not yet available). No-op when the client
/// is already set, so safe to call unconditionally after auth succeeds.
pub fn init_if_needed(
    config: TelemetryConfig,
    mode: TelemetryMode,
    user_id: Option<String>,
    team_id: Option<String>,
    deployment_key: Option<String>,
    origin_client: Option<OriginClientInfo>,
    shell_version: String,
    subscription_tier: Option<String>,
    http_client: reqwest::Client,
) {
    if mode.is_disabled() {
        return;
    }
    let lock = TELEMETRY_CLIENT.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().unwrap_or_else(|err| err.into_inner());
    if guard.is_none() {
        *guard = Some(TelemetryClient::from_config(
            config,
            mode,
            user_id,
            team_id,
            deployment_key,
            origin_client,
            shell_version,
            subscription_tier,
            http_client,
        ));
        drop(guard);
        sync_profile();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shell events must still strip to their bare suffix, byte-for-byte
    /// identical to the previous `strip_prefix("grok-shell-")` behavior.
    #[test]
    fn event_value_strips_shell_prefix() {
        assert_eq!(event_value("grok-shell-turn"), "turn");
        assert_eq!(
            event_value("grok-shell-trace_upload_attempted"),
            "trace_upload_attempted"
        );
    }

    /// Workspace events strip their own prefix to the same bare suffix.
    #[test]
    fn event_value_strips_workspace_prefix() {
        assert_eq!(event_value("grok-workspace-turn"), "turn");
    }

    /// SessionMetrics must not attempt Mixpanel profile engage — sync_profile
    /// is a no-op unless mode is fully Enabled.
    #[test]
    fn sync_profile_is_noop_in_session_metrics_mode() {
        // No tokio runtime here BY DESIGN: if the gate wrongly falls through,
        // sync_profile's tokio::spawn panics and fails this test. Converting
        // this to #[tokio::test] would silently turn it into theater.
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "this test must run without a tokio runtime"
        );
        // Clear the global client even if an assert below panics.
        struct ClearClient;
        impl Drop for ClearClient {
            fn drop(&mut self) {
                let lock = TELEMETRY_CLIENT.get_or_init(|| Mutex::new(None));
                *lock.lock().unwrap_or_else(|err| err.into_inner()) = None;
            }
        }
        let _clear = ClearClient;

        // Mixpanel configured, but no events endpoint: the global must never
        // carry a live funnel out of this test.
        let cfg = TelemetryConfig {
            mixpanel_enabled: true,
            mixpanel_token: Some("test-token".into()),
            events_url: None,
            events_api_key: None,
            ..TelemetryConfig::default()
        };
        init(
            cfg,
            TelemetryMode::SessionMetrics,
            Some("user-1".into()),
            None,
            None,
            None,
            "0.0.0-test".into(),
            None,
            reqwest::Client::new(),
        );
        // Explicit call must no-op too (init already invoked it once).
        sync_profile();
        assert!(
            is_session_metrics_enabled(),
            "client must be live for session metrics"
        );
        assert!(!is_enabled(), "product analytics must stay off");
    }

    /// Names without a known emitter prefix pass through unchanged (preserves
    /// the old `unwrap_or(event_name)` fallback).
    #[test]
    fn event_value_passes_through_unprefixed() {
        assert_eq!(event_value("turn"), "turn");
        assert_eq!(event_value(""), "");
    }

    /// Only the leading emitter prefix is stripped; a suffix that itself looks
    /// like another prefix is left intact.
    #[test]
    fn event_value_strips_only_leading_prefix() {
        assert_eq!(event_value("grok-shell-workspace-x"), "workspace-x");
    }

    /// The stripper recovers the bare suffix for every origin the emitter can
    /// produce — ties `event_value` to `EmitterOrigin::event_prefix`.
    #[test]
    fn event_value_round_trips_every_emitter_prefix() {
        for origin in EmitterOrigin::ALL {
            let name = format!("{}my_event", origin.event_prefix());
            assert_eq!(event_value(&name), "my_event");
        }
    }

    /// Mixpanel `subscription_tier` must be a stable snake_case key. Free
    /// users arrive as CCP display `"Free"` or JWT-fallback `"free"`; both
    /// must land as `"free"` (not omitted / not `"Free"`).
    #[test]
    fn normalize_tier_maps_display_and_claim_names() {
        assert_eq!(normalize_tier("Free"), "free");
        assert_eq!(normalize_tier("free"), "free");
        assert_eq!(normalize_tier("SuperGrok"), "supergrok");
        assert_eq!(normalize_tier("SuperGrok Heavy"), "supergrok_heavy");
        assert_eq!(normalize_tier("supergrok_heavy"), "supergrok_heavy");
        assert_eq!(normalize_tier("X Basic"), "x_basic");
        assert_eq!(normalize_tier("X Premium+"), "x_premium_plus");
        assert_eq!(normalize_tier("X Premium"), "x_premium");
        assert_eq!(normalize_tier("SuperGrok Lite"), "supergrok_lite");
        assert_eq!(normalize_tier("SuperGrok Plus"), "supergrok_plus");
        assert_eq!(normalize_tier("supergrok_plus"), "supergrok_plus");
        // API key is a dedicated Mixpanel segment — never free.
        assert_eq!(normalize_tier("API Key"), "api_key");
        assert_eq!(normalize_tier("api_key"), "api_key");
    }

    /// `event_value`'s first-match-wins over `EmitterOrigin::ALL` is only
    /// correct because the emitter prefixes are mutually exclusive: no origin's
    /// `event_prefix()` is a prefix of another's. If that invariant ever broke
    /// (e.g. a future `"grok-shell-ext-"` origin), an earlier `ALL` entry could
    /// strip a shorter prefix first and yield the wrong `event_value`. Pin the
    /// invariant so adding such a variant fails the suite rather than silently
    /// corrupting analytics.
    #[test]
    fn emitter_prefixes_are_mutually_exclusive() {
        for a in EmitterOrigin::ALL {
            for b in EmitterOrigin::ALL {
                if a != b {
                    assert!(
                        !a.event_prefix().starts_with(b.event_prefix()),
                        "{a:?} prefix {:?} must not start with {b:?} prefix {:?}",
                        a.event_prefix(),
                        b.event_prefix(),
                    );
                }
            }
        }
    }
}
