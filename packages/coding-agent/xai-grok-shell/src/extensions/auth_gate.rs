use agent_client_protocol as acp;

use crate::auth::{AuthManager, GrokAuth};

/// Require xAI auth from a sync context, accepting tokens in the client-side buffer window.
pub(crate) fn require_xai_auth(
    auth_manager: &AuthManager,
    missing_message: &'static str,
    non_xai_message: &'static str,
) -> Result<GrokAuth, acp::Error> {
    let auth = auth_manager
        .current_or_expired()
        .ok_or_else(|| acp::Error::auth_required().data(missing_message))?;
    if !auth.is_xai_auth() {
        return Err(acp::Error::auth_required().data(non_xai_message));
    }
    Ok(auth)
}
