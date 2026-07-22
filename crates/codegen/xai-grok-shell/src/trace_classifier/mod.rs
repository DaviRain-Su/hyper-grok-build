//! Trace-replay classifier.
//!
//! Reads an offline session trace JSON and replays the Layer-2
//! `evaluate_todo_gate` and Layer-3 LazinessDetector classifier
//! against every turn, emitting one JSONL record per turn so an
//! operator can see what the gate and classifier *would have decided*
//! at each turn-end.
//!
//! Production wiring lives in
//! `crate::session::acp_session::{maybe_fire_laziness_check, evaluate_todo_gate}`
//! — this module is a thin replay harness around the same `pub(crate)`
//! helpers. New crate-internal so we get visibility for free without
//! widening the production surface.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use xai_grok_sampling_types::{
    ContentPart, ConversationItem, ConversationRequest, SystemItem, UserItem,
};

use crate::session::{
    CollectedTodoGateInput, DebugDecision, LAZINESS_CLASSIFIER_PROMPT,
    LAZINESS_CLASSIFIER_TIMEOUT_MS, LAZINESS_CONTEXT_ITEM_LIMIT, LAZINESS_DEFAULT_MIN_CONFIDENCE,
    LAZINESS_INCLUDE_REASONING, LAZINESS_MAX_OUTPUT_TOKENS, LAZINESS_MIN_ASSISTANT_TURNS,
    LAZINESS_MIN_USER_TURNS, LAZINESS_REQ_ID_PREFIX, LAZINESS_USER_PREAMBLE, TodoGateDecision,
    TodoGateReason, classify_debug_decision, evaluate_todo_gate, flatten_transcript_for_classifier,
    format_runtime_state_line, laziness_window_start, parse_classifier_output,
};
use crate::tools::todo::{TodoItem, TodoPriority, TodoState, TodoStatus};

// Trace JSON shape

/// One element of the top-level trace JSON array.
#[derive(Debug, Deserialize)]
pub struct TurnRecord {
    pub turn: String,
    pub trace: TurnTrace,
}

#[derive(Debug, Deserialize)]
pub struct TurnTrace {
    pub metadata: TurnMetadata,
    /// Post-turn conversation snapshot. Deserializes directly into
    /// the canonical [`ConversationItem`] enum — the production wire
    /// format already matches.
    #[serde(rename = "afterStateHistory")]
    pub after_state_history: Vec<ConversationItem>,
}

#[derive(Debug, Deserialize)]
pub struct TurnMetadata {
    #[serde(default)]
    pub turn_number: Option<u64>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    /// ISO 8601 timestamp emitted by the trace serializer at turn
    /// start (e.g. `"2026-05-21T03:29:30.605351+00:00"`). Used to
    /// compute a lower bound on per-turn wall-clock duration —
    /// see [`compute_turn_elapsed_seconds`].
    #[serde(default)]
    pub turn_started_at: Option<String>,
}

// Output schema. Borrowed slices throughout so we serialize once
// straight from `process_turn`'s working state without any clones.
// (N2/N12/N15.)

#[derive(Debug, Serialize)]
pub struct TodoSnapshotOut<'a> {
    #[serde(serialize_with = "serialize_str_slice")]
    pub pending: &'a [String],
    #[serde(serialize_with = "serialize_str_slice")]
    pub in_progress_unbacked: &'a [String],
    #[serde(serialize_with = "serialize_str_slice")]
    pub in_progress_backed: &'a [String],
}

#[derive(Debug, Serialize)]
pub struct TodoGateOut<'a> {
    pub decision: GateDecisionKind,
    pub reason: Option<GateReasonKind>,
    pub reminder: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecisionKind {
    Continue,
    Nudge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateReasonKind {
    InFlight,
}

#[derive(Debug, Serialize)]
pub struct ParsedClassifierOut<'a> {
    pub category: &'static str,
    pub confidence: f32,
    pub evidence: &'a str,
}

#[derive(Debug, Serialize)]
pub struct LazinessOut<'a> {
    pub model_id: &'a str,
    pub elapsed_ms: u64,
    pub raw_output: Option<&'a str>,
    pub parsed: Option<ParsedClassifierOut<'a>>,
    pub decision: LazinessDecisionKind,
    /// `classifier_error` / `timeout` / `parse_error`. `None` when a
    /// verdict was produced.
    pub abort_reason: Option<AbortReasonKind>,
    pub error_detail: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LazinessDecisionKind {
    WouldNudge,
    NoNudgeLowConfidence,
    NoNudgeNotStalled,
    Aborted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortReasonKind {
    ClassifierError,
    Timeout,
    ParseError,
}

#[derive(Debug, Serialize)]
pub struct TurnLine<'a> {
    pub turn_id: &'a str,
    pub turn_number: Option<u64>,
    pub request_id: Option<&'a str>,
    pub items_in_history: usize,
    pub items_after_window_trim: usize,
    /// Subagents + backgrounded terminal tasks outstanding at turn end.
    /// This is the count fed to the Layer-2 TodoGate (production:
    /// `collect_todo_gate_input.backing_task_count`).
    pub gate_backing_task_count: usize,
    /// Backgrounded terminal tasks only (excludes subagents) — what
    /// the Layer-3 classifier `[runtime_state]` line carries
    /// (production: `snapshot_backing_task_count_for_debug_log`).
    pub classifier_backing_task_count: usize,
    pub todo_state: TodoSnapshotOut<'a>,
    pub todo_gate: TodoGateOut<'a>,
    pub laziness_classifier: LazinessOut<'a>,
    /// The resolved `include_reasoning` value used to render the
    /// classifier transcript for this turn. Lives next to
    /// `laziness_classifier` so log-diffs of two A/B runs are obvious.
    pub include_reasoning: bool,
    /// Wall-clock seconds from this turn's `turn_started_at` to the
    /// NEXT turn's `turn_started_at`. Includes agent compute, harness
    /// latency, AND post-turn user think-time — a LOWER bound on the
    /// actual turn cost the classifier needs to evaluate "minutes vs
    /// hours" claims. `None` for the last turn (no follow-up to delta
    /// against) and for turns whose metadata lacks `turn_started_at`.
    /// Mirrors what the classifier saw in its `[runtime_state]` line
    /// for the same turn. See `compute_turn_elapsed_seconds` for the
    /// computation details.
    pub turn_elapsed_seconds: Option<u64>,
}

/// Serialize `&[String]` as a JSON array without allocating an
/// intermediate `Vec<&str>` — `SerializeSeq` accepts borrowed `&str`s
/// directly. (N12)
fn serialize_str_slice<S: serde::Serializer>(
    items: &&[String],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;
    let mut seq = serializer.serialize_seq(Some(items.len()))?;
    for item in *items {
        seq.serialize_element(item.as_str())?;
    }
    seq.end()
}

// Summary counters

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub turns: usize,
    pub gate_continue: usize,
    pub gate_nudge: usize,
    pub laz_would_nudge: usize,
    pub laz_not_stalled: usize,
    pub laz_low_confidence: usize,
    pub laz_aborted: usize,
}

impl Summary {
    pub fn render(&self) -> String {
        // Lead with the common (non-action) class for parity with the
        // laziness counter ordering. (F28)
        format!(
            "Processed {} turns. TodoGate: {} Continue, {} Nudge. \
             Laziness: {} NoNudge-NotStalled, {} NoNudge-LowConfidence, {} WouldNudge, {} Aborted.",
            self.turns,
            self.gate_continue,
            self.gate_nudge,
            self.laz_not_stalled,
            self.laz_low_confidence,
            self.laz_would_nudge,
            self.laz_aborted,
        )
    }
}

// TodoState reconstruction

const TODO_WRITE_TOOL_NAME: &str = "todo_write";

/// Replay every assistant `todo_write` call in order so the resulting
/// `TodoState` matches what the production tool runtime would have
/// held at turn-end.
///
/// Production's `TodoWriteTool` validates inputs (no duplicate IDs)
/// before dispatching to `apply_replace` / `apply_merge`. We mirror
/// that guard here so duplicate-id payloads don't drift the replay
/// off the production trajectory. (F2)
///
/// If a call's `arguments` field fails to parse OR fails validation
/// we skip the call (consistent with the "skip-on-malformed" policy)
/// and keep going.
pub fn reconstruct_todo_state(items: &[ConversationItem]) -> TodoState {
    let mut state = TodoState::default();
    for item in items {
        let ConversationItem::Assistant(asst) = item else {
            continue;
        };
        for tc in &asst.tool_calls {
            if tc.name != TODO_WRITE_TOOL_NAME {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<TodoWriteArgs>(&tc.arguments) else {
                continue;
            };
            if has_duplicate_ids(&parsed.todos) {
                tracing::debug!(
                    tool_call_id = %tc.id,
                    "trace_classifier: skipping todo_write with duplicate IDs",
                );
                continue;
            }
            if parsed.merge {
                apply_merge(&mut state, parsed.todos);
            } else {
                apply_replace(&mut state, parsed.todos);
            }
        }
    }
    state
}

fn has_duplicate_ids(updates: &[TodoUpdateArgs]) -> bool {
    use std::collections::HashSet;
    let mut seen = HashSet::with_capacity(updates.len());
    updates.iter().any(|u| !seen.insert(u.id.as_str()))
}

#[derive(Debug, Deserialize)]
struct TodoWriteArgs {
    #[serde(default = "default_merge")]
    merge: bool,
    #[serde(default)]
    todos: Vec<TodoUpdateArgs>,
}

const fn default_merge() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct TodoUpdateArgs {
    id: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    status: Option<TodoStatus>,
}

/// `merge=false`: replace the state entirely. Mirrors production's
/// `apply_replace`. Split out from the merge path to match the
/// two-function shape in `xai-grok-tools` (F29).
fn apply_replace(state: &mut TodoState, updates: Vec<TodoUpdateArgs>) {
    state.clear();
    for u in updates {
        push_new(state, u);
    }
}

/// `merge=true`: upsert by id. Mirrors `apply_merge` in
/// `xai-grok-tools` — existing items keep their prior content when
/// the update omits `content`. (F29)
fn apply_merge(state: &mut TodoState, updates: Vec<TodoUpdateArgs>) {
    for u in updates {
        if state.update(&u.id, u.content.as_deref(), u.status) {
            continue;
        }
        push_new(state, u);
    }
}

fn push_new(state: &mut TodoState, u: TodoUpdateArgs) {
    let TodoUpdateArgs {
        id,
        content,
        status,
    } = u;
    let content = content
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| id.clone());
    let status = status.unwrap_or(TodoStatus::Pending);
    state.push(
        id,
        TodoItem {
            content,
            priority: TodoPriority::default(),
            status,
            meta: None,
        },
    );
}

// Backing-task counts.
//
// Production has TWO different counts (F1):
//
//   * Layer-2 (`TodoGate`): outstanding_subagents + incomplete
//     terminal/monitor tasks. See `collect_todo_gate_input` in
//     `acp_session.rs`.
//   * Layer-3 (`LazinessDetector` runtime_state line): incomplete
//     terminal/monitor tasks ONLY (no subagents). See
//     `snapshot_backing_task_count_for_debug_log` and the explanatory
//     comment at `acp_session.rs:10018-10025` ("the prompt-scoped
//     subagent count isn't available without plumbing the prompt_id
//     through").
//
// We mirror that asymmetry exactly.

/// Background-dispatching tool kinds. The `Option<BackgroundDispatchKind>`
/// returned by [`background_kind`] makes "not a background dispatch"
/// a compile-time-tracked absence rather than a sentinel variant. (N14)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundDispatchKind {
    Terminal,
    Subagent,
}

/// Categorise an assistant tool call as background-dispatching. Order
/// matters: production traces ship the renamed `background` key
/// before the legacy `is_background` alias. (F26)
fn background_kind(name: &str, arguments: &str) -> Option<BackgroundDispatchKind> {
    match name {
        "spawn_subagent" => Some(BackgroundDispatchKind::Subagent),
        "monitor" => Some(BackgroundDispatchKind::Terminal),
        "run_terminal_command" => {
            let v: serde_json::Value = serde_json::from_str(arguments).ok()?;
            let truthy = v.get("background").and_then(serde_json::Value::as_bool) == Some(true)
                || v.get("is_background").and_then(serde_json::Value::as_bool) == Some(true);
            truthy.then_some(BackgroundDispatchKind::Terminal)
        }
        _ => None,
    }
}

/// Per-turn outstanding-dispatch counts. Walks the history in a
/// single forward pass so a `tool_result` only counts as completing
/// a dispatch that appeared *earlier* in the same history; orphan
/// results (result before its call) are logged as a trace-integrity
/// anomaly but do not satisfy the dispatch. (F3)
///
/// Locality note: this counts dispatches *within the current turn's
/// `afterStateHistory` only*. A subagent dispatched in turn N that is
/// still outstanding at turn N+1 will not appear in turn N+1's
/// history (the production trace serializer drops it). Production's
/// `ToolBridge` tracks live state across turns; the replay can't
/// reconstruct that without an additional cross-turn correlator,
/// which is out of scope. (F10)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackingCounts {
    /// Outstanding `monitor` + `run_terminal_command{background:true}`.
    pub terminal_only: usize,
    /// `terminal_only` + outstanding `spawn_subagent`.
    pub terminal_plus_subagents: usize,
}

pub fn count_outstanding_dispatches(items: &[ConversationItem]) -> BackingCounts {
    use std::collections::HashMap;

    let mut unresolved: HashMap<&str, BackgroundDispatchKind> = HashMap::new();
    for item in items {
        match item {
            ConversationItem::Assistant(asst) => {
                for tc in &asst.tool_calls {
                    if let Some(kind) = background_kind(&tc.name, &tc.arguments) {
                        unresolved.insert(tc.id.as_ref(), kind);
                    }
                }
            }
            ConversationItem::ToolResult(tr) => {
                if unresolved.remove(tr.tool_call_id.as_str()).is_none() {
                    // Either this result is for a non-background tool
                    // call (the common case) or it precedes its
                    // dispatch (trace integrity issue). The latter is
                    // worth surfacing.
                    tracing::trace!(
                        tool_call_id = %tr.tool_call_id,
                        "trace_classifier: tool_result without matching prior dispatch (expected for non-background calls)",
                    );
                }
            }
            _ => {}
        }
    }
    let mut counts = BackingCounts::default();
    for kind in unresolved.values() {
        match kind {
            BackgroundDispatchKind::Terminal => {
                counts.terminal_only += 1;
                counts.terminal_plus_subagents += 1;
            }
            BackgroundDispatchKind::Subagent => {
                counts.terminal_plus_subagents += 1;
            }
        }
    }
    counts
}

// TodoGate — partition + decision.

/// Borrowed view over a `TodoState` partitioned the same way
/// `CollectedTodoGateInput::as_input` does. Single source of truth
/// for both the gate input AND the JSON-output snapshot. We still
/// run the partition twice (here for the snapshot, and again inside
/// `as_input` for the gate decision) because `TodoGateInput`'s fields
/// are module-private to `acp_session.rs` and the task brief forbids
/// widening them — but the *logical* duplication is contained to
/// `partition_todos`, which is unit-tested against the production
/// heuristic.
struct GateView<'a> {
    pending: Vec<&'a str>,
    in_progress_backed: Vec<&'a str>,
    in_progress_unbacked: Vec<&'a str>,
}

fn partition_todos(state: &TodoState, backing_task_count: usize) -> GateView<'_> {
    let mut pending = Vec::new();
    let mut in_progress: Vec<&str> = Vec::new();
    for (_, item) in state.todo_items_with_ids() {
        match item.status {
            TodoStatus::Pending => pending.push(item.content.as_str()),
            TodoStatus::InProgress => in_progress.push(item.content.as_str()),
            TodoStatus::Completed | TodoStatus::Cancelled => {}
        }
    }
    let backed_count = in_progress.len().min(backing_task_count);
    let in_progress_unbacked = in_progress.split_off(backed_count);
    GateView {
        pending,
        in_progress_backed: in_progress,
        in_progress_unbacked,
    }
}

fn gate_reason_kind(reason: TodoGateReason) -> GateReasonKind {
    match reason {
        TodoGateReason::InFlight => GateReasonKind::InFlight,
    }
}

/// Owned-string version of [`TodoGateOut`] held on [`TurnData`] —
/// `as_line()` lends out a borrowed `TodoGateOut<'_>` view.
#[derive(Debug)]
pub struct TodoGateOwned {
    pub decision: GateDecisionKind,
    pub reason: Option<GateReasonKind>,
    pub reminder: Option<String>,
}

fn run_todo_gate(state: &TodoState, gate_backing_task_count: usize) -> TodoGateOwned {
    let collected = CollectedTodoGateInput {
        todos: state
            .todo_items_with_ids()
            .map(|(id, item)| (id.clone(), item.content.clone(), item.status))
            .collect(),
        backing_task_count: gate_backing_task_count,
    };
    let input = collected.as_input();
    match evaluate_todo_gate(&input) {
        TodoGateDecision::Continue => TodoGateOwned {
            decision: GateDecisionKind::Continue,
            reason: None,
            reminder: None,
        },
        TodoGateDecision::Nudge { reminder, reason } => TodoGateOwned {
            decision: GateDecisionKind::Nudge,
            reason: Some(gate_reason_kind(reason)),
            reminder: Some(reminder),
        },
    }
}

// Laziness classifier.

/// Trait so tests can mock the sampler call.
#[async_trait::async_trait]
pub trait ClassifierClient: Send + Sync {
    async fn run(&self, request: ConversationRequest) -> Result<String, String>;
}

/// Production sampler-backed [`ClassifierClient`] used by the CLI
/// binary. Wraps a `xai_grok_sampler::SamplingClient` and pulls the
/// text content out of the response.
pub struct SamplerClassifierClient {
    inner: xai_grok_sampler::SamplingClient,
}

impl SamplerClassifierClient {
    pub fn new(inner: xai_grok_sampler::SamplingClient) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl ClassifierClient for SamplerClassifierClient {
    async fn run(&self, request: ConversationRequest) -> Result<String, String> {
        self.inner
            .conversation_collect(request)
            .await
            .map(|r| {
                r.assistant()
                    .map(|a| a.content.as_ref().to_owned())
                    .unwrap_or_default()
            })
            .map_err(|e| e.to_string())
    }
}

/// Build the two-item `[System, User]` classifier request. Matches
/// `maybe_fire_laziness_check` exactly: same prompt, same wrapper
/// text ([`LAZINESS_USER_PREAMBLE`]), same `temperature: 0.0`, same
/// `reasoning_effort: None`, same telemetry headers shape (random
/// [`LAZINESS_REQ_ID_PREFIX`]`-<uuid>` req id).
///
/// `classifier_backing_task_count` is the *Layer-3* count (terminal
/// tasks only — no subagents); see [`BackingCounts`] and F1.
pub fn build_classifier_request(
    items: &[ConversationItem],
    classifier_backing_task_count: usize,
    model_id: &str,
    session_id: Option<&str>,
    include_reasoning: bool,
    turn_elapsed_seconds: Option<u64>,
) -> ConversationRequest {
    let runtime_state =
        format_runtime_state_line(classifier_backing_task_count, turn_elapsed_seconds);
    let transcript_text = flatten_transcript_for_classifier(items, include_reasoning);

    let convo_items = vec![
        ConversationItem::System(SystemItem {
            content: std::sync::Arc::<str>::from(LAZINESS_CLASSIFIER_PROMPT),
        }),
        ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: std::sync::Arc::<str>::from(format!(
                    "{LAZINESS_USER_PREAMBLE}\
                     === BEGIN TRANSCRIPT ===\n\
                     {runtime_state}\
                     {transcript_text}\
                     === END TRANSCRIPT ===\n"
                )),
            }],
            synthetic_reason: None,
            ..Default::default()
        }),
    ];

    let session_id_str = session_id.map(str::to_owned);
    ConversationRequest {
        items: convo_items,
        tools: vec![],
        hosted_tools: vec![],
        tool_choice: None,
        model: Some(model_id.to_owned()),
        temperature: Some(0.0),
        max_output_tokens: Some(LAZINESS_MAX_OUTPUT_TOKENS),
        reasoning_effort: None,
        x_grok_conv_id: Some(format!("trace-classifier-{}", uuid::Uuid::new_v4())),
        x_grok_req_id: Some(format!("{LAZINESS_REQ_ID_PREFIX}{}", uuid::Uuid::new_v4())),
        x_grok_session_id: session_id_str,
        x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
        ..ConversationRequest::default()
    }
}

fn decision_kind(d: DebugDecision) -> LazinessDecisionKind {
    match d {
        DebugDecision::WouldNudge => LazinessDecisionKind::WouldNudge,
        DebugDecision::NoNudgeNotStalled => LazinessDecisionKind::NoNudgeNotStalled,
        DebugDecision::NoNudgeLowConfidence => LazinessDecisionKind::NoNudgeLowConfidence,
        DebugDecision::Aborted => LazinessDecisionKind::Aborted,
        // Classifier returned stalled_* above threshold; harness blocked
        // injection because the session was not in an active goal.
        DebugDecision::SuppressedNotGoalMode => LazinessDecisionKind::WouldNudge,
    }
}

/// Owned-string twin of [`LazinessOut`]. Held on [`TurnData`];
/// `as_line()` lends out a borrowed `LazinessOut<'_>` view so the
/// JSONL serialization is zero-clone. (N2/N15)
#[derive(Debug)]
pub struct LazinessOwned {
    pub model_id: String,
    pub elapsed_ms: u64,
    pub raw_output: Option<String>,
    pub parsed: Option<ParsedClassifierOwned>,
    pub decision: LazinessDecisionKind,
    pub abort_reason: Option<AbortReasonKind>,
    pub error_detail: Option<String>,
}

#[derive(Debug)]
pub struct ParsedClassifierOwned {
    pub category: &'static str,
    pub confidence: f32,
    pub evidence: String,
}

/// Run a single turn end-to-end against the classifier client.
///
/// `started` is captured AFTER the request is built so `elapsed_ms`
/// reflects only the sampler call wall-clock (F36) — directly
/// comparable to production's `LazinessFireOutcome::Verdict`
/// `classifier_elapsed_ms` which times the same span.
///
/// Production runs the sampler call inside a `tokio::select!` with a
/// biased timeout arm and an abort poller. We use the simpler
/// `tokio::time::timeout` (F9) — replay has no user-input to abort on,
/// so the abort poller is dead weight, and the biased ordering only
/// matters for a "user cancels while sampler is still running"
/// race that can't happen offline.
async fn classify_turn(
    items: &[ConversationItem],
    classifier_backing_task_count: usize,
    model_id: &str,
    session_id: Option<&str>,
    min_confidence: f32,
    include_reasoning: bool,
    turn_elapsed_seconds: Option<u64>,
    client: &dyn ClassifierClient,
) -> LazinessOwned {
    let request = build_classifier_request(
        items,
        classifier_backing_task_count,
        model_id,
        session_id,
        include_reasoning,
        turn_elapsed_seconds,
    );
    let started = std::time::Instant::now();
    let timeout = Duration::from_millis(LAZINESS_CLASSIFIER_TIMEOUT_MS);
    let outcome = tokio::time::timeout(timeout, client.run(request)).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    match outcome {
        Err(_) => LazinessOwned {
            model_id: model_id.to_owned(),
            elapsed_ms,
            parsed: None,
            decision: LazinessDecisionKind::Aborted,
            abort_reason: Some(AbortReasonKind::Timeout),
            error_detail: None,
            raw_output: None,
        },
        Ok(Err(detail)) => LazinessOwned {
            model_id: model_id.to_owned(),
            elapsed_ms,
            parsed: None,
            decision: LazinessDecisionKind::Aborted,
            abort_reason: Some(AbortReasonKind::ClassifierError),
            error_detail: Some(detail),
            raw_output: None,
        },
        Ok(Ok(raw_text)) => match parse_classifier_output(&raw_text) {
            Err(parse_err) => LazinessOwned {
                model_id: model_id.to_owned(),
                elapsed_ms,
                raw_output: Some(raw_text),
                parsed: None,
                decision: LazinessDecisionKind::Aborted,
                abort_reason: Some(AbortReasonKind::ParseError),
                error_detail: Some(parse_err.to_string()),
            },
            Ok(parsed) => {
                let decision = classify_debug_decision(&parsed, min_confidence);
                LazinessOwned {
                    model_id: model_id.to_owned(),
                    elapsed_ms,
                    raw_output: Some(raw_text),
                    parsed: Some(ParsedClassifierOwned {
                        category: parsed.category.as_const_str(),
                        confidence: parsed.confidence,
                        evidence: parsed.evidence,
                    }),
                    decision: decision_kind(decision),
                    abort_reason: None,
                    error_detail: None,
                }
            }
        },
    }
}

fn bump_summary(summary: &mut Summary, line: &TurnLine<'_>) {
    summary.turns += 1;
    match line.todo_gate.decision {
        GateDecisionKind::Continue => summary.gate_continue += 1,
        GateDecisionKind::Nudge => summary.gate_nudge += 1,
    }
    match line.laziness_classifier.decision {
        LazinessDecisionKind::WouldNudge => summary.laz_would_nudge += 1,
        LazinessDecisionKind::NoNudgeNotStalled => summary.laz_not_stalled += 1,
        LazinessDecisionKind::NoNudgeLowConfidence => summary.laz_low_confidence += 1,
        LazinessDecisionKind::Aborted => summary.laz_aborted += 1,
    }
}

// Top-level per-turn pipeline.

/// Per-turn working data. `turn_id` and `request_id` borrow from the
/// source `TurnRecord` so we don't clone identifiers we already
/// own elsewhere; the rest is owned because it's produced fresh
/// (todo snapshot, classifier output) and outlives any single
/// `as_line()` call.
pub struct TurnData<'a> {
    pub turn_id: &'a str,
    pub turn_number: Option<u64>,
    pub request_id: Option<&'a str>,
    pub items_in_history: usize,
    pub items_after_window_trim: usize,
    pub gate_backing_task_count: usize,
    pub classifier_backing_task_count: usize,
    pub pending: Vec<String>,
    pub in_progress_unbacked: Vec<String>,
    pub in_progress_backed: Vec<String>,
    pub todo_gate: TodoGateOwned,
    pub laziness: LazinessOwned,
    /// Resolved `include_reasoning` value the classifier ran with —
    /// CLI override winning over `LAZINESS_INCLUDE_REASONING`. Surfaced
    /// in the per-turn JSONL line so two runs that differ only in this
    /// flag are easy to diff.
    pub include_reasoning: bool,
    /// See [`TurnLine::turn_elapsed_seconds`].
    pub turn_elapsed_seconds: Option<u64>,
}

impl<'a> TurnData<'a> {
    /// Lend a borrowed `TurnLine<'_>` view. Zero clones — every
    /// string-typed field is a borrow into a field on `self`
    /// (or transitively into the source `TurnRecord`). (N2/N15)
    pub fn as_line(&self) -> TurnLine<'_> {
        TurnLine {
            turn_id: self.turn_id,
            turn_number: self.turn_number,
            request_id: self.request_id,
            items_in_history: self.items_in_history,
            items_after_window_trim: self.items_after_window_trim,
            gate_backing_task_count: self.gate_backing_task_count,
            classifier_backing_task_count: self.classifier_backing_task_count,
            todo_state: TodoSnapshotOut {
                pending: self.pending.as_slice(),
                in_progress_unbacked: self.in_progress_unbacked.as_slice(),
                in_progress_backed: self.in_progress_backed.as_slice(),
            },
            todo_gate: TodoGateOut {
                decision: self.todo_gate.decision,
                reason: self.todo_gate.reason,
                reminder: self.todo_gate.reminder.as_deref(),
            },
            laziness_classifier: LazinessOut {
                model_id: self.laziness.model_id.as_str(),
                elapsed_ms: self.laziness.elapsed_ms,
                raw_output: self.laziness.raw_output.as_deref(),
                parsed: self.laziness.parsed.as_ref().map(|p| ParsedClassifierOut {
                    category: p.category,
                    confidence: p.confidence,
                    evidence: p.evidence.as_str(),
                }),
                decision: self.laziness.decision,
                abort_reason: self.laziness.abort_reason,
                error_detail: self.laziness.error_detail.as_deref(),
            },
            include_reasoning: self.include_reasoning,
            turn_elapsed_seconds: self.turn_elapsed_seconds,
        }
    }
}

/// Replay a single turn record against the gate + classifier. Pure
/// over `client`; the binary uses the sampler-backed implementation,
/// tests use a recorded-response double.
///
/// `turn_elapsed_seconds` is the wall-clock duration the *current*
/// turn took, derived by the caller from the delta between this
/// turn's `turn_started_at` and the next turn's. `None` for the last
/// turn (no follow-up) or when the metadata lacks the timestamp.
pub async fn process_turn<'a>(
    record: &'a TurnRecord,
    model_id: &str,
    min_confidence: f32,
    include_reasoning: bool,
    turn_elapsed_seconds: Option<u64>,
    client: &dyn ClassifierClient,
) -> TurnData<'a> {
    let history = record.trace.after_state_history.as_slice();
    let items_in_history = history.len();

    // Audit note (N13): outstanding-dispatch counts are computed over
    // the FULL un-windowed history. Production's
    // `snapshot_backing_task_count_for_debug_log` reads the live
    // `ToolBridge` (current-instant truth, NOT the classifier
    // window). Counting over the windowed slice would silently miss
    // outstanding dispatches older than the window — which is wrong.
    let counts = count_outstanding_dispatches(history);

    let todo_state = reconstruct_todo_state(history);
    let view = partition_todos(&todo_state, counts.terminal_plus_subagents);
    let todo_gate = run_todo_gate(&todo_state, counts.terminal_plus_subagents);

    // Trim to the classifier window without cloning the history —
    // the slice borrows directly out of the record. (F12)
    let window_start = laziness_window_start(
        history,
        LAZINESS_CONTEXT_ITEM_LIMIT,
        LAZINESS_MIN_USER_TURNS,
        LAZINESS_MIN_ASSISTANT_TURNS,
    );
    let trimmed = &history[window_start..];
    let items_after_window_trim = trimmed.len();

    let laziness = classify_turn(
        trimmed,
        counts.terminal_only,
        model_id,
        record.trace.metadata.session_id.as_deref(),
        min_confidence,
        include_reasoning,
        turn_elapsed_seconds,
        client,
    )
    .await;

    TurnData {
        turn_id: record.turn.as_str(),
        turn_number: record.trace.metadata.turn_number,
        request_id: record.trace.metadata.request_id.as_deref(),
        items_in_history,
        items_after_window_trim,
        gate_backing_task_count: counts.terminal_plus_subagents,
        classifier_backing_task_count: counts.terminal_only,
        pending: view.pending.iter().map(|&s| s.to_owned()).collect(),
        in_progress_unbacked: view
            .in_progress_unbacked
            .iter()
            .map(|&s| s.to_owned())
            .collect(),
        in_progress_backed: view
            .in_progress_backed
            .iter()
            .map(|&s| s.to_owned())
            .collect(),
        todo_gate,
        laziness,
        include_reasoning,
        turn_elapsed_seconds,
    }
}

/// Compute per-turn `turn_elapsed_seconds` from the `turn_started_at`
/// timestamps in the trace metadata. For turn N where N < last, the
/// elapsed value is `turn_{N+1}.turn_started_at - turn_N.turn_started_at`
/// — a LOWER bound on the wall-clock duration of turn N (the user
/// may have spent additional time after the agent finished before
/// re-engaging, but at minimum N seconds must have elapsed). The
/// last turn has no follow-up, so its slot is `None`.
///
/// Timestamps are parsed as RFC3339 with timezone offset via
/// `chrono::DateTime<chrono::FixedOffset>` (already a workspace dep,
/// no new dep introduced). A malformed timestamp on either end
/// yields `None` for that pair.
pub fn compute_turn_elapsed_seconds(trace: &[TurnRecord]) -> Vec<Option<u64>> {
    fn parse(ts: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
        chrono::DateTime::parse_from_rfc3339(ts).ok()
    }
    let mut out = Vec::with_capacity(trace.len());
    for (i, rec) in trace.iter().enumerate() {
        let elapsed = if i + 1 == trace.len() {
            None
        } else {
            let cur = rec
                .trace
                .metadata
                .turn_started_at
                .as_deref()
                .and_then(parse);
            let next = trace[i + 1]
                .trace
                .metadata
                .turn_started_at
                .as_deref()
                .and_then(parse);
            match (cur, next) {
                (Some(a), Some(b)) => {
                    let delta = (b - a).num_seconds();
                    u64::try_from(delta).ok()
                }
                _ => None,
            }
        };
        out.push(elapsed);
    }
    out
}

// CLI plumbing

/// Inputs to [`run`]. Constructed by the bin's clap layer.
pub struct RunArgs {
    pub trace: PathBuf,
    pub output: Option<PathBuf>,
    pub model_id: String,
    pub api_base_url: String,
    pub api_key: Option<String>,
    /// Per-model `min_confidence` override (F6). The CLI clap layer
    /// validates this is in `[0.0, 1.0]` before constructing the
    /// struct (N5). Defaults to [`LAZINESS_DEFAULT_MIN_CONFIDENCE`]
    /// via [`Self::min_confidence_value`].
    pub min_confidence: Option<f32>,
    /// CLI override for the `[assistant reasoning]` emission flag.
    /// `None` defers to the harness default
    /// ([`LAZINESS_INCLUDE_REASONING`]). The trace_classify binary
    /// has no per-model config to consult, so this is the *only*
    /// override surface for the offline tool — production resolves
    /// `LazinessDetectorPerModelConfig::include_reasoning` separately.
    pub include_reasoning: Option<bool>,
    /// Grok-home directory containing the `auth.json` to consult as
    /// the third API-key fallback. Defaults to
    /// `xai_grok_shell::util::grok_home::grok_home()` when `None`.
    /// Exposed as a CLI flag for testability.
    pub grok_home: Option<PathBuf>,
}

impl RunArgs {
    /// Effective min_confidence threshold (override or default).
    pub fn min_confidence_value(&self) -> f32 {
        self.min_confidence
            .unwrap_or(LAZINESS_DEFAULT_MIN_CONFIDENCE)
    }

    /// Effective `include_reasoning` value (CLI override or harness
    /// default). Threaded into `process_turn` and surfaced on the
    /// per-turn JSONL line.
    pub fn include_reasoning_value(&self) -> bool {
        self.include_reasoning.unwrap_or(LAZINESS_INCLUDE_REASONING)
    }
}

/// Validate `min_confidence` is finite and in the closed unit
/// interval. Used by clap's `value_parser` (N5).
pub fn validate_min_confidence(raw: &str) -> std::result::Result<f32, String> {
    let v: f32 = raw
        .parse()
        .map_err(|e: std::num::ParseFloatError| e.to_string())?;
    if !v.is_finite() {
        return Err(format!("min_confidence must be finite, got {v}"));
    }
    if !(0.0..=1.0).contains(&v) {
        return Err(format!("min_confidence must be in [0.0, 1.0], got {v}"));
    }
    Ok(v)
}

/// Maximum trace file size accepted by [`parse_trace_file`]. Per-session
/// traces are typically a few MB; this bound protects against accidentally
/// pointing the tool at a 10 GB log dump that would OOM the parser. (N7)
pub const MAX_TRACE_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// Read and parse the full trace from `path`. Loads into memory in
/// one go because `serde_json` does not stream JSON arrays without a
/// custom incremental driver; the [`MAX_TRACE_FILE_BYTES`] check
/// keeps the memory budget predictable. For traces beyond that
/// bound, split the array per-turn upstream and call this once per
/// chunk. (N7)
pub fn parse_trace_file(path: &Path) -> Result<Vec<TurnRecord>> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?;
    if meta.len() > MAX_TRACE_FILE_BYTES {
        return Err(anyhow!(
            "trace file {} is {} bytes; exceeds MAX_TRACE_FILE_BYTES = {}. \
             Split the array per turn upstream if you really want to replay this.",
            path.display(),
            meta.len(),
            MAX_TRACE_FILE_BYTES,
        ));
    }
    // `from_reader` over a `BufReader` keeps the actual I/O bounded
    // by `BufReader`'s default buffer, even though serde still
    // materialises the full `Vec<TurnRecord>` into memory.
    let reader = BufReader::new(file);
    serde_json::from_reader(reader)
        .with_context(|| format!("parse trace {} as Vec<TurnRecord>", path.display()))
}

/// Resolve the API key from `--api-key` → `$XAI_API_KEY` →
/// non-interactive `auth.json` (with silent OIDC refresh) → error.
/// Empty or whitespace-only values at every layer fall through.
///
/// The auth.json branch routes through the same `AuthManager` code
/// path the shell uses (see [`crate::auth::try_ensure_fresh_auth`]):
/// cached non-expired credentials return immediately; an expired
/// OIDC token with a `refresh_token` is silently refreshed and the
/// refreshed credential is re-persisted to disk; the external auth
/// provider command (if `GROK_AUTH_PROVIDER_COMMAND` is set) is
/// tried last. The replay never prompts interactively — an auth.json
/// with a stale OIDC entry and no `refresh_token` surfaces an error
/// the user can act on.
pub async fn resolve_api_key(explicit: Option<&str>, grok_home: &Path) -> Result<String> {
    if let Some(k) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(k.to_owned());
    }
    let env_key = std::env::var("XAI_API_KEY").ok();
    if let Some(k) = env_key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(k.to_owned());
    }
    if let Some(key) = non_interactive_auth_key(grok_home).await? {
        return Ok(key);
    }
    Err(anyhow!(
        "no API key: pass --api-key, set XAI_API_KEY, or run `grok login` to populate \
         <grok-home>/auth.json. An expired OIDC token is auto-refreshed when a refresh_token \
         is present; if not, re-login is required."
    ))
}

/// Build an `AuthManager` for `grok_home`, install the
/// non-interactive refresher chain, and ask for a usable key. Mirrors
/// the body of [`crate::auth::try_ensure_fresh_auth`] but accepts an
/// explicit `grok_home` so `--grok-home` overrides work in test
/// fixtures (the upstream helper hardcodes the global `grok_home()`
/// path).
///
/// Returns:
/// * `Ok(Some(key))` — non-empty key after trim.
/// * `Ok(None)` — auth.json has no entry (`NotLoggedIn`) or the
///   resolved key was empty/whitespace. The caller renders the
///   unified "no API key" error naming all three sources.
/// * `Err(_)` — refresh attempt failed in a way the operator needs to
///   see (network error, refresh_token rejected by the IdP, etc.).
async fn non_interactive_auth_key(grok_home: &Path) -> Result<Option<String>> {
    use crate::auth::{AuthError, AuthManager, GrokComConfig};

    // Production's `try_ensure_fresh_auth` clones the whole config to
    // pass into `AuthManager::new` AND clones `auth_provider_command`
    // again for `configure_refresher`. We extract the single field we
    // need first, then move the rest of `config` into `AuthManager`
    // — one `Option<String>` clone instead of one full struct clone
    // plus one Option clone.
    let config = GrokComConfig::default();
    let auth_provider_command = config.auth_provider_command.clone();
    let manager = std::sync::Arc::new(AuthManager::new(grok_home, config));
    manager.configure_refresher(auth_provider_command, None);
    match manager.auth().await {
        Ok(auth) => {
            let trimmed = auth.key.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_owned()))
            }
        }
        Err(AuthError::NotLoggedIn) => Ok(None),
        Err(e) => Err(anyhow!(
            "auth.json refresh failed: {e}. Run `grok login` to re-authenticate, \
             or pass --api-key / set $XAI_API_KEY to bypass auth.json."
        )),
    }
}

/// Resolve the API key and build the sampler client. Crate-internal:
/// the only caller is [`run`], which has already destructured
/// `RunArgs` so the owned strings can move into `SamplerConfig`
/// instead of being cloned.
async fn build_sampler_client(
    base_url: String,
    model: String,
    api_key: Option<&str>,
    grok_home: Option<&Path>,
) -> Result<xai_grok_sampler::SamplingClient> {
    let default_home;
    let grok_home_path: &Path = match grok_home {
        Some(p) => p,
        None => {
            default_home = crate::util::grok_home::grok_home();
            default_home.as_path()
        }
    };
    let resolved = resolve_api_key(api_key, grok_home_path).await?;
    let config = xai_grok_sampler::SamplerConfig {
        api_key: Some(resolved),
        base_url,
        model,
        max_completion_tokens: Some(LAZINESS_MAX_OUTPUT_TOKENS),
        ..xai_grok_sampler::SamplerConfig::default()
    };
    xai_grok_sampler::SamplingClient::new(config).map_err(|e| anyhow!("build SamplingClient: {e}"))
}

/// End-to-end entry point used by the binary. Writes one JSONL line
/// per turn to `args.output` (or stdout). Per-turn failures are
/// surfaced inside the line — they never abort the whole run.
pub async fn run(args: RunArgs) -> Result<Summary> {
    let RunArgs {
        trace: trace_path,
        output,
        model_id,
        api_base_url,
        api_key,
        min_confidence,
        include_reasoning,
        grok_home,
    } = args;

    // Fail fast on bad paths so the user doesn't wait for sampler
    // setup before discovering a typo. (F31/F32)
    if !trace_path.is_file() {
        return Err(anyhow!(
            "--trace path is not a regular file: {}",
            trace_path.display()
        ));
    }
    if let Some(out) = output.as_ref()
        && let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
        && !parent.is_dir()
    {
        return Err(anyhow!(
            "--output parent directory does not exist: {}",
            parent.display()
        ));
    }

    let trace = parse_trace_file(&trace_path)?;
    // `model_id` is needed twice (sampler config + per-turn request
    // header). The single clone here is the only unavoidable one;
    // `api_base_url` and `api_key` move straight into the sampler.
    let sampling_client = build_sampler_client(
        api_base_url,
        model_id.clone(),
        api_key.as_deref(),
        grok_home.as_deref(),
    )
    .await?;
    let client = SamplerClassifierClient::new(sampling_client);
    let min_confidence = min_confidence.unwrap_or(LAZINESS_DEFAULT_MIN_CONFIDENCE);
    let include_reasoning = include_reasoning.unwrap_or(LAZINESS_INCLUDE_REASONING);
    run_with_client(
        &trace,
        &model_id,
        min_confidence,
        include_reasoning,
        output.as_deref(),
        &client,
    )
    .await
}

/// Construct the line-buffered sink (file or stdout) and delegate to
/// [`run_with_writer`]. Each branch is spelled out separately because
/// `StdoutLock` is `!Send`; the binary uses
/// `#[tokio::main(flavor = "current_thread")]` so this is fine.
pub async fn run_with_client(
    trace: &[TurnRecord],
    model_id: &str,
    min_confidence: f32,
    include_reasoning: bool,
    output: Option<&Path>,
    client: &dyn ClassifierClient,
) -> Result<Summary> {
    match output {
        Some(path) => {
            let file = std::fs::File::create(path)
                .with_context(|| format!("create {}", path.display()))?;
            let mut sink = std::io::LineWriter::new(file);
            run_with_writer(
                trace,
                model_id,
                min_confidence,
                include_reasoning,
                &mut sink,
                client,
            )
            .await
        }
        None => {
            let stdout = std::io::stdout();
            let mut sink = std::io::LineWriter::new(stdout.lock());
            run_with_writer(
                trace,
                model_id,
                min_confidence,
                include_reasoning,
                &mut sink,
                client,
            )
            .await
        }
    }
}

/// Drives the per-turn loop against an arbitrary writer. Factored
/// out so tests can pass a `Vec<u8>` and exercise the same
/// serialization path the stdout/file branches use. (N1)
pub async fn run_with_writer<W: std::io::Write + ?Sized>(
    trace: &[TurnRecord],
    model_id: &str,
    min_confidence: f32,
    include_reasoning: bool,
    sink: &mut W,
    client: &dyn ClassifierClient,
) -> Result<Summary> {
    let mut summary = Summary::default();
    let elapsed_per_turn = compute_turn_elapsed_seconds(trace);
    for (record, &turn_elapsed_seconds) in trace.iter().zip(elapsed_per_turn.iter()) {
        let data = process_turn(
            record,
            model_id,
            min_confidence,
            include_reasoning,
            turn_elapsed_seconds,
            client,
        )
        .await;
        let line = data.as_line();
        serde_json::to_writer(&mut *sink, &line)?;
        sink.write_all(b"\n")?;
        bump_summary(&mut summary, &line);
    }
    sink.flush()?;
    Ok(summary)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_sampling_types::{AssistantItem, ToolCall, ToolResultItem};

    /// Tiny synthetic 4-turn fixture so the end-to-end tests can run
    /// in CI without external trace artifacts. (F16)
    ///
    /// Per-turn `afterStateHistory` is CUMULATIVE — turn_N's history
    /// contains turn_0..N's items plus turn_N's own. This mirrors the
    /// production trace serializer, which snapshots the full
    /// conversation at turn end. (N4)
    const SYNTHETIC_TRACE: &str = r#"[
        {
            "turn": "turn_0",
            "trace": {
                "metadata": {"turn_number": 0, "request_id": "req-0", "session_id": "sess-x"},
                "afterStateHistory": [
                    {"type": "system", "content": "sys"},
                    {"type": "user", "content": [{"type": "text", "text": "do thing"}]},
                    {"type": "assistant", "content": "on it",
                     "tool_calls": [{"id": "t0", "name": "todo_write",
                       "arguments": "{\"merge\":false,\"todos\":[{\"id\":\"a\",\"content\":\"step1\",\"status\":\"in_progress\"}]}"}]},
                    {"type": "tool_result", "tool_call_id": "t0", "content": "ok"}
                ]
            }
        },
        {
            "turn": "turn_1",
            "trace": {
                "metadata": {"turn_number": 1, "request_id": "req-1", "session_id": "sess-x"},
                "afterStateHistory": [
                    {"type": "system", "content": "sys"},
                    {"type": "user", "content": [{"type": "text", "text": "do thing"}]},
                    {"type": "assistant", "content": "on it",
                     "tool_calls": [{"id": "t0", "name": "todo_write",
                       "arguments": "{\"merge\":false,\"todos\":[{\"id\":\"a\",\"content\":\"step1\",\"status\":\"in_progress\"}]}"}]},
                    {"type": "tool_result", "tool_call_id": "t0", "content": "ok"},
                    {"type": "user", "content": [{"type": "text", "text": "go"}]},
                    {"type": "assistant", "content": "starting subagent",
                     "tool_calls": [{"id": "s1", "name": "spawn_subagent", "arguments": "{}"}]}
                ]
            }
        },
        {
            "turn": "turn_2",
            "trace": {
                "metadata": {"turn_number": 2, "request_id": "req-2", "session_id": "sess-x"},
                "afterStateHistory": [
                    {"type": "system", "content": "sys"},
                    {"type": "user", "content": [{"type": "text", "text": "do thing"}]},
                    {"type": "assistant", "content": "on it",
                     "tool_calls": [{"id": "t0", "name": "todo_write",
                       "arguments": "{\"merge\":false,\"todos\":[{\"id\":\"a\",\"content\":\"step1\",\"status\":\"in_progress\"}]}"}]},
                    {"type": "tool_result", "tool_call_id": "t0", "content": "ok"},
                    {"type": "user", "content": [{"type": "text", "text": "go"}]},
                    {"type": "assistant", "content": "starting subagent",
                     "tool_calls": [{"id": "s1", "name": "spawn_subagent", "arguments": "{}"}]},
                    {"type": "tool_result", "tool_call_id": "s1", "content": "subagent done"},
                    {"type": "user", "content": [{"type": "text", "text": "now"}]},
                    {"type": "assistant", "content": "backgrounding monitor",
                     "tool_calls": [{"id": "m1", "name": "monitor", "arguments": "{}"}]}
                ]
            }
        },
        {
            "turn": "turn_3",
            "trace": {
                "metadata": {"turn_number": 3, "request_id": "req-3", "session_id": "sess-x"},
                "afterStateHistory": [
                    {"type": "system", "content": "sys"},
                    {"type": "user", "content": [{"type": "text", "text": "do thing"}]},
                    {"type": "assistant", "content": "on it",
                     "tool_calls": [{"id": "t0", "name": "todo_write",
                       "arguments": "{\"merge\":false,\"todos\":[{\"id\":\"a\",\"content\":\"step1\",\"status\":\"in_progress\"}]}"}]},
                    {"type": "tool_result", "tool_call_id": "t0", "content": "ok"},
                    {"type": "user", "content": [{"type": "text", "text": "go"}]},
                    {"type": "assistant", "content": "starting subagent",
                     "tool_calls": [{"id": "s1", "name": "spawn_subagent", "arguments": "{}"}]},
                    {"type": "tool_result", "tool_call_id": "s1", "content": "subagent done"},
                    {"type": "user", "content": [{"type": "text", "text": "now"}]},
                    {"type": "assistant", "content": "backgrounding monitor",
                     "tool_calls": [{"id": "m1", "name": "monitor", "arguments": "{}"}]},
                    {"type": "tool_result", "tool_call_id": "m1", "content": "monitor stopped"},
                    {"type": "user", "content": [{"type": "text", "text": "wrap"}]},
                    {"type": "assistant", "content": "marking done",
                     "tool_calls": [{"id": "t1", "name": "todo_write",
                       "arguments": "{\"merge\":true,\"todos\":[{\"id\":\"a\",\"status\":\"completed\"}]}"}]}
                ]
            }
        }
    ]"#;

    fn parse_synthetic() -> Vec<TurnRecord> {
        serde_json::from_str(SYNTHETIC_TRACE).expect("synthetic trace parses")
    }

    fn assistant_with_tool_calls(tool_calls: Vec<ToolCall>) -> ConversationItem {
        ConversationItem::Assistant(AssistantItem {
            content: String::new().into(),
            tool_calls,
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        })
    }

    fn tc(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.to_owned(),
            arguments: args.into(),
        }
    }

    /// 1. Synthetic trace deserialize round-trip.
    #[test]
    fn synthetic_trace_round_trips() {
        let trace = parse_synthetic();
        assert_eq!(trace.len(), 4);
        for (i, r) in trace.iter().enumerate() {
            assert_eq!(r.turn, format!("turn_{i}"));
            assert_eq!(r.trace.metadata.turn_number, Some(i as u64));
        }
        // N4: per-turn histories are cumulative (monotonically growing).
        let lens: Vec<usize> = trace
            .iter()
            .map(|r| r.trace.after_state_history.len())
            .collect();
        assert!(
            lens.windows(2).all(|w| w[0] < w[1]),
            "history is monotone-growing: {lens:?}",
        );
    }

    #[test]
    fn reference_trace_round_trips_when_available() {
        let path =
            Path::new("/root/traces/trace-019e4888-ded4-7632-9bb5-a7964974d34e-all-turns.json");
        if !path.exists() {
            eprintln!(
                "SKIP reference_trace_round_trips_when_available: {} not present",
                path.display()
            );
            return;
        }
        let trace = parse_trace_file(path).expect("parse");
        assert_eq!(trace.len(), 4);
        assert!(
            trace[2]
                .trace
                .metadata
                .request_id
                .as_deref()
                .unwrap_or("")
                .starts_with("subagent"),
            "turn_2 request_id should start with `subagent`"
        );
    }

    /// 2. TodoState reconstruction.
    #[test]
    fn reconstructs_todo_state_replace_then_merge() {
        let items = vec![
            assistant_with_tool_calls(vec![tc(
                "c1",
                "todo_write",
                r#"{"merge":false,"todos":[
                    {"id":"a","content":"do A","status":"pending"},
                    {"id":"b","content":"do B","status":"in_progress"}
                ]}"#,
            )]),
            assistant_with_tool_calls(vec![tc(
                "c2",
                "todo_write",
                r#"{"merge":true,"todos":[
                    {"id":"b","status":"completed"},
                    {"id":"c","content":"do C","status":"pending"}
                ]}"#,
            )]),
        ];
        let state = reconstruct_todo_state(&items);
        let by_id: std::collections::HashMap<_, _> = state
            .todo_items_with_ids()
            .map(|(id, it)| (id.clone(), it.clone()))
            .collect();
        assert_eq!(by_id.len(), 3);
        assert_eq!(by_id["a"].status, TodoStatus::Pending);
        assert_eq!(by_id["b"].status, TodoStatus::Completed);
        // F23: assert content is preserved across content-less merge.
        assert_eq!(by_id["b"].content, "do B");
        assert_eq!(by_id["c"].status, TodoStatus::Pending);
        assert_eq!(by_id["c"].content, "do C");
    }

    /// N4 follow-up: turn_3 of the cumulative synthetic fixture should
    /// show `a:completed` (seeded in turn_0, merged in turn_3).
    #[test]
    fn synthetic_turn_3_completes_seed_from_turn_0() {
        let trace = parse_synthetic();
        let state = reconstruct_todo_state(&trace[3].trace.after_state_history);
        let by_id: std::collections::HashMap<_, _> = state
            .todo_items_with_ids()
            .map(|(id, it)| (id.clone(), it.clone()))
            .collect();
        assert_eq!(by_id["a"].status, TodoStatus::Completed);
        // Content preserved across the content-less merge.
        assert_eq!(by_id["a"].content, "step1");
    }

    #[test]
    fn skips_malformed_todo_write_args() {
        let items = vec![assistant_with_tool_calls(vec![tc(
            "c1",
            "todo_write",
            "not json",
        )])];
        let state = reconstruct_todo_state(&items);
        assert!(state.is_empty());
    }

    /// F2/F24: duplicate ids in one call → whole call is skipped.
    #[test]
    fn skips_todo_write_with_duplicate_ids() {
        let items = vec![
            assistant_with_tool_calls(vec![tc(
                "c1",
                "todo_write",
                r#"{"merge":false,"todos":[{"id":"a","content":"first","status":"pending"}]}"#,
            )]),
            assistant_with_tool_calls(vec![tc(
                "c2",
                "todo_write",
                r#"{"merge":false,"todos":[
                    {"id":"x","content":"X","status":"pending"},
                    {"id":"x","content":"X2","status":"in_progress"}
                ]}"#,
            )]),
        ];
        let state = reconstruct_todo_state(&items);
        let ids: Vec<_> = state
            .todo_items_with_ids()
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(ids, vec!["a".to_owned()]);
    }

    /// 3. TodoGateInput partition.
    #[test]
    fn partition_respects_backing_task_count() {
        let mut state = TodoState::default();
        for (id, content, status) in [
            ("p1", "pending one", TodoStatus::Pending),
            ("i1", "in flight one", TodoStatus::InProgress),
            ("i2", "in flight two", TodoStatus::InProgress),
            ("i3", "in flight three", TodoStatus::InProgress),
            ("c1", "done one", TodoStatus::Completed),
        ] {
            state.push(
                id.to_owned(),
                TodoItem {
                    content: content.to_owned(),
                    priority: TodoPriority::default(),
                    status,
                    meta: None,
                },
            );
        }
        let view = partition_todos(&state, 1);
        assert_eq!(view.pending, vec!["pending one"]);
        assert_eq!(view.in_progress_backed, vec!["in flight one"]);
        assert_eq!(
            view.in_progress_unbacked,
            vec!["in flight two", "in flight three"]
        );
    }

    #[test]
    fn partition_with_more_backing_than_in_progress_clamps() {
        let mut state = TodoState::default();
        state.push(
            "i1".into(),
            TodoItem {
                content: "only".into(),
                priority: TodoPriority::default(),
                status: TodoStatus::InProgress,
                meta: None,
            },
        );
        let view = partition_todos(&state, 5);
        assert_eq!(view.in_progress_backed, vec!["only"]);
        assert!(view.in_progress_unbacked.is_empty());
    }

    /// F1: gate count includes subagents; classifier count does not.
    #[test]
    fn outstanding_dispatches_split_terminal_vs_subagents() {
        let items = vec![
            assistant_with_tool_calls(vec![
                tc("s1", "spawn_subagent", "{}"),
                tc("b1", "run_terminal_command", r#"{"background":true}"#),
                tc("b2", "run_terminal_command", r#"{"is_background":true}"#),
                tc("b3", "run_terminal_command", r#"{"background":false}"#),
                tc("m1", "monitor", "{}"),
                tc("s2", "spawn_subagent", "{}"),
            ]),
            ConversationItem::ToolResult(ToolResultItem {
                tool_call_id: "s1".into(),
                content: "done".into(),
                images: vec![],
            }),
            ConversationItem::ToolResult(ToolResultItem {
                tool_call_id: "b1".into(),
                content: "done".into(),
                images: vec![],
            }),
        ];
        let counts = count_outstanding_dispatches(&items);
        assert_eq!(counts.terminal_only, 2, "terminal: b2 + m1");
        assert_eq!(
            counts.terminal_plus_subagents, 3,
            "terminal + subagent (s2)"
        );
    }

    /// F3: orphan tool_result (result before call) does NOT satisfy
    /// the later dispatch.
    #[test]
    fn orphan_tool_result_before_call_does_not_satisfy() {
        let items = vec![
            ConversationItem::ToolResult(ToolResultItem {
                tool_call_id: "later".into(),
                content: "preemptive".into(),
                images: vec![],
            }),
            assistant_with_tool_calls(vec![tc("later", "spawn_subagent", "{}")]),
        ];
        let counts = count_outstanding_dispatches(&items);
        assert_eq!(counts.terminal_plus_subagents, 1);
    }

    /// 4. End-to-end with stubbed classifier.
    struct StubClient(String);

    #[async_trait::async_trait]
    impl ClassifierClient for StubClient {
        async fn run(&self, _request: ConversationRequest) -> Result<String, String> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn end_to_end_synthetic_with_stub() {
        let trace = parse_synthetic();
        let stub = StubClient(
            r#"{"category":"not_stalled_complete","confidence":0.9,"evidence":"stub"}"#.to_owned(),
        );
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let out = tmpdir.path().join("out.jsonl");
        let summary = run_with_client(
            &trace,
            "stub-model",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            LAZINESS_INCLUDE_REASONING,
            Some(&out),
            &stub,
        )
        .await
        .expect("run");
        assert_eq!(summary.turns, 4);
        assert_eq!(summary.laz_not_stalled, 4);
        assert_eq!(summary.laz_aborted, 0);

        let body = std::fs::read_to_string(&out).expect("read out");
        let mut lines = 0;
        for line in body.lines() {
            let v: serde_json::Value = serde_json::from_str(line).expect("line is json");
            assert!(v.get("turn_id").is_some());
            assert!(v.get("todo_gate").is_some());
            assert!(v.get("laziness_classifier").is_some());
            assert!(v.get("gate_backing_task_count").is_some());
            assert!(v.get("classifier_backing_task_count").is_some());
            // The resolved `include_reasoning` is surfaced on every
            // per-turn record so two A/B runs can be diffed line by
            // line.
            assert_eq!(
                v.get("include_reasoning"),
                Some(&serde_json::Value::Bool(LAZINESS_INCLUDE_REASONING))
            );
            lines += 1;
        }
        assert_eq!(lines, 4);
    }

    /// N9: every non-Aborted laziness decision discriminator round-trips
    /// correctly through the JSONL.
    #[tokio::test]
    async fn end_to_end_per_category_jsonl_round_trip() {
        let trace = parse_synthetic();
        let stubs = [
            (
                LazinessDecisionKind::NoNudgeNotStalled,
                "not_stalled_complete",
                0.9_f32,
                "no_nudge_not_stalled",
            ),
            (
                LazinessDecisionKind::WouldNudge,
                "stalled_narration",
                0.9,
                "would_nudge",
            ),
            (
                LazinessDecisionKind::NoNudgeLowConfidence,
                "stalled_narration",
                0.4,
                "no_nudge_low_confidence",
            ),
        ];
        for (expected_kind, category, confidence, wire_str) in stubs {
            let stub = StubClient(format!(
                r#"{{"category":"{category}","confidence":{confidence},"evidence":"e"}}"#
            ));
            let mut buf = Vec::<u8>::new();
            run_with_writer(
                &trace,
                "stub",
                LAZINESS_DEFAULT_MIN_CONFIDENCE,
                LAZINESS_INCLUDE_REASONING,
                &mut buf,
                &stub,
            )
            .await
            .expect("run");
            for line in std::str::from_utf8(&buf).expect("utf8").lines() {
                let v: serde_json::Value = serde_json::from_str(line).expect("json");
                let dec = v
                    .pointer("/laziness_classifier/decision")
                    .and_then(|d| d.as_str())
                    .expect("decision");
                assert_eq!(dec, wire_str, "decision wire-string for {expected_kind:?}");
            }
        }
    }

    /// F1 (in-pipeline): on the cumulative synthetic, turn_2 has a
    /// `monitor` outstanding (m1) but turn_1's `spawn_subagent` (s1) is
    /// already resolved by a `tool_result` — so the classifier count
    /// (terminal_only) sees `m1` only, while the gate count
    /// (terminal_plus_subagents) sees the same `m1` plus no
    /// outstanding subagents → equal. The discriminating turn is
    /// turn_1, where s1 is outstanding for the gate but invisible to
    /// the classifier.
    #[tokio::test]
    async fn synthetic_turn_1_has_asymmetric_counts() {
        let trace = parse_synthetic();
        let stub = StubClient(
            r#"{"category":"not_stalled_waiting_on_background","confidence":0.9,"evidence":"stub"}"#
                .to_owned(),
        );
        let data = process_turn(
            &trace[1],
            "stub",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            LAZINESS_INCLUDE_REASONING,
            None,
            &stub,
        )
        .await;
        assert_eq!(
            data.gate_backing_task_count, 1,
            "gate sees the spawn_subagent"
        );
        assert_eq!(
            data.classifier_backing_task_count, 0,
            "classifier excludes subagents"
        );
    }

    /// F22: parse-error line carries the right discriminator + detail.
    #[tokio::test]
    async fn parse_error_is_surfaced_per_turn() {
        let trace = parse_synthetic();
        let stub = StubClient("not json at all".into());
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let out = tmpdir.path().join("out.jsonl");
        let summary = run_with_client(
            &trace,
            "stub-model",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            LAZINESS_INCLUDE_REASONING,
            Some(&out),
            &stub,
        )
        .await
        .expect("run");
        assert_eq!(summary.turns, 4);
        assert_eq!(summary.laz_aborted, 4);
        let body = std::fs::read_to_string(&out).expect("read out");
        for line in body.lines() {
            let v: serde_json::Value = serde_json::from_str(line).expect("line is json");
            let laz = v.get("laziness_classifier").expect("laziness_classifier");
            assert_eq!(
                laz.get("abort_reason").and_then(|x| x.as_str()),
                Some("parse_error")
            );
            assert!(
                laz.get("error_detail")
                    .and_then(|x| x.as_str())
                    .is_some_and(|s| !s.is_empty()),
                "error_detail populated",
            );
            assert_eq!(
                laz.get("decision").and_then(|x| x.as_str()),
                Some("aborted")
            );
            assert!(laz.get("raw_output").and_then(|x| x.as_str()).is_some());
        }
    }

    // F17 — fidelity test. Capture site for the most-recent
    // `ConversationRequest`. `RefCell` would be the natural pick for
    // a single-threaded test, but the `ClassifierClient` trait is
    // `Send + Sync` (so production `SamplingClient` callers can hold
    // it via `Arc<dyn ClassifierClient>` across threads) and
    // `RefCell: !Sync`. `std::sync::Mutex<T>` is the minimal Sync
    // interior-mutability primitive — the lock is always uncontended
    // here (single-threaded `#[tokio::test]`) so the cost is one
    // atomic CAS per call.
    struct CapturingStub {
        last: std::sync::Mutex<Option<ConversationRequest>>,
        response: String,
    }

    impl CapturingStub {
        fn new(response: &str) -> Self {
            Self {
                last: std::sync::Mutex::new(None),
                response: response.to_owned(),
            }
        }
        fn take(&self) -> ConversationRequest {
            self.last
                .lock()
                .expect("mutex poisoned")
                .take()
                .expect("request captured")
        }
    }

    #[async_trait::async_trait]
    impl ClassifierClient for CapturingStub {
        async fn run(&self, request: ConversationRequest) -> Result<String, String> {
            *self.last.lock().expect("mutex poisoned") = Some(request);
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn classifier_request_matches_production_shape() {
        let trace = parse_synthetic();
        let stub = CapturingStub::new(
            r#"{"category":"not_stalled_complete","confidence":0.9,"evidence":"e"}"#,
        );
        let _ = process_turn(
            &trace[0],
            "fidelity-model",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            LAZINESS_INCLUDE_REASONING,
            None,
            &stub,
        )
        .await;
        let req = stub.take();

        // Exactly two items: System(classifier prompt) + User(transcript).
        assert_eq!(req.items.len(), 2);
        match &req.items[0] {
            // N3: assert against the shared production constant, not a
            // re-typed literal — drift on either side fails the test.
            ConversationItem::System(s) => {
                assert_eq!(s.content.as_ref(), LAZINESS_CLASSIFIER_PROMPT)
            }
            other => panic!("expected System, got {other:?}"),
        }
        let user_text = match &req.items[1] {
            ConversationItem::User(u) => {
                assert!(u.synthetic_reason.is_none());
                assert_eq!(u.content.len(), 1);
                match &u.content[0] {
                    ContentPart::Text { text } => text.clone(),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected User, got {other:?}"),
        };
        // N3: assert against the shared `LAZINESS_USER_PREAMBLE`.
        assert!(
            user_text.starts_with(LAZINESS_USER_PREAMBLE),
            "user text starts with the shared preamble const",
        );
        assert!(
            user_text.contains("=== BEGIN TRANSCRIPT ===\n[runtime_state] outstanding_background_tasks_and_subagents="),
            "user text contains runtime_state line right after BEGIN sentinel",
        );
        assert!(user_text.ends_with("=== END TRANSCRIPT ===\n"));

        assert_eq!(req.model.as_deref(), Some("fidelity-model"));
        assert_eq!(req.temperature, Some(0.0));
        assert_eq!(req.max_output_tokens, Some(LAZINESS_MAX_OUTPUT_TOKENS));
        assert!(req.reasoning_effort.is_none());
        assert!(req.tools.is_empty());
        assert!(req.hosted_tools.is_empty());
        assert!(req.tool_choice.is_none());

        let conv_id = req.x_grok_conv_id.as_deref().expect("conv id");
        let conv_suffix = conv_id
            .strip_prefix("trace-classifier-")
            .expect("conv id starts with trace-classifier-");
        assert_eq!(
            uuid::Uuid::parse_str(conv_suffix)
                .expect("conv suffix parses as UUID")
                .get_version_num(),
            4,
        );
        assert_eq!(req.x_grok_session_id.as_deref(), Some("sess-x"));
        let req_id = req.x_grok_req_id.as_deref().expect("req id");
        // N3: shared const for the prefix.
        let suffix = req_id
            .strip_prefix(LAZINESS_REQ_ID_PREFIX)
            .expect("req_id starts with shared prefix const");
        // N8: actually parse the suffix as a UUIDv4 — length-only was
        // a weak proxy.
        let parsed = uuid::Uuid::parse_str(suffix).expect("suffix parses as UUID");
        assert_eq!(parsed.get_version_num(), 4, "UUIDv4");
        assert!(!parsed.is_nil());

        assert_eq!(
            req.x_grok_agent_id.as_deref(),
            Some(xai_grok_telemetry::id::agent_id().as_str())
        );
    }

    /// N11: the captured transcript ends with the most recent item's
    /// marker text — proves `laziness_window_start` does not drop the
    /// tail.
    #[tokio::test]
    async fn captured_transcript_ends_with_most_recent_item() {
        // Build a 50-item history with a unique marker on the LAST
        // user item.
        let mut hist: Vec<ConversationItem> = Vec::new();
        hist.push(ConversationItem::System(SystemItem {
            content: "sys".into(),
        }));
        for i in 0..24 {
            hist.push(ConversationItem::User(UserItem {
                content: vec![ContentPart::Text {
                    text: format!("u{i}").into(),
                }],
                synthetic_reason: None,
                ..Default::default()
            }));
            hist.push(ConversationItem::Assistant(AssistantItem {
                content: format!("a{i}").into(),
                tool_calls: vec![],
                model_id: None,
                model_fingerprint: None,
                reasoning_effort: None,
            }));
        }
        hist.push(ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: "ulast-MARKER-tail".into(),
            }],
            synthetic_reason: None,
            ..Default::default()
        }));

        let record = TurnRecord {
            turn: "turn_synth".into(),
            trace: TurnTrace {
                metadata: TurnMetadata {
                    turn_number: Some(0),
                    request_id: Some("r".into()),
                    session_id: Some("s".into()),
                    turn_started_at: None,
                },
                after_state_history: hist,
            },
        };

        let stub = CapturingStub::new(
            r#"{"category":"not_stalled_complete","confidence":0.9,"evidence":"e"}"#,
        );
        let _ = process_turn(
            &record,
            "m",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            LAZINESS_INCLUDE_REASONING,
            None,
            &stub,
        )
        .await;
        let req = stub.take();
        let user_text = match &req.items[1] {
            ConversationItem::User(u) => match &u.content[0] {
                ContentPart::Text { text } => text.clone(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        assert!(
            user_text.contains("ulast-MARKER-tail"),
            "transcript retained the most-recent user item: {user_text}",
        );
    }

    /// F17/sub: per-model `min_confidence` override flows through.
    #[tokio::test]
    async fn min_confidence_override_is_threaded_through() {
        let trace = parse_synthetic();
        let stub = StubClient(
            r#"{"category":"stalled_narration","confidence":0.65,"evidence":"e"}"#.to_owned(),
        );
        let data_default = process_turn(
            &trace[0],
            "m",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            LAZINESS_INCLUDE_REASONING,
            None,
            &stub,
        )
        .await;
        assert_eq!(
            data_default.laziness.decision,
            LazinessDecisionKind::NoNudgeLowConfidence,
        );
        let data_low =
            process_turn(&trace[0], "m", 0.5, LAZINESS_INCLUDE_REASONING, None, &stub).await;
        assert_eq!(data_low.laziness.decision, LazinessDecisionKind::WouldNudge);
    }

    /// The `include_reasoning` flag is threaded all the way through
    /// `process_turn` and surfaced on the per-turn JSONL line — so two
    /// A/B runs can be byte-diffed and the difference attributed to
    /// the flag.
    #[tokio::test]
    async fn include_reasoning_override_is_threaded_through_and_surfaced_on_jsonl() {
        let trace = parse_synthetic();
        let stub = StubClient(
            r#"{"category":"not_stalled_complete","confidence":0.9,"evidence":"e"}"#.to_owned(),
        );

        let data_on = process_turn(
            &trace[0],
            "m",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            true,
            None,
            &stub,
        )
        .await;
        assert!(data_on.include_reasoning);
        let line_on = serde_json::to_value(data_on.as_line()).expect("serialize on");
        assert_eq!(
            line_on.get("include_reasoning"),
            Some(&serde_json::json!(true))
        );

        let data_off = process_turn(
            &trace[0],
            "m",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            false,
            None,
            &stub,
        )
        .await;
        assert!(!data_off.include_reasoning);
        let line_off = serde_json::to_value(data_off.as_line()).expect("serialize off");
        assert_eq!(
            line_off.get("include_reasoning"),
            Some(&serde_json::json!(false))
        );
    }

    /// `RunArgs::include_reasoning_value` resolves the CLI flag to
    /// the harness default when absent.
    #[test]
    fn run_args_include_reasoning_value_resolves_to_harness_default_when_absent() {
        let args = RunArgs {
            trace: PathBuf::from("ignored"),
            output: None,
            model_id: "m".into(),
            api_base_url: "https://x".into(),
            api_key: None,
            min_confidence: None,
            include_reasoning: None,
            grok_home: None,
        };
        assert_eq!(args.include_reasoning_value(), LAZINESS_INCLUDE_REASONING);
    }

    #[test]
    fn run_args_include_reasoning_value_honors_cli_override() {
        let args_off = RunArgs {
            trace: PathBuf::from("ignored"),
            output: None,
            model_id: "m".into(),
            api_base_url: "https://x".into(),
            api_key: None,
            min_confidence: None,
            include_reasoning: Some(false),
            grok_home: None,
        };
        assert!(!args_off.include_reasoning_value());

        let args_on = RunArgs {
            trace: PathBuf::from("ignored"),
            output: None,
            model_id: "m".into(),
            api_base_url: "https://x".into(),
            api_key: None,
            min_confidence: None,
            include_reasoning: Some(true),
            grok_home: None,
        };
        assert!(args_on.include_reasoning_value());
    }

    /// N1: stdout-vs-file branches actually exercise the same writer
    /// code path. Both go through `run_with_writer`; the file branch
    /// hands it a `LineWriter<File>`, the test hands it a `Vec<u8>`.
    /// Byte-equal after normalising `elapsed_ms`.
    #[tokio::test]
    async fn stdout_and_file_branches_byte_equal() {
        let trace = parse_synthetic();
        let stub = StubClient(
            r#"{"category":"not_stalled_complete","confidence":0.9,"evidence":"stub"}"#.to_owned(),
        );

        let tmpdir = tempfile::tempdir().expect("tempdir");
        let path = tmpdir.path().join("a.jsonl");
        run_with_client(
            &trace,
            "stub",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            LAZINESS_INCLUDE_REASONING,
            Some(&path),
            &stub,
        )
        .await
        .expect("file");
        let file_body = std::fs::read_to_string(&path).expect("read");

        // Exercise the SAME loop body via `run_with_writer` with a
        // Vec<u8> sink — the same path the stdout branch takes.
        let mut buf = Vec::<u8>::new();
        run_with_writer(
            &trace,
            "stub",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            LAZINESS_INCLUDE_REASONING,
            &mut buf,
            &stub,
        )
        .await
        .expect("buf");
        let buf_body = String::from_utf8(buf).expect("utf8");

        fn strip_elapsed(s: &str) -> String {
            let re = regex::Regex::new(r#""elapsed_ms":\d+"#).expect("regex");
            re.replace_all(s, r#""elapsed_ms":0"#).into_owned()
        }
        assert_eq!(strip_elapsed(&file_body), strip_elapsed(&buf_body));
    }

    /// Format an RFC3339 timestamp with `seconds` added to "now".
    /// Used so test fixtures aren't sensitive to the absolute wall-clock.
    fn now_offset(seconds: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::seconds(seconds)).to_rfc3339()
    }

    /// Write an `auth.json` whose only entry is at the production
    /// OIDC scope (the same scope `grok login` writes today and
    /// `AuthManager` reads). `auth_mode: api_key` skips the refresh
    /// path entirely — useful for "plain key, no refresh wanted"
    /// fixtures.
    fn write_auth_json(grok_home: &Path, key: &str) {
        let scope = crate::auth::GrokComConfig::default().auth_scope();
        let body = serde_json::json!({
            scope: {
                "key": key,
                "auth_mode": "api_key",
                "create_time": now_offset(0),
                "user_id": "test-user",
            }
        });
        std::fs::write(grok_home.join("auth.json"), body.to_string()).expect("write auth.json");
    }

    /// `auth.json` with no scope entries — equivalent to "user never
    /// logged in". `AuthManager::auth()` returns `NotLoggedIn` →
    /// `non_interactive_auth_key` returns `Ok(None)` →
    /// `resolve_api_key` falls through to the unified error.
    fn write_empty_auth_json(grok_home: &Path) {
        std::fs::write(grok_home.join("auth.json"), "{}").expect("write auth.json");
    }

    /// Write a non-expired OIDC entry (`expires_at` 1 hour in the
    /// future, `refresh_token` populated). `AuthManager::auth()`
    /// returns it via the fast path; the refresher chain is NOT
    /// invoked, so no network call fires.
    fn write_fresh_oidc_auth_json(grok_home: &Path, key: &str) {
        let scope = crate::auth::GrokComConfig::default().auth_scope();
        let body = serde_json::json!({
            scope: {
                "key": key,
                "auth_mode": "oidc",
                "create_time": now_offset(0),
                "expires_at": now_offset(3600),
                "refresh_token": "test-refresh-token",
                "oidc_issuer": "https://example.test",
                "oidc_client_id": "test-client",
                "user_id": "test-user",
            }
        });
        std::fs::write(grok_home.join("auth.json"), body.to_string()).expect("write auth.json");
    }

    /// Write an expired OIDC entry with NO `refresh_token`. The
    /// refresh chain has nothing to refresh against, so the auth
    /// call fails non-interactively.
    fn write_expired_oidc_auth_json_no_refresh(grok_home: &Path, key: &str) {
        let scope = crate::auth::GrokComConfig::default().auth_scope();
        let body = serde_json::json!({
            scope: {
                "key": key,
                "auth_mode": "oidc",
                "create_time": now_offset(-7200),
                "expires_at": now_offset(-3600),
                "user_id": "test-user",
            }
        });
        std::fs::write(grok_home.join("auth.json"), body.to_string()).expect("write auth.json");
    }

    /// F20: `resolve_api_key` precedence (flag > env > error). Now
    /// also covers whitespace-only flag (N6). Auth.json fallback is
    /// covered separately so this test can keep working with an
    /// empty scratch `grok_home`.
    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_api_key_precedence() {
        if skip_if_devbox("resolve_api_key_precedence") {
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let grok_home = tmp.path();
        // No auth.json at all → AuthManager sees nothing on disk.
        write_empty_auth_json(grok_home);

        // Tear down + restore env inline (the async resolver can't
        // be called from inside a sync closure).
        let saved: Vec<(&str, Option<String>)> = ISOLATED_ENV_KEYS
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        for (k, _) in &saved {
            unsafe { std::env::remove_var(k) };
        }

        assert!(
            resolve_api_key(None, grok_home).await.is_err(),
            "no flag, no env, no auth.json → error",
        );
        assert!(
            resolve_api_key(Some(""), grok_home).await.is_err(),
            "empty flag → error",
        );
        assert!(
            resolve_api_key(Some("   "), grok_home).await.is_err(),
            "whitespace-only flag → error",
        );
        assert_eq!(
            resolve_api_key(Some("from-flag"), grok_home)
                .await
                .expect("ok"),
            "from-flag",
        );

        unsafe { std::env::set_var("XAI_API_KEY", "from-env") };
        assert_eq!(
            resolve_api_key(None, grok_home).await.expect("ok"),
            "from-env"
        );
        assert_eq!(
            resolve_api_key(Some("from-flag"), grok_home)
                .await
                .expect("ok"),
            "from-flag",
            "CLI flag overrides env",
        );
        assert_eq!(
            resolve_api_key(Some(""), grok_home).await.expect("ok"),
            "from-env",
            "empty flag falls through to env",
        );
        assert_eq!(
            resolve_api_key(Some("   "), grok_home).await.expect("ok"),
            "from-env",
            "whitespace-only flag falls through to env",
        );

        unsafe { std::env::set_var("XAI_API_KEY", "   ") };
        assert!(
            resolve_api_key(None, grok_home).await.is_err(),
            "whitespace-only env is treated as absent",
        );

        // Restore env.
        for (k, v) in saved {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }

    /// Auth.json with an `auth_mode: api_key` entry under the
    /// production OIDC scope: `AuthManager::auth()` returns it via
    /// the fast path, no refresh fires, no network needed.
    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_api_key_falls_back_to_auth_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let grok_home = tmp.path();
        write_auth_json(grok_home, "from-auth-json");
        // `with_no_env` returns sync; we re-implement the clearing
        // inline so we can `.await` the resolver.
        let key =
            with_env_isolated(async { resolve_api_key(None, grok_home).await.expect("ok") }).await;
        assert_eq!(key, "from-auth-json", "auth.json is the third source");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_api_key_skips_auth_json_when_flag_provided() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let grok_home = tmp.path();
        write_auth_json(grok_home, "from-auth-json");
        let key = with_env_isolated(async {
            resolve_api_key(Some("from-flag"), grok_home)
                .await
                .expect("ok")
        })
        .await;
        assert_eq!(key, "from-flag", "flag wins over auth.json");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_api_key_skips_auth_json_when_env_provided() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let grok_home = tmp.path();
        write_auth_json(grok_home, "from-auth-json");
        let key = with_env_isolated(async {
            unsafe { std::env::set_var("XAI_API_KEY", "from-env") };
            let resolved = resolve_api_key(None, grok_home).await.expect("ok");
            unsafe { std::env::remove_var("XAI_API_KEY") };
            resolved
        })
        .await;
        assert_eq!(key, "from-env", "env wins over auth.json");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_api_key_errors_when_all_three_sources_missing() {
        if skip_if_devbox("resolve_api_key_errors_when_all_three_sources_missing") {
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let grok_home = tmp.path();
        write_empty_auth_json(grok_home);
        let msg = with_env_isolated(async {
            let err = resolve_api_key(None, grok_home)
                .await
                .expect_err("missing everywhere");
            format!("{err:#}")
        })
        .await;
        assert!(
            msg.contains("--api-key")
                && msg.contains("XAI_API_KEY")
                && msg.contains("grok login")
                && msg.contains("auth.json"),
            "error names all three sources: {msg}",
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_api_key_skips_empty_auth_json_entry() {
        if skip_if_devbox("resolve_api_key_skips_empty_auth_json_entry") {
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let grok_home = tmp.path();
        // Entry exists with `.key == ""` → fast-path tries to return
        // it, our caller trims it down to empty → falls through to
        // the unified error.
        write_auth_json(grok_home, "");
        let err1 = with_env_isolated(async { resolve_api_key(None, grok_home).await }).await;
        assert!(err1.is_err(), "empty auth.json entry is treated as absent");

        write_auth_json(grok_home, "   ");
        let err2 = with_env_isolated(async { resolve_api_key(None, grok_home).await }).await;
        assert!(
            err2.is_err(),
            "whitespace-only auth.json entry is treated as absent",
        );
    }

    /// A non-expired OIDC entry returns via the AuthManager fast
    /// path — refresh chain not invoked, no network call. Pins the
    /// wiring between `non_interactive_auth_key` and
    /// `AuthManager::auth()` for the common steady-state case.
    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_api_key_returns_refreshable_oidc_token() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let grok_home = tmp.path();
        write_fresh_oidc_auth_json(grok_home, "fresh-oidc-key");
        let key =
            with_env_isolated(async { resolve_api_key(None, grok_home).await.expect("ok") }).await;
        assert_eq!(key, "fresh-oidc-key");
    }

    /// An OIDC entry with `expires_at` in the past AND no
    /// `refresh_token` → `AuthManager::auth()` cannot refresh and
    /// has no interactive flow to fall back to → returns an error.
    /// Pins the "auth.json present but no usable token" branch.
    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_api_key_returns_error_when_oidc_token_expired_and_no_refresh_token() {
        if skip_if_devbox(
            "resolve_api_key_returns_error_when_oidc_token_expired_and_no_refresh_token",
        ) {
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let grok_home = tmp.path();
        write_expired_oidc_auth_json_no_refresh(grok_home, "stale-oidc-key");
        let err = with_env_isolated(async { resolve_api_key(None, grok_home).await }).await;
        assert!(
            err.is_err(),
            "expired OIDC + no refresh_token → error (no usable token)",
        );
    }

    /// Mirrors `crate::auth::devbox_login::SA_TOKEN_PATH`. Tests that
    /// expect the no-credentials branches of `AuthManager::auth()`
    /// must skip when this file exists, because `AuthManager` will
    /// otherwise mint fresh credentials via the remote devbox login helper and
    /// return them — the same recovery path that lets the binary
    /// "just work" the first time it runs in a devbox environment.
    const DEVBOX_SA_TOKEN_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";
    fn skip_if_devbox(test_name: &str) -> bool {
        if std::path::Path::new(DEVBOX_SA_TOKEN_PATH).exists() {
            eprintln!(
                "SKIP {test_name}: running in a devbox environment (AuthManager would mint \
                 fresh credentials via the remote devbox login helper, bypassing the \
                 no-credentials test fixture). This is by design for production \
                 use of the binary."
            );
            true
        } else {
            false
        }
    }

    /// Env vars cleared by [`with_env_isolated`] / inline test
    /// teardown. Each can short-circuit our `AuthManager` setup
    /// against the test's tempdir if left in place:
    /// * `XAI_API_KEY` — would return early from `resolve_api_key`.
    /// * `GROK_AUTH` — inline-JSON credentials override that bypasses
    ///   the on-disk read entirely (`AuthManager::new`).
    /// * `GROK_AUTH_PATH` — overrides the auth.json path; if set to
    ///   the operator's real `~/.grok/auth.json`, the test would read
    ///   live OIDC credentials instead of the scratch fixture.
    /// * `GROK_AUTH_PROVIDER_COMMAND` — selects an external
    ///   refresher that could mint credentials independent of the
    ///   fixture.
    const ISOLATED_ENV_KEYS: &[&str] = &[
        "XAI_API_KEY",
        "GROK_AUTH",
        "GROK_AUTH_PATH",
        "GROK_AUTH_PROVIDER_COMMAND",
    ];

    /// Async helper: clear every env var in [`ISOLATED_ENV_KEYS`]
    /// before running `fut`, restoring them after. Callers must be
    /// `#[serial_test::serial]` because env mutation is process-global.
    async fn with_env_isolated<F, T>(fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let saved: Vec<(&str, Option<String>)> = ISOLATED_ENV_KEYS
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        for (k, _) in &saved {
            unsafe { std::env::remove_var(k) };
        }
        let out = fut.await;
        for (k, v) in saved {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        out
    }

    /// F21: pin the operator-visible summary string.
    #[test]
    fn summary_render_format_is_pinned() {
        let s = Summary {
            turns: 4,
            gate_continue: 3,
            gate_nudge: 1,
            laz_would_nudge: 0,
            laz_not_stalled: 2,
            laz_low_confidence: 1,
            laz_aborted: 1,
        };
        assert_eq!(
            s.render(),
            "Processed 4 turns. TodoGate: 3 Continue, 1 Nudge. \
             Laziness: 2 NoNudge-NotStalled, 1 NoNudge-LowConfidence, 0 WouldNudge, 1 Aborted."
        );
    }

    /// F25: synthetic 50-item history is trimmed by `window_start`.
    #[tokio::test]
    async fn laziness_window_trim_is_applied() {
        let mut hist: Vec<ConversationItem> = Vec::new();
        hist.push(ConversationItem::System(SystemItem {
            content: "sys".into(),
        }));
        for i in 0..24 {
            hist.push(ConversationItem::User(UserItem {
                content: vec![ContentPart::Text {
                    text: format!("u{i}").into(),
                }],
                synthetic_reason: None,
                ..Default::default()
            }));
            hist.push(ConversationItem::Assistant(AssistantItem {
                content: format!("a{i}").into(),
                tool_calls: vec![],
                model_id: None,
                model_fingerprint: None,
                reasoning_effort: None,
            }));
        }
        hist.push(ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: "ulast".into(),
            }],
            synthetic_reason: None,
            ..Default::default()
        }));

        let record = TurnRecord {
            turn: "turn_synth".into(),
            trace: TurnTrace {
                metadata: TurnMetadata {
                    turn_number: Some(0),
                    request_id: Some("r".into()),
                    session_id: Some("s".into()),
                    turn_started_at: None,
                },
                after_state_history: hist,
            },
        };

        let stub = StubClient(
            r#"{"category":"not_stalled_complete","confidence":0.9,"evidence":"stub"}"#.to_owned(),
        );
        let data = process_turn(
            &record,
            "m",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            LAZINESS_INCLUDE_REASONING,
            None,
            &stub,
        )
        .await;
        assert_eq!(data.items_in_history, 50);
        assert!(data.items_after_window_trim <= data.items_in_history);
        assert!(
            data.items_after_window_trim >= LAZINESS_CONTEXT_ITEM_LIMIT,
            "trimmed window respects min-user/min-assistant minimums",
        );
    }

    /// F27: every decision pair is counted in the right bucket.
    #[test]
    fn bump_summary_buckets_every_decision_pair() {
        let gates = [GateDecisionKind::Continue, GateDecisionKind::Nudge];
        let lazs = [
            LazinessDecisionKind::WouldNudge,
            LazinessDecisionKind::NoNudgeNotStalled,
            LazinessDecisionKind::NoNudgeLowConfidence,
            LazinessDecisionKind::Aborted,
        ];

        for g in gates {
            for l in lazs {
                let mut s = Summary::default();
                let empty: Vec<String> = Vec::new();
                let line = TurnLine {
                    turn_id: "t",
                    turn_number: None,
                    request_id: None,
                    items_in_history: 0,
                    items_after_window_trim: 0,
                    gate_backing_task_count: 0,
                    classifier_backing_task_count: 0,
                    todo_state: TodoSnapshotOut {
                        pending: empty.as_slice(),
                        in_progress_unbacked: empty.as_slice(),
                        in_progress_backed: empty.as_slice(),
                    },
                    todo_gate: TodoGateOut {
                        decision: g,
                        reason: None,
                        reminder: None,
                    },
                    laziness_classifier: LazinessOut {
                        model_id: "m",
                        elapsed_ms: 0,
                        parsed: None,
                        decision: l,
                        abort_reason: None,
                        error_detail: None,
                        raw_output: None,
                    },
                    include_reasoning: LAZINESS_INCLUDE_REASONING,
                    turn_elapsed_seconds: None,
                };
                bump_summary(&mut s, &line);
                let gate_bucket = match g {
                    GateDecisionKind::Continue => s.gate_continue,
                    GateDecisionKind::Nudge => s.gate_nudge,
                };
                let laz_bucket = match l {
                    LazinessDecisionKind::WouldNudge => s.laz_would_nudge,
                    LazinessDecisionKind::NoNudgeNotStalled => s.laz_not_stalled,
                    LazinessDecisionKind::NoNudgeLowConfidence => s.laz_low_confidence,
                    LazinessDecisionKind::Aborted => s.laz_aborted,
                };
                assert_eq!(gate_bucket, 1, "gate {g:?} bucket");
                assert_eq!(laz_bucket, 1, "laz {l:?} bucket");
                assert_eq!(s.turns, 1);
            }
        }
    }

    #[test]
    fn decision_kind_is_exhaustive() {
        assert_eq!(
            decision_kind(DebugDecision::WouldNudge),
            LazinessDecisionKind::WouldNudge
        );
        assert_eq!(
            decision_kind(DebugDecision::NoNudgeNotStalled),
            LazinessDecisionKind::NoNudgeNotStalled
        );
        assert_eq!(
            decision_kind(DebugDecision::NoNudgeLowConfidence),
            LazinessDecisionKind::NoNudgeLowConfidence
        );
        assert_eq!(
            decision_kind(DebugDecision::Aborted),
            LazinessDecisionKind::Aborted
        );
        assert_eq!(
            decision_kind(DebugDecision::SuppressedNotGoalMode),
            LazinessDecisionKind::WouldNudge
        );
    }

    /// N5: clap value-parser rejects out-of-range / non-finite floats.
    #[test]
    fn validate_min_confidence_rejects_bad_values() {
        assert!(validate_min_confidence("0.0").is_ok());
        assert!(validate_min_confidence("1.0").is_ok());
        assert!(validate_min_confidence("0.5").is_ok());
        assert!(validate_min_confidence("-0.1").is_err());
        assert!(validate_min_confidence("1.1").is_err());
        assert!(validate_min_confidence("nan").is_err());
        assert!(validate_min_confidence("inf").is_err());
        assert!(validate_min_confidence("not-a-float").is_err());
    }

    /// N7: `parse_trace_file` enforces a size bound, and known-good
    /// inputs under the bound parse cleanly. We don't allocate 256 MB
    /// on disk just to exercise the rejection arm — the const value
    /// is read directly and the parse path is exercised on the
    /// in-tree synthetic.
    #[test]
    fn parse_trace_file_round_trips_under_bound() {
        const _BOUND_SANITY: () = assert!(MAX_TRACE_FILE_BYTES > 1024);
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("ok.json");
        std::fs::write(&path, SYNTHETIC_TRACE).expect("write");
        let parsed = parse_trace_file(&path).expect("parse");
        assert_eq!(parsed.len(), 4);
    }

    /// N10: ordering-independent failure path — when both `--trace`
    /// is missing AND `--output` parent is missing AND `--api-key` is
    /// absent (with env unset), the error message names the FIRST
    /// failure, which is `--trace`.
    #[tokio::test]
    #[serial_test::serial]
    async fn run_rejects_missing_trace_path() {
        let prev = std::env::var("XAI_API_KEY").ok();
        unsafe { std::env::remove_var("XAI_API_KEY") };
        let tmp = tempfile::tempdir().expect("tempdir");
        let args = RunArgs {
            trace: PathBuf::from("/definitely/does/not/exist.json"),
            output: None,
            model_id: "m".into(),
            api_base_url: "https://x".into(),
            // N10: api_key None + env unset + empty auth.json — the
            // test fails for the right reason regardless of check
            // ordering (no API-key source resolves successfully, but
            // the `--trace` check runs first).
            api_key: None,
            min_confidence: None,
            include_reasoning: None,
            grok_home: Some(tmp.path().to_path_buf()),
        };
        let err = run(args).await.expect_err("missing trace");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--trace path is not a regular file"),
            "msg: {msg}"
        );
        match prev {
            Some(v) => unsafe { std::env::set_var("XAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("XAI_API_KEY") },
        }
    }

    fn make_record(turn_id: &str, turn_started_at: Option<&str>) -> TurnRecord {
        TurnRecord {
            turn: turn_id.to_owned(),
            trace: TurnTrace {
                metadata: TurnMetadata {
                    turn_number: None,
                    request_id: None,
                    session_id: None,
                    turn_started_at: turn_started_at.map(str::to_owned),
                },
                after_state_history: vec![],
            },
        }
    }

    #[test]
    fn compute_turn_elapsed_seconds_uses_next_turn_timestamp_for_lower_bound() {
        let trace = vec![
            make_record("turn_0", Some("2026-05-21T03:18:51.618550+00:00")),
            make_record("turn_1", Some("2026-05-21T03:29:30.605351+00:00")),
            make_record("turn_2", Some("2026-05-21T03:40:00.292197+00:00")),
            make_record("turn_3", Some("2026-05-21T13:45:15.889765+00:00")),
        ];
        let elapsed = compute_turn_elapsed_seconds(&trace);
        assert_eq!(elapsed.len(), 4);
        // turn_0: 03:18:51 → 03:29:30 = 638 s
        assert_eq!(elapsed[0], Some(638));
        // turn_1: 03:29:30 → 03:40:00 = 629 s (~10.5 min)
        assert_eq!(elapsed[1], Some(629));
        // turn_2: 03:40:00 → 13:45:15 ≈ 36315 s
        assert_eq!(elapsed[2], Some(36315));
        // Last turn has no follow-up.
        assert_eq!(elapsed[3], None);
    }

    #[test]
    fn compute_turn_elapsed_seconds_returns_none_when_timestamp_missing() {
        let trace = vec![
            make_record("turn_0", None),
            make_record("turn_1", Some("2026-05-21T03:29:30.605351+00:00")),
        ];
        let elapsed = compute_turn_elapsed_seconds(&trace);
        // Missing on either end → None.
        assert_eq!(elapsed[0], None);
        // Last turn is always None.
        assert_eq!(elapsed[1], None);
    }

    #[test]
    fn compute_turn_elapsed_seconds_returns_none_when_next_timestamp_missing() {
        // Symmetric counterpart of the previous test: `(Some, None)`.
        let trace = vec![
            make_record("turn_0", Some("2026-05-21T03:29:30+00:00")),
            make_record("turn_1", None),
        ];
        let elapsed = compute_turn_elapsed_seconds(&trace);
        assert_eq!(elapsed[0], None, "next is missing ⇒ no delta available");
        assert_eq!(elapsed[1], None);
    }

    #[test]
    fn compute_turn_elapsed_seconds_returns_none_on_malformed_timestamp() {
        // Malformed on either end ⇒ parse fails ⇒ `None`. Two
        // assertions to cover both positions.
        let trace_bad_cur = vec![
            make_record("turn_0", Some("not-a-timestamp")),
            make_record("turn_1", Some("2026-05-21T03:29:30+00:00")),
        ];
        assert_eq!(compute_turn_elapsed_seconds(&trace_bad_cur)[0], None);

        let trace_bad_next = vec![
            make_record("turn_0", Some("2026-05-21T03:29:30+00:00")),
            make_record("turn_1", Some("garbage")),
        ];
        assert_eq!(compute_turn_elapsed_seconds(&trace_bad_next)[0], None);
    }

    #[test]
    fn compute_turn_elapsed_seconds_zero_on_identical_timestamps() {
        let trace = vec![
            make_record("turn_0", Some("2026-05-21T03:29:30+00:00")),
            make_record("turn_1", Some("2026-05-21T03:29:30+00:00")),
        ];
        assert_eq!(
            compute_turn_elapsed_seconds(&trace)[0],
            Some(0),
            "identical timestamps ⇒ Some(0), not None",
        );
    }

    #[test]
    fn compute_turn_elapsed_seconds_returns_none_on_reversed_order() {
        // Trace is malformed (turn_1's timestamp predates turn_0's),
        // which produces a negative delta — `u64::try_from` rejects
        // it and the slot is `None`. The trace replay should not
        // emit nonsense data when the input is non-chronological.
        let trace = vec![
            make_record("turn_0", Some("2026-05-21T03:29:30+00:00")),
            make_record("turn_1", Some("2026-05-21T03:00:00+00:00")),
        ];
        assert_eq!(compute_turn_elapsed_seconds(&trace)[0], None);
    }

    #[tokio::test]
    async fn runtime_state_line_includes_turn_elapsed_seconds_when_present() {
        let trace = parse_synthetic();
        let stub = CapturingStub::new(
            r#"{"category":"not_stalled_complete","confidence":0.9,"evidence":"e"}"#,
        );
        let _ = process_turn(
            &trace[0],
            "m",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            LAZINESS_INCLUDE_REASONING,
            Some(629),
            &stub,
        )
        .await;
        let req = stub.take();
        let user_text = match &req.items[1] {
            ConversationItem::User(u) => match &u.content[0] {
                ContentPart::Text { text } => text.clone(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        assert!(
            user_text.contains(
                "[runtime_state] outstanding_background_tasks_and_subagents=0 turn_elapsed_seconds=629\n"
            ),
            "runtime_state line carries turn_elapsed_seconds: {user_text}",
        );
    }

    #[tokio::test]
    async fn runtime_state_line_omits_turn_elapsed_seconds_when_absent() {
        let trace = parse_synthetic();
        let stub = CapturingStub::new(
            r#"{"category":"not_stalled_complete","confidence":0.9,"evidence":"e"}"#,
        );
        let _ = process_turn(
            &trace[0],
            "m",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            LAZINESS_INCLUDE_REASONING,
            None,
            &stub,
        )
        .await;
        let req = stub.take();
        let user_text = match &req.items[1] {
            ConversationItem::User(u) => match &u.content[0] {
                ContentPart::Text { text } => text.clone(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        // Constrain the negative assertion to the runtime_state line
        // slice — a future refactor that inlines any prompt language
        // mentioning `turn_elapsed_seconds` into the user wrapper
        // won't cause this test to fire for the wrong reason.
        let begin = "=== BEGIN TRANSCRIPT ===\n";
        let begin_pos = user_text.find(begin).expect("BEGIN sentinel present");
        let after_begin = &user_text[begin_pos + begin.len()..];
        let runtime_state_line = after_begin
            .split_once('\n')
            .map(|(line, _)| line)
            .expect("runtime_state line terminated with newline");
        assert_eq!(
            runtime_state_line, "[runtime_state] outstanding_background_tasks_and_subagents=0",
            "runtime_state line omits turn_elapsed_seconds when absent",
        );
        assert!(
            !runtime_state_line.contains("turn_elapsed_seconds"),
            "no stray turn_elapsed_seconds key when None: {runtime_state_line}",
        );
    }

    /// End-to-end: a 2-turn trace whose `turn_started_at` timestamps
    /// are 60s apart emits `turn_elapsed_seconds=60` on the first
    /// turn's JSONL line and omits the field on the last turn. The
    /// JSONL line for every turn carries the (possibly-null)
    /// `turn_elapsed_seconds` field for downstream operators.
    #[tokio::test]
    async fn end_to_end_jsonl_carries_turn_elapsed_seconds_field() {
        let trace = vec![
            TurnRecord {
                turn: "turn_0".into(),
                trace: TurnTrace {
                    metadata: TurnMetadata {
                        turn_number: Some(0),
                        request_id: Some("r0".into()),
                        session_id: Some("s".into()),
                        turn_started_at: Some("2026-05-21T03:29:30+00:00".into()),
                    },
                    after_state_history: vec![ConversationItem::User(UserItem {
                        content: vec![ContentPart::Text { text: "hi".into() }],
                        synthetic_reason: None,
                        ..Default::default()
                    })],
                },
            },
            TurnRecord {
                turn: "turn_1".into(),
                trace: TurnTrace {
                    metadata: TurnMetadata {
                        turn_number: Some(1),
                        request_id: Some("r1".into()),
                        session_id: Some("s".into()),
                        turn_started_at: Some("2026-05-21T03:30:30+00:00".into()),
                    },
                    after_state_history: vec![ConversationItem::User(UserItem {
                        content: vec![ContentPart::Text { text: "hi".into() }],
                        synthetic_reason: None,
                        ..Default::default()
                    })],
                },
            },
        ];
        let stub = StubClient(
            r#"{"category":"not_stalled_complete","confidence":0.9,"evidence":"e"}"#.to_owned(),
        );
        let mut buf = Vec::<u8>::new();
        run_with_writer(
            &trace,
            "m",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            LAZINESS_INCLUDE_REASONING,
            &mut buf,
            &stub,
        )
        .await
        .expect("run");
        let body = String::from_utf8(buf).expect("utf8");
        let mut lines = body.lines();
        let first: serde_json::Value =
            serde_json::from_str(lines.next().expect("first line")).expect("json");
        let second: serde_json::Value =
            serde_json::from_str(lines.next().expect("second line")).expect("json");
        assert_eq!(
            first.get("turn_elapsed_seconds"),
            Some(&serde_json::json!(60))
        );
        assert_eq!(
            second.get("turn_elapsed_seconds"),
            Some(&serde_json::Value::Null)
        );
    }

    /// End-to-end against the reference trace (skipped when absent):
    /// turn_1 must carry `turn_elapsed_seconds≈630` (10.5 min), and
    /// the stub forces `stalled_false_completion` to confirm the
    /// JSONL surfaces the new category cleanly.
    #[tokio::test]
    async fn reference_trace_turn_1_carries_elapsed_and_new_category() {
        let path =
            Path::new("/root/traces/trace-019e4888-ded4-7632-9bb5-a7964974d34e-all-turns.json");
        if !path.exists() {
            eprintln!(
                "SKIP reference_trace_turn_1_carries_elapsed_and_new_category: {} not present",
                path.display(),
            );
            return;
        }
        let trace = parse_trace_file(path).expect("parse reference trace");
        let stub = StubClient(
            r#"{"category":"stalled_false_completion","confidence":0.9,"evidence":"unbacked completion claims in final message"}"#
                .to_owned(),
        );
        let mut buf = Vec::<u8>::new();
        run_with_writer(
            &trace,
            "stub-model",
            LAZINESS_DEFAULT_MIN_CONFIDENCE,
            LAZINESS_INCLUDE_REASONING,
            &mut buf,
            &stub,
        )
        .await
        .expect("run");
        let body = String::from_utf8(buf).expect("utf8");
        let lines: Vec<serde_json::Value> = body
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        assert_eq!(lines.len(), trace.len());
        // turn_1 (second entry) — 03:29:30 → 03:40:00 ≈ 630 s.
        let turn_1 = &lines[1];
        let elapsed = turn_1
            .get("turn_elapsed_seconds")
            .and_then(|v| v.as_u64())
            .expect("elapsed present for turn_1");
        assert!(
            (600..=660).contains(&elapsed),
            "turn_1 elapsed should be ~630 s, got {elapsed}",
        );
        // The classifier's stubbed verdict must be surfaced in the
        // parsed-category field on the JSONL line.
        let category = turn_1
            .pointer("/laziness_classifier/parsed/category")
            .and_then(|v| v.as_str())
            .expect("category present");
        assert_eq!(category, "stalled_false_completion");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn run_rejects_missing_output_parent() {
        let prev = std::env::var("XAI_API_KEY").ok();
        unsafe { std::env::remove_var("XAI_API_KEY") };
        let tmp = tempfile::tempdir().expect("tempdir");
        let trace_path = tmp.path().join("trace.json");
        std::fs::write(&trace_path, "[]").expect("write");
        let args = RunArgs {
            trace: trace_path,
            output: Some(PathBuf::from("/definitely/does/not/exist/out.jsonl")),
            model_id: "m".into(),
            api_base_url: "https://x".into(),
            api_key: None,
            min_confidence: None,
            include_reasoning: None,
            grok_home: Some(tmp.path().to_path_buf()),
        };
        let err = run(args).await.expect_err("bad parent");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--output parent directory does not exist"),
            "msg: {msg}"
        );
        match prev {
            Some(v) => unsafe { std::env::set_var("XAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("XAI_API_KEY") },
        }
    }
}
