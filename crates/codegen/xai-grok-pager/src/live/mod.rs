//! Codex Live (`/live`): real-time bidirectional voice sessions against the
//! Codex sideband, integrated into the pager's session/event-loop/dispatch
//! lifecycle.
//!
//! Layering (pager-owned):
//! - **Live gate** — default **on**. `GROK_CODEX_LIVE=0` or a requirements/config
//!   `[features] codex_live: false` kill switch disables it. Independent of the
//!   xAI `/voice` subscription tier (Codex Live is a separate channel).
//! - **Session scope** — `/live` is bound to `(generation, AgentId, SessionId)`
//!   and stops idempotently on session/view switch, close/exit/quit, ACP
//!   disconnect, and teardown.
//! - **Mutual exclusion with `/voice`** — starting either deterministically
//!   stops/blocks the other.
//! - **Delegation** — `LiveEvent::Delegation` submits literal plain text through
//!   the existing prompt/effect pipeline to the bound current AgentSession,
//!   preserving the draft, and the [`broker::LiveDelegationBroker`] correlates
//!   ACP ingress to flush commentary and send `CompleteDelegation` exactly once.
//! - **UI** — a fixed-height 5-row full-width visualizer replaces the editor
//!   while Live is active (phases, waveform, user transcript, phase footer).
//!
//! ## Feature model
//! The `codex-live` cargo feature forwards `xai-grok-voice/live`. Production
//! code directly uses the real `xai_grok_voice::live` types and
//! `run_live_session`.

pub mod acp_bridge;
pub mod auth;
pub mod broker;
pub mod config;
pub mod gate;
pub mod handle;
pub mod prompts;
pub mod state;
pub mod visualizer;

#[cfg(test)]
mod tests;

// ── Real API re-exports ─────────────────────────────────────────────────────
//
// Directly re-export the real `xai_grok_voice::live` types so all pager
// production code uses the actual API shapes.

pub use xai_grok_voice::live::{
    LiveAuth, LiveAuthProvider, LiveCommand, LiveConfig, LiveContextChannel, LiveEvent, LivePhase,
    LiveRole, SharedLiveAuth, TranscriptKind, run_live_session,
};
