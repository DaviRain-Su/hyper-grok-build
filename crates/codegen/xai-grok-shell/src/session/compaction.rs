//! Compaction methods for `SessionActor`.
//!
//! This module contains all compaction-related methods: manual `/compact`,
//! auto-compact threshold checks, inline auto-compact with auto-continue,
//! error-recovery compaction, preflight overflow detection, and checkpoint
//! persistence. These methods form a second `impl SessionActor` block that
//! lives alongside the primary one in `acp_session.rs`.
use super::SessionActor;
use super::is_project_instructions;
use crate::remote::DEFAULT_CONTEXT_WINDOW;
use crate::session::compaction_config::{
    AsyncCompactionCache, SUPPRESS_NONE, SUPPRESS_STICKY, SUPPRESS_TURN, SUPPRESS_UNTIL_SUCCESS,
};
use crate::session::helpers::CompactionStateContext;
use crate::session::helpers::compaction_context::CompactionInputs;
use crate::session::helpers::compaction_context::to_system_reminder;
use crate::session::helpers::session_compact::{
    CompactOutput, CompactionOutcome, build_compaction_chat_history,
    build_two_pass_compaction_prompt, generate_session_compact, is_context_length_error,
};
use crate::session::persistence::PersistenceMsg;
use crate::session::two_pass::{
    TWO_PASS_DEFAULT_SPLIT_FRACTION, build_two_pass_pass1_history, build_two_pass_pass2_history,
    note_for_two_pass_pass2, split_conversation_for_two_pass,
};
use agent_client_protocol as acp;
use std::sync::Arc;
use xai_chat_state::compaction_utils::{
    CompactedHistoryInput, CompactionAttempt, build_compacted_history, is_degenerate_summary,
    prepare_conversation_for_verbatim_summarization, sanitize_compacted_history,
    validate_compacted_history,
};
use xai_grok_sampling_types::{ApiBackend, ConversationItem};
/// Default percentage points below the auto-compact threshold at which prefire
/// (background pass-1) starts, giving pass-1 runway to finish before the limit.
/// Override with `GROK_PREFIRE_LEAD_PERCENT`.
const DEFAULT_PREFIRE_LEAD_PERCENT: u64 = 10;
fn prefire_lead_percent() -> u64 {
    std::env::var("GROK_PREFIRE_LEAD_PERCENT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_PREFIRE_LEAD_PERCENT)
}
/// Cheap fingerprint of a conversation prefix for prefire NOTE₁ validity. A
/// mismatch means the prefix changed (edit / rewind / branch) since pass-1, so
/// the cached NOTE₁ no longer summarizes the current prefix and must be dropped.
fn fingerprint_prefix(items: &[ConversationItem]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    items.len().hash(&mut h);
    for it in items {
        let tag: u8 = match it {
            ConversationItem::System(_) => 0,
            ConversationItem::User(_) => 1,
            ConversationItem::Assistant(_) => 2,
            ConversationItem::ToolResult(_) => 3,
            ConversationItem::BackendToolCall(_) => 4,
            ConversationItem::Reasoning(_) => 5,
        };
        tag.hash(&mut h);
        it.text_content().hash(&mut h);
    }
    h.finish()
}
/// Outcome of a background prefire pass-1 run, recorded on the
/// `session.prefire_pass1` span as `compaction_prefire_outcome`.
/// [`PrefireOutcome::as_str`] values are stable telemetry keys
/// (telemetry/dashboards key off them) — don't rename the strings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PrefireOutcome {
    Cached,
    Disabled,
    DebugFailPass1,
    TooSmall,
    EmptySplit,
    SampleFailed,
    EmptyNote1,
}
impl PrefireOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cached => "cached",
            Self::Disabled => "disabled",
            Self::DebugFailPass1 => "debug_fail_pass1",
            Self::TooSmall => "too_small",
            Self::EmptySplit => "empty_split",
            Self::SampleFailed => "sample_failed",
            Self::EmptyNote1 => "empty_note1",
        }
    }
}
/// Telemetry from one prefire pass-1 run; recorded onto the
/// `session.prefire_pass1` span by [`SessionActor::run_prefire_pass1`].
/// `None` fields = the run exited before that stage.
struct PrefirePass1Run {
    outcome: PrefireOutcome,
    prefix_len: Option<usize>,
    prefix_est_tokens: Option<u64>,
    pass1_latency_ms: Option<u64>,
    note1_chars: Option<usize>,
}
impl From<PrefireOutcome> for PrefirePass1Run {
    /// A run that exited before splitting/sampling — outcome only.
    fn from(outcome: PrefireOutcome) -> Self {
        Self {
            outcome,
            prefix_len: None,
            prefix_est_tokens: None,
            pass1_latency_ms: None,
            note1_chars: None,
        }
    }
}
#[cfg(test)]
mod two_pass_prefire_helper_tests {
    use super::{fingerprint_prefix, prefire_lead_percent};
    use xai_grok_sampling_types::ConversationItem;
    #[test]
    fn fingerprint_stable_for_same_prefix() {
        let items = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("hello"),
            ConversationItem::assistant("hi"),
        ];
        assert_eq!(fingerprint_prefix(&items), fingerprint_prefix(&items));
    }
    #[test]
    fn fingerprint_changes_when_prefix_content_changes() {
        let base = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("hello"),
        ];
        let edited = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("HELLO there"),
        ];
        assert_ne!(
            fingerprint_prefix(&base),
            fingerprint_prefix(&edited),
            "a changed prefix must invalidate the cached NOTE1 fingerprint"
        );
    }
    #[test]
    fn fingerprint_changes_with_length() {
        let short = vec![ConversationItem::user("a")];
        let long = vec![
            ConversationItem::user("a"),
            ConversationItem::assistant("b"),
        ];
        assert_ne!(fingerprint_prefix(&short), fingerprint_prefix(&long));
    }
    #[test]
    fn prefire_lead_percent_defaults_to_10() {
        unsafe { std::env::remove_var("GROK_PREFIRE_LEAD_PERCENT") };
        assert_eq!(prefire_lead_percent(), 10);
    }
}
impl SessionActor {
    /// Two-pass active for this session: flag resolved on at build AND not an
    /// agent that keeps its single short self-summary.
    pub(crate) fn two_pass_active(&self) -> bool {
        let agent = self.agent.borrow();
        agent.compaction_policy().two_pass_enabled
    }
    /// Run one summarization sample over a fully-built two-pass history (the
    /// prompt is already embedded, so this bypasses the single-pass sampler and
    /// calls `generate_session_compact` directly). Returns `None` on any error
    /// so callers fall back to single-pass.
    ///
    /// Agent `RefCell` borrows are only taken for synchronous snapshots (never
    /// held across `.await`). Prefire is `spawn_local` on the same LocalSet as
    /// the turn loop; a long-lived borrow would race with turn/compact/cancel
    /// and panic on double-borrow.
    async fn two_pass_sample(&self, history: Vec<ConversationItem>) -> Option<CompactOutput> {
        let sampling_config = self.reconstruct_full_config().await;
        let client = match self.prepare_chat_completion(false).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    error = % e, "two_pass: failed to prepare sampling client"
                );
                return None;
            }
        };
        let tool_defs = self.prepare_tool_definitions().await;
        let tools = self.turn_base_tool_specs(&tool_defs);
        let (hosted_tools, wall_clock_budget_secs) = {
            let agent = self.agent.borrow();
            let use_backend_search =
                agent.backend_search_enabled() && self.supports_backend_search.get();
            (
                if use_backend_search {
                    agent.hosted_tools().to_vec()
                } else {
                    Vec::new()
                },
                agent.compaction_policy().wall_clock_budget_secs,
            )
        };
        match generate_session_compact(
            history,
            tools,
            hosted_tools,
            client,
            self.session_info.id.clone(),
            &sampling_config,
            self.inference_idle_timeout,
            wall_clock_budget_secs,
            self.compaction.tool_choice,
        )
        .await
        {
            Ok(out) => Some(out),
            Err(e) => {
                tracing::warn!(error = ? e, "two_pass: summarization sample failed");
                None
            }
        }
    }
    /// Per-turn prefire decision: usage has reached `threshold - lead` (so there
    /// is still runway before the hard auto-compact line at `threshold`).
    pub(crate) async fn should_prefire_two_pass(&self) -> bool {
        let sampling_cfg = self.chat_state_handle.get_sampling_config().await;
        let Some(cw) = sampling_cfg.as_ref().map(|c| c.context_window.get()) else {
            return false;
        };
        let estimated_total = self.chat_state_handle.get_estimated_total_tokens().await;
        let threshold = self.compaction.threshold_percent.get() as u64;
        let start_pct = threshold.saturating_sub(prefire_lead_percent());
        xai_token_estimation::exceeds_threshold(estimated_total, cw, start_pct as u8)
    }
    /// Background pass-1: summarize the ~95% prefix → NOTE₁ and cache it for a
    /// later pass-2 apply. Always releases the in-flight guard. Spawned via
    /// `spawn_local` from the turn loop; reads a conversation snapshot and does
    /// not mutate session state. The span makes speculative pass-1 spend
    /// measurable (hit rate, wasted input tokens) ahead of the fleet-wide ramp.
    #[tracing::instrument(
        name = "session.prefire_pass1",
        skip_all,
        fields(
            session_id = %self.session_info.id.0,
            compaction_prefire_outcome = tracing::field::Empty,
            compaction_pass1_latency_ms = tracing::field::Empty,
            compaction_prefire_prefix_len = tracing::field::Empty,
            compaction_prefire_prefix_est_tokens = tracing::field::Empty,
            compaction_prefire_note1_chars = tracing::field::Empty,
        )
    )]
    pub(crate) async fn run_prefire_pass1(self: &Arc<Self>) {
        struct InFlightGuard<'a>(&'a crate::session::compaction_config::PrefireState);
        impl Drop for InFlightGuard<'_> {
            fn drop(&mut self) {
                self.0.finish();
            }
        }
        let _guard = InFlightGuard(&self.compaction.prefire);
        let run = self.run_prefire_pass1_inner().await;
        let span = tracing::Span::current();
        span.record("compaction_prefire_outcome", run.outcome.as_str());
        if let Some(v) = run.prefix_len {
            span.record("compaction_prefire_prefix_len", v as i64);
        }
        if let Some(v) = run.prefix_est_tokens {
            span.record("compaction_prefire_prefix_est_tokens", v as i64);
        }
        if let Some(v) = run.pass1_latency_ms {
            span.record("compaction_pass1_latency_ms", v as i64);
        }
        if let Some(v) = run.note1_chars {
            span.record("compaction_prefire_note1_chars", v as i64);
        }
    }
    async fn run_prefire_pass1_inner(self: &Arc<Self>) -> PrefirePass1Run {
        if !self.two_pass_active() {
            return PrefireOutcome::Disabled.into();
        }
        if std::env::var("GROK_DEBUG_TWO_PASS_FAIL_PASS1")
            .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        {
            tracing::info!(
                target : "two_pass",
                "two_pass: DEBUG GROK_DEBUG_TWO_PASS_FAIL_PASS1 — prefire pass1 produces no cache"
            );
            return PrefireOutcome::DebugFailPass1.into();
        }
        let conversation = self.chat_state_handle.get_conversation().await;
        if conversation.len() < 4 {
            return PrefireOutcome::TooSmall.into();
        }
        let split = split_conversation_for_two_pass(&conversation, TWO_PASS_DEFAULT_SPLIT_FRACTION);
        if split.prefix.is_empty() || split.tail.is_empty() {
            return PrefireOutcome::EmptySplit.into();
        }
        let sampling_cfg = self.chat_state_handle.get_sampling_config().await;
        let strips = sampling_cfg
            .as_ref()
            .map(|c| c.api_backend == ApiBackend::Messages)
            .unwrap_or(false);
        let model_slug = sampling_cfg
            .as_ref()
            .map(|c| c.model.to_string())
            .unwrap_or_default();
        let prefix_prepared =
            prepare_conversation_for_verbatim_summarization(split.prefix.to_vec(), strips);
        let prefix_est_tokens = prefix_prepared
            .iter()
            .map(xai_chat_state::estimate_item_tokens)
            .sum::<u64>();
        let prompt = build_two_pass_compaction_prompt(None);
        let pass1_history = build_two_pass_pass1_history(&prefix_prepared, &prompt);
        let started = std::time::Instant::now();
        let out = self.two_pass_sample(pass1_history).await;
        let pass1_latency_ms = started.elapsed().as_millis() as u64;
        let attempted = |outcome: PrefireOutcome, note1_chars: Option<usize>| PrefirePass1Run {
            outcome,
            prefix_len: Some(split.split_idx),
            prefix_est_tokens: Some(prefix_est_tokens),
            pass1_latency_ms: Some(pass1_latency_ms),
            note1_chars,
        };
        let Some(out) = out else {
            return attempted(PrefireOutcome::SampleFailed, None);
        };
        let note1 = note_for_two_pass_pass2(&out.content);
        if note1.trim().is_empty() {
            return attempted(PrefireOutcome::EmptyNote1, None);
        }
        let note1_chars = note1.chars().count();
        let cache = AsyncCompactionCache {
            note1,
            prefix_len: split.split_idx,
            fingerprint: fingerprint_prefix(&conversation[..split.split_idx]),
            model_slug,
            pass1_latency_ms,
        };
        tracing::info!(
            target : "two_pass", prefix_len = cache.prefix_len, pass1_latency_ms = cache
            .pass1_latency_ms, "two_pass: prefire pass1 cached NOTE1"
        );
        self.compaction.prefire.store(cache);
        attempted(PrefireOutcome::Cached, Some(note1_chars))
    }
    /// Pass-2 apply: if a valid cached NOTE₁ exists for the current conversation,
    /// summarize (NOTE₁ + recent tail + special prompt) → final summary and
    /// return its `CompactOutput`. `None` → caller runs the single-pass path.
    ///
    /// **telemetry / `session.compact_inner` latency:** the returned `CompactOutput`
    /// stream timings are what land on `compaction_ttft_ms` /
    /// `compaction_stream_ms`. Those reflect **user-visible sync wait only**:
    /// - background pass-1 that already finished before compact is *not*
    ///   included (prefire hid that cost);
    /// - if pass-1 is still in flight we **do** add that await into
    ///   `ttft_ms` (time until first token of the final summary), because the
    ///   user is blocked on it;
    /// - `stream_ms` / `delta_count` / `itl_max_ms` are always pass-2 only
    ///   (the only sample that streams the successor-visible summary).
    async fn try_two_pass_pass2_apply(
        &self,
        user_context: Option<&str>,
        strips_reasoning: bool,
    ) -> Option<CompactOutput> {
        if !self.two_pass_active() {
            return None;
        }
        let mut prefire_waited_ms = 0u64;
        if let Some(handle) = self.compaction.prefire.take_handle() {
            let was_in_flight = self.compaction.prefire.is_in_flight();
            let waited = std::time::Instant::now();
            let _ = handle.await;
            if was_in_flight {
                prefire_waited_ms = waited.elapsed().as_millis() as u64;
                tracing::Span::current()
                    .record("compaction_prefire_waited_ms", prefire_waited_ms as i64);
                tracing::info!(
                    target : "two_pass", wait_ms = prefire_waited_ms,
                    "two_pass: waited for in-flight prefire pass1 before pass2"
                );
            }
        }
        let cache = self.compaction.prefire.take()?;
        let live = self.chat_state_handle.get_conversation().await;
        let model_slug = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model.to_string())
            .unwrap_or_default();
        if cache.prefix_len == 0
            || cache.prefix_len > live.len()
            || cache.model_slug != model_slug
            || fingerprint_prefix(&live[..cache.prefix_len]) != cache.fingerprint
        {
            tracing::Span::current().record("compaction_prefire_stale", true);
            tracing::info!(
                target : "two_pass",
                "two_pass: cached NOTE1 stale or model changed; falling back to single-pass"
            );
            return None;
        }
        let prefix = &live[..cache.prefix_len];
        let tail = &live[cache.prefix_len..];
        let prepared_tail =
            prepare_conversation_for_verbatim_summarization(tail.to_vec(), strips_reasoning);
        let prompt = build_two_pass_compaction_prompt(user_context);
        let pass2_history =
            build_two_pass_pass2_history(prefix, &prepared_tail, &cache.note1, &prompt);
        let started = std::time::Instant::now();
        let mut out = self.two_pass_sample(pass2_history).await?;
        if is_degenerate_summary(&out.content) {
            tracing::Span::current().record("compaction_prefire_stale", true);
            tracing::info!(
                target : "two_pass",
                "two_pass: pass2 summary empty/degenerate; falling back to single-pass"
            );
            return None;
        }
        let pass2_latency_ms = started.elapsed().as_millis() as u64;
        if prefire_waited_ms > 0 {
            out.ttft_ms = Some(out.ttft_ms.unwrap_or(0).saturating_add(prefire_waited_ms));
        }
        let span = tracing::Span::current();
        span.record("compaction_two_pass_used", true);
        span.record("compaction_prefire_hit", true);
        span.record("compaction_pass2_latency_ms", pass2_latency_ms as i64);
        tracing::info!(
            target : "two_pass", prefix_len = cache.prefix_len, tail_len = tail.len(),
            prefire_waited_ms, pass2_latency_ms, pass1_bg_latency_ms = cache
            .pass1_latency_ms, "two_pass: pass2 applied cached NOTE1 (prefire hit)"
        );
        Some(out)
    }
}
/// Trigger info for auto-compact decisions.
pub(crate) struct AutoCompactTriggerInfo {
    pub tokens_used: u64,
    pub context_window: u64,
    pub percentage: u8,
}
/// Why auto-compaction was suppressed after a deterministic failure.
/// [`SuppressReason::as_str`] is a stable telemetry value (BQ/OTLP/dashboards key
/// off it) — don't rename the strings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SuppressReason {
    CreditBlock,
    Size,
    Auth,
    Schema,
    Other,
}
impl SuppressReason {
    fn as_str(self) -> &'static str {
        match self {
            SuppressReason::CreditBlock => "credit_block",
            SuppressReason::Size => "size",
            SuppressReason::Auth => "auth",
            SuppressReason::Schema => "schema",
            SuppressReason::Other => "other",
        }
    }
    /// Suppression scope for this reason:
    /// - `size | schema` → [`SUPPRESS_STICKY`]: retrying the same conversation
    ///   can't help; cleared only on a context-budget change.
    /// - `credit_block | auth` → [`SUPPRESS_UNTIL_SUCCESS`]: re-sending fails the
    ///   same way every turn until the user acts, so don't clear per-turn — wait
    ///   for an actual successful model call (a `200` proves recovery).
    /// - `other` → [`SUPPRESS_TURN`]: optimistic per-turn retry.
    fn suppress_state(self) -> u8 {
        match self {
            SuppressReason::Size | SuppressReason::Schema => SUPPRESS_STICKY,
            SuppressReason::CreditBlock | SuppressReason::Auth => SUPPRESS_UNTIL_SUCCESS,
            SuppressReason::Other => SUPPRESS_TURN,
        }
    }
}
/// Splice the preserved prefix (`conversation[0..prefix_len]`) onto the compacted
/// suffix, dropping the suffix's leading System and — if the prefix already has an
/// AGENTS.md item — its re-injected AGENTS.md too (else the model sees it twice).
/// Returns `Err(compacted_history)` unchanged when `prefix_len` is 0 or out of range.
fn preserve_inherited_prefix(
    conversation: &[ConversationItem],
    compacted_history: Vec<ConversationItem>,
    prefix_len: usize,
) -> Result<Vec<ConversationItem>, Vec<ConversationItem>> {
    if prefix_len == 0 || prefix_len > conversation.len() {
        return Err(compacted_history);
    }
    let inherited = &conversation[..prefix_len];
    let drop_reinjected_agents_md = inherited.iter().any(is_project_instructions);
    let mut preserved = inherited.to_vec();
    let child_items = compacted_history
        .into_iter()
        .skip_while(|i| matches!(i, ConversationItem::System(_)))
        .filter(|i| !(drop_reinjected_agents_md && is_project_instructions(i)));
    preserved.extend(child_items);
    Ok(preserved)
}
/// Project the token count a re-pinned (preserved) history would reseed to, so the
/// release decision compares against the same threshold the auto-compact trigger
/// applies next turn. This only APPROXIMATES the compaction reseed
/// (`xai-chat-state` `replace_conversation`, the authority): it matches the reseed's
/// round-and-cap but divides by the current conversation estimate, not the reseed's
/// frozen `estimate_at_last_response`. The conversation only grows, so the current
/// estimate is >= that frozen value; this therefore under-estimates the reseed (a
/// lower bound) and can lean toward preserve. That never re-loops: the post-replace
/// `exceeds_threshold` check on the real reseeded total still sets sticky Size
/// suppression if a preserve leaves the fork over budget.
fn project_preserved_reseed_tokens(
    preserved_estimate: u64,
    tokens_before: u64,
    full_conv_estimate: u64,
) -> u64 {
    let ratio = tokens_before as f64 / full_conv_estimate.max(1) as f64;
    ((preserved_estimate as f64 * ratio).round() as u64).min(tokens_before)
}
impl SessionActor {
    /// Path to the raw `updates.jsonl` transcript if it exists, else `None`.
    /// `pub(crate)` so the `Transcript`-mode dispatch in `compaction_segments`
    /// and transcript-location pointers can both reuse it.
    ///
    /// The `path.exists()` guard keeps the pointer safe when a session (e.g. a
    /// nested sub-agent) never wrote one -- the hint is simply omitted rather
    /// than dangling.
    pub(crate) fn get_transcript_path(&self) -> Option<String> {
        let path =
            crate::session::persistence::session_dir(&self.session_info).join("updates.jsonl");
        if path.exists() {
            Some(path.to_string_lossy().into_owned())
        } else {
            None
        }
    }
    /// Increment the compaction counter and launch a pre-compaction memory flush.
    ///
    /// The counter is incremented before the flush check so the once-per-cycle
    /// guard does not suppress the first eligible flush.
    async fn maybe_pre_compaction_flush(
        self: &Arc<Self>,
        total_tokens: u64,
        context_window: u64,
        trigger: &'static str,
    ) {
        let compaction_count = self
            .compaction
            .count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if !self.agent.borrow().compaction_policy().memory_flush_enabled {
            return;
        }
        let last_flush = self
            .memory
            .last_flush_compaction
            .load(std::sync::atomic::Ordering::Relaxed);
        if crate::session::helpers::memory_flush::should_flush(
            total_tokens,
            context_window,
            self.compaction.threshold_percent.get(),
            &self.memory.flush_config,
            last_flush,
            compaction_count,
        ) {
            let snapshot = self.snapshot_memory_flush_state().await;
            tokio::task::spawn_local({
                let session = self.clone();
                async move {
                    if session.run_memory_flush(trigger, Some(snapshot)).await {
                        session
                            .memory
                            .last_flush_compaction
                            .store(compaction_count, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            });
        }
    }
    /// Tag the current `session.compact` span with `mode` (and `detail`, for
    /// `segments`) — the A/B variant key for grouping outcomes in telemetry.
    fn record_compaction_variant(&self) {
        let mode = self.compaction.compaction_mode;
        let span = tracing::Span::current();
        span.record("mode", tracing::field::display(mode));
        if let Some(detail) = mode.segment_detail() {
            span.record("detail", tracing::field::display(detail));
        }
    }
    /// Runs the compact operation over here which compresses the current conversation
    /// and helps with saving the context for the model
    #[tracing::instrument(
        name = "session.compact",
        skip_all,
        fields(
            session_id = %self.session_info.id.0,
            trigger = "manual",
            mode = tracing::field::Empty,
            detail = tracing::field::Empty,
            pre_tokens = tracing::field::Empty,
            post_tokens = tracing::field::Empty,
            success = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    )]
    pub(crate) async fn run_compact(
        self: &Arc<Self>,
        user_context: Option<String>,
    ) -> Result<(), acp::Error> {
        self.record_compaction_variant();
        let total_tokens = self.chat_state_handle.get_total_tokens().await;
        tracing::Span::current().record("pre_tokens", total_tokens as i64);
        let sampling_config = self.chat_state_handle.get_sampling_config().await;
        let context_window = sampling_config
            .as_ref()
            .map(|c| c.context_window.get())
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        self.maybe_pre_compaction_flush(total_tokens, context_window, "pre_compaction")
            .await;
        if let Err(e) = self
            .run_compact_inner(
                user_context,
                None,
                xai_grok_telemetry::events::CompactionTrigger::Manual,
            )
            .await
        {
            let span = tracing::Span::current();
            span.record("success", false);
            span.record("error", e.to_string().as_str());
            return Err(e);
        }
        use crate::extensions::notification::SessionUpdate as XaiSessionUpdate;
        let tokens_after = self.chat_state_handle.get_total_tokens().await;
        let span = tracing::Span::current();
        span.record("post_tokens", tokens_after as i64);
        span.record("success", true);
        self.send_xai_notification(XaiSessionUpdate::AutoCompactCompleted {
            tokens_before: Some(total_tokens),
            tokens_after,
            elapsed_ms: None,
            summary_preview: None,
        })
        .await;
        Ok(())
    }
    /// Suppress AUTO compaction after a deterministic failure so the gates stop
    /// re-firing a doomed compaction. Scope depends on the reason (see
    /// [`SuppressReason::suppress_state`]): size/schema are sticky, credit/auth
    /// hold until a model call succeeds, other clears next turn. Fires telemetry +
    /// one notification per transition; manual `/compact` is exempt.
    async fn suppress_auto_compaction(
        &self,
        reason: SuppressReason,
        estimated_tokens: u64,
        context_window: u64,
    ) {
        let new_state = reason.suppress_state();
        if self
            .compaction
            .auto_compact_suppressed
            .compare_exchange(
                SUPPRESS_NONE,
                new_state,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
        {
            tracing::warn!(
                suppress_reason = reason.as_str(),
                estimated_tokens,
                context_window,
                "auto-compaction suppressed after deterministic compaction failure"
            );
            xai_grok_telemetry::session_ctx::log_event(
                xai_grok_telemetry::events::AutoCompactSuppressed {
                    reason: reason.as_str(),
                    estimated_tokens,
                    context_window,
                },
            );
            let message = match reason {
                SuppressReason::CreditBlock => {
                    "out of credits or over your spending limit. Add credits and retry."
                }
                SuppressReason::Auth => {
                    "authentication problem — re-authenticate using /login and retry."
                }
                SuppressReason::Size => "this conversation is too large to compact.",
                SuppressReason::Schema => "this conversation can't be summarized.",
                SuppressReason::Other => {
                    "it'll retry on the next turn, or start a new session using /new."
                }
            };
            self.send_xai_notification(
                crate::extensions::notification::SessionUpdate::AutoCompactFailed {
                    error: message.to_string(),
                },
            )
            .await;
        }
    }
    /// Map a deterministic failure's error text to a fixed, content-free
    /// [`SuppressReason`] (drives telemetry + sticky-vs-per-turn scope).
    fn classify_suppress_reason(error_msg: &str) -> SuppressReason {
        let m = error_msg.to_ascii_lowercase();
        if m.contains("spending-limit")
            || m.contains("spending limit")
            || m.contains("out of credits")
            || m.contains("usage balance exhausted")
            || m.contains("usage limit reached")
        {
            SuppressReason::CreditBlock
        } else if is_context_length_error(&m) {
            SuppressReason::Size
        } else if m.contains("status 401") || m.contains("unauthorized") {
            SuppressReason::Auth
        } else if m.contains("invalid_request_error") {
            SuppressReason::Schema
        } else {
            SuppressReason::Other
        }
    }
    /// Choose the post-compaction history for a forked session: re-pin the inherited
    /// prefix, or release it (fall back to the self-contained summary the summarizer
    /// already built from the whole conversation) when re-pinning would leave the fork
    /// at/over the auto-compact threshold. On release, sets the sticky flag and records
    /// the release span field (this runs within the `run_compact_inner` span).
    ///
    /// This runtime release compensates for a verbatim mirror-fork that pinned its whole
    /// parent transcript; bounding the inherited prefix at fork admission is the
    /// structural alternative that would remove this path.
    async fn resolve_forked_compacted_history(
        &self,
        compacted_history: Vec<ConversationItem>,
        prefix_len: usize,
        tokens_before: u64,
        context_window: u64,
    ) -> Vec<ConversationItem> {
        let full_conv = self.chat_state_handle.get_conversation().await;
        let compacted_len = compacted_history.len();
        let release_candidate = compacted_history.clone();
        match preserve_inherited_prefix(&full_conv, compacted_history, prefix_len) {
            Ok(preserved) => {
                let projected_preserved = project_preserved_reseed_tokens(
                    xai_chat_state::estimate_conversation_tokens(&preserved),
                    tokens_before,
                    xai_chat_state::estimate_conversation_tokens(&full_conv),
                );
                if xai_token_estimation::exceeds_threshold(
                    projected_preserved,
                    context_window,
                    self.compaction.threshold_percent.get(),
                ) {
                    self.compaction
                        .prefix_released
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    tracing::Span::current().record("compaction_prefix_released", true);
                    tracing::info!(
                        session_id = % self.session_info.id.0, prefix_len,
                        projected_preserved,
                        "compaction: releasing inherited prefix under pressure"
                    );
                    release_candidate
                } else {
                    tracing::info!(
                        session_id = % self.session_info.id.0, prefix_len, compacted_len,
                        "Preserving inherited prefix across compaction"
                    );
                    preserved
                }
            }
            Err(original) => {
                tracing::warn!(
                    session_id = % self.session_info.id.0, prefix_len, conversation_len =
                    full_conv.len(),
                    "Inherited prefix invalid, using compacted history as-is"
                );
                original
            }
        }
    }
    /// Inner implementation of compaction that supports an optional `auto_continue`
    /// payload for the checkpoint.
    #[tracing::instrument(
        name = "session.compact_inner",
        skip_all,
        fields(
            session_id = %self.session_info.id.0,
            compaction_tokens_before = tracing::field::Empty,
            compaction_tokens_after = tracing::field::Empty,
            compaction_summary_chars = tracing::field::Empty,
            compaction_degenerate_rejections = tracing::field::Empty,
            compaction_input_overflow_rejections = tracing::field::Empty,
            compaction_deterministic_rejections = tracing::field::Empty,
            compaction_transient_rejections = tracing::field::Empty,
            compaction_attempts = tracing::field::Empty,
            compaction_trigger = tracing::field::Empty,
            compaction_trigger_pct = tracing::field::Empty,
            compaction_threshold_pct = tracing::field::Empty,
            compaction_outcome = tracing::field::Empty,
            compaction_stop_reason = tracing::field::Empty,
            compaction_ttft_ms = tracing::field::Empty,
            compaction_stream_ms = tracing::field::Empty,
            compaction_delta_count = tracing::field::Empty,
            compaction_itl_max_ms = tracing::field::Empty,
            compaction_two_pass_used = tracing::field::Empty,
            compaction_prefire_hit = tracing::field::Empty,
            compaction_pass2_latency_ms = tracing::field::Empty,
            compaction_prefire_waited_ms = tracing::field::Empty,
            compaction_prefire_stale = tracing::field::Empty,
            compaction_prefix_released = tracing::field::Empty,
        )
    )]
    async fn run_compact_inner(
        &self,
        user_context: Option<String>,
        auto_continue: Option<crate::extensions::notification::AutoContinueInfo>,
        trigger: xai_grok_telemetry::events::CompactionTrigger,
    ) -> Result<(), acp::Error> {
        let tokens_before = self.chat_state_handle.get_total_tokens().await;
        tracing::Span::current().record("compaction_tokens_before", tokens_before as i64);
        self.signals_handle().record_compaction(tokens_before);
        let trigger_str = match trigger {
            xai_grok_telemetry::events::CompactionTrigger::Manual => "manual",
            xai_grok_telemetry::events::CompactionTrigger::Auto => "auto",
        };
        let sampling_config = self.chat_state_handle.get_sampling_config().await;
        let context_window = sampling_config
            .as_ref()
            .map(|c| c.context_window.get())
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        {
            let span = tracing::Span::current();
            let trigger_pct = if context_window == 0 {
                0
            } else {
                ((tokens_before as f64 / context_window as f64) * 100.0).round() as i64
            };
            span.record("compaction_trigger_pct", trigger_pct);
            span.record(
                "compaction_threshold_pct",
                self.compaction.threshold_percent.get() as i64,
            );
            span.record("compaction_trigger", trigger_str);
        }
        let summary_strips_reasoning = sampling_config
            .as_ref()
            .map(|c| c.api_backend == ApiBackend::Messages)
            .unwrap_or(false);
        let model_id = sampling_config.map(|c| c.model).unwrap_or_default();
        let compaction = xai_grok_telemetry::events::CompactionScope::begin(
            trigger,
            tokens_before,
            context_window,
            model_id.clone(),
            user_context.is_some(),
        );
        let compact_source = trigger_str;
        self.dispatch_hook(
            xai_grok_hooks::event::HookEventName::PreCompact,
            xai_grok_hooks::event::HookPayload::PreCompact {
                source: compact_source.into(),
            },
            None,
            None,
        )
        .await;
        let max_retries = 3u32;
        let retry_delay_secs = 3u64;
        let (conv_len, system_message, full_conversation) = tokio::join!(
            self.chat_state_handle.get_conversation_len(),
            self.chat_state_handle.get_system_message(),
            self.chat_state_handle.get_conversation(),
        );
        let segment_messages = if self.compaction.compaction_mode.writes_segments() {
            xai_chat_state::compaction_utils::prepare_conversation_for_segment(
                full_conversation.clone(),
            )
        } else {
            Vec::new()
        };
        const SUMMARY_BUDGET_RESERVE_TOKENS: u64 = 32_768;
        let verbatim_input_enabled = self.compaction.verbatim_input;
        let simplified_messages = if verbatim_input_enabled {
            xai_chat_state::compaction_utils::prepare_conversation_for_verbatim_summarization(
                full_conversation,
                summary_strips_reasoning,
            )
        } else {
            xai_chat_state::compaction_utils::prepare_conversation_for_summarization(
                full_conversation,
            )
        };
        if conv_len == 0 {
            tracing::error!(
                session_id = % self.session_info.id.0,
                "Compaction failed: conversation is empty (ChatStateActor may have died)"
            );
            return Err(
                acp::Error::internal_error().data("Compaction failed: conversation is empty")
            );
        }
        let system_message = match system_message {
            Some(msg) => msg,
            None => {
                tracing::error!(
                    session_id = % self.session_info.id.0, conversation_len = conv_len,
                    "Compaction failed: no system message in conversation history"
                );
                return Err(acp::Error::internal_error()
                    .data("Compaction failed: no system message in conversation history"));
            }
        };
        if simplified_messages.is_empty() {
            tracing::error!(
                session_id = % self.session_info.id.0, conversation_len = conv_len,
                "Compaction failed: simplified conversation is empty"
            );
            return Err(acp::Error::internal_error()
                .data("Compaction failed: simplified conversation is empty"));
        }
        if !simplified_messages
            .iter()
            .any(|msg| matches!(msg, ConversationItem::System(_)))
        {
            tracing::error!(
                session_id = % self.session_info.id.0, conversation_len = conv_len,
                simplified_len = simplified_messages.len(),
                "Compaction failed: no system message in simplified conversation"
            );
            return Err(acp::Error::internal_error()
                .data("Compaction failed: no system message in simplified conversation"));
        }
        let sampling_config = self.reconstruct_full_config().await;
        let sampling_client = self.prepare_chat_completion(false).await?;
        let use_backend_search =
            self.agent.borrow().backend_search_enabled() && self.supports_backend_search.get();
        let effective_tool_defs: Vec<xai_grok_sampling_types::ToolDefinition> = self
            .prepare_tool_definitions()
            .await
            .into_iter()
            .filter(|td| !use_backend_search || td.function.name != "web_search")
            .collect();
        let compaction_tool_tokens =
            xai_chat_state::estimate_tool_definitions_tokens(&effective_tool_defs);
        let compaction_tools: Vec<xai_grok_sampling_types::ToolSpec> = effective_tool_defs
            .into_iter()
            .map(xai_grok_sampling_types::ToolSpec::from)
            .collect();
        let compaction_hosted_tools: Vec<xai_grok_sampling_types::HostedTool> =
            if use_backend_search {
                self.agent.borrow().hosted_tools().to_vec()
            } else {
                Vec::new()
            };
        tracing::info!(
            num_tools = compaction_tools.len(),
            tool_tokens = compaction_tool_tokens,
            "Running compact with model '{}' (user model: '{}')",
            &sampling_config.model,
            &sampling_config.model
        );
        let mut last_error: Option<acp::Error> = None;
        let mut last_failure_outcome = CompactionOutcome::Failed;
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum InputStage {
            Verbatim,
            VerbatimFitted,
            Lossy,
        }
        impl InputStage {
            fn as_str(self) -> &'static str {
                match self {
                    Self::Verbatim => "verbatim",
                    Self::VerbatimFitted => "verbatim_fitted",
                    Self::Lossy => "lossy",
                }
            }
        }
        let mut input_stage = if verbatim_input_enabled {
            InputStage::Verbatim
        } else {
            InputStage::Lossy
        };
        let use_short_prompt = false;
        let started_at = chrono::Utc::now().to_rfc3339();
        let estimated_input_tokens =
            xai_chat_state::estimate_conversation_tokens(&simplified_messages);
        let auto_trigger = matches!(trigger, xai_grok_telemetry::events::CompactionTrigger::Auto);
        let wall_clock_budget_secs = self
            .agent
            .borrow()
            .compaction_policy()
            .wall_clock_budget_secs;
        let sampler = crate::session::helpers::full_replace_compaction::ShellCompactionSampler::new(
            use_short_prompt,
            user_context.clone(),
            compaction_tools.clone(),
            compaction_hosted_tools.clone(),
            sampling_client,
            self.session_info.id.clone(),
            sampling_config.clone(),
            self.inference_idle_timeout,
            wall_clock_budget_secs,
            self.compaction.tool_choice,
        );
        let observer =
            crate::session::helpers::full_replace_compaction::ShellFullReplaceObserver::new(
                trigger,
                context_window,
                compaction.compaction_id.clone(),
                self.session_info.id.0.to_string(),
                estimated_input_tokens,
                retry_delay_secs,
            );
        let fr_config = xai_grok_compaction::FullReplaceConfig {
            max_attempts: max_retries,
            retry_delay_secs,
            sampling_timeout_secs: 0,
        };
        let mut request_turns = simplified_messages.clone();
        let mut input_overflow_rejections: u32 = 0;
        let two_pass_output = self
            .try_two_pass_pass2_apply(user_context.as_deref(), summary_strips_reasoning)
            .await;
        let mut compact_summary: Option<String> =
            two_pass_output.as_ref().map(|o| o.content.clone());
        while compact_summary.is_none() {
            match xai_grok_compaction::sample_full_replace_summary(
                &sampler,
                &request_turns,
                user_context.as_deref(),
                &fr_config,
                &observer,
            )
            .await
            {
                Ok(summary) => {
                    compact_summary = Some(summary.summary);
                    break;
                }
                Err(xai_grok_compaction::FullReplaceError::NothingToCompact) => {
                    last_error = Some(
                        acp::Error::internal_error().data("compact failed: nothing to compact"),
                    );
                    break;
                }
                Err(xai_grok_compaction::FullReplaceError::EmptyResponse) => {
                    last_failure_outcome = if observer.degenerate_seen() {
                        CompactionOutcome::Degenerate
                    } else {
                        CompactionOutcome::Transient
                    };
                    last_error = Some(acp::Error::internal_error().data(
                        observer.last_error_message().unwrap_or_else(|| {
                            "compact failed: model returned empty response".to_string()
                        }),
                    ));
                    break;
                }
                Err(xai_grok_compaction::FullReplaceError::Sampler {
                    message,
                    deterministic,
                    context_overflow,
                }) => {
                    if context_overflow {
                        let next_stage = match input_stage {
                            InputStage::Verbatim => Some(InputStage::VerbatimFitted),
                            InputStage::VerbatimFitted => Some(InputStage::Lossy),
                            InputStage::Lossy => None,
                        };
                        if let Some(stage) = next_stage {
                            input_overflow_rejections += 1;
                            xai_grok_telemetry::session_ctx::log_event(
                                xai_grok_telemetry::events::CompactionRetryDegraded {
                                    trigger,
                                    reason: "input_overflow",
                                    from_stage: Some(input_stage.as_str()),
                                    to_stage: Some(stage.as_str()),
                                    summary_chars: None,
                                    attempt: observer.attempt_count(),
                                    context_window,
                                    compaction_id: compaction.compaction_id.clone(),
                                },
                            );
                            tracing::warn!(
                                session_id = % self.session_info.id.0, ? stage, error = %
                                message,
                                "Compaction input overflowed deterministically; stepping down the input ladder to avoid an incompactable state"
                            );
                            let conv = self.chat_state_handle.get_conversation().await;
                            request_turns = match stage {
                                InputStage::VerbatimFitted => {
                                    let budget = context_window
                                        .saturating_sub(SUMMARY_BUDGET_RESERVE_TOKENS)
                                        .saturating_sub(compaction_tool_tokens);
                                    let verbatim = xai_chat_state::compaction_utils::prepare_conversation_for_verbatim_summarization(
                                        conv,
                                        summary_strips_reasoning,
                                    );
                                    xai_chat_state::compaction_utils::fit_conversation_to_budget(
                                        verbatim, budget,
                                    )
                                }
                                InputStage::Lossy => {
                                    let lossy_budget = (context_window.saturating_mul(7) / 10)
                                        .saturating_sub(compaction_tool_tokens);
                                    xai_chat_state::compaction_utils::fit_conversation_to_budget(
                                        xai_chat_state::compaction_utils::prepare_conversation_for_summarization(
                                            conv,
                                        ),
                                        lossy_budget,
                                    )
                                }
                                InputStage::Verbatim => {
                                    unreachable!("ladder only steps forward")
                                }
                            };
                            input_stage = stage;
                            continue;
                        }
                        last_failure_outcome = CompactionOutcome::Deterministic;
                        if auto_trigger {
                            self.suppress_auto_compaction(
                                SuppressReason::Size,
                                estimated_input_tokens,
                                context_window,
                            )
                            .await;
                        }
                        last_error = Some(acp::Error::internal_error().data(message));
                        break;
                    }
                    if deterministic {
                        last_failure_outcome = CompactionOutcome::Deterministic;
                        if auto_trigger {
                            let reason = Self::classify_suppress_reason(&message);
                            self.suppress_auto_compaction(
                                reason,
                                estimated_input_tokens,
                                context_window,
                            )
                            .await;
                        }
                        last_error = Some(acp::Error::internal_error().data(message));
                        break;
                    }
                    last_failure_outcome = CompactionOutcome::Transient;
                    last_error = Some(acp::Error::internal_error().data(message));
                    break;
                }
            }
        }
        let telemetry = observer.into_telemetry();
        if two_pass_output.is_none() {
            let request_chat_history = build_compaction_chat_history(
                request_turns,
                user_context.as_deref(),
                use_short_prompt,
            );
            self.persist_compaction_request_artifact(
                request_chat_history,
                compaction_tools,
                user_context.as_deref(),
                use_short_prompt,
                &sampling_config.model,
                trigger,
                compact_summary
                    .as_deref()
                    .or(telemetry.last_rejected_summary.as_deref()),
                last_error.as_ref(),
                telemetry.attempts,
                telemetry.attempt_details,
                started_at,
            );
        }
        let compact_output = match compact_summary {
            Some(_) => match two_pass_output {
                Some(tp) => tp,
                None => sampler
                    .take_last_success()
                    .expect("a successful full-replace sample stashes its CompactOutput"),
            },
            None => {
                let span = tracing::Span::current();
                span.record("compaction_attempts", telemetry.attempts as i64);
                span.record(
                    "compaction_degenerate_rejections",
                    telemetry.degenerate_rejections as i64,
                );
                span.record(
                    "compaction_input_overflow_rejections",
                    input_overflow_rejections as i64,
                );
                span.record(
                    "compaction_deterministic_rejections",
                    telemetry.deterministic_rejections as i64,
                );
                span.record(
                    "compaction_transient_rejections",
                    telemetry.transient_rejections as i64,
                );
                span.record("compaction_outcome", last_failure_outcome.as_str());
                return Err(last_error.unwrap_or_else(|| {
                    acp::Error::internal_error().data("compaction failed: unknown error")
                }));
            }
        };
        let generate_session_compact = compact_output.content.clone();
        let user_message_prefix = self.build_user_message_prefix().await;
        let conversation = self.chat_state_handle.get_conversation().await;
        let (discovered_agents_md, all_skills_for_compaction, _agent_edited_paths, state_context) =
            if use_short_prompt {
                let empty_edited: std::collections::BTreeSet<String> = Default::default();
                let ctx =
                    CompactionStateContext::build(&conversation, CompactionInputs::default()).await;
                (Vec::<std::path::PathBuf>::new(), vec![], empty_edited, ctx)
            } else {
                let agents_md: Vec<std::path::PathBuf> = self
                    .agent
                    .borrow()
                    .tool_bridge()
                    .agents_md_reminded_paths()
                    .await
                    .into_iter()
                    .collect();
                let bridge_for_skills = self.agent.borrow().tool_bridge().clone();
                let skills = bridge_for_skills.slash_skills().await;
                let edited_paths = self.chat_state_handle.get_agent_edited_paths().await;
                let ctx = {
                    let bridge_tasks = self
                        .agent
                        .borrow()
                        .tool_bridge()
                        .list_background_tasks()
                        .await;
                    let pending_tasks: Vec<_> =
                        bridge_tasks.into_iter().filter(|t| !t.completed).collect();
                    let (execute_tool_name, monitor_tool_name) = if pending_tasks.is_empty() {
                        (None, None)
                    } else {
                        let agent_ref = self.agent.borrow();
                        let bridge = agent_ref.tool_bridge();
                        let empty = serde_json::json!({});
                        let execute = bridge
                            .render_prompt("${{ tools.by_kind.execute }}", &empty)
                            .await
                            .filter(|s| !s.is_empty() && !s.contains("by_kind"));
                        let monitor = bridge
                            .render_prompt("${{ tools.by_kind.monitor }}", &empty)
                            .await
                            .filter(|s| !s.is_empty() && !s.contains("by_kind"));
                        (execute, monitor)
                    };
                    let running_tasks: Vec<_> = pending_tasks
                        .into_iter()
                        .map(|t| {
                            let tool_name = match t.kind {
                                xai_grok_tools::computer::types::TaskKind::Monitor => {
                                    monitor_tool_name.clone()
                                }
                                xai_grok_tools::computer::types::TaskKind::Bash => {
                                    execute_tool_name.clone()
                                }
                            };
                            CompactionStateContext::task_summary(
                                t.task_id, t.command, "running", tool_name,
                            )
                        })
                        .collect();
                    let running_subagents = if let Some(ref event_tx) =
                        self.tool_context.subagent_event_tx
                    {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        use xai_grok_tools::implementations::grok_build::task::types::{
                            SubagentEvent, SubagentListActiveRequest,
                        };
                        let _ =
                            event_tx.send(SubagentEvent::ListActive(SubagentListActiveRequest {
                                parent_session_id: self.session_id_string(),
                                respond_to: tx,
                            }));
                        rx.await
                        .unwrap_or_default()
                        .into_iter()
                        .map(|s| crate::session::helpers::compaction_context::RunningSubagentSummary {
                            subagent_id: s.subagent_id,
                            subagent_type: s.subagent_type,
                            description: s.description,
                            elapsed_ms: s.elapsed_ms,
                        })
                        .collect()
                    } else {
                        vec![]
                    };
                    let connected_mcp_servers = {
                        use crate::session::helpers::compaction_context::CompactionServerSummary;
                        use xai_grok_tools::implementations::search_tool::{
                            sanitize_description, truncate_description,
                        };
                        self.connected_server_summaries()
                            .into_iter()
                            .map(|s| {
                                let desc = s
                                    .description
                                    .map(|d| truncate_description(&sanitize_description(&d)))
                                    .filter(|d| !d.is_empty());
                                CompactionServerSummary {
                                    name: s.name,
                                    tool_count: s.tool_count,
                                    description: desc,
                                }
                            })
                            .collect()
                    };
                    let todos = {
                        use crate::session::helpers::compaction_context::{
                            TodoSummary, TodoSummaryStatus,
                        };
                        use crate::tools::todo::{TodoState, TodoStatus};
                        use xai_grok_tools::types::resources::State;
                        let bridge = self.agent.borrow().tool_bridge().clone();
                        bridge
                            .read_resource::<State<TodoState>>()
                            .await
                            .map(|s| {
                                s.0.todo_items_with_ids()
                                    .map(|(id, item)| TodoSummary {
                                        id: id.clone(),
                                        content: item.content.clone(),
                                        status: match item.status {
                                            TodoStatus::Pending => TodoSummaryStatus::Pending,
                                            TodoStatus::InProgress => TodoSummaryStatus::InProgress,
                                            TodoStatus::Completed => TodoSummaryStatus::Completed,
                                            TodoStatus::Cancelled => TodoSummaryStatus::Cancelled,
                                        },
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    CompactionStateContext::build(
                        &conversation,
                        CompactionInputs {
                            running_tasks,
                            running_subagents,
                            agent_edited_paths: edited_paths.clone(),
                            connected_mcp_servers,
                            todos,
                        },
                    )
                    .await
                };
                (agents_md, skills, edited_paths, ctx)
            };
        use crate::session::helpers::compaction_context::SubagentToolNames;
        let subagent_tool_names: Option<SubagentToolNames> =
            if use_short_prompt || state_context.running_subagents.is_empty() {
                None
            } else {
                let agent_ref = self.agent.borrow();
                let bridge = agent_ref.tool_bridge();
                let empty = serde_json::json!({});
                let poll_name = bridge
                    .render_prompt("${{ tools.by_kind.background_task_action }}", &empty)
                    .await
                    .filter(|s| !s.is_empty() && !s.contains("by_kind"));
                let cancel_name = bridge
                    .render_prompt("${{ tools.by_kind.kill_task_action }}", &empty)
                    .await
                    .filter(|s| !s.is_empty() && !s.contains("by_kind"));
                match (poll_name, cancel_name) {
                    (Some(poll), Some(cancel)) => Some(SubagentToolNames { poll, cancel }),
                    (poll, cancel) => {
                        tracing::warn!(
                            session_id = % self.session_info.id.0, poll_resolved = poll
                            .is_some(), cancel_resolved = cancel.is_some(),
                            "could not resolve subagent tool names, \
                                 omitting subagent reminder from compacted conversation"
                        );
                        None
                    }
                }
            };
        use crate::session::helpers::compaction_context::McpToolNames;
        let mcp_tool_names: Option<McpToolNames> =
            if use_short_prompt || state_context.connected_mcp_servers.is_empty() {
                None
            } else {
                let agent_ref = self.agent.borrow();
                let bridge = agent_ref.tool_bridge();
                let empty = serde_json::json!({});
                let search_name = bridge
                    .render_prompt("${{ tools.by_kind.search_tool }}", &empty)
                    .await
                    .filter(|s| !s.is_empty() && !s.contains("by_kind"));
                let call_name = bridge
                    .render_prompt("${{ tools.by_kind.use_tool }}", &empty)
                    .await
                    .filter(|s| !s.is_empty() && !s.contains("by_kind"));
                match (search_name, call_name) {
                    (Some(search), Some(call)) => Some(McpToolNames { search, call }),
                    _ => None,
                }
            };
        let memory_backend_impl = {
            let g = self.memory.storage.borrow();
            g.as_ref()
                .zip(self.memory.backend_params.as_ref())
                .map(|(storage, params)| {
                    crate::session::memory::MemoryBackendImpl::from_session_params(
                        storage.clone(),
                        &crate::session::memory::MemoryBackendParams {
                            search_source: "compaction_recovery",
                            ..params.clone()
                        },
                    )
                })
        };
        let memory_opt_out = false;
        let memory_ref: Option<&dyn xai_grok_tools::types::memory_backend::MemoryBackend> =
            if memory_opt_out {
                None
            } else {
                memory_backend_impl
                    .as_ref()
                    .map(|b| b as &dyn xai_grok_tools::types::memory_backend::MemoryBackend)
            };
        let suppress_state_reminder = false;
        let system_reminder = if suppress_state_reminder {
            None
        } else {
            to_system_reminder(
                &state_context,
                &discovered_agents_md,
                &all_skills_for_compaction,
                memory_ref,
                subagent_tool_names.as_ref(),
                mcp_tool_names.as_ref(),
            )
            .await
        };
        let system_reminder = {
            let plan_path = {
                let guard = self.plan_mode.lock();
                guard
                    .is_active()
                    .then(|| guard.plan_file_path().to_path_buf())
            };
            if let Some(plan_path) = plan_path {
                let plan_has_content =
                    crate::session::plan_mode::plan_file_has_content(&plan_path).await;
                let template = crate::session::plan_mode::plan_mode_reminder_full_template();
                let wrapper = self.reminder_wrapper_tag();
                let rendered = self
                    .render_plan_template(template, &plan_path, plan_has_content)
                    .await;
                match (system_reminder, rendered) {
                    (Some(mut existing), Some(plan_section)) => {
                        if let Some(pos) = existing.rfind("</system-reminder>") {
                            existing.insert_str(pos, &format!("\n\n{}\n", plan_section));
                        } else {
                            existing.push_str("\n\n");
                            existing.push_str(&plan_section);
                        }
                        Some(existing)
                    }
                    (None, Some(plan_section)) => Some(format!(
                        "<{tag}>\n{body}\n</{tag}>",
                        tag = wrapper,
                        body = plan_section,
                    )),
                    (existing, None) => {
                        tracing::warn!(
                            session_id = % self.session_info.id.0,
                            "compaction: plan mode active but template render failed"
                        );
                        existing
                    }
                }
            } else {
                system_reminder
            }
        };
        if let Some(ref recovery_backend) = memory_backend_impl {
            let n = recovery_backend
                .search_counter
                .load(std::sync::atomic::Ordering::Relaxed);
            if n > 0 {
                self.memory
                    .compaction_recovery_count
                    .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                tracing::debug!(
                    target : xai_grok_telemetry::memory_log::TARGET, count = n,
                    "MEMORY_COMPACTION_RECOVERY: {} search(es) performed", n,
                );
            }
        }
        let agents_md_reminder = self.agent.borrow().agents_md_user_reminder();
        let compaction_context = state_context.for_compaction();
        let compaction_state_context: &CompactionStateContext = &compaction_context;
        self.persist_compaction_segment(&segment_messages, &generate_session_compact);
        let transcript_hint = self.transcript_hint();
        let summary_count = self
            .compaction
            .count
            .load(std::sync::atomic::Ordering::Relaxed);
        let raw_compacted = build_compacted_history(CompactedHistoryInput {
            system_message: system_message.clone(),
            user_message_prefix: user_message_prefix.clone(),
            agents_md_reminder: agents_md_reminder.clone(),
            state_context: compaction_state_context,
            compaction_summary: generate_session_compact.clone(),
            system_reminder: system_reminder.clone(),
            summary_before_recent: use_short_prompt,
            transcript_hint: transcript_hint.clone(),
            summary_count,
        });
        let sanitize_result = sanitize_compacted_history(raw_compacted);
        let compacted_history = if sanitize_result.stripped_tool_call_ids.is_empty() {
            sanitize_result.items
        } else {
            tracing::warn!(
                session_id = % self.session_info.id, stripped_count = sanitize_result
                .stripped_tool_call_ids.len(), stripped_ids = ? sanitize_result
                .stripped_tool_call_ids,
                "compaction: stripped orphaned ToolResults from compacted history"
            );
            sanitize_result.items
        };
        let remaining_violations = validate_compacted_history(&compacted_history);
        let compacted_history = if remaining_violations.is_empty() {
            compacted_history
        } else {
            tracing::error!(
                session_id = % self.session_info.id, violation_count =
                remaining_violations.len(), violation_ids = ? remaining_violations,
                "compaction: sanitized history still has invalid ToolResults -- \
                 falling back to minimal compacted history (no recent_messages)"
            );
            build_compacted_history(CompactedHistoryInput {
                system_message,
                user_message_prefix,
                agents_md_reminder,
                state_context: &state_context.for_compaction(),
                compaction_summary: generate_session_compact,
                system_reminder,
                summary_before_recent: use_short_prompt,
                transcript_hint,
                summary_count,
            })
        };
        let prompt_index_at_compaction = self.chat_state_handle.get_prompt_index().await;
        self.chat_state_handle
            .record_compaction_at(prompt_index_at_compaction);
        let original_user_info = self
            .chat_state_handle
            .get_conversation_item_at(1)
            .await
            .and_then(|item| match item {
                ConversationItem::User(parts) => {
                    parts.content.into_iter().next().and_then(|p| match p {
                        xai_grok_sampling_types::ContentPart::Text { text } => {
                            Some(text.as_ref().to_owned())
                        }
                        _ => None,
                    })
                }
                _ => None,
            });
        self.persist_compaction_checkpoint(
            &compacted_history,
            prompt_index_at_compaction,
            auto_continue,
            original_user_info,
        );
        let prefix_len = if self
            .compaction
            .prefix_released
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            0
        } else {
            self.startup_hints.inherited_prefix_len.unwrap_or(0)
        };
        let compacted_history = if prefix_len == 0 {
            compacted_history
        } else {
            self.resolve_forked_compacted_history(
                compacted_history,
                prefix_len,
                tokens_before,
                context_window,
            )
            .await
        };
        let new_len = compacted_history.len();
        self.chat_state_handle
            .replace_conversation_for_compaction(compacted_history);
        if self.startup_hints.inherited_prefix_len.is_some() {
            let post_replace_tokens = self.chat_state_handle.get_total_tokens().await;
            if xai_token_estimation::exceeds_threshold(
                post_replace_tokens,
                context_window,
                self.compaction.threshold_percent.get(),
            ) {
                self.compaction
                    .auto_compact_suppressed
                    .store(SUPPRESS_STICKY, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    session_id = % self.session_info.id.0, post_replace_tokens,
                    context_window,
                    "compaction: released history still over threshold; suppressing AUTO to avoid a re-loop"
                );
            } else {
                self.compaction
                    .auto_compact_suppressed
                    .store(SUPPRESS_NONE, std::sync::atomic::Ordering::Relaxed);
            }
        } else {
            self.compaction
                .auto_compact_suppressed
                .store(SUPPRESS_NONE, std::sync::atomic::Ordering::Relaxed);
        }
        self.last_idle_flush_conversation_len
            .store(new_len, std::sync::atomic::Ordering::Relaxed);
        self.memory
            .context_injected
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if self.memory.is_enabled() {
            tracing::info!(
                target : xai_grok_telemetry::memory_log::TARGET,
                "MEMORY_COMPACT: post-compaction reset, next turn re-checks injection (search only if no block persisted)"
            );
        }
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::PlanState(
                crate::tools::todo::TodoState::default(),
            ));
        self.agent
            .borrow()
            .tool_bridge()
            .on_agents_md_compaction()
            .await;
        self.agent
            .borrow()
            .tool_bridge()
            .on_skill_discovery_compaction()
            .await;
        self.persist_announcement_state().await;
        self.plan_mode.lock().reset_after_compaction();
        self.persist_plan_mode_state();
        self.dispatch_hook(
            xai_grok_hooks::event::HookEventName::PostCompact,
            xai_grok_hooks::event::HookPayload::PostCompact {
                source: compact_source.into(),
            },
            None,
            None,
        )
        .await;
        let tokens_after = self.chat_state_handle.get_total_tokens().await;
        {
            let span = tracing::Span::current();
            span.record("compaction_tokens_after", tokens_after as i64);
            span.record(
                "compaction_summary_chars",
                compact_output.content.chars().count() as i64,
            );
            span.record("compaction_attempts", telemetry.attempts as i64);
            span.record(
                "compaction_degenerate_rejections",
                telemetry.degenerate_rejections as i64,
            );
            span.record(
                "compaction_input_overflow_rejections",
                input_overflow_rejections as i64,
            );
            span.record(
                "compaction_deterministic_rejections",
                telemetry.deterministic_rejections as i64,
            );
            span.record(
                "compaction_transient_rejections",
                telemetry.transient_rejections as i64,
            );
            let stop_reason = compact_output.stop_reason.as_deref().unwrap_or("stop");
            span.record("compaction_stop_reason", stop_reason);
            let outcome = if compact_output.truncated {
                CompactionOutcome::Truncated
            } else {
                CompactionOutcome::Success
            };
            span.record("compaction_outcome", outcome.as_str());
            span.record("compaction_delta_count", compact_output.delta_count as i64);
            if let Some(ms) = compact_output.ttft_ms {
                span.record("compaction_ttft_ms", ms as i64);
            }
            if let Some(ms) = compact_output.stream_ms {
                span.record("compaction_stream_ms", ms as i64);
            }
            if let Some(ms) = compact_output.itl_max_ms {
                span.record("compaction_itl_max_ms", ms as i64);
            }
        }
        compaction.complete(tokens_after);
        Ok(())
    }
    /// Check if auto-compact should be triggered based on context window usage.
    /// Returns Some(AutoCompactTriggerInfo) if threshold is reached, None otherwise.
    pub(crate) fn should_auto_compact(
        &self,
        total_tokens: u64,
        context_window: std::num::NonZeroU64,
    ) -> Option<AutoCompactTriggerInfo> {
        let cw = context_window.get();
        if xai_token_estimation::exceeds_threshold(
            total_tokens,
            cw,
            self.compaction.threshold_percent.get(),
        ) {
            let percentage = xai_token_estimation::usage_percentage_u8(total_tokens, cw);
            Some(AutoCompactTriggerInfo {
                tokens_used: total_tokens,
                context_window: cw,
                percentage,
            })
        } else {
            None
        }
    }
    /// Returns true if the error response indicates tokens exceed the
    /// model's context window. Inspects only the model-metadata
    /// portion of the [`SamplingErrorInfo`] (the `context_window`
    /// field) against the session's tracked token estimate.
    ///
    /// Called from `handle_sampling_failure` with the
    /// `SamplingErrorInfo` the sampler hands back.
    pub(crate) async fn should_compact_on_error(
        &self,
        err: &xai_grok_sampler::SamplingErrorInfo,
    ) -> bool {
        if self
            .compaction
            .auto_compact_suppressed
            .load(std::sync::atomic::Ordering::Relaxed)
            != SUPPRESS_NONE
        {
            return false;
        }
        let Some(ref metadata) = err.model_metadata else {
            return false;
        };
        let Some(context_window) = metadata.context_window else {
            return false;
        };
        if context_window == 0 {
            return false;
        }
        let estimated_total = self.chat_state_handle.get_estimated_total_tokens().await;
        estimated_total > context_window
    }
    /// Pre-sampling compaction check. Uses `get_estimated_total_tokens()`
    /// (exact prior count + byte-estimate of items since last response) so
    /// tool results are accounted for. Returns `None` when `is_flushing`.
    pub(crate) async fn check_auto_compact_needed(&self) -> Option<AutoCompactTriggerInfo> {
        if self
            .memory
            .is_flushing
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }
        let sampling_cfg = self.chat_state_handle.get_sampling_config().await;
        let context_window = sampling_cfg.as_ref().map(|c| c.context_window)?;
        let cw = context_window.get();
        let model = sampling_cfg
            .as_ref()
            .map(|c| c.model.clone())
            .unwrap_or_default();
        let estimated_total = self.chat_state_handle.get_estimated_total_tokens().await;
        self.signals_handle()
            .update_context_usage(estimated_total, cw);
        if self
            .compaction
            .auto_compact_suppressed
            .load(std::sync::atomic::Ordering::Relaxed)
            != SUPPRESS_NONE
        {
            return None;
        }
        if self
            .compaction
            .force_compact
            .compare_exchange(
                true,
                false,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
        {
            let percentage = xai_token_estimation::usage_percentage_u8(estimated_total, cw);
            tracing::info!(
                "Forced auto-compact trigger (debug): model={model}, \
                 {percentage}% full ({estimated_total}/{cw} tokens)",
            );
            return Some(AutoCompactTriggerInfo {
                tokens_used: estimated_total,
                context_window: cw,
                percentage,
            });
        }
        if let Some(trigger_info) = self.should_auto_compact(estimated_total, context_window) {
            tracing::info!(
                "Pre-sampling auto-compact trigger: model={model}, \
                 {}% full ({}/{} tokens)",
                trigger_info.percentage,
                trigger_info.tokens_used,
                trigger_info.context_window,
            );
            return Some(trigger_info);
        }
        None
    }
    /// Returns `Some` when tool call outputs have pushed the estimated token
    /// count past the context window, indicating pre-emptive compaction is needed.
    pub(crate) async fn check_preflight_overflow(&self) -> Option<AutoCompactTriggerInfo> {
        if self
            .compaction
            .auto_compact_suppressed
            .load(std::sync::atomic::Ordering::Relaxed)
            != SUPPRESS_NONE
        {
            return None;
        }
        let estimated_total = self.chat_state_handle.get_estimated_total_tokens().await;
        let cfg = self.chat_state_handle.get_sampling_config().await?;
        let cw = cfg.context_window.get();
        if estimated_total <= cw {
            return None;
        }
        let overflow = estimated_total.saturating_sub(cw);
        let percentage = xai_token_estimation::usage_percentage_u8(estimated_total, cw);
        tracing::warn!(
            estimated_total, context_window = cw, overflow, model = % cfg.model,
            "CONTEXT_OVERFLOW_PREFLIGHT: estimated tokens exceed context window \
             after tool call outputs"
        );
        Some(AutoCompactTriggerInfo {
            tokens_used: estimated_total,
            context_window: cw,
            percentage,
        })
    }
    /// On a model change, clear stale suppression the switch can resolve (sticky
    /// size/schema — the new window may fit — and a stale per-turn `other`), then
    /// compact now if the new window is smaller. Account-state suppression
    /// (credit/auth → `SUPPRESS_UNTIL_SUCCESS`) is left intact — a switch can't
    /// restore credits or fix auth — and short-circuits the compaction.
    pub(crate) async fn maybe_compact_on_model_switch(self: &Arc<Self>) {
        let Some(prev) = self.compaction.previous_model.take() else {
            return;
        };
        let Some(cfg) = self.chat_state_handle.get_sampling_config().await else {
            return;
        };
        if cfg.model == prev.model_slug {
            return;
        }
        if self
            .compaction
            .auto_compact_suppressed
            .load(std::sync::atomic::Ordering::Relaxed)
            == SUPPRESS_UNTIL_SUCCESS
        {
            return;
        }
        self.compaction
            .auto_compact_suppressed
            .store(SUPPRESS_NONE, std::sync::atomic::Ordering::Relaxed);
        if prev.context_window <= cfg.context_window.get() {
            return;
        }
        let total_tokens = self.chat_state_handle.get_estimated_total_tokens().await;
        let Some(trigger_info) = self.should_auto_compact(total_tokens, cfg.context_window) else {
            return;
        };
        tracing::info!(
            "Proactive model-switch compact: {} ({}) -> {} ({}), {}% full",
            prev.model_slug,
            prev.context_window,
            cfg.model,
            cfg.context_window.get(),
            trigger_info.percentage,
        );
        if let Err(e) = self.run_compact_only(trigger_info).await {
            tracing::error!(error = % e, "Model-switch compaction failed");
        }
    }
    /// Record the current model for model-switch detection on the next turn.
    pub(crate) async fn record_turn_model(&self) {
        if let Some(cfg) = self.chat_state_handle.get_sampling_config().await {
            self.compaction.previous_model.set(Some(
                crate::session::compaction_config::PreviousModelInfo {
                    model_slug: cfg.model.clone(),
                    context_window: cfg.context_window.get(),
                },
            ));
        }
    }
    /// Compact without auto-continue. The outer turn loop rebuilds and retries.
    /// Emits telemetry (`auto_compact_fired`) and UI notifications automatically.
    #[tracing::instrument(
        name = "session.compact",
        skip_all,
        fields(
            session_id = %self.session_info.id.0,
            trigger = "auto",
            mode = tracing::field::Empty,
            detail = tracing::field::Empty,
            pre_tokens = tracing::field::Empty,
            post_tokens = tracing::field::Empty,
            success = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    )]
    pub(crate) async fn run_compact_only(
        self: &Arc<Self>,
        trigger_info: AutoCompactTriggerInfo,
    ) -> Result<(), acp::Error> {
        use crate::extensions::notification::SessionUpdate as XaiSessionUpdate;
        self.record_compaction_variant();
        let tokens_before = self.chat_state_handle.get_total_tokens().await;
        tracing::Span::current().record("pre_tokens", tokens_before as i64);
        xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::AutoCompactFired {
            tokens_before: trigger_info.tokens_used,
            percentage: trigger_info.percentage,
        });
        self.signals_handle()
            .record_compaction(trigger_info.tokens_used);
        self.send_xai_notification(XaiSessionUpdate::AutoCompactStarted {
            tokens_used: trigger_info.tokens_used,
            context_window: trigger_info.context_window,
            percentage: trigger_info.percentage,
            reason: format!("Context window {}% full", trigger_info.percentage),
        })
        .await;
        self.maybe_pre_compaction_flush(
            trigger_info.tokens_used,
            trigger_info.context_window,
            "pre_compact_on_error",
        )
        .await;
        let compact_start = std::time::Instant::now();
        let result = self
            .run_compact_inner(
                None,
                None,
                xai_grok_telemetry::events::CompactionTrigger::Auto,
            )
            .await;
        let elapsed_ms = compact_start.elapsed().as_millis() as i64;
        match result {
            Ok(()) => {
                let tokens_after = self.chat_state_handle.get_total_tokens().await;
                let span = tracing::Span::current();
                span.record("post_tokens", tokens_after as i64);
                span.record("success", true);
                self.send_xai_notification(XaiSessionUpdate::AutoCompactCompleted {
                    tokens_before: Some(trigger_info.tokens_used),
                    tokens_after,
                    elapsed_ms: Some(elapsed_ms),
                    summary_preview: None,
                })
                .await;
                Ok(())
            }
            Err(e) => {
                let span = tracing::Span::current();
                span.record("success", false);
                span.record("error", e.to_string().as_str());
                if self
                    .compaction
                    .auto_compact_suppressed
                    .load(std::sync::atomic::Ordering::Relaxed)
                    == SUPPRESS_NONE
                {
                    self.send_xai_notification(XaiSessionUpdate::AutoCompactFailed {
                        error: String::new(),
                    })
                    .await;
                }
                Err(e)
            }
        }
    }
    /// Persist a compaction request artifact for offline prompt iteration.
    ///
    /// Writes `{session_dir}/compaction_requests/{request_id}.json` containing
    /// the exact ConversationItem list sent to the compaction model plus the
    /// summary (or final error) it produced. The file rides on
    /// the post-turn session archive to cloud storage via the existing per-turn upload
    /// pipeline — no separate upload path is needed.
    ///
    /// `created_at` is taken from the caller-supplied `started_at` (captured
    /// before the retry loop) rather than `Utc::now()` here, so transient
    /// retries don't skew the timestamp away from when the call actually
    /// started.
    ///
    /// Best-effort: send-failures are logged at `warn` and never surfaced to
    /// the user, because the artifact is purely for offline analysis.
    #[allow(clippy::too_many_arguments)]
    fn persist_compaction_request_artifact(
        &self,
        chat_history: Vec<ConversationItem>,
        tools: Vec<xai_grok_sampling_types::ToolSpec>,
        user_context: Option<&str>,
        use_short_prompt: bool,
        model: &str,
        trigger: xai_grok_telemetry::events::CompactionTrigger,
        summary: Option<&str>,
        error: Option<&acp::Error>,
        attempts: u32,
        attempt_details: Vec<CompactionAttempt>,
        started_at: String,
    ) {
        use crate::extensions::notification::CompactionRequestFile;
        let request_id = uuid::Uuid::new_v4().to_string();
        let trigger_str = match trigger {
            xai_grok_telemetry::events::CompactionTrigger::Manual => "manual",
            xai_grok_telemetry::events::CompactionTrigger::Auto => "auto",
        };
        let prompt_variant = if use_short_prompt {
            "short"
        } else {
            "detailed"
        };
        let error_str = error.map(|e| {
            e.data
                .as_ref()
                .and_then(|d| d.as_str())
                .unwrap_or("<no error data>")
                .to_owned()
        });
        let artifact = CompactionRequestFile {
            schema_version: 2,
            request_id,
            created_at: started_at,
            trigger: trigger_str.to_owned(),
            prompt_variant: prompt_variant.to_owned(),
            model: model.to_owned(),
            user_context: user_context.map(str::to_owned),
            chat_history,
            tools,
            summary: summary.map(str::to_owned),
            error: error_str,
            attempts,
            attempt_details,
        };
        if self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::CompactionRequest(artifact))
            .is_err()
        {
            tracing::warn!(
                session_id = % self.session_info.id.0,
                "Failed to send compaction request artifact to persistence channel"
            );
        }
    }
    /// Persist a compaction checkpoint: writes the compacted history to a separate file
    /// and records a `CompactionCheckpoint` marker in `updates.jsonl`.
    ///
    /// `auto_continue` should be `Some` when this compaction was triggered by auto-compact
    /// and an auto-continue prompt will follow.
    fn persist_compaction_checkpoint(
        &self,
        compacted_history: &[ConversationItem],
        prompt_index_at_compaction: usize,
        auto_continue: Option<crate::extensions::notification::AutoContinueInfo>,
        original_user_info: Option<String>,
    ) {
        use crate::extensions::notification::{
            CompactionCheckpointFile, CompactionCheckpointInfo, SessionUpdate as XaiSessionUpdate,
        };
        let checkpoint_id = uuid::Uuid::new_v4().to_string();
        let checkpoint_file = format!("compaction_checkpoints/{checkpoint_id}.json");
        let created_at = chrono::Utc::now().to_rfc3339();
        let file_data = CompactionCheckpointFile {
            checkpoint_id: checkpoint_id.clone(),
            prompt_index_at_compaction,
            compacted_history: compacted_history.to_vec(),
            schema_version: 1,
            created_at: created_at.clone(),
            original_user_info,
            reread_file_paths: vec![],
        };
        if self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::CompactionCheckpoint(file_data))
            .is_err()
        {
            tracing::warn!("Failed to send compaction checkpoint file to persistence channel");
        }
        let info = CompactionCheckpointInfo {
            checkpoint_id,
            prompt_index_at_compaction,
            checkpoint_file,
            auto_continue,
            schema_version: 1,
            created_at,
        };
        self.persist_xai_update_only(XaiSessionUpdate::CompactionCheckpoint(Box::new(info)));
        tracing::info!(
            prompt_index_at_compaction,
            "Persisted compaction checkpoint"
        );
    }
}
#[cfg(test)]
mod inline_auto_compact_flow_tests {
    use super::super::support::*;
    use super::super::*;
    use super::{AutoCompactTriggerInfo, SuppressReason};
    use crate::session::acp_session::McpReminderMode;
    use crate::terminal::AsyncTerminalRunner;
    use crate::terminal::runner::{TerminalError, TerminalRunRequest, TerminalRunResult};
    use std::sync::OnceLock;
    use tokio::sync::mpsc;
    use xai_grok_paths::AbsPathBuf;
    use xai_grok_workspace::file_system::MockFs;
    use xai_grok_workspace::permission::PermissionHandle;
    #[derive(Debug)]
    struct DummyTerminal;
    #[async_trait::async_trait]
    impl AsyncTerminalRunner for DummyTerminal {
        async fn run(
            &self,
            _request: TerminalRunRequest,
        ) -> Result<TerminalRunResult, TerminalError> {
            Err(TerminalError::Other("dummy terminal".into()))
        }
    }
    /// Create a minimal SessionActor for testing auto-compact logic.
    async fn create_test_actor(
        total_tokens: u64,
        context_window: u64,
        threshold_percent: u8,
        gateway_tx: mpsc::UnboundedSender<xai_acp_lib::AcpClientMessage>,
        persistence_tx: mpsc::UnboundedSender<PersistenceMsg>,
    ) -> SessionActor {
        let cwd = AbsPathBuf::new(std::path::PathBuf::from("/tmp")).unwrap();
        let fs = Arc::new(MockFs::new(cwd.to_path_buf()));
        let terminal = Arc::new(DummyTerminal {});
        let (hunk_tx, _hunk_rx) = tokio::sync::mpsc::unbounded_channel();
        let hunk_tracker_handle = xai_hunk_tracker::HunkTrackerActor::spawn(
            "test-auto-compact".to_string(),
            cwd.to_path_buf(),
            hunk_tx,
            xai_hunk_tracker::TrackingMode::AgentOnly,
            tokio_util::sync::CancellationToken::new(),
        );
        let tool_context =
            ToolContext::new(cwd.clone(), None, None, fs, terminal, hunk_tracker_handle);
        let state = TokioMutex::new(State {
            running_task: None,
            pending_inputs: VecDeque::new(),
            pending_notifications: Vec::new(),
            notifications_suppressed: false,
            rewindable: false,
            nudges_used_this_session: 0,
        });
        let (chat_event_tx, _chat_event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, _event_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::session::replay_events::SessionEvent>();
        let chat_state_handle = xai_chat_state::ChatStateActor::spawn(
            vec![],
            xai_grok_sampling_types::SamplingConfig {
                base_url: "http://localhost".to_string(),
                model: "test".to_string(),
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                api_backend: Default::default(),
                extra_headers: Default::default(),
                context_window: std::num::NonZeroU64::new(context_window)
                    .expect("test context_window must be non-zero"),
                reasoning_effort: None,
                stream_tool_calls: None,
            },
            Box::new(xai_chat_state::NullChatPersistence),
            chat_event_tx,
            tokio_util::sync::CancellationToken::new(),
        );
        chat_state_handle.record_token_usage(total_tokens);
        SessionActor {
            session_info: SessionInfo {
                id: acp::SessionId::new("test-auto-compact"),
                cwd: cwd.as_str().to_string(),
            },
            auth_method_id: test_auth_method_id("test-auth"),
            model_auth_memo: std::cell::RefCell::new(None),
            attribution_callback: None,
            auth_manager: None,
            state,
            notifications: NotificationSender {
                gateway: GatewaySender::new(gateway_tx),
                gateway_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                persistence_tx,
            },
            permissions: PermissionHandle::allow_all(),
            tool_context,
            deny_read_globs: Vec::new(),
            mcp_state: Arc::new(TokioMutex::new(McpState::new(vec![]))),
            mcp_strategy: McpInitStrategy::Blocking,
            chat_state_handle,
            current_prompt_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            pending_interactions: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            telemetry_enabled: false,
            supports_backend_search: std::cell::Cell::new(false),
            compactions_remaining: std::cell::Cell::new(None),
            compaction_at_tokens: std::cell::Cell::new(None),
            doom_loop_recovery: None,
            doom_loop_turn_tally: Default::default(),
            file_state_tracker: Arc::new(FileStateTracker::new()),
            rewind_pending_prompt: std::sync::Mutex::new(None),
            startup_hints: StartupHints::default(),
            forked_tool_override: None,
            compaction: crate::session::compaction_config::CompactionConfig {
                threshold_percent: std::cell::Cell::new(threshold_percent),
                force_compact: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                context_window_override: None,
                count: std::sync::atomic::AtomicU64::new(0),
                auto_compact_suppressed: std::sync::atomic::AtomicU8::new(0),
                previous_model: std::cell::Cell::new(None),
                compaction_mode: xai_chat_state::CompactionMode::Transcript,
                verbatim_input: true,
                tool_choice: crate::util::config::CompactionToolChoice::Auto,
                prefire: crate::session::compaction_config::PrefireState::default(),
                prefix_released: std::sync::atomic::AtomicBool::new(false),
            },
            memory: crate::session::memory_state::SessionMemory {
                flush_config: crate::config::MemoryFlushConfig::default(),
                is_flushing: std::sync::atomic::AtomicBool::new(false),
                last_flush_compaction: std::sync::atomic::AtomicU64::new(0),
                storage: std::cell::RefCell::new(None),
                save_on_end: true,
                backend_params: None,
                initial_injection_config: Default::default(),
                context_injected: std::sync::atomic::AtomicBool::new(false),
                flush_count: std::sync::atomic::AtomicU64::new(0),
                last_flush_content: std::cell::RefCell::new(None),
                flush_success_count: std::sync::atomic::AtomicU64::new(0),
                flush_error_count: std::sync::atomic::AtomicU64::new(0),
                search_counter: std::cell::RefCell::new(None),
                injection_count: std::sync::atomic::AtomicU64::new(0),
                compaction_recovery_count: std::sync::atomic::AtomicU64::new(0),
                chunks_added: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                dream_config: Default::default(),
                dream_count: std::sync::atomic::AtomicU64::new(0),
                dream_success_count: std::sync::atomic::AtomicU64::new(0),
                dream_error_count: std::sync::atomic::AtomicU64::new(0),
            },
            session_start: std::time::Instant::now(),
            inference_idle_timeout: std::time::Duration::from_secs(300),
            max_retries: 3,
            max_turns: None,
            pending_interjections: InterjectionBuffer::new(),
            pending_skill_reminders: Mutex::new(Vec::new()),
            idle_flush_timeout: None,
            dream_check_timeout: None,
            last_idle_flush_conversation_len: std::sync::atomic::AtomicUsize::new(0),
            event_tx,
            buffering_settings: None,
            client_identifier: None,
            origin_client: None,
            feedback_manager: Arc::new(FeedbackManager::local_only("test-session")),
            upload_queue: Arc::new(OnceLock::new()),
            sync_loop_cancel: None,
            agent: std::cell::RefCell::new(test_agent_default().await),
            last_reported_branch: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            git_head_enabled: false,
            models_manager: Default::default(),
            display_cwd: std::sync::OnceLock::new(),
            active_agent_type: parking_lot::Mutex::new(None),
            queue_exit_reminder_on_approved_exit: Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            active_skill: parking_lot::Mutex::new(None),
            current_prompt_mode: Arc::new(parking_lot::Mutex::new(PromptMode::Agent)),
            turn_start_prompt_mode: parking_lot::Mutex::new(PromptMode::Agent),
            turn_prompt_mode: Arc::new(parking_lot::Mutex::new(PromptMode::Agent)),
            plan_mode: Arc::new(parking_lot::Mutex::new(
                crate::session::plan_mode::PlanModeTracker::new(std::path::PathBuf::from(
                    "/tmp/test-session",
                )),
            )),
            goal_enabled: false,
            goal_harness_enabled: std::sync::atomic::AtomicBool::new(false),
            goal_harness_availability_reconciled: std::sync::atomic::AtomicBool::new(false),
            goal_tracker: Arc::new(parking_lot::Mutex::new(
                crate::session::goal_tracker::GoalTracker::new(std::path::PathBuf::from(
                    "/tmp/test-session",
                )),
            )),
            goal_turn_task_ids: parking_lot::Mutex::new(std::collections::HashSet::new()),
            goal_continuation_streak: std::sync::atomic::AtomicU32::new(0),
            goal_blocked_streak: std::sync::atomic::AtomicU32::new(0),
            goal_update_rx: std::cell::RefCell::new(Some(tokio::sync::mpsc::unbounded_channel().1)),
            goal_update_tx: tokio::sync::mpsc::unbounded_channel().0,
            goal_classifier_enabled: false,
            goal_planner_enabled: false,
            goal_summary_enabled: false,
            goal_verifier_skeptic_count: 1,
            goal_role_models: Default::default(),
            goal_use_current_model_only: false,
            goal_classifier_max_runs:
                crate::session::goal_classifier::GOAL_CLASSIFIER_MAX_RUNS_DEFAULT,
            goal_strategist_every: 5,
            goal_reverify_after: crate::session::acp_session::GOAL_REVERIFY_AFTER_DEFAULT,
            goal_plan_reconciled: std::sync::atomic::AtomicBool::new(false),
            pending_classifier_completions: parking_lot::Mutex::new(
                std::collections::VecDeque::new(),
            ),
            goal_classifier_in_flight: std::sync::atomic::AtomicBool::new(false),
            managed_mcp_handle: Default::default(),
            managed_mcp_expires_at: std::sync::Mutex::new(None),
            initial_client_mcp_servers: vec![],
            tool_metadata_snapshot: Arc::new(std::sync::Mutex::new(Default::default())),
            mcp_announced_servers: parking_lot::Mutex::new(std::collections::HashMap::new()),
            mcp_reminder_mode: McpReminderMode::Delta,
            mcp_reminder_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            mcp_connecting_reminder_injected: std::cell::Cell::new(false),
            mcp_handshakes_done: Arc::new(tokio::sync::Notify::new()),
            user_input_generation: std::sync::atomic::AtomicU64::new(0),
            laziness_debug_log: None,
            deferred_prefix: TaskSlot::new(),
            extension_registry: xai_agent_lifecycle::LocalExtensionRegistry::default(),
            last_announced_local_date: std::cell::Cell::new(chrono::Local::now().date_naive()),
            last_search_prompt_index: std::sync::atomic::AtomicI64::new(-1),
            last_api_request_at: std::sync::atomic::AtomicI64::new(0),
            hook_registry: std::cell::RefCell::new(None),
            client_hooks: Default::default(),
            hook_resolved_workspace_root: String::new(),
            vcs_kind: xai_grok_workspace::session::git::VcsKind::Git,
            hook_load_errors: std::cell::RefCell::new(Vec::new()),
            plugin_registry: std::cell::RefCell::new(None),
            plugin_registry_handle: None,
            events: crate::session::events::EventTracker::new(std::path::Path::new("/tmp")),
            observability_bridge: noop_observability_bridge(),
            current_turn_number: std::cell::Cell::new(0),
            last_recap_main_turn: std::cell::Cell::new(0),
            recap_in_flight: std::cell::Cell::new(false),
            recap_epoch: std::cell::Cell::new(0),
            session_turn_active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            streaming_turn_capture: parking_lot::Mutex::new(
                crate::session::acp_session::StreamingTurnCapture::default(),
            ),
            turn_stream_drained: parking_lot::Mutex::new(None),
            sampler_handle: xai_grok_sampler::SamplerHandle::noop(),
            rebuild_spec: crate::session::agent_rebuild::test_rebuild_spec_default(),
            image_description_model: crate::test_support::TEST_MODEL.to_owned(),
            image_describe_cache: Arc::new(
                crate::session::image_describe::ImageDescribeCache::new(),
            ),
            subagent_token_records: parking_lot::Mutex::new(std::collections::HashMap::new()),
            workspace_ops: xai_grok_workspace::WorkspaceOps::for_test(),
            trace_config_template: std::cell::RefCell::new(None),
        }
    }
    /// Test check_auto_compact_needed uses state values.
    #[tokio::test(flavor = "current_thread")]
    async fn test_check_auto_compact_needed_uses_state() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) =
                    mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
                let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
                let actor =
                    create_test_actor(90_000, 100_000, 85, gateway_tx, persistence_tx).await;
                let result = actor.check_auto_compact_needed().await;
                assert!(result.is_some(), "Should trigger at 90%");
                let info = result.unwrap();
                assert_eq!(info.percentage, 90);
            })
            .await;
    }
    /// Test that overriding context_window on the sampling config changes
    /// auto-compact behavior. Forked sessions must use the new model's
    /// context window, not the source session's. Without this, auto-compact
    /// fires at the wrong threshold.
    #[tokio::test(flavor = "current_thread")]
    async fn test_context_window_override_affects_auto_compact() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) =
                    mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
                let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
                let actor =
                    create_test_actor(86_000, 100_000, 85, gateway_tx, persistence_tx).await;
                let result = actor.check_auto_compact_needed().await;
                assert!(result.is_some(), "Should trigger at 86% of 100K window");
                if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                    cfg.model = "larger-model".to_string();
                    cfg.context_window = std::num::NonZeroU64::new(200_000).unwrap();
                    actor.chat_state_handle.update_sampling_config(cfg);
                }
                let result = actor.check_auto_compact_needed().await;
                assert!(
                    result.is_none(),
                    "Should NOT trigger at 43% of 200K window after context_window override"
                );
            })
            .await;
    }
    /// Test the reverse direction: overriding to a smaller context window
    /// should make auto-compact trigger sooner.
    #[tokio::test(flavor = "current_thread")]
    async fn test_context_window_override_to_smaller_triggers_compact() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) =
                    mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
                let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
                let actor =
                    create_test_actor(86_000, 200_000, 85, gateway_tx, persistence_tx).await;
                let result = actor.check_auto_compact_needed().await;
                assert!(result.is_none(), "Should NOT trigger at 43% of 200K window");
                if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                    cfg.model = "smaller-model".to_string();
                    cfg.context_window = std::num::NonZeroU64::new(100_000).unwrap();
                    actor.chat_state_handle.update_sampling_config(cfg);
                }
                let result = actor.check_auto_compact_needed().await;
                assert!(
                    result.is_some(),
                    "Should trigger at 86% of 100K window after context_window override"
                );
            })
            .await;
    }
    /// Suppression gates both AUTO paths; the reset scope depends on the reason:
    /// `other` clears next turn, `credit_block` holds until a successful model call,
    /// `size` is sticky until a full reset (success / rewind / model switch).
    #[tokio::test(flavor = "current_thread")]
    async fn suppression_gates_and_reset_is_reason_scoped() {
        use crate::session::compaction_config::{
            SUPPRESS_NONE, SUPPRESS_TURN, SUPPRESS_UNTIL_SUCCESS,
        };
        use std::sync::atomic::Ordering::Relaxed;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
                let actor =
                    create_test_actor(214_000, 200_000, 85, gateway_tx, persistence_tx).await;
                let err = api_error_with_context_window(200_000);
                assert!(actor.check_auto_compact_needed().await.is_some());
                assert!(actor.should_compact_on_error(&err).await);
                actor
                    .suppress_auto_compaction(SuppressReason::Other, 1_000, 200_000)
                    .await;
                assert!(actor.check_auto_compact_needed().await.is_none());
                assert!(!actor.should_compact_on_error(&err).await);
                let _ = actor.compaction.auto_compact_suppressed.compare_exchange(
                    SUPPRESS_TURN,
                    SUPPRESS_NONE,
                    Relaxed,
                    Relaxed,
                );
                assert!(actor.check_auto_compact_needed().await.is_some());
                actor
                    .suppress_auto_compaction(SuppressReason::CreditBlock, 1_000, 200_000)
                    .await;
                assert_eq!(
                    actor.compaction.auto_compact_suppressed.load(Relaxed),
                    SUPPRESS_UNTIL_SUCCESS
                );
                assert!(actor.check_auto_compact_needed().await.is_none());
                assert!(!actor.should_compact_on_error(&err).await);
                let _ = actor.compaction.auto_compact_suppressed.compare_exchange(
                    SUPPRESS_TURN,
                    SUPPRESS_NONE,
                    Relaxed,
                    Relaxed,
                );
                assert!(
                    actor.check_auto_compact_needed().await.is_none(),
                    "credit-block suppression must survive the per-turn reset"
                );
                let _ = actor.compaction.auto_compact_suppressed.compare_exchange(
                    SUPPRESS_UNTIL_SUCCESS,
                    SUPPRESS_NONE,
                    Relaxed,
                    Relaxed,
                );
                assert!(actor.check_auto_compact_needed().await.is_some());
                actor
                    .suppress_auto_compaction(SuppressReason::Size, 1_000, 200_000)
                    .await;
                assert!(actor.check_auto_compact_needed().await.is_none());
                let _ = actor.compaction.auto_compact_suppressed.compare_exchange(
                    SUPPRESS_TURN,
                    SUPPRESS_NONE,
                    Relaxed,
                    Relaxed,
                );
                assert!(
                    actor.check_auto_compact_needed().await.is_none(),
                    "sticky suppression must survive the per-turn reset"
                );
                actor
                    .compaction
                    .auto_compact_suppressed
                    .store(SUPPRESS_NONE, Relaxed);
                assert!(actor.check_auto_compact_needed().await.is_some());
            })
            .await;
    }
    /// A model switch clears suppression the switch (or the fresh budget-driven
    /// trigger) can resolve — sticky size/schema and a stale per-turn `other` — so
    /// the gates re-evaluate against the new window. Account-state credit/auth is
    /// covered by `model_switch_keeps_account_state_suppression`.
    #[tokio::test(flavor = "current_thread")]
    async fn model_switch_clears_sticky_suppression() {
        use crate::session::compaction_config::{PreviousModelInfo, SUPPRESS_NONE};
        use std::sync::atomic::Ordering::Relaxed;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
                let actor = Arc::new(
                    create_test_actor(50_000, 200_000, 85, gateway_tx, persistence_tx).await,
                );
                for reason in [SuppressReason::Size, SuppressReason::Other] {
                    actor.suppress_auto_compaction(reason, 1_000, 200_000).await;
                    assert_ne!(
                        actor.compaction.auto_compact_suppressed.load(Relaxed),
                        SUPPRESS_NONE,
                        "{reason:?} should set suppression"
                    );
                    actor.compaction.previous_model.set(Some(PreviousModelInfo {
                        model_slug: "old-small-model".to_string(),
                        context_window: 100_000,
                    }));
                    actor.maybe_compact_on_model_switch().await;
                    assert_eq!(
                        actor.compaction.auto_compact_suppressed.load(Relaxed),
                        SUPPRESS_NONE,
                        "model switch must clear {reason:?} suppression so the gates re-evaluate"
                    );
                }
            })
            .await;
    }
    /// A model switch resets the context budget, so it clears sticky (size/schema)
    /// suppression — but NOT account-state suppression (credit/auth →
    /// SUPPRESS_UNTIL_SUCCESS), which a switch can't resolve. It must also not
    /// proactively compact while that suppression is active (the switch-to-smaller
    /// window path would otherwise fire a doomed compaction).
    #[tokio::test(flavor = "current_thread")]
    async fn model_switch_keeps_account_state_suppression() {
        use crate::session::compaction_config::{PreviousModelInfo, SUPPRESS_UNTIL_SUCCESS};
        use std::sync::atomic::Ordering::Relaxed;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
                let actor = Arc::new(
                    create_test_actor(214_000, 200_000, 85, gateway_tx, persistence_tx).await,
                );
                actor
                    .suppress_auto_compaction(SuppressReason::CreditBlock, 1_000, 200_000)
                    .await;
                assert_eq!(
                    actor.compaction.auto_compact_suppressed.load(Relaxed),
                    SUPPRESS_UNTIL_SUCCESS
                );
                actor.compaction.previous_model.set(Some(PreviousModelInfo {
                    model_slug: "old-big-model".to_string(),
                    context_window: 400_000,
                }));
                actor.maybe_compact_on_model_switch().await;
                assert_eq!(
                    actor.compaction.auto_compact_suppressed.load(Relaxed),
                    SUPPRESS_UNTIL_SUCCESS,
                    "model switch must NOT clear credit/auth suppression"
                );
            })
            .await;
    }
    /// The per-turn suppression notification is tailored to the failure reason.
    #[tokio::test(flavor = "current_thread")]
    async fn suppression_notification_is_reason_specific() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                async fn notification_for(reason: SuppressReason) -> String {
                    let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
                    let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
                    let actor = create_test_actor(
                            10_000,
                            200_000,
                            85,
                            gateway_tx,
                            persistence_tx,
                        )
                        .await;
                    actor.suppress_auto_compaction(reason, 1_000, 200_000).await;
                    let mut text = None;
                    while let Ok(msg) = persistence_rx.try_recv() {
                        if let PersistenceMsg::Update(
                            crate::session::storage::SessionUpdate::Xai(notif),
                        ) = msg
                            && let crate::extensions::notification::SessionUpdate::AutoCompactFailed {
                                error,
                            } = &notif.update
                        {
                            text = Some(error.clone());
                        }
                    }
                    text.expect("expected an AutoCompactFailed notification")
                }
                let credit = notification_for(SuppressReason::CreditBlock).await;
                assert!(credit.contains("spending limit"), "credit_block: {credit}");
                let auth = notification_for(SuppressReason::Auth).await;
                assert!(auth.contains("/login"), "auth: {auth}");
                let size = notification_for(SuppressReason::Size).await;
                assert!(size.contains("too large to compact"), "size: {size}");
                let schema = notification_for(SuppressReason::Schema).await;
                assert!(schema.contains("can't be summarized"), "schema: {schema}");
                let other = notification_for(SuppressReason::Other).await;
                assert!(other.contains("/new"), "other: {other}");
            })
            .await;
    }
    /// Mock LLM endpoint answering every request with a deterministic 400.
    async fn spawn_deterministic_400_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let body =
                        r#"{"error":{"type":"invalid_request_error","message":"bad schema"}}"#;
                    let resp = format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }
    /// A deterministic failure suppresses auto-compaction only on the AUTO
    /// path — never for a bare manual `/compact`.
    #[tokio::test(flavor = "current_thread")]
    async fn bare_manual_compact_failure_does_not_suppress_auto() {
        use crate::session::compaction_config::SUPPRESS_NONE;
        use std::sync::atomic::Ordering::Relaxed;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
                let actor = Arc::new(
                    create_test_actor(50_000, 200_000, 85, gateway_tx, persistence_tx).await,
                );
                let base_url = spawn_deterministic_400_server().await;
                let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
                cfg.base_url = base_url;
                actor.chat_state_handle.update_sampling_config(cfg);
                actor.chat_state_handle.replace_conversation(vec![
                    ConversationItem::system("sys"),
                    ConversationItem::user("hello"),
                ]);
                let result = actor.run_compact(None).await;
                assert!(result.is_err(), "mock 400 must fail the compaction");
                assert_eq!(
                    actor.compaction.auto_compact_suppressed.load(Relaxed),
                    SUPPRESS_NONE,
                    "manual /compact (even without args) must never set auto-compact suppression"
                );
                let result = actor
                    .run_compact_only(AutoCompactTriggerInfo {
                        tokens_used: 180_000,
                        context_window: 200_000,
                        percentage: 90,
                    })
                    .await;
                assert!(result.is_err(), "mock 400 must fail the compaction");
                assert_ne!(
                    actor.compaction.auto_compact_suppressed.load(Relaxed),
                    SUPPRESS_NONE,
                    "the same deterministic failure on the AUTO path must suppress"
                );
            })
            .await;
    }
    /// A forked session whose whole-transcript inherited prefix alone exceeds
    /// the auto-compact threshold releases the prefix on compaction (so the
    /// conversation can actually shrink below the threshold) and keeps the
    /// release sticky across further compactions (no unbounded compaction loop).
    #[tokio::test(flavor = "current_thread")]
    async fn forked_prefix_released_under_pressure_and_stays_released() {
        use crate::session::compaction_config::SUPPRESS_NONE;
        use std::sync::atomic::Ordering::Relaxed;
        use xai_grok_test_support::MockInferenceServer;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
                let filler = "x".repeat(8_000);
                let mut conv = vec![ConversationItem::system("small system prompt")];
                for i in 0..9 {
                    conv.push(ConversationItem::user(format!("u{i} {filler}")));
                    conv.push(ConversationItem::assistant(format!("a{i} {filler}")));
                }
                conv.push(ConversationItem::user("final query"));
                let prefix_len = conv.len();
                let mut actor = create_test_actor(0, 40_000, 80, gateway_tx, persistence_tx).await;
                actor.startup_hints.inherited_prefix_len = Some(prefix_len);
                let actor = Arc::new(actor);
                let server = MockInferenceServer::start().await.unwrap();
                server.set_response("Summary of prior work. ".repeat(30));
                let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
                cfg.base_url = server.url();
                actor.chat_state_handle.update_sampling_config(cfg);
                actor.chat_state_handle.replace_conversation(conv);
                let threshold_tokens = 40_000u64 * 80 / 100;
                let before = actor.chat_state_handle.get_total_tokens().await;
                assert!(
                    before > threshold_tokens,
                    "seed must exceed threshold: {before} <= {threshold_tokens}"
                );
                let result = actor.run_compact(None).await;
                assert!(result.is_ok(), "compaction should succeed: {result:?}");
                assert!(
                    actor.compaction.prefix_released.load(Relaxed),
                    "prefix must be released under pressure"
                );
                let after = actor.chat_state_handle.get_total_tokens().await;
                assert!(
                    after < threshold_tokens,
                    "released history must drop below threshold: {after} >= {threshold_tokens}"
                );
                assert!(
                    actor.chat_state_handle.get_conversation_len().await < prefix_len,
                    "conversation must shrink below the pinned prefix floor"
                );
                assert_eq!(
                    actor.compaction.auto_compact_suppressed.load(Relaxed),
                    SUPPRESS_NONE,
                    "a shrunk conversation must not suppress AUTO"
                );
                let result = actor.run_compact(None).await;
                assert!(
                    result.is_ok(),
                    "second compaction should succeed: {result:?}"
                );
                assert!(
                    actor.compaction.prefix_released.load(Relaxed),
                    "release must stay sticky across compactions"
                );
                let after2 = actor.chat_state_handle.get_total_tokens().await;
                assert!(
                    after2 < threshold_tokens,
                    "sticky release must keep the session under threshold: {after2}"
                );
            })
            .await;
    }
    /// When even the released (summarized) history still exceeds the threshold
    /// -- the pathological case where the system prompt alone is over budget --
    /// a forked session sets sticky suppression (WITHOUT a user-facing failure
    /// event) instead of clearing it, so AUTO is not immediately re-armed while the
    /// compaction itself still reports success.
    #[tokio::test(flavor = "current_thread")]
    async fn forked_release_still_over_threshold_suppresses_auto() {
        use crate::session::compaction_config::SUPPRESS_STICKY;
        use std::sync::atomic::Ordering::Relaxed;
        use xai_grok_test_support::MockInferenceServer;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
                let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
                let huge_system = "s".repeat(150_000);
                let conv = vec![
                    ConversationItem::system(huge_system),
                    ConversationItem::user("q"),
                    ConversationItem::assistant("a"),
                    ConversationItem::user("final query"),
                ];
                let prefix_len = conv.len();
                let mut actor = create_test_actor(0, 40_000, 80, gateway_tx, persistence_tx).await;
                actor.startup_hints.inherited_prefix_len = Some(prefix_len);
                let actor = Arc::new(actor);
                let server = MockInferenceServer::start().await.unwrap();
                server.set_response("Summary. ".repeat(70));
                let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
                cfg.base_url = server.url();
                actor.chat_state_handle.update_sampling_config(cfg);
                actor.chat_state_handle.replace_conversation(conv);
                let threshold_tokens = 40_000u64 * 80 / 100;
                let before = actor.chat_state_handle.get_total_tokens().await;
                assert!(
                    before > threshold_tokens,
                    "seed must exceed threshold: {before}"
                );
                let result = actor.run_compact(None).await;
                assert!(result.is_ok(), "compaction should succeed: {result:?}");
                assert!(
                    actor.compaction.prefix_released.load(Relaxed),
                    "prefix must be released under pressure"
                );
                assert_eq!(
                    actor.compaction.auto_compact_suppressed.load(Relaxed),
                    SUPPRESS_STICKY,
                    "an over-threshold released history must set sticky suppression"
                );
                let mut saw_failure = false;
                while let Ok(msg) = persistence_rx.try_recv() {
                    if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(
                        notif,
                    )) = msg
                        && matches!(
                            &notif.update,
                            crate::extensions::notification::SessionUpdate::AutoCompactFailed { .. }
                        )
                    {
                        saw_failure = true;
                    }
                }
                assert!(
                    !saw_failure,
                    "successful compaction must not emit AutoCompactFailed"
                );
            })
            .await;
    }
    /// `classify_suppress_reason` maps each deterministic-failure shape to its
    /// fixed [`SuppressReason`].
    #[test]
    fn classify_suppress_reason_maps_error_text() {
        let classify = SessionActor::classify_suppress_reason;
        assert_eq!(
            classify("caller does not have permission … spending-limit reached"),
            SuppressReason::CreditBlock
        );
        assert_eq!(
            classify("you have run out of credits"),
            SuppressReason::CreditBlock
        );
        assert_eq!(
            classify("API error (status 402 Payment Required): Grok Build usage balance exhausted"),
            SuppressReason::CreditBlock
        );
        assert_eq!(
            classify("Grok Build usage limit reached"),
            SuppressReason::CreditBlock
        );
        assert_eq!(
            classify("This model's maximum prompt length is 500000"),
            SuppressReason::Size
        );
        assert_eq!(
            classify("compact failed: The prompt is too long for this model's context window."),
            SuppressReason::Size
        );
        assert_eq!(
            classify("provider error: context_length_exceeded"),
            SuppressReason::Size
        );
        assert_eq!(
            classify("API error (status 401 Unauthorized)"),
            SuppressReason::Auth
        );
        assert_eq!(
            classify("provider returned invalid_request_error: messages.3"),
            SuppressReason::Schema
        );
        assert_eq!(
            classify("upstream 500 internal error"),
            SuppressReason::Other
        );
    }
    /// `SuppressReason::as_str` is the stable telemetry wire value — BQ/OTLP and
    /// dashboards key off these exact strings. Lock them so a rename can't break monitoring.
    #[test]
    fn suppress_reason_as_str_is_stable() {
        assert_eq!(SuppressReason::CreditBlock.as_str(), "credit_block");
        assert_eq!(SuppressReason::Size.as_str(), "size");
        assert_eq!(SuppressReason::Auth.as_str(), "auth");
        assert_eq!(SuppressReason::Schema.as_str(), "schema");
        assert_eq!(SuppressReason::Other.as_str(), "other");
    }
    mod preserve_prefix {
        use super::super::preserve_inherited_prefix;
        use super::super::project_preserved_reseed_tokens;
        use xai_grok_sampling_types::conversation::ConversationItem;
        #[test]
        fn splices_inherited_with_compacted_suffix() {
            let conversation = vec![
                ConversationItem::system("sys"),
                ConversationItem::user("parent q1"),
                ConversationItem::assistant("parent a1"),
                ConversationItem::user("child q1"),
            ];
            let compacted = vec![
                ConversationItem::system("sys"),
                ConversationItem::user("summary"),
            ];
            let items = preserve_inherited_prefix(&conversation, compacted, 3).expect("Ok");
            assert_eq!(items.len(), 4);
            assert!(matches!(items[0], ConversationItem::System(_)));
        }
        /// Invariant: a head-only prefix lets compaction shrink the conversation;
        /// a whole-transcript prefix does not (that pinned floor is the loop).
        #[test]
        fn head_only_shrinks_full_transcript_does_not() {
            let mut conversation = vec![ConversationItem::system("sys")];
            for i in 0..8 {
                conversation.push(ConversationItem::user(format!("u{i}")));
                conversation.push(ConversationItem::assistant(format!("a{i}")));
            }
            let compacted = vec![
                ConversationItem::system("sys"),
                ConversationItem::assistant("summary"),
            ];
            let fixed = preserve_inherited_prefix(&conversation, compacted.clone(), 1).expect("Ok");
            assert!(fixed.len() < conversation.len(), "head-only shrinks");
            let buggy = preserve_inherited_prefix(&conversation, compacted, conversation.len())
                .expect("Ok");
            assert!(
                buggy.len() >= conversation.len(),
                "full prefix never shrinks"
            );
        }
        /// The reseed projection calibrates the bytes/4 estimate to real tokens
        /// (ratio != 1) and caps at the pre-compaction total, so the release
        /// decision reflects what the trigger applies next turn.
        #[test]
        fn project_preserved_reseed_tokens_calibrates_and_caps() {
            assert_eq!(
                project_preserved_reseed_tokens(30_000, 100_000, 50_000),
                60_000
            );
            assert_eq!(
                project_preserved_reseed_tokens(40_000, 70_000, 35_000),
                70_000
            );
            assert_eq!(
                project_preserved_reseed_tokens(20_000, 40_000, 40_000),
                20_000
            );
            assert_eq!(project_preserved_reseed_tokens(10, 5, 0), 5);
        }
        /// Both prefix and re-injected suffix may carry AGENTS.md; the splice must
        /// leave exactly one (else the model sees project instructions twice).
        #[test]
        fn does_not_duplicate_agents_md() {
            let conversation = vec![
                ConversationItem::system("sys"),
                ConversationItem::project_instructions("AGENTS.md"),
                ConversationItem::user("work"),
            ];
            let compacted = vec![
                ConversationItem::system("sys"),
                ConversationItem::project_instructions("AGENTS.md"),
                ConversationItem::user("summary"),
            ];
            let items = preserve_inherited_prefix(&conversation, compacted, 2).expect("Ok");
            let pi = items
                .iter()
                .filter(|i| super::super::is_project_instructions(i))
                .count();
            assert_eq!(pi, 1, "exactly one project-instructions item, not two");
        }
        #[test]
        fn keeps_reinjected_agents_md_when_prefix_lacks_it() {
            let conversation = vec![
                ConversationItem::system("sys"),
                ConversationItem::user("work"),
            ];
            let compacted = vec![
                ConversationItem::system("sys"),
                ConversationItem::project_instructions("AGENTS.md"),
                ConversationItem::user("summary"),
            ];
            let items = preserve_inherited_prefix(&conversation, compacted, 1).expect("Ok");
            let pi = items
                .iter()
                .filter(|i| super::super::is_project_instructions(i))
                .count();
            assert_eq!(
                pi, 1,
                "re-injected AGENTS.md preserved when prefix lacks one"
            );
        }
    }
    #[allow(clippy::field_reassign_with_default)]
    async fn create_test_actor_with_memory(
        total_tokens: u64,
        context_window: u64,
        threshold_percent: u8,
        gateway_tx: mpsc::UnboundedSender<xai_acp_lib::AcpClientMessage>,
        persistence_tx: mpsc::UnboundedSender<PersistenceMsg>,
        memory_config: Option<crate::config::MemoryConfig>,
    ) -> SessionActor {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd_path = tmp.path().to_path_buf();
        let memory_storage = memory_config
            .as_ref()
            .filter(|mc| mc.enabled)
            .map(|_| crate::session::memory::MemoryStorage::new(&cwd_path, None));
        std::mem::forget(tmp);
        let memory_initial_injection_config = memory_config
            .as_ref()
            .map_or_else(Default::default, |mc| mc.initial_injection.clone());
        let mut actor = create_test_actor(
            total_tokens,
            context_window,
            threshold_percent,
            gateway_tx,
            persistence_tx,
        )
        .await;
        actor.memory = crate::session::memory_state::SessionMemory {
            flush_config: memory_config
                .as_ref()
                .map_or_else(Default::default, |mc| mc.flush.clone()),
            is_flushing: std::sync::atomic::AtomicBool::new(false),
            last_flush_compaction: std::sync::atomic::AtomicU64::new(0),
            storage: std::cell::RefCell::new(memory_storage),
            save_on_end: true,
            backend_params: None,
            initial_injection_config: memory_initial_injection_config,
            context_injected: std::sync::atomic::AtomicBool::new(false),
            flush_count: std::sync::atomic::AtomicU64::new(0),
            last_flush_content: std::cell::RefCell::new(None),
            flush_success_count: std::sync::atomic::AtomicU64::new(0),
            flush_error_count: std::sync::atomic::AtomicU64::new(0),
            search_counter: std::cell::RefCell::new(None),
            injection_count: std::sync::atomic::AtomicU64::new(0),
            compaction_recovery_count: std::sync::atomic::AtomicU64::new(0),
            chunks_added: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dream_config: Default::default(),
            dream_count: std::sync::atomic::AtomicU64::new(0),
            dream_success_count: std::sync::atomic::AtomicU64::new(0),
            dream_error_count: std::sync::atomic::AtomicU64::new(0),
        };
        actor.idle_flush_timeout = memory_config
            .as_ref()
            .and_then(|mc| mc.flush.idle_timeout_secs)
            .map(std::time::Duration::from_secs);
        actor.dream_check_timeout = memory_config
            .as_ref()
            .filter(|mc| mc.dream.enabled)
            .and_then(|mc| mc.dream.check_interval_secs)
            .filter(|&s| s > 0)
            .map(std::time::Duration::from_secs);
        actor
    }
    /// Verify that `last_idle_flush_conversation_len` is reset after
    /// compaction shrinks the conversation. Without this reset the
    /// interval flush guard (`current_len > last_len`) stays false
    /// because the compacted conversation is shorter than the stored
    /// pre-compaction length.
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::field_reassign_with_default)]
    async fn test_idle_flush_conversation_len_reset_after_compaction() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _) = mpsc::unbounded_channel();
                let (persistence_tx, _) = mpsc::unbounded_channel();
                let mut config = crate::config::MemoryConfig::default();
                config.enabled = true;
                config.flush.idle_timeout_secs = Some(60);
                let actor = create_test_actor_with_memory(
                    50_000,
                    100_000,
                    85,
                    gateway_tx,
                    persistence_tx,
                    Some(config),
                )
                .await;
                for _ in 0..80 {
                    actor
                        .chat_state_handle
                        .push_user_message(ConversationItem::user("hello".to_string()));
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                actor
                    .last_idle_flush_conversation_len
                    .store(80, std::sync::atomic::Ordering::Relaxed);
                {
                    let current_len = actor.chat_state_handle.get_conversation_len().await;
                    let last_len = actor
                        .last_idle_flush_conversation_len
                        .load(std::sync::atomic::Ordering::Relaxed);
                    assert_eq!(current_len, 80);
                    assert!(
                        current_len <= last_len,
                        "guard should block: no new messages"
                    );
                }
                {
                    let compacted = vec![ConversationItem::user("compacted summary".to_string())];
                    let new_len = compacted.len();
                    actor
                        .chat_state_handle
                        .replace_conversation_for_compaction(compacted);
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    actor
                        .last_idle_flush_conversation_len
                        .store(new_len, std::sync::atomic::Ordering::Relaxed);
                }
                {
                    actor
                        .chat_state_handle
                        .push_user_message(ConversationItem::user("new message".to_string()));
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    let current_len = actor.chat_state_handle.get_conversation_len().await;
                    let last_len = actor
                        .last_idle_flush_conversation_len
                        .load(std::sync::atomic::Ordering::Relaxed);
                    assert_eq!(current_len, 2, "summary + new message");
                    assert_eq!(last_len, 1, "reset to post-compaction length");
                    assert!(
                        current_len > last_len,
                        "guard should allow flush after compaction + new message"
                    );
                }
            })
            .await;
    }
    fn api_error_with_context_window(context_window: u64) -> xai_grok_sampler::SamplingErrorInfo {
        xai_grok_sampler::SamplingErrorInfo {
            kind: xai_grok_sampler::SamplingErrorKind::Api,
            status_code: Some(400),
            message: "prompt is too long".to_string(),
            is_retryable: false,
            retry_after_secs: None,
            model_metadata: Some(crate::sampling::ResponseModelMetadata {
                context_window: Some(context_window),
                max_completion_tokens: None,
                models_etag: None,
            }),
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
        }
    }
    /// Primary scenario: remote settings shrinks the context window mid-session.
    /// The shell's last-known token count (214K) exceeds the new limit (200K) —
    /// should_compact_on_error must return true so the session can recover.
    #[tokio::test(flavor = "current_thread")]
    async fn test_compact_on_error_triggers_when_tokens_exceed_new_window() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _) = mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
                let (persistence_tx, _) = mpsc::unbounded_channel::<PersistenceMsg>();
                let actor =
                    create_test_actor(214_000, 1_000_000, 85, gateway_tx, persistence_tx).await;
                let err = api_error_with_context_window(200_000);
                assert!(actor.should_compact_on_error(&err).await);
            })
            .await;
    }
    /// When tracked tokens are within the new limit, the error was not a context
    /// overflow — do not compact.
    #[tokio::test(flavor = "current_thread")]
    async fn test_compact_on_error_no_trigger_when_tokens_within_new_window() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _) = mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
                let (persistence_tx, _) = mpsc::unbounded_channel::<PersistenceMsg>();
                let actor =
                    create_test_actor(150_000, 1_000_000, 85, gateway_tx, persistence_tx).await;
                let err = api_error_with_context_window(200_000);
                assert!(!actor.should_compact_on_error(&err).await);
            })
            .await;
    }
    /// If the proxy hasn't been updated yet, model_metadata is None — must be
    /// a no-op for backwards compatibility.
    #[tokio::test(flavor = "current_thread")]
    async fn test_compact_on_error_noop_without_model_metadata() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _) = mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
                let (persistence_tx, _) = mpsc::unbounded_channel::<PersistenceMsg>();
                let actor =
                    create_test_actor(500_000, 200_000, 85, gateway_tx, persistence_tx).await;
                let err = xai_grok_sampler::SamplingErrorInfo {
                    kind: xai_grok_sampler::SamplingErrorKind::Api,
                    status_code: Some(400),
                    message: "prompt is too long".to_string(),
                    is_retryable: false,
                    retry_after_secs: None,
                    model_metadata: None,
                    empty_response_context: None,
                    doom_loop_triggers: None,
                    doom_loop_aborted_at_chunk: None,
                };
                assert!(!actor.should_compact_on_error(&err).await);
            })
            .await;
    }
    /// Pre-sampling check uses estimated tokens (includes tool-result delta).
    #[tokio::test(flavor = "current_thread")]
    async fn test_pre_sampling_uses_estimated_tokens() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _) = mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
                let (persistence_tx, _) = mpsc::unbounded_channel::<PersistenceMsg>();
                let actor =
                    create_test_actor(80_000, 100_000, 85, gateway_tx, persistence_tx).await;
                let result = actor.check_auto_compact_needed().await;
                assert!(result.is_none(), "80% should not trigger at 85% threshold");
                actor.chat_state_handle.record_token_usage(90_000);
                let result = actor.check_auto_compact_needed().await;
                assert!(result.is_some(), "90% should trigger");
                assert_eq!(result.unwrap().percentage, 90);
            })
            .await;
    }
    /// Model-switch compaction fires when switching to a smaller context window.
    #[tokio::test(flavor = "current_thread")]
    async fn test_model_switch_compaction_triggers_on_downgrade() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _) = mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
                let (persistence_tx, _) = mpsc::unbounded_channel::<PersistenceMsg>();
                let actor =
                    create_test_actor(86_000, 100_000, 85, gateway_tx, persistence_tx).await;
                actor.compaction.previous_model.set(Some(
                    crate::session::compaction_config::PreviousModelInfo {
                        model_slug: "large-model".to_string(),
                        context_window: 200_000,
                    },
                ));
                let prev = actor.compaction.previous_model.take();
                assert!(prev.is_some());
                let prev = prev.unwrap();
                assert_eq!(prev.context_window, 200_000);
                let cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
                assert!(prev.context_window > cfg.context_window.get());
                let total = actor.chat_state_handle.get_estimated_total_tokens().await;
                let trigger = actor.should_auto_compact(total, cfg.context_window);
                assert!(trigger.is_some(), "86% > 85% threshold, should trigger");
                actor.compaction.previous_model.set(Some(
                    crate::session::compaction_config::PreviousModelInfo {
                        model_slug: "small-model".to_string(),
                        context_window: 50_000,
                    },
                ));
                let prev = actor.compaction.previous_model.take().unwrap();
                assert!(prev.context_window <= cfg.context_window.get());
            })
            .await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn get_transcript_path_returns_some_when_file_exists() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) =
                    mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
                let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
                let mut actor =
                    create_test_actor(50_000, 200_000, 85, gateway_tx, persistence_tx).await;
                actor.compaction.compaction_mode = xai_chat_state::CompactionMode::Transcript;
                let session_dir = crate::session::persistence::session_dir(&actor.session_info);
                std::fs::create_dir_all(&session_dir).unwrap();
                let updates_path = session_dir.join("updates.jsonl");
                std::fs::write(&updates_path, "{}\n").unwrap();
                let result = actor.get_transcript_path();
                assert!(result.is_some(), "file exists → Some");
                assert!(
                    result.as_ref().unwrap().ends_with("updates.jsonl"),
                    "path should end with updates.jsonl, got: {:?}",
                    result,
                );
                let hint = actor.transcript_hint().expect("transcript hint present");
                assert!(hint.contains("read the full transcript"));
                assert!(hint.ends_with("updates.jsonl"));
                actor.compaction.compaction_mode = xai_chat_state::CompactionMode::Summary;
                assert!(actor.transcript_hint().is_none());
                let _ = std::fs::remove_file(&updates_path);
                let _ = std::fs::remove_dir_all(&session_dir);
            })
            .await;
    }
}
