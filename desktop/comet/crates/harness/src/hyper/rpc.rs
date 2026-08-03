//! Hyper ACP client helpers: binary discovery + the `x.ai/interject` steer
//! request builder. The actual `ClientSideConnection` lives in `mod.rs`
//! (it is `!Send`, so it runs on a dedicated `LocalSet` thread).

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol as acp;
use crate::HarnessError;

/// Resolve the `hyper` (or `grok`) CLI binary.
///
/// Order: `HYPER_AGENT_BIN` → beside this exe → `hyper`/`grok` on PATH →
/// workspace `target/{debug,release}/{hyper,grok}`. Mirrors the desktop's
/// `resolve_agent_bin` (which tries BOTH `hyper` and `grok` — a user may have
/// only `grok` installed at `~/.grok/bin/grok`).
pub fn resolve_hyper_bin() -> Result<PathBuf, HarnessError> {
    if let Ok(p) = std::env::var("HYPER_AGENT_BIN") {
        return Ok(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for name in ["hyper", "grok"] {
            let cand = dir.join(name);
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }
    for name in ["hyper", "grok"] {
        if let Some(p) = which(name) {
            return Ok(p);
        }
    }
    for rel in [
        "target/debug/hyper",
        "target/release/hyper",
        "target/debug/grok",
        "target/release/grok",
    ] {
        let p = PathBuf::from(rel);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(HarnessError::NotInstalled(
        "hyper (or grok) not found; set HYPER_AGENT_BIN".into(),
    ))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Build the `x.ai/interject` ACP `ExtRequest` for a steer.
///
/// Wire method becomes `_x.ai/interject` (the ACP lib prefixes `_`); the grok
/// agent strips the `_` and dispatches to its registered `x.ai/interject`
/// handler, which sends `SessionCommand::Interject` → the turn loop drains it
/// at the next safe point. See `docs/design-acp-steer.md`.
pub fn interject_request(
    session_id: &acp::SessionId,
    text: &str,
    interjection_id: Option<&str>,
) -> Result<acp::ExtRequest, HarnessError> {
    let mut params = serde_json::json!({
        "sessionId": session_id.0.as_ref(),
        "text": text,
    });
    if let Some(id) = interjection_id {
        params["interjectionId"] = serde_json::json!(id);
    }
    // ExtRequest::new takes Arc<RawValue>; build from the JSON value string.
    let raw = acp::RawValue::from_string(params.to_string())
        .map_err(|e| HarnessError::Protocol(format!("interject params: {e}")))?;
    Ok(acp::ExtRequest::new("x.ai/interject", Arc::from(raw)))
}