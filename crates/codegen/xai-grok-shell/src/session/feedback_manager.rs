//! Feedback manager for session-level feedback collection.
//!
//! This manager coordinates:
//! - Signal tracking via SessionSignalsHandle
//! - Heuristics evaluation to determine when to request feedback
//! - Periodic sync of signals to the feedback/analytics backend
//! - Background loading of feedback configuration from the backend
//! - Creating feedback request records when triggered
//! - Sending feedback request notifications to clients
//!
//! ## Usage
//! ```ignore
//! // Create the manager when a session starts
//! let manager = FeedbackManager::new(session_id, feedback_api_url, user_token);
//!
//! // Get the signals handle to pass around for event tracking
//! let signals = manager.signals_handle();
//!
//! // Spawn the background sync task (also loads config)
//! tokio::spawn(manager.run_sync_loop());
//!
//! // Track events
//! signals.increment_turn();
//! signals.record_tool_call("read_file");
//!
//! // Check for feedback after each turn
//! // This also records the request with the feedback API if triggered
//! if let Some(request) = manager.maybe_request_feedback(None).await {
//!     // Send FeedbackRequest notification to client
//! }
//! ```

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::RwLock;

use crate::agent::feedback_client::{
    FeedbackApiError, FeedbackClient, signals_to_update, snapshot_to_turn_delta,
};
use crate::session::feedback::{
    FeedbackEvaluation, FeedbackHeuristics, FeedbackRequest, FeedbackTier, TriggerCondition,
};
use crate::session::signals::{SessionSignalsActor, SessionSignalsHandle, TurnDeltaSnapshot};

use prod_mc_cli_chat_proxy_types::feedback_types::{
    ClientType, ContextType, CreateFeedbackRequestInput, FeedbackContent, FeedbackMode,
    FeedbackSubmission, FeedbackToolOutcome,
};

use crate::session::persistence::{LocalFeedbackEntry, PersistenceMsg, UserFeedbackEntry};

pub(crate) enum SubmitOutcome {
    Submitted,
    /// No server configured for this session.
    LocalOnly,
    /// Server request failed.
    Failed(anyhow::Error),
}

/// Shell-crate constructor: `with_content` + `shell_version`.
pub(crate) fn new_submission(
    session_id: String,
    client_type: ClientType,
    content: FeedbackContent,
) -> FeedbackSubmission {
    let mut s = FeedbackSubmission::with_content(session_id, client_type, content);
    s.shell_version = Some(xai_grok_version::VERSION.to_string());
    s
}

#[derive(Debug)]
pub(crate) struct SubmitFeedbackOptions {
    pub solicited: bool,
    pub telemetry_enabled: bool,
    pub author_identity: Option<crate::util::user_identity::ResolvedUserIdentity>,
}

pub(crate) async fn submit_feedback_workflow(
    submission: &mut FeedbackSubmission,
    feedback_client: Option<&FeedbackClient>,
    persistence_tx: Option<&tokio::sync::mpsc::UnboundedSender<PersistenceMsg>>,
    opts: SubmitFeedbackOptions,
) -> SubmitOutcome {
    let SubmitFeedbackOptions {
        solicited,
        telemetry_enabled,
        author_identity,
    } = opts;

    if let Some(user_meta) = crate::agent::mvp_agent::parse_json_object_env("GROK_USER_METADATA") {
        submission.merge_metadata(user_meta);
    }
    // Exhaustive destructure (no `..`) so a new field must be handled, not dropped.
    if let Some(crate::util::user_identity::ResolvedUserIdentity { name, email }) = author_identity
    {
        if let Some(name) = name {
            submission.author_name = Some(name);
        }
        if let Some(email) = email {
            submission.author_email = Some(email);
        }
    }

    if let Some(tx) = persistence_tx {
        let entry = LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
            submitted_at: chrono::Utc::now(),
            session_id: submission.session_id.clone(),
            turn_number: submission.turn_number,
            solicited,
            request_id: submission.request_id.clone(),
            dismissed: false,
            submission: Some(submission.clone()),
        });
        if tx.send(PersistenceMsg::Feedback(entry)).is_err() {
            tracing::warn!(
                session_id = %submission.session_id,
                "feedback persistence channel closed; entry dropped",
            );
        }
    }

    let telemetry_model_id = submission.model_id.clone();
    let telemetry_rating_value = submission.rating_value;
    let telemetry_session_id = submission.session_id.clone();
    let has_feedback_text = submission
        .feedback_text
        .as_ref()
        .is_some_and(|t| !t.is_empty());
    let request_id = submission.request_id.clone();
    let appearance_id = request_id.clone();

    // Send the full submission: the feedback backend surfaces these triage
    // fields, so session context and metadata are intentionally not stripped here.

    let outcome = if let Some(client) = feedback_client {
        let result = if let Some(req_id) = request_id {
            with_one_shot_auth_retry(client, || async {
                client
                    .complete_request(&req_id, submission)
                    .await
                    .map(|_| ())
            })
            .await
        } else {
            with_one_shot_auth_retry(client, || async {
                client.submit_feedback(submission).await.map(|_| ())
            })
            .await
        };
        match result {
            Ok(()) => SubmitOutcome::Submitted,
            Err(e) => {
                tracing::warn!(error = %e, "feedback submission failed");
                SubmitOutcome::Failed(e)
            }
        }
    } else {
        SubmitOutcome::LocalOnly
    };

    if telemetry_enabled {
        let feedback_span = tracing::info_span!(
            "feedback.survey",
            survey_type = "session",
            event_type = "responded",
            appearance_id = %appearance_id.as_deref().unwrap_or(""),
            has_feedback_text = has_feedback_text,
            rating = tracing::field::Empty,
            is_solicited = solicited,
        );
        // Record `rating` only for star ratings; text-only feedback has no
        // rating and must not export a fake 0.
        if let Some(rating) = telemetry_rating_value {
            feedback_span.record("rating", rating);
        }
        feedback_span.in_scope(|| {});
        xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::UserFeedback {
            session_id: telemetry_session_id,
            has_feedback_text,
            model_id: telemetry_model_id,
            rating_value: telemetry_rating_value,
            is_solicited: solicited,
        });
    }

    outcome
}

/// Chat-state fields the session actor passes to [`FeedbackManager::submit_text_feedback`].
pub(crate) struct SessionFeedbackData {
    pub model_id: Option<String>,
    pub resolved_model_id: Option<String>,
    pub client_version: Option<String>,
    pub session_cwd: String,
}

/// Feedback feature flags threaded through session spawn.
#[derive(Debug, Clone, Default)]
pub struct FeedbackFlags {
    pub enabled: bool,
    pub user: Option<crate::agent::config::FeedbackUserConfig>,
}

/// Configuration for the feedback manager.
///
/// Two concerns gated by separate flags (`feedback_enabled`, `telemetry_enabled`).
/// Both default to `false`.
#[derive(Debug, Clone)]
pub struct FeedbackManagerConfig {
    /// Interval for syncing signals to the analytics backend (default: 30s)
    pub sync_interval: Duration,
    /// Whether user-facing feedback features are enabled (popups, `/feedback`,
    /// ratings). Gated by `GROK_FEEDBACK_ENABLED`.
    pub feedback_enabled: bool,
    /// Whether session analytics (signal sync, turn deltas) are enabled.
    /// Gated by `GROK_TELEMETRY_ENABLED`. These are analytics data that
    /// flow continuously without user action.
    pub telemetry_enabled: bool,
    /// Client type (Agent, Tui, Web, Extension)
    pub client_type: ClientType,
    /// Whether LOC attribution tracking is enabled for this session.
    /// Propagated into every `SessionTurnDelta` so the server can
    /// distinguish "tracking off" (zeros are noise) from "tracking on,
    /// no code changed" (zeros are real data).
    pub loc_tracking_enabled: bool,
    /// Timeout for draining the upload queue on shutdown (default: 30s).
    /// If uploads don't complete within this time, remaining items are abandoned.
    pub drain_timeout: Duration,
    pub user: Option<crate::agent::config::FeedbackUserConfig>,
}

impl Default for FeedbackManagerConfig {
    fn default() -> Self {
        Self {
            sync_interval: Duration::from_secs(60),
            feedback_enabled: false,
            telemetry_enabled: false,
            client_type: ClientType::Agent,
            loc_tracking_enabled: false,
            drain_timeout: Duration::from_secs(30),
            user: None,
        }
    }
}

/// Manages feedback collection for a single session.
pub struct FeedbackManager {
    /// Session ID
    session_id: String,
    /// Handle for sending signals (cheap to clone)
    signals_handle: SessionSignalsHandle,
    /// Feedback heuristics evaluator
    heuristics: Arc<RwLock<FeedbackHeuristics>>,
    /// REST client for the feedback/analytics backend
    feedback_client: Option<FeedbackClient>,
    /// Configuration
    config: FeedbackManagerConfig,
    /// Whether config has been loaded from server
    config_loaded: Arc<AtomicBool>,
    /// GCS upload queue stats for periodic snapshots into signals.
    /// Set once after the first upload queue is created via `set_upload_queue_stats()`.
    /// `OnceLock` because `FeedbackManager` is behind `Arc` and this is set after construction.
    upload_queue_stats: std::sync::OnceLock<Arc<xai_file_utils::queue::UploadQueueStats>>,
}

impl FeedbackManager {
    /// Create a new feedback manager for a session.
    ///
    /// If `feedback_client` is None, signal syncing is disabled but local
    /// tracking and heuristics evaluation still work.
    pub fn new(
        session_id: impl Into<String>,
        feedback_client: Option<FeedbackClient>,
        config: FeedbackManagerConfig,
    ) -> Self {
        let (signals_handle, actor) = SessionSignalsActor::with_sync_interval(config.sync_interval);

        // Spawn the signals actor
        tokio::spawn(actor.run());

        let session_id = session_id.into();
        let feedback_client = feedback_client.map(|c| c.with_session_id(session_id.clone()));
        tracing::info!(
            session_id = %session_id,
            feedback_enabled = config.feedback_enabled,
            telemetry_enabled = config.telemetry_enabled,
            has_client = feedback_client.is_some(),
            "FeedbackManager initialized"
        );

        Self {
            session_id,
            signals_handle,
            heuristics: Arc::new(RwLock::new(FeedbackHeuristics::new())),
            feedback_client,
            config,
            config_loaded: Arc::new(AtomicBool::new(false)),
            upload_queue_stats: std::sync::OnceLock::new(),
        }
    }

    /// Create a feedback manager without a REST client (local tracking only).
    pub fn local_only(session_id: impl Into<String>) -> Self {
        Self::new(session_id, None, FeedbackManagerConfig::default())
    }

    /// Attach GCS upload queue stats for periodic snapshotting into signals.
    ///
    /// Called once after the first upload queue is created. The Arc is stored
    /// and read (via atomic loads) before each signal sync to populate GCS
    /// queue metrics. Safe to call from `&self` (behind Arc) via OnceLock.
    pub fn set_upload_queue_stats(&self, stats: Arc<xai_file_utils::queue::UploadQueueStats>) {
        let _ = self.upload_queue_stats.set(stats);
    }

    /// Get a clone of the signals handle for tracking events.
    pub fn signals_handle(&self) -> SessionSignalsHandle {
        self.signals_handle.clone()
    }

    /// Get the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Check if feedback collection is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.feedback_enabled
    }

    /// REST client for the feedback/analytics backend, if configured.
    pub fn feedback_client(&self) -> Option<&FeedbackClient> {
        self.feedback_client.as_ref()
    }

    /// Client type for this session (Agent, Tui, Web, etc.).
    pub fn client_type(&self) -> prod_mc_cli_chat_proxy_types::feedback_types::ClientType {
        self.config.client_type
    }

    /// Build and submit text feedback from the `/feedback` slash command.
    pub(crate) async fn submit_text_feedback(
        &self,
        text: String,
        session_data: SessionFeedbackData,
        persistence_tx: Option<&tokio::sync::mpsc::UnboundedSender<PersistenceMsg>>,
        telemetry_enabled: bool,
    ) -> SubmitOutcome {
        let sh = self.signals_handle();
        let (signals, tool_outcomes) = tokio::join!(sh.snapshot(), sh.last_turn_tool_outcomes());
        let signals = signals.unwrap_or_default();
        let turn_number = signals.turn_count.saturating_sub(1) as i64;
        let tool_outcomes: Vec<FeedbackToolOutcome> = tool_outcomes
            .into_iter()
            .map(|o| FeedbackToolOutcome {
                tool_name: o.tool_name,
                calls: o.successes + o.failures,
                failures: o.failures,
            })
            .collect();

        let mut submission = new_submission(
            self.session_id.clone(),
            self.config.client_type,
            FeedbackContent::Text(text),
        );
        submission.turn_number = Some(turn_number);
        submission.model_id = session_data.model_id;
        submission.resolved_model_id = session_data.resolved_model_id;
        submission.last_user_message = None;
        submission.last_assistant_message = None;
        submission.tool_outcomes = tool_outcomes;
        submission.session_cwd = Some(session_data.session_cwd);
        submission.compaction_count = Some(signals.compaction_count as i64);
        submission.context_window_usage = Some(signals.context_window_usage);
        submission.context_tokens_used = Some(signals.context_tokens_used);
        submission.context_window_tokens = Some(signals.context_window_tokens);
        submission.client_version = session_data.client_version;

        let author_identity =
            crate::util::user_identity::cached_identity(self.config.user.as_ref()).await;

        submit_feedback_workflow(
            &mut submission,
            self.feedback_client.as_ref(),
            persistence_tx,
            SubmitFeedbackOptions {
                solicited: false,
                telemetry_enabled,
                author_identity,
            },
        )
        .await
    }

    /// Check if config has been loaded from the server.
    pub fn is_config_loaded(&self) -> bool {
        self.config_loaded.load(Ordering::Relaxed)
    }

    /// Load feedback heuristics config from the backend.
    /// This is called automatically in run_sync_loop but can be called manually.
    /// Does not block - errors are logged and defaults are used.
    #[tracing::instrument(name = "feedback.load_config", skip_all, fields(
        session_id = %self.session_id,
    ))]
    pub async fn load_config(&self) {
        let Some(client) = &self.feedback_client else {
            return; // No client, use defaults
        };

        if self.config.feedback_enabled {
            match client.get_feedback_config().await {
                Ok(config) => {
                    let mut heuristics = self.heuristics.write().await;
                    heuristics.update_config(&config);
                    self.config_loaded.store(true, Ordering::Relaxed);
                    tracing::info!(
                        session_id = %self.session_id,
                        config_id = %config.config_id,
                        config_version = config.config_version,
                        enabled = config.enabled,
                        "Loaded feedback heuristics config from server"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %self.session_id,
                        error = %e,
                        "Failed to load feedback heuristics config, using defaults"
                    );
                }
            }
        }
    }

    /// Evaluate heuristics and return a FeedbackRequest if one should be sent.
    ///
    /// Call this after each turn to check if feedback should be requested.
    /// Returns None if:
    /// - No tier criteria are met
    /// - The tier was already triggered this session
    /// - Probabilistic sampling says no
    ///
    /// When a request is triggered, this method also creates a record via the
    /// feedback API for tracking and analytics.
    #[tracing::instrument(name = "feedback.maybe_request_feedback", skip_all, fields(
        session_id = %self.session_id,
    ))]
    pub async fn maybe_request_feedback(
        &self,
        prompt_id: Option<String>,
    ) -> Option<FeedbackRequest> {
        if !self.config.feedback_enabled {
            return None;
        }

        let signals = self.signals_handle.snapshot().await?;
        let mut heuristics = self.heuristics.write().await;

        // Check if heuristics are globally enabled (from server config)
        if !heuristics.is_enabled() {
            return None;
        }

        let eval = heuristics.evaluate(&signals);

        if let (true, Some(trigger_condition)) =
            (eval.should_request, eval.trigger_condition.as_ref())
        {
            let tier = trigger_condition.tier;
            // Use the feedback mode configured for this tier
            let feedback_mode = heuristics.feedback_mode(tier);
            let dismissible = heuristics.dismissible(tier);
            let prompt = heuristics.prompt(tier);
            let request = FeedbackRequest::with_mode(
                self.session_id.clone(),
                trigger_condition.clone(),
                feedback_mode,
                dismissible,
                Some(prompt),
            );
            tracing::info!(
                session_id = %self.session_id,
                tier = ?request.tier,
                trigger_type = %request.trigger_type,
                feedback_mode = ?request.feedback_mode,
                "Feedback request triggered"
            );

            self.record_feedback_request(&request, trigger_condition, feedback_mode, prompt_id)
                .await;

            return Some(request);
        }

        None
    }

    /// Force check heuristics without sampling (for testing).
    /// Returns the evaluation result.
    pub async fn evaluate_heuristics(&self) -> Option<FeedbackEvaluation> {
        let signals = self.signals_handle.snapshot().await?;
        let mut heuristics = self.heuristics.write().await;
        Some(heuristics.evaluate(&signals))
    }

    /// Force-generate a feedback request for local testing, bypassing all
    /// heuristics, sampling, cooldown, and enabled checks.
    ///
    /// Engineers developing clients can call this via the
    /// `x.ai/debug/trigger_feedback` ACP extension method to exercise
    /// the full feedback notification ↔ response flow without needing a
    /// real session that meets tier criteria.
    ///
    /// When a `feedback_client` is configured, the request is also recorded
    /// via the feedback API — exactly like a real trigger — so that the
    /// subsequent `complete_request` / `dismiss_request` round-trip from the
    /// client works end-to-end.
    #[tracing::instrument(name = "feedback.force_feedback_request", skip_all, fields(
        session_id = %self.session_id,
    ))]
    pub async fn force_feedback_request(
        &self,
        tier: FeedbackTier,
        mode: FeedbackMode,
    ) -> FeedbackRequest {
        use crate::session::feedback::TriggerSignalSnapshot;

        // Build a synthetic trigger condition that makes it obvious this was
        // manually triggered for testing purposes.
        let condition = TriggerCondition {
            tier,
            condition: "debug/trigger_feedback (manual test trigger)".to_string(),
            signal_snapshot: TriggerSignalSnapshot {
                turn_count: 0,
                tool_calls_count: 0,
                compactions_count: 0,
                errors_count: 0,
                cancellations_count: 0,
                has_reverted: false,
            },
        };

        // Manual/debug triggers are always dismissible regardless of tier config,
        // since they exist for developer testing, not real user feedback collection.
        let request = FeedbackRequest::with_mode(
            self.session_id.clone(),
            condition.clone(),
            mode,
            true,
            None,
        );

        self.record_feedback_request(&request, &condition, mode, None)
            .await;

        request
    }

    /// Record a feedback request via the feedback API.
    ///
    /// This is a best-effort operation — errors are logged but do not
    /// prevent the request from being sent to the client.
    #[tracing::instrument(name = "feedback.record_feedback_request", skip_all, fields(
        session_id = %self.session_id,
    ))]
    async fn record_feedback_request(
        &self,
        request: &FeedbackRequest,
        trigger_condition: &TriggerCondition,
        feedback_mode: FeedbackMode,
        prompt_id: Option<String>,
    ) {
        let Some(client) = &self.feedback_client else {
            return;
        };

        let input = CreateFeedbackRequestInput {
            request_id: request.request_id.clone(),
            session_id: self.session_id.clone(),
            client_type: self.config.client_type,
            feedback_mode,
            feedback_prompt: Some(request.prompt.clone()),
            priority: tier_to_priority(trigger_condition.tier),
            trigger_type: request.trigger_type.clone(),
            trigger_reason: Some(trigger_condition.trigger_reason()),
            context_type: Some(ContextType::Session),
            context_message_ids: vec![],
            expires_at: None,
            experiment_id: None,
            trigger_condition: serde_json::to_value(trigger_condition).ok(),
            prompt_id,
        };

        match with_one_shot_auth_retry(client, || client.create_feedback_request(&input)).await {
            Ok(response) => {
                tracing::debug!(
                    request_id = %response.request_id,
                    "Feedback request recorded with feedback API"
                );
            }
            Err(e) => {
                tracing::warn!(
                    request_id = %request.request_id,
                    error = %e,
                    "Failed to record feedback request (continuing anyway)"
                );
            }
        }
    }

    /// Capture a turn-end snapshot and send the delta to the analytics backend.
    ///
    /// Call this once per user turn, after the agent has finished all tool-call
    /// rounds and produced a final response (i.e. alongside `record_turn_complete`).
    /// Intermediate tool-call steps within the same turn do NOT need their own
    /// call — the signals actor accumulates tool calls, errors, and latency
    /// continuously, so the single snapshot at turn end captures the full diff.
    ///
    /// The caller provides a pre-captured `TurnDeltaSnapshot` (taken exactly
    /// once inside the session actor). This avoids double-advancing the delta
    /// baseline.  If the snapshot is `None` (e.g. the signals actor was shut
    /// down), this is a no-op.
    ///
    /// The delta is converted and sent asynchronously to the backend. Errors
    /// are logged but never block the turn flow.
    ///
    /// `request_id` is the prompt/request identifier for the turn.
    #[tracing::instrument(skip_all, fields(session_id = %self.session_id))]
    pub async fn send_turn_delta_with_snapshot(
        &self,
        snapshot: Option<TurnDeltaSnapshot>,
        request_id: Option<String>,
        turn_duration_ms: Option<i64>,
        turn_outcome: Option<String>,
        model_fingerprint: Option<String>,
    ) {
        if !self.config.telemetry_enabled {
            tracing::debug!("Turn delta skipped: telemetry is disabled");
            return;
        }

        tracing::debug!("Turn delta: sending pre-captured snapshot");

        let Some(client) = &self.feedback_client else {
            tracing::debug!("Turn delta skipped: no feedback client configured");
            return;
        };

        let Some(snapshot) = snapshot else {
            tracing::debug!("Turn delta skipped: no snapshot available (signals actor shut down)");
            return;
        };

        let (fb_requests_sent, fb_last_request_at) = {
            let h = self.heuristics.read().await;
            (h.requests_sent(), h.last_request_at())
        };
        let delta = snapshot_to_turn_delta(
            &snapshot,
            self.config.client_type,
            request_id,
            fb_requests_sent,
            fb_last_request_at,
            self.config.loc_tracking_enabled,
            turn_duration_ms,
            turn_outcome,
            model_fingerprint,
        );

        let session_id = self.session_id.clone();
        let client = client.clone();
        // Fire-and-forget: send in background so we never block the turn.
        tokio::spawn(async move {
            let result =
                with_one_shot_auth_retry(&client, || client.send_turn_delta(&session_id, &delta))
                    .await;
            match result {
                Ok(_) => {
                    tracing::debug!(
                        session_id = %session_id,
                        turn = delta.turn_number,
                        "Turn delta sent to analytics backend"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %session_id,
                        turn = delta.turn_number,
                        error = %e,
                        "Failed to send turn delta (non-fatal)"
                    );
                }
            }
        });
    }

    /// Sync current signals to the analytics backend.
    /// Returns Ok(()) if sync succeeded or was skipped (no client).
    pub async fn sync_signals(&self) -> anyhow::Result<()> {
        self.sync_signals_inner(false).await
    }

    /// Force-sync current signals to the analytics backend, bypassing the cooldown check.
    /// Used for the final sync on shutdown to ensure no data is lost.
    pub async fn force_sync_signals(&self) -> anyhow::Result<()> {
        self.sync_signals_inner(true).await
    }

    /// Inner sync implementation with optional cooldown bypass.
    async fn sync_signals_inner(&self, force: bool) -> anyhow::Result<()> {
        if !self.config.telemetry_enabled {
            return Ok(());
        }

        let Some(client) = &self.feedback_client else {
            return Ok(()); // No client, skip sync
        };

        // Check if sync is needed (skip cooldown check if forced)
        if !force && !self.signals_handle.check_and_mark_sync().await {
            return Ok(()); // Not time to sync yet
        }

        // Snapshot GCS upload queue stats into signals before taking the snapshot.
        // This ensures the sync payload includes the latest queue metrics.
        if let Some(stats) = self.upload_queue_stats.get() {
            self.signals_handle.snapshot_gcs_queue(stats);
        }

        let Some(signals) = self.signals_handle.snapshot().await else {
            return Ok(()); // Actor shut down
        };

        let update = signals_to_update(&signals, self.config.client_type);

        match client.update_signals(&self.session_id, &update).await {
            Ok(_) => {
                tracing::debug!(
                    session_id = %self.session_id,
                    turns = signals.turn_count,
                    "Synced session signals to analytics backend"
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "Failed to sync session signals"
                );
                Err(e)
            }
        }
    }

    /// Attempt OIDC token refresh after a 401 and retry the signal sync once.
    /// Returns the classified outcome for `handle_auth_outcome` to act on.
    ///
    /// Prefers waiting for the proactive-refresh task or main-request-path
    /// recovery over driving a `ServerRejected` refresh itself.  This
    /// prevents the signals loop from amplifying 401 bursts at the API
    /// during token-expiry windows.
    async fn try_refresh_and_retry_sync(&self) -> SyncAuthOutcome {
        let Some(client) = &self.feedback_client else {
            return SyncAuthOutcome::Unrecoverable;
        };
        if !client.has_token_refresher() {
            return SyncAuthOutcome::Unrecoverable;
        }
        // 1. Wait briefly for the proactive refresh or main-path recovery
        //    to land a fresh token before driving our own ServerRejected.
        let refreshed = client.wait_for_token_refresh(Duration::from_secs(3)).await;
        // 2. If nobody refreshed, drive our own recovery as fallback.
        if !refreshed && !client.try_refresh_credentials().await {
            if client.is_auth_permanently_failed() {
                return SyncAuthOutcome::Permanent;
            }
            return SyncAuthOutcome::Transient;
        }
        // Retry with fresh token. Any error after a successful refresh
        // counts as a transient signals-failed-to-land tick: this keeps
        // the safety net alive on a pathological `401 → refresh OK → 5xx`
        // flap and on the rare sibling-rotation race where the IdP cache
        // got re-set between our refresh and our retry.
        match self.sync_signals().await {
            Ok(_) => SyncAuthOutcome::Recovered,
            Err(e) if client.is_auth_permanently_failed() => {
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "Signal sync retry failed; IdP confirmed permanent failure"
                );
                SyncAuthOutcome::Permanent
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "Signal sync retry failed after successful token refresh"
                );
                SyncAuthOutcome::Transient
            }
        }
    }

    /// Run a background loop that periodically syncs signals.
    /// Also loads feedback heuristics config on startup.
    /// This should be spawned as a background task.
    #[tracing::instrument(skip_all, fields(session_id = %self.session_id))]
    pub async fn run_sync_loop(self: Arc<Self>, cancel: tokio_util::sync::CancellationToken) {
        // Load config in background (non-blocking, errors logged)
        self.load_config().await;

        let mut interval = tokio::time::interval(self.config.sync_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut consecutive_auth_failures: u8 = 0;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::debug!("Feedback sync loop cancelled");
                    // Final sync before exiting — force-bypass cooldown to ensure latest signals are persisted
                    let _ = self.force_sync_signals().await;
                    break;
                }
                _ = interval.tick() => {
                    match self.sync_signals().await {
                        Ok(_) => {
                            consecutive_auth_failures = 0;
                        }
                        Err(e) if is_forbidden_error(&e) => {
                            tracing::warn!(
                                session_id = %self.session_id,
                                error = %e,
                                "Signals sync loop stopped: 403 forbidden (session ownership mismatch)"
                            );
                            xai_grok_telemetry::unified_log::warn(
                                "signals sync loop stopped: 403 forbidden",
                                Some(&self.session_id),
                                None,
                            );
                            break;
                        }
                        Err(e) if is_auth_error(&e) => {
                            let outcome = self.try_refresh_and_retry_sync().await;
                            if handle_auth_outcome(
                                outcome,
                                &mut consecutive_auth_failures,
                                &self.session_id,
                            )
                            .is_break()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                session_id = %self.session_id,
                                error = %e,
                                "Periodic signal sync failed"
                            );
                            // Non-auth error does not touch the counter.
                        }
                    }
                }
            }
        }
    }

    /// Shutdown the manager, performing a final sync and draining the upload queue.
    ///
    /// This ensures:
    /// 1. Final signal sync to the analytics backend (bypass cooldown)
    /// 2. Upload queue is drained (pending uploads complete before exit)
    /// 3. Signals actor is shut down
    ///
    /// The drain uses a configurable timeout (`config.drain_timeout`) to avoid
    /// hanging indefinitely on stuck uploads. Items not uploaded within the
    /// timeout are abandoned with a warning log.
    ///
    /// The caller passes the upload queue from `SessionHandle` — the
    /// `FeedbackManager` no longer owns the queue.
    pub async fn shutdown(&self, queue: Option<&xai_file_utils::queue::UploadQueue>) {
        // Final sync — force-bypass cooldown
        let _ = self.force_sync_signals().await;

        // Drain the upload queue to ensure pending uploads complete before exit.
        // Uses configurable timeout to avoid hanging indefinitely on stuck uploads.
        if let Some(queue) = queue {
            let remaining = queue.drain(self.config.drain_timeout).await;
            if remaining > 0 {
                let pending_bytes = queue.stats().pending_bytes.load(Ordering::Relaxed);
                tracing::warn!(
                    session_id = %self.session_id,
                    remaining,
                    pending_bytes,
                    "Upload queue drain incomplete, {} items abandoned ({} bytes pending)",
                    remaining,
                    pending_bytes
                );
            } else {
                tracing::debug!(
                    session_id = %self.session_id,
                    "Upload queue drained successfully"
                );
            }
        }

        // Shutdown the signals actor
        self.signals_handle.shutdown();
    }
}

// Auth outcome handler used by run_sync_loop on 401.

/// Max consecutive failed sync ticks tolerated before stopping the loop.
/// ~10 minutes at the default 60s interval.
const MAX_CONSECUTIVE_AUTH_FAILURES: u8 = 10;

/// telemetry `reason` discriminators on the `signals sync loop stopped permanently`
/// event. Pinned because alerts filter on these strings.
const REASON_AUTH_PERMANENT_FAILURE: &str = "auth_permanent_failure";
const REASON_NO_CLIENT_OR_REFRESHER: &str = "no_client_or_refresher";

const LOG_TITLE_TRANSIENT: &str = "signals sync transient auth failure";
const LOG_TITLE_STOPPED_PERMANENTLY: &str = "signals sync loop stopped permanently";

/// Classification of one 401-recovery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncAuthOutcome {
    /// Refresh + retry succeeded.
    Recovered,
    /// Refresh or retry failed transiently (lock timeout, network, sibling
    /// race, post-refresh 5xx). Increment the counter and retry next tick.
    Transient,
    /// IdP confirmed a terminal failure (`invalid_grant` / `invalid_client`).
    /// Only re-login will recover.
    Permanent,
    /// No client or no refresher configured — nothing to retry.
    Unrecoverable,
}

fn handle_auth_outcome(
    outcome: SyncAuthOutcome,
    consecutive_auth_failures: &mut u8,
    session_id: &str,
) -> ControlFlow<()> {
    match outcome {
        SyncAuthOutcome::Recovered => {
            *consecutive_auth_failures = 0;
            tracing::info!(
                session_id = %session_id,
                "Signal sync recovered after token refresh"
            );
            ControlFlow::Continue(())
        }
        SyncAuthOutcome::Transient => {
            *consecutive_auth_failures = consecutive_auth_failures.saturating_add(1);
            tracing::warn!(
                session_id = %session_id,
                consecutive_failures = *consecutive_auth_failures,
                max = MAX_CONSECUTIVE_AUTH_FAILURES,
                "Signals sync transient auth failure"
            );
            xai_grok_telemetry::unified_log::warn(
                LOG_TITLE_TRANSIENT,
                Some(session_id),
                Some(serde_json::json!({
                    "consecutive_failures": *consecutive_auth_failures,
                    "max": MAX_CONSECUTIVE_AUTH_FAILURES,
                })),
            );
            if *consecutive_auth_failures >= MAX_CONSECUTIVE_AUTH_FAILURES {
                tracing::warn!(
                    session_id = %session_id,
                    consecutive_failures = *consecutive_auth_failures,
                    "Signals sync loop stopped: consecutive transient auth failures"
                );
                xai_grok_telemetry::unified_log::warn(
                    "signals sync loop stopped: consecutive transient auth failures",
                    Some(session_id),
                    Some(serde_json::json!({
                        "consecutive_failures": *consecutive_auth_failures,
                        "max": MAX_CONSECUTIVE_AUTH_FAILURES,
                    })),
                );
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        }
        SyncAuthOutcome::Permanent => {
            tracing::warn!(
                session_id = %session_id,
                reason = REASON_AUTH_PERMANENT_FAILURE,
                "Signals sync loop stopped: IdP confirmed permanent auth failure"
            );
            xai_grok_telemetry::unified_log::warn(
                LOG_TITLE_STOPPED_PERMANENTLY,
                Some(session_id),
                Some(serde_json::json!({ "reason": REASON_AUTH_PERMANENT_FAILURE })),
            );
            ControlFlow::Break(())
        }
        SyncAuthOutcome::Unrecoverable => {
            tracing::warn!(
                session_id = %session_id,
                reason = REASON_NO_CLIENT_OR_REFRESHER,
                "Signals sync loop stopped: no client or no refresher configured"
            );
            xai_grok_telemetry::unified_log::warn(
                LOG_TITLE_STOPPED_PERMANENTLY,
                Some(session_id),
                Some(serde_json::json!({ "reason": REASON_NO_CLIENT_OR_REFRESHER })),
            );
            ControlFlow::Break(())
        }
    }
}

/// Check if an error is an HTTP 401 Unauthorized response.
///
/// Uses typed downcast on [`FeedbackApiError`] instead of string matching,
/// so it stays correct even if error messages change.
fn is_auth_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<FeedbackApiError>()
        .is_some_and(|e| e.is_unauthorized())
}

/// Check if an error is an HTTP 403 Forbidden response.
///
/// 403 from the signals endpoint means the session does not belong to the
/// current user — a permanent condition that will never self-resolve.
fn is_forbidden_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<FeedbackApiError>()
        .is_some_and(|e| e.is_forbidden())
}

/// Run `op` once; on 401, wait for an in-flight refresh to land, then
/// retry once.  Prefers waiting for the proactive-refresh task or
/// main-request-path recovery over driving a `ServerRejected` refresh
/// itself, avoiding the 401-amplification pattern during token-expiry
/// windows.
async fn with_one_shot_auth_retry<T, F, Fut>(
    client: &FeedbackClient,
    mut op: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    match op().await {
        Ok(v) => Ok(v),
        Err(e) if is_auth_error(&e) => {
            // 1. Wait briefly for the proactive refresh or main-path
            //    recovery to land a fresh token.
            let refreshed = client.wait_for_token_refresh(Duration::from_secs(3)).await;
            // 2. If nobody refreshed, drive our own recovery as fallback.
            if refreshed || client.try_refresh_credentials().await {
                op().await
            } else {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

/// Convert a FeedbackTier to a priority value (1-10, higher = more important).
fn tier_to_priority(tier: crate::session::feedback::FeedbackTier) -> i32 {
    use crate::session::feedback::FeedbackTier;
    match tier {
        FeedbackTier::Tier1 => 5, // Standard engagement
        FeedbackTier::Tier2 => 6, // Complex session with recovery
        FeedbackTier::Tier3 => 7, // Recovery from friction
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_feedback_manager_local_only() {
        let manager = FeedbackManager::local_only("test-session-123");

        // Track some events
        let signals = manager.signals_handle();
        for _ in 0..10 {
            signals.increment_turn();
        }
        for _ in 0..5 {
            signals.record_tool_call("read_file");
        }
        for _ in 0..2 {
            signals.record_compaction(10_000);
        }

        // Give time for actor to process
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Check signals were tracked
        let snapshot = signals.snapshot().await.unwrap();
        assert_eq!(snapshot.turn_count, 10);
        assert_eq!(snapshot.tool_call_count, 5);
        assert_eq!(snapshot.compaction_count, 2);

        // Evaluate heuristics - should trigger Tier 1
        let eval = manager.evaluate_heuristics().await.unwrap();
        assert!(eval.trigger_condition.is_some());
        assert_eq!(
            eval.trigger_condition.as_ref().unwrap().tier,
            crate::session::feedback::FeedbackTier::Tier1
        );

        manager.shutdown(None).await;
    }

    #[test]
    fn test_is_auth_error_detects_401() {
        use crate::agent::feedback_client::FeedbackApiError;
        let err: anyhow::Error = FeedbackApiError {
            status: reqwest::StatusCode::UNAUTHORIZED,
            context: "Signals update",
            body: "Invalid or expired credentials".to_string(),
        }
        .into();
        assert!(is_auth_error(&err));
    }

    #[test]
    fn test_is_auth_error_ignores_other_statuses() {
        use crate::agent::feedback_client::FeedbackApiError;
        let err_500: anyhow::Error = FeedbackApiError {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            context: "Signals update",
            body: "oops".to_string(),
        }
        .into();
        assert!(!is_auth_error(&err_500));

        let err_403: anyhow::Error = FeedbackApiError {
            status: reqwest::StatusCode::FORBIDDEN,
            context: "Signals update",
            body: "ZDR team".to_string(),
        }
        .into();
        assert!(!is_auth_error(&err_403));
    }

    #[test]
    fn test_is_auth_error_ignores_non_api_errors() {
        assert!(!is_auth_error(&anyhow::anyhow!("network timeout")));
        assert!(!is_auth_error(&anyhow::anyhow!("connection refused")));
    }

    #[test]
    fn test_is_forbidden_error_detects_403() {
        use crate::agent::feedback_client::FeedbackApiError;
        let err: anyhow::Error = FeedbackApiError {
            status: reqwest::StatusCode::FORBIDDEN,
            context: "Signals update",
            body: "Access denied: session does not belong to this user".to_string(),
        }
        .into();
        assert!(is_forbidden_error(&err));
    }

    #[test]
    fn test_is_forbidden_error_ignores_other_statuses() {
        use crate::agent::feedback_client::FeedbackApiError;
        let err_401: anyhow::Error = FeedbackApiError {
            status: reqwest::StatusCode::UNAUTHORIZED,
            context: "Signals update",
            body: "Invalid credentials".to_string(),
        }
        .into();
        assert!(!is_forbidden_error(&err_401));
        assert!(!is_forbidden_error(&anyhow::anyhow!("network error")));
    }

    #[test]
    fn test_is_auth_error_works_through_anyhow_conversion() {
        use crate::agent::feedback_client::FeedbackApiError;
        // Verify the FeedbackApiError survives anyhow::Error round-trip
        // (this is the actual path: send_json returns FeedbackApiError.into())
        let api_err = FeedbackApiError {
            status: reqwest::StatusCode::UNAUTHORIZED,
            context: "Signals update",
            body: "token expired".to_string(),
        };
        let anyhow_err: anyhow::Error = api_err.into();
        assert!(is_auth_error(&anyhow_err));
    }

    #[tokio::test]
    async fn test_feedback_manager_disabled() {
        let config = FeedbackManagerConfig {
            feedback_enabled: false,
            ..Default::default()
        };
        let manager = FeedbackManager::new("test-session", None, config);

        // Even with signals, disabled manager should not request feedback
        let signals = manager.signals_handle();
        for _ in 0..20 {
            signals.increment_turn();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;

        let request = manager.maybe_request_feedback(None).await;
        assert!(request.is_none());

        manager.shutdown(None).await;
    }

    #[tokio::test]
    async fn test_shutdown_without_upload_queue_completes() {
        // Verify that shutdown() completes successfully when no upload queue is set.
        // This tests the None path in the drain logic.
        let manager = FeedbackManager::local_only("test-session-no-queue");

        // Shutdown should complete without errors even without an upload queue
        manager.shutdown(None).await;

        // Verify signals actor was shut down (snapshot returns None after shutdown)
        let snapshot = manager.signals_handle().snapshot().await;
        assert!(snapshot.is_none(), "Signals actor should be shut down");
    }

    #[tokio::test]
    async fn test_shutdown_with_upload_queue_drains() {
        use crate::session::repo_changes::{TraceExportConfig, UploadMethod};
        use std::sync::Arc;
        use xai_file_utils::queue::{TraceExportSource, UploadQueue, UploadRetryPolicy};

        // Create a mock resolver for the queue
        struct MockResolver;
        impl TraceExportSource for MockResolver {
            fn resolve(&self) -> TraceExportConfig {
                TraceExportConfig {
                    bucket_url: Some("gs://test-bucket".to_string()),
                    service_account_key: None,
                    upload_method: UploadMethod::Direct {
                        service_account_key: None,
                    },
                    prefix_dir: None,
                    gcs_prefix: None,
                    absolute_paths: false,
                    archive_name_override: None,
                }
            }
        }

        let temp = tempfile::TempDir::new().unwrap();
        let queue = UploadQueue::spawn(
            temp.path(),
            Arc::new(MockResolver),
            UploadRetryPolicy::default(),
        );

        let manager = FeedbackManager::local_only("test-session-with-queue");

        // Shutdown should complete and drain the queue
        manager.shutdown(Some(&queue)).await;

        // Verify signals actor was shut down
        let snapshot = manager.signals_handle().snapshot().await;
        assert!(snapshot.is_none(), "Signals actor should be shut down");
    }

    #[tokio::test]
    async fn test_drain_timeout_is_configurable() {
        // Verify that drain_timeout can be customized via config
        let config = FeedbackManagerConfig {
            drain_timeout: Duration::from_secs(5), // Custom timeout
            ..Default::default()
        };
        let manager = FeedbackManager::new("test-session-custom-timeout", None, config.clone());

        // Verify config was stored
        assert_eq!(manager.config.drain_timeout, Duration::from_secs(5));

        manager.shutdown(None).await;
    }

    // ── handle_auth_outcome tests ──────────────────────────────────────────

    /// 9 transient failures must NOT break, and a subsequent `Recovered`
    /// must reset the counter to 0.
    #[test]
    fn test_sync_loop_continues_through_transient_auth_failures() {
        let mut counter: u8 = 0;
        for _ in 0..(MAX_CONSECUTIVE_AUTH_FAILURES - 1) {
            let flow = handle_auth_outcome(SyncAuthOutcome::Transient, &mut counter, "s");
            assert_eq!(flow, ControlFlow::Continue(()));
        }
        assert_eq!(counter, MAX_CONSECUTIVE_AUTH_FAILURES - 1);
        let flow = handle_auth_outcome(SyncAuthOutcome::Recovered, &mut counter, "s");
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(counter, 0, "Recovered must reset the counter");
    }

    /// Exactly `MAX_CONSECUTIVE_AUTH_FAILURES` consecutive `Transient`
    /// outcomes break the loop.
    #[test]
    fn test_sync_loop_breaks_after_max_transient_auth_failures() {
        let mut counter: u8 = 0;
        for i in 0..(MAX_CONSECUTIVE_AUTH_FAILURES - 1) {
            let flow = handle_auth_outcome(SyncAuthOutcome::Transient, &mut counter, "s");
            assert_eq!(
                flow,
                ControlFlow::Continue(()),
                "iteration {i} should still continue"
            );
        }
        // The 10th (== MAX_CONSECUTIVE_AUTH_FAILURES) transient breaks.
        let flow = handle_auth_outcome(SyncAuthOutcome::Transient, &mut counter, "s");
        assert_eq!(flow, ControlFlow::Break(()));
        assert_eq!(counter, MAX_CONSECUTIVE_AUTH_FAILURES);
    }

    /// `Permanent` breaks immediately and does not bump the counter.
    #[test]
    fn test_sync_loop_breaks_immediately_on_permanent_failure() {
        let mut counter: u8 = 0;
        let flow = handle_auth_outcome(SyncAuthOutcome::Permanent, &mut counter, "s");
        assert_eq!(flow, ControlFlow::Break(()));
        assert_eq!(counter, 0);
    }

    /// 5 transient → 1 recovered → 5 transient must not break.
    #[test]
    fn test_sync_loop_counter_resets_on_successful_sync() {
        let mut counter: u8 = 0;
        for _ in 0..5 {
            assert_eq!(
                handle_auth_outcome(SyncAuthOutcome::Transient, &mut counter, "s"),
                ControlFlow::Continue(())
            );
        }
        assert_eq!(counter, 5);
        assert_eq!(
            handle_auth_outcome(SyncAuthOutcome::Recovered, &mut counter, "s"),
            ControlFlow::Continue(())
        );
        assert_eq!(counter, 0, "Recovered must reset the counter");
        for _ in 0..5 {
            assert_eq!(
                handle_auth_outcome(SyncAuthOutcome::Transient, &mut counter, "s"),
                ControlFlow::Continue(())
            );
        }
        assert_eq!(counter, 5, "second burst should be re-counted from zero");
    }

    /// `Unrecoverable` breaks the loop and does not bump the counter.
    #[test]
    fn test_sync_loop_breaks_on_unrecoverable() {
        let mut counter: u8 = 0;
        let flow = handle_auth_outcome(SyncAuthOutcome::Unrecoverable, &mut counter, "s");
        assert_eq!(flow, ControlFlow::Break(()));
        assert_eq!(counter, 0);
    }

    /// `FeedbackClient::is_auth_permanently_failed` reflects the attached
    /// `AuthManager`'s `permanent_failure()` cache (record → true,
    /// age-out → false).
    #[tokio::test]
    async fn test_is_auth_permanently_failed_reads_auth_manager() {
        use crate::agent::feedback_client::FeedbackClient;
        use crate::auth::error::RefreshTokenFailedReason;
        use crate::auth::{AuthManager, GrokAuth, GrokComConfig};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
        let client = FeedbackClient::new("http://example/v1", None).with_auth_manager(am.clone());

        assert!(!client.is_auth_permanently_failed());

        // The verdict is scoped to the live credential's key.
        am.hot_swap(GrokAuth {
            key: "tok".into(),
            ..GrokAuth::test_default()
        });
        // Use a non-sticky reason: only recoverable verdicts age out (a sticky
        // `RefreshTokenRejected` never expires), and this exercises the TTL path.
        am.record_permanent_failure("tok".to_string(), RefreshTokenFailedReason::Other.into());
        assert!(client.is_auth_permanently_failed());

        am.force_permanent_failure_aged_out();
        assert!(!client.is_auth_permanently_failed());
    }

    /// With no `AuthManager` attached, `is_auth_permanently_failed` is false.
    #[test]
    fn test_is_auth_permanently_failed_without_auth_manager() {
        use crate::agent::feedback_client::FeedbackClient;
        let client = FeedbackClient::new("http://example/v1", None);
        assert!(!client.is_auth_permanently_failed());
    }

    /// `has_token_refresher` requires BOTH an `AuthManager` AND a refresher
    /// wired in. Without this, a static-deployment-key session would be
    /// mis-classified as recoverable.
    #[tokio::test]
    async fn test_has_token_refresher_requires_refresher_attached() {
        use crate::agent::feedback_client::FeedbackClient;
        use crate::auth::{AuthManager, GrokComConfig};
        use std::sync::Arc;

        struct NoOpRefresher;
        #[async_trait::async_trait]
        impl crate::auth::refresh::TokenRefresher for NoOpRefresher {
            async fn refresh(
                &self,
                _reason: crate::auth::refresh::RefreshReason,
            ) -> crate::auth::refresh::RefreshOutcome {
                crate::auth::refresh::RefreshOutcome::TransientFailure {
                    message: "noop".into(),
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));

        let bare = FeedbackClient::new("http://example/v1", None);
        assert!(!bare.has_token_refresher());

        let with_am = FeedbackClient::new("http://example/v1", None).with_auth_manager(am.clone());
        assert!(
            !with_am.has_token_refresher(),
            "AuthManager without a refresher must NOT be reported as recoverable"
        );

        am.set_refresher(std::sync::Arc::new(NoOpRefresher));
        assert!(with_am.has_token_refresher());
    }
}

#[cfg(test)]
mod author_identity_tests {
    use super::*;
    use crate::util::user_identity::ResolvedUserIdentity;
    use axum::{Router, routing::post};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    /// Mock feedback backend: capture the POST /v1/feedback JSON body.
    async fn start_capture_server() -> (
        SocketAddr,
        Arc<parking_lot::Mutex<Option<serde_json::Value>>>,
    ) {
        let captured = Arc::new(parking_lot::Mutex::new(None::<serde_json::Value>));
        let captured_for_handler = captured.clone();
        let router = Router::new().route(
            "/v1/feedback",
            post(move |body: axum::Json<serde_json::Value>| {
                let captured = captured_for_handler.clone();
                async move {
                    *captured.lock() = Some(body.0);
                    axum::Json(serde_json::json!({
                        "feedbackId": "fb-1",
                        "createdAt": chrono::Utc::now(),
                    }))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (addr, captured)
    }

    fn text_submission() -> FeedbackSubmission {
        let mut s = new_submission(
            "sess-1".to_string(),
            ClientType::Tui,
            FeedbackContent::Text("great session".to_string()),
        );
        s.model_id = Some("grok-4".to_string());
        s
    }

    /// End-to-end: an env var (as a device-management launcher would inject)
    /// referenced by `[feedback.user]` with `$VAR` is expanded at config load,
    /// resolved, carried on the feedback POST alongside the rest of the
    /// submission, and retained on the local entry.
    #[tokio::test]
    #[serial_test::serial]
    async fn env_var_identity_reaches_the_wire_end_to_end() {
        let _email =
            xai_grok_test_support::env::EnvGuard::set("GROK_TEST_WORK_EMAIL", "ada@corp.example");
        let _name =
            xai_grok_test_support::env::EnvGuard::set("GROK_TEST_WORK_NAME", "Ada Lovelace");

        // The loader expands `$VAR` at load, exactly as a trusted config tier ships it.
        let mut value = toml::from_str::<toml::Value>(
            r#"
[feedback.user]
name = ["$GROK_TEST_WORK_NAME"]
email = ["$GROK_TEST_WORK_EMAIL"]
"#,
        )
        .unwrap();
        crate::config::expand_env_vars_in_toml(&mut value);
        let cfg = crate::agent::config::Config::new_from_toml_cfg(&value).unwrap();
        let user = cfg.feedback.user.expect("[feedback.user] present");

        // Resolve through the real production entry point.
        let identity = crate::util::user_identity::cached_identity(Some(&user))
            .await
            .expect("identity resolved");
        assert_eq!(identity.name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(identity.email.as_deref(), Some("ada@corp.example"));

        let (addr, captured) = start_capture_server().await;
        let client = crate::agent::feedback_client::FeedbackClient::with_client(
            reqwest::Client::new(),
            format!("http://{addr}/v1"),
            Some("tok".into()),
        );
        let mut submission = text_submission();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = submit_feedback_workflow(
            &mut submission,
            Some(&client),
            Some(&tx),
            SubmitFeedbackOptions {
                solicited: false,
                telemetry_enabled: false,
                author_identity: Some(identity),
            },
        )
        .await;
        assert!(matches!(outcome, SubmitOutcome::Submitted));

        // Author identity rides on the same submission as the rest of the
        // feedback; nothing is stripped here.
        let body = captured.lock().clone().expect("server saw the POST");
        assert_eq!(body["authorName"], "Ada Lovelace");
        assert_eq!(body["authorEmail"], "ada@corp.example");
        assert_eq!(body["modelId"], "grok-4");
        assert_eq!(body["feedbackText"], "great session");

        // The local entry keeps the author fields and the full context.
        let msg = rx.try_recv().expect("persistence entry was sent");
        let PersistenceMsg::Feedback(LocalFeedbackEntry::UserFeedback(entry)) = msg else {
            panic!("expected a feedback persistence entry");
        };
        let persisted = entry.submission.expect("submission persisted");
        assert_eq!(persisted.author_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(persisted.author_email.as_deref(), Some("ada@corp.example"));
        assert_eq!(persisted.model_id.as_deref(), Some("grok-4"));
    }

    /// `GROK_USER_METADATA` is merged into the submission and travels with it:
    /// onto the wire body for triage and onto the local feedback.jsonl entry.
    #[tokio::test]
    #[serial_test::serial]
    async fn workflow_merges_user_metadata_into_submission() {
        let _guard = xai_grok_test_support::env::EnvGuard::set(
            "GROK_USER_METADATA",
            r#"{"team": "platform-tools"}"#,
        );
        let (addr, captured) = start_capture_server().await;
        let client = crate::agent::feedback_client::FeedbackClient::with_client(
            reqwest::Client::new(),
            format!("http://{addr}/v1"),
            Some("tok".into()),
        );
        let mut submission = text_submission();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let outcome = submit_feedback_workflow(
            &mut submission,
            Some(&client),
            Some(&tx),
            SubmitFeedbackOptions {
                solicited: false,
                telemetry_enabled: false,
                author_identity: None,
            },
        )
        .await;
        assert!(matches!(outcome, SubmitOutcome::Submitted));

        let body = captured.lock().clone().expect("server saw the POST");
        assert_eq!(body["metadata"]["team"], "platform-tools");

        let msg = rx.try_recv().expect("persistence entry was sent");
        let PersistenceMsg::Feedback(LocalFeedbackEntry::UserFeedback(entry)) = msg else {
            panic!("expected a feedback persistence entry");
        };
        let persisted = entry.submission.expect("submission persisted");
        assert_eq!(
            persisted.metadata.expect("metadata merged before persist")["team"],
            "platform-tools"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn workflow_without_identity_omits_author_fields() {
        let (addr, captured) = start_capture_server().await;
        let client = crate::agent::feedback_client::FeedbackClient::with_client(
            reqwest::Client::new(),
            format!("http://{addr}/v1"),
            Some("tok".into()),
        );

        // Both no opt-in and an unresolved opt-in must leave the author keys
        // out of the body and the local entry.
        for (case, author_identity) in [
            ("no opt-in", None),
            ("unresolved", Some(ResolvedUserIdentity::default())),
        ] {
            // Reset so this case can't pass on the previous case's body.
            *captured.lock() = None;
            let mut submission = text_submission();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let outcome = submit_feedback_workflow(
                &mut submission,
                Some(&client),
                Some(&tx),
                SubmitFeedbackOptions {
                    solicited: false,
                    telemetry_enabled: false,
                    author_identity,
                },
            )
            .await;
            assert!(matches!(outcome, SubmitOutcome::Submitted), "{case}");

            let body = captured.lock().clone().expect("server saw the POST");
            assert!(body.get("authorName").is_none(), "{case}: {body}");
            assert!(body.get("authorEmail").is_none(), "{case}: {body}");

            let msg = rx.try_recv().expect("persistence entry was sent");
            let PersistenceMsg::Feedback(LocalFeedbackEntry::UserFeedback(entry)) = msg else {
                panic!("expected a feedback persistence entry");
            };
            let persisted = entry.submission.expect("submission persisted");
            assert_eq!(persisted.author_name, None, "{case}");
            assert_eq!(persisted.author_email, None, "{case}");
        }
    }
}
