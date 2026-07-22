pub mod auth;
pub(crate) mod auth_gate;
pub mod billing;
pub mod bundle;
pub mod chat_conversation_history;
pub mod code_nav;
pub mod debug;
pub mod feedback;
pub mod fs;
pub mod git;
pub mod hooks;
pub mod hunk_tracker;
pub mod interject;
pub mod jj;
pub mod marketplace;
pub mod mcp;
pub mod memory;
pub mod notification;
pub mod plugins;
pub mod pr;
pub mod privacy;
pub mod prompt_history;
pub mod prompt_meta;
pub mod recap;
pub mod repair;
pub mod rewind;
pub mod rollout;
pub mod routing;
pub mod search;
pub mod session_admin;
pub mod session_search;
pub mod session_state;
pub mod session_updates;
pub mod share;
pub mod skills;
pub mod suggest;
pub mod task;
pub mod terminal;
pub mod usage;
pub mod worktree;
use crate::session::ExtMethodResult;
use agent_client_protocol as acp;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;
pub type ExtResult = Result<acp::ExtResponse, acp::Error>;
pub fn parse_params<T: DeserializeOwned>(args: &acp::ExtRequest) -> Result<T, acp::Error> {
    parse_params_str(args.params.get())
}
/// Deserialize ACP params from their raw JSON string, mapping a parse failure
/// to `invalid_params`. Used by [`parse_params`] and the bridge `encode` hooks,
/// which hold the params `RawValue` directly.
pub fn parse_params_str<T: DeserializeOwned>(raw: &str) -> Result<T, acp::Error> {
    serde_json::from_str(raw)
        .map_err(|e| acp::Error::invalid_params().data(format!("invalid params: {}", e)))
}
/// Extract the session ID from an extension request's params.
pub fn parse_session_id(args: &acp::ExtRequest) -> Option<acp::SessionId> {
    let v: serde_json::Value = serde_json::from_str(args.params.get()).ok()?;
    let sid = v.get("sessionId")?.as_str()?;
    Some(acp::SessionId::new(sid))
}
pub fn to_ext_response<T: Serialize>(result: anyhow::Result<T>) -> ExtResult {
    ExtMethodResult::from_result(result)
        .to_ext_response()
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))
}
/// Wrap a serializable value as an `ExtResponse` without the `ExtMethodResult` envelope.
pub fn to_raw_response<T: Serialize>(v: &T) -> ExtResult {
    serde_json::value::to_raw_value(v)
        .map(|raw| acp::ExtResponse::new(Arc::from(raw)))
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))
}
/// Convert a result with optional warning to an ExtResponse.
pub fn to_ext_response_partial<T: Serialize>(
    result: anyhow::Result<T>,
    warning: Option<String>,
) -> ExtResult {
    let ext_result = match (result, warning) {
        (Ok(data), Some(warn)) => ExtMethodResult::partial(data, warn),
        (Ok(data), None) => ExtMethodResult::success(data),
        (Err(e), _) => ExtMethodResult::failure(e),
    };
    ext_result
        .to_ext_response()
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))
}
/// Empty response for operations that return no data.
#[derive(Debug, Serialize)]
pub struct Empty {}
