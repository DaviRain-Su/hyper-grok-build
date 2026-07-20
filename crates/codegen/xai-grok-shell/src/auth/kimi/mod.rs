//! Kimi Code subscription auth (device OAuth).
//!
//! Credentials are stored under [`crate::auth::model::KIMI_CODE_OAUTH_SCOPE`]
//! in `~/.grok/auth.json` and stamped onto `kimi-code/*` catalog entries.
//! This path is independent of the primary xAI AuthManager session.

mod device;
mod login;
mod oauth;

pub use device::device_headers;
pub use login::{ensure_kimi_code_access_token, run_kimi_code_login};
pub use crate::auth::model::KIMI_CODE_OAUTH_SCOPE;
