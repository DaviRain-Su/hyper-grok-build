//! Live session configuration builder: assembles a [`LiveConfig`] from the
//! active session id, platform Codex base, OMP client version, and voice.

use xai_grok_models::PlatformId;

use super::LiveConfig;
use crate::live::prompts;

/// The OMP client version string sent in the Live handshake.
pub const OMP_CLIENT_VERSION: &str = "0.144.1";

/// The default voice persona for Codex Live.
pub const LIVE_VOICE: &str = "sol";

/// Resolve the sideband base URL (env override or default). Returns `None` to
/// let the voice core derive the sideband wss URL from the server-assigned
/// call id, unless an explicit override is set.
pub fn resolve_sideband_base() -> Option<String> {
    match std::env::var("GROK_CODEX_LIVE_SIDEBAND_BASE") {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

/// Resolve the platform Codex base URL (env override or default).
///
/// Reuse the catalog resolver so Live and normal Codex inference share the
/// exact `https://chatgpt.com/backend-api/codex` default and the same
/// `GROK_OPENAI_CODEX_BASE_URL` override. The voice core appends
/// `/realtime/calls?intent=quicksilver&architecture=avas` to this base.
pub fn resolve_codex_base() -> String {
    PlatformId::OpenAiCodex.base_url()
}

/// Build a [`LiveConfig`] for the active session.
///
/// - `session_id`: the active ACP session id.
/// - `codex_base`: the platform Codex base URL (from [`resolve_codex_base`]).
/// - `sideband_base`: the sideband base URL (from [`resolve_sideband_base`]).
/// - `voice`: the voice persona id (default `sol`).
pub fn build_live_config(
    session_id: &str,
    codex_base: &str,
    sideband_base: Option<String>,
    voice: &str,
) -> LiveConfig {
    LiveConfig {
        codex_base: codex_base.to_string(),
        sideband_base,
        session_id: session_id.to_string(),
        instructions: prompts::live_instructions().to_string(),
        voice: voice.to_string(),
        client_version: OMP_CLIENT_VERSION.to_string(),
    }
}

/// Build a [`LiveConfig`] with defaults resolved from env.
pub fn build_live_config_default(session_id: &str) -> LiveConfig {
    build_live_config(
        session_id,
        &resolve_codex_base(),
        resolve_sideband_base(),
        LIVE_VOICE,
    )
}
