//! Session summary (title) generation lifecycle.
//!
//! Encapsulates the full lifecycle: check if a summary already exists,
//! generate one via the LLM, persist it, sync to remote, update the
//! session registry, and notify the client. The persistence actor just
//! calls [`SummaryGenerator::update`] — all state transitions are internal.

use crate::extensions::notification::{SessionNotification, SessionUpdate as XaiSessionUpdate};
use crate::sampling::Client as OaiCompatClient;
use crate::session::helpers::session_summary::generate_session_summary;
use crate::session::info::Info;
use crate::session::persistence::PersistenceMsg;
use agent_client_protocol as acp;
use tokio::sync::mpsc;
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;

/// Internal state for the summary generation lifecycle.
enum State {
    /// No summary generated yet. Will attempt on the next [`SummaryGenerator::update`] call.
    Idle,
    /// Summary generation has been attempted (spawned or already on disk). No further work needed.
    Done,
}

/// Dependencies for session title generation and fan-out.
pub(crate) struct SummaryConfig {
    pub(crate) sampling_client: OaiCompatClient,
    pub(crate) model: String,
    /// Channel back to the persistence actor for sequential storage writes.
    /// Weak: a strong sender here would keep the actor's own channel and task alive.
    pub(crate) persistence_tx: mpsc::WeakUnboundedSender<PersistenceMsg>,
}

/// Manages session title generation with explicit lifecycle state.
///
/// Created once per persistence actor. The only public method is [`update`],
/// which is called from the `ContentChunk` handler. Internally it transitions
/// through `Idle -> Done`, spawning the LLM call as a background task and
/// routing the result back through the persistence channel for storage.
pub(crate) struct SummaryGenerator {
    state: State,
    config: SummaryConfig,
}

impl SummaryGenerator {
    pub(crate) fn new(config: SummaryConfig) -> Self {
        Self {
            state: State::Idle,
            config,
        }
    }

    /// Generate a session summary from the first content chunk.
    ///
    /// - **Idle**: checks disk for an existing summary, spawns a background
    ///   task for LLM title generation so the persistence actor is not blocked.
    ///   Empty content is skipped (stays Idle) so the next chunk can retry.
    /// - **Done**: no-op.
    pub(crate) fn update(&mut self, content: String) {
        match self.state {
            State::Done => {}
            State::Idle => {
                // No text to generate a title from (e.g. image-only message).
                // Stay Idle so the next ContentChunk with actual text retries.
                if content.trim().is_empty() {
                    return;
                }

                // Transition to Done so subsequent ContentChunk messages
                // don't spawn duplicate title generation tasks.
                self.state = State::Done;

                let sampling_client = self.config.sampling_client.clone();
                let model = self.config.model.clone();
                let persistence_tx = self.config.persistence_tx.clone();

                // Spawn title generation as a background task so the
                // persistence actor can continue processing messages
                // (updates, flushes) without waiting for the LLM call.
                tokio::spawn(async move {
                    let mut title =
                        generate_session_summary(content.clone(), sampling_client, &model).await;
                    if title.trim().is_empty() {
                        title =
                            crate::session::helpers::session_summary::title_fallback_from_user_text(
                                &content,
                            );
                    }

                    // Route the result through the persistence channel. The
                    // actor persists it (only if the session has no title yet)
                    // and notifies the client there, so a title rejected for
                    // racing a manual `/rename` never reaches the client.
                    match persistence_tx.upgrade() {
                        Some(tx) => {
                            let _ = tx.send(PersistenceMsg::GeneratedTitle(title));
                        }
                        None => tracing::debug!("session closed before its title was generated"),
                    }
                });
            }
        }
    }

    /// Mark as Done (e.g. when disk already has a summary during load).
    pub(crate) fn mark_done(&mut self) {
        self.state = State::Done;
    }
}

/// Notify the client that a session summary is available.
pub(crate) fn notify_client(gateway: &Option<GatewaySender>, info: &Info, title: &str) {
    let Some(gateway) = gateway else {
        return;
    };

    let notification = SessionNotification {
        session_id: info.id.clone(),
        update: XaiSessionUpdate::SessionSummaryGenerated {
            session_summary: title.to_owned(),
        },
        meta: None,
    };
    if let Ok(params) = serde_json::value::to_raw_value(&notification) {
        gateway.forward_fire_and_forget(acp::ExtNotification::new(
            "x.ai/session_notification",
            params.into(),
        ));
    }

    gateway.forward_fire_and_forget(session_info_update(info.id.clone(), title));
}

pub(crate) fn session_info_update(
    session_id: acp::SessionId,
    title: &str,
) -> acp::SessionNotification {
    // `updatedAt` is omitted, not refreshed: renaming is not activity, and
    // `session/list` sorts on `last_active_at`, which a title write never moves.
    acp::SessionNotification::new(
        session_id,
        acp::SessionUpdate::SessionInfoUpdate(
            acp::SessionInfoUpdate::new().title(title.to_owned()),
        ),
    )
}
