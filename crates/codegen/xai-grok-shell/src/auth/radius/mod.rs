//! Radius gateway subscription auth.

mod login;
pub(crate) mod oauth;

pub use crate::auth::model::RADIUS_OAUTH_SCOPE;
pub(crate) use login::radius_catalog_oauth_cached;
pub use login::{
    RadiusBearerResolver, RadiusLoginMethod, ensure_radius_auth, ensure_radius_auth_blocking,
    force_refresh_radius_auth, radius_catalog_access_token_cached, run_radius_login,
    run_radius_login_with_channels,
};
pub use oauth::{
    DEFAULT_RADIUS_GATEWAY, config_url, gateway_from_env_or_default, normalize_gateway_root,
    try_gateway_from_env_or_default,
};

/// Radius stores Pi's 60-second expiry skew in `expires_at` itself, so applying
/// the generic five-minute auth buffer here would double-count early expiry.
pub(crate) fn is_radius_auth_expired(auth: &crate::auth::model::GrokAuth) -> bool {
    crate::auth::model::is_expired_with_buffer(auth, chrono::Duration::zero())
}
