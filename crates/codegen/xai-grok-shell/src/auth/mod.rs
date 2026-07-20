pub(crate) mod attribution;
mod config;
pub mod credential_provider;
#[path = "devbox_login_stub.rs"]
pub(crate) mod devbox_login;
pub mod device_code;
pub mod error;
mod external_auth;
mod flow;
mod jwt;
pub mod kimi;
pub(crate) mod manager;
mod model;
pub mod oidc;
pub(crate) mod recovery;
pub(crate) mod refresh;
pub(crate) mod single_flight;
mod storage;
pub(crate) mod token_type;
pub(crate) use config::LEGACY_AUTH_SCOPE;
pub use config::{
    ForceLoginTeam, GrokComConfig, OAuth2ProviderConfig, OidcAuthConfig, PreferredAuthMethod,
    XAI_OAUTH2_ISSUER, is_xai_oauth2_issuer, xai_oauth2_issuer,
};
pub(crate) use external_auth::{parse_output, refresh_with_command};
pub(crate) use flow::{
    AuthChannels, run_auth_flow, run_auth_flow_with_stderr_bridge,
    try_ensure_session_noninteractive,
};
pub use flow::{
    AuthUrlInfo, AuthUrlMode, LoginTransportOverride, LogoutResult, ensure_authenticated,
    ensure_authenticated_or_noninteractive, ensure_authenticated_with_override, perform_logout,
    run_cli_login, run_cli_logout, run_cli_logout_kimi, try_ensure_fresh_auth,
};
pub use jwt::{is_jwt_expired_or_near, parse_jwt_expiration};
mod meta;
pub use error::{AuthError, RefreshTokenError, RefreshTokenFailedReason};
pub use manager::{AuthManager, shared_api_key_provider};
pub use meta::{AuthMeta, GateInfo};
pub use model::{
    AuthMode, GrokAuth, KIMI_CODE_OAUTH_SCOPE, lookup_auth, platform_api_key_scope,
};
pub(crate) use model::{
    TOKEN_TTL, UserInfo, default_coding_data_retention_opt_out, is_expired, token_suffix,
};
pub(crate) use refresh::DiagnosticUploader;
pub use storage::{
    auth_json_path, clear_api_key, clear_kimi_code_auth, clear_platform_api_key, read_api_key,
    read_auth_json, read_kimi_code_auth, read_platform_api_key, read_token_by_scope, store_api_key,
    store_kimi_code_auth, store_platform_api_key,
};
