//! Goal mode state machine.
//!
//! This module contains [`GoalTracker`], a pure state machine (no async I/O)
//! modeled after [`PlanModeTracker`](super::plan_mode::PlanModeTracker).
//! The `SessionActor` owns one `GoalTracker` behind a `Mutex` and calls
//! its methods at the appropriate orchestration points.
//!
//! Persisted `"infra_paused"` requires this shell version (one-way upgrade).
//! Unknown wire values (including unknown `*_paused` forms) deserialize to
//! [`GoalStatus::UserPaused`] so a corrupt or forward-version snapshot can
//! never resurrect as a self-driving goal.

use std::path::PathBuf;
use std::time::Instant;

/// Consecutive identical gap fingerprints that trip the stall
/// early-exit: two in a row means the model produced no change in the
/// flagged gaps between attempts, so iterating further is futile and
/// the goal auto-pauses before exhausting the run cap.
pub(crate) const GOAL_CLASSIFIER_STALL_THRESHOLD: u32 = 2;

/// Extra classifier rounds granted (once) when the strategist fires, so its
/// restructure isn't starved under a small cap.
pub(crate) const GOAL_STRATEGIST_CAP_BONUS: u32 = 3;

/// Relaxed stall threshold while a strategist restructure is in flight (cap
/// bonus active): sized to cover the granted bonus rounds, yet bounded so a
/// stuck restructure still exits.
pub(crate) const GOAL_STRATEGIST_STALL_THRESHOLD: u32 =
    GOAL_CLASSIFIER_STALL_THRESHOLD + GOAL_STRATEGIST_CAP_BONUS;

/// Max retained goal-history entries. Only the last is surfaced on the wire
/// (`GoalUpdated.last_event`), but the whole list is persisted, so it is
/// capped (oldest dropped) to keep a long goal's snapshot bounded.
const GOAL_HISTORY_MAX: usize = 64;

// Phase / Status enums

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GoalPhase {
    Idle,
    Planning,
    Executing,
}

/// Lifecycle status of a goal. The paused variants encode the
/// reason the goal was paused — `UserPaused` for Ctrl+C / `/goal pause`,
/// `BackOffPaused` when the classifier run cap is hit, `NoProgressPaused`
/// when the verifier flags the same gaps with no progress before the cap,
/// `InfraPaused` when a turn finishes with an infrastructure error,
/// and `Blocked` when the model determined the goal is not achievable
/// in the current environment. Use [`GoalStatus::is_paused`] to test
/// paused-ness uniformly across all six variants.
///
/// **Backwards-compat serde aliases:** older shells serialized this
/// enum with the default PascalCase form (`"Active"`, `"Paused"`,
/// `"BudgetLimited"`, `"Complete"`). The `#[serde(alias = ...)]`
/// attributes preserve in-flight goal snapshots written by older shells
/// — legacy `"Paused"` maps to `UserPaused` (matches the pager-side
/// fallback). New
/// snapshots emit snake_case per `rename_all`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    #[serde(alias = "Active")]
    Active,
    #[serde(alias = "Paused")]
    UserPaused,
    BackOffPaused,
    /// Verifier flagged the same gaps across consecutive attempts (no
    /// progress) and auto-paused before the run cap. Resumable, same paused
    /// family as `BackOffPaused`; split out so the UI distinguishes a stall
    /// from a cap pause.
    NoProgressPaused,
    /// Infrastructure turn failure (`PromptTurnResult::Err`). The
    /// human-readable reason is stashed in [`GoalOrchestration::pause_message`].
    InfraPaused,
    /// stashed in [`GoalOrchestration::pause_message`].
    Blocked,
    #[serde(alias = "BudgetLimited")]
    BudgetLimited,
    #[serde(alias = "Complete")]
    Complete,
}

impl<'de> serde::Deserialize<'de> for GoalStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_wire_str(&s))
    }
}

impl GoalStatus {
    /// Parse a persisted/wire status string. Unknown values map to
    /// `UserPaused`: a status this shell cannot interpret must restore as
    /// a resumable paused goal, never an Active self-driving one.
    pub fn from_wire_str(s: &str) -> Self {
        match s {
            "active" | "Active" => Self::Active,
            "user_paused" | "paused" | "Paused" => Self::UserPaused,
            // Historical status from shells that had doom-loop auto-pause.
            "doom_loop_paused" => Self::UserPaused,
            "back_off_paused" => Self::BackOffPaused,
            "no_progress_paused" => Self::NoProgressPaused,
            "infra_paused" => Self::InfraPaused,
            "blocked" => Self::Blocked,
            "budget_limited" | "BudgetLimited" => Self::BudgetLimited,
            "complete" | "Complete" => Self::Complete,
            _ => Self::UserPaused,
        }
    }
    /// `true` for any paused variant (`UserPaused`, `BackOffPaused`,
    /// `NoProgressPaused`, `InfraPaused`, `Blocked`).
    pub fn is_paused(&self) -> bool {
        matches!(
            self,
            Self::UserPaused
                | Self::BackOffPaused
                | Self::NoProgressPaused
                | Self::InfraPaused
                | Self::Blocked
        )
    }
}

/// Input to [`GoalTracker::pause`] / [`GoalTracker::pause_with_message`]
/// and the auto-pause helpers. Maps 1:1 to one of the paused variants on
/// [`GoalStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalPauseReason {
    User,
    BackOff,
    /// Verification stage saw no change in the flagged-gap fingerprint
    /// across consecutive attempts and auto-paused before the run cap.
    /// Maps to [`GoalStatus::NoProgressPaused`] — same resumable paused
    /// family as the cap, surfaced distinctly in the UI / telemetry.
    NoProgress,
    /// [`GoalStatus::Blocked`]; pairs with a human-readable message on
    /// [`GoalOrchestration::pause_message`].
    Verification,
    /// Turn finished with `PromptTurnResult::Err`. Maps to
    /// [`GoalStatus::InfraPaused`]; pairs with a human-readable message on
    /// [`GoalOrchestration::pause_message`].
    Infra,
}

impl GoalPauseReason {
    fn to_status(self) -> GoalStatus {
        match self {
            Self::User => GoalStatus::UserPaused,
            Self::BackOff => GoalStatus::BackOffPaused,
            Self::NoProgress => GoalStatus::NoProgressPaused,
            Self::Verification => GoalStatus::Blocked,
            Self::Infra => GoalStatus::InfraPaused,
        }
    }

    /// Short, stable label stashed in the `GoalPaused` history entry's
    /// `detail` so the pager's Recent History distinguishes pause causes.
    fn history_detail(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::BackOff => "back_off",
            Self::NoProgress => "no_progress",
            Self::Verification => "blocked",
            Self::Infra => "infra",
        }
    }
}

/// Aggregate verdict produced by the goal-verification stage.
/// `Achieved` indicates the adversarial skeptic panel judged the goal
/// complete; `NotAchieved` means another worker round is warranted.
/// Serialized in snake_case to match `GoalStatus` / `GoalPhase`. The
/// enum name retains the `Classifier` prefix for wire stability across
/// the verification-stage rewire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalClassifierVerdict {
    Achieved,
    NotAchieved,
}

// History

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalEvent {
    GoalCreated,
    PlanningStarted,
    PlanningCompleted,
    PlanningFailed,
    WorkerStarted,
    WorkerCompleted,
    WorkerFailed,
    ContextRotated,
    GoalPaused,
    GoalResumed,
    GoalCompleted,
    GoalCleared,
    BudgetExceeded,
    /// The model tried to stop early (a "giving up"-style bail) while the
    /// goal still had open work and the harness re-nudged it. `detail`
    /// carries the matched stop-pattern label.
    PrematureStopDetected,
    /// Forward-compat sink: a history event written by a newer shell that
    /// this binary doesn't know. Lets an older binary deserialize a newer
    /// snapshot's history instead of failing the whole field.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoalHistoryEntry {
    pub timestamp: String,
    pub event: GoalEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_used: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmet: Vec<String>,
}

impl GoalHistoryEntry {
    /// Minimal lifecycle entry stamped with the current time (`round` /
    /// `tokens_used` / `unmet` left empty). Single constructor so the
    /// timestamp + defaults aren't re-spelled at every call site.
    pub(crate) fn now(event: GoalEvent, detail: Option<String>) -> Self {
        Self {
            timestamp: chrono::Utc::now().to_rfc3339(),
            event,
            detail,
            round: None,
            tokens_used: None,
            unmet: Vec::new(),
        }
    }
}

// GoalOrchestration (full persisted state)

/// Generate a short opaque identifier used to scope the per-goal
/// scratch root (`<temp_dir>/grok-goal-<id>`) and the verifier
/// verdict/details files inside it.
///
/// The id is a 12-char prefix of a UUIDv4 simple form — ~48 bits of
/// entropy, enough to avoid collision between concurrent goals on the
/// same machine while staying short enough that the orchestrator model
/// can copy it verbatim into spawned verifier prompts without
/// truncation or typo risk (see the past-issue memory note on
/// "UUID-in-prompt copy-fidelity failure mode").
pub(crate) fn generate_verifier_id() -> String {
    let mut s = uuid::Uuid::new_v4().simple().to_string();
    s.truncate(12);
    s
}

/// Private per-goal scratch root: `<temp_dir>/grok-goal-<verifier_id>`.
///
/// Rooted at [`std::env::temp_dir`] (respects `TMPDIR`) and namespaced by the
/// goal's `verifier_id`, so concurrent goals never collide and cleanup of one
/// never touches another. Removed wholesale on every terminal goal transition.
pub(crate) fn goal_scratch_root(verifier_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("grok-goal-{verifier_id}"))
}

/// Create (or verify) the goal's scratch root, locked to the owner
/// (0700 on unix). Artifact names under it are predictable from the
/// prompt/log-visible `verifier_id`, so the root itself is the
/// symlink/squat defense: creation is atomic with mode 0700 (no
/// default-mode window), and a pre-existing entry is accepted only if
/// [`verify_owned_real_dir`] passes, then re-pinned to 0700 (safe to
/// chmod: the entry is proven ours and the sticky-bit temp parent
/// prevents swapping it). Callers treat `Err` as "do not write
/// classifier artifacts here".
pub(crate) fn ensure_goal_scratch_root(verifier_id: &str) -> std::io::Result<PathBuf> {
    let root = goal_scratch_root(verifier_id);
    #[cfg(unix)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(not(unix))]
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(&root) {
        Ok(()) => Ok(root),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            verify_owned_real_dir(&root)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
            }
            Ok(root)
        }
        Err(e) => Err(e),
    }
}

/// Shared squat predicate: `Ok` iff `path` is a REAL directory
/// (`symlink_metadata`, never follows) owned by the current euid.
fn verify_owned_real_dir(path: &std::path::Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Err(std::io::Error::other(
            "goal scratch root exists but is not a real directory (symlink squat?)",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions and cannot fail.
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::other(
                "goal scratch root exists but is not owned by the current user",
            ));
        }
    }
    Ok(())
}

/// Cross-filesystem rescue copy; `create_new` (O_EXCL) makes any
/// pre-existing destination — symlink included — fail the copy instead
/// of being written through.
fn copy_no_follow(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    let mut src_f = std::fs::File::open(src)?;
    let mut dest_f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    std::io::copy(&mut src_f, &mut dest_f)?;
    Ok(())
}

/// Parse `(attempt, skeptic_idx)` out of a per-skeptic report file name
/// (`goal-classifier-{vid}-{attempt}-skeptic-{idx}.md`); `None` otherwise.
fn parse_skeptic_report_order(name: &str) -> Option<(u32, u32)> {
    let stem = name.strip_suffix(".md")?;
    let (head, idx) = stem.rsplit_once("-skeptic-")?;
    let (_, attempt) = head.rsplit_once('-')?;
    Some((attempt.parse().ok()?, idx.parse().ok()?))
}

/// Inline the per-skeptic reports below the just-rescued canonical
/// details file — its path references die with the scratch root, so the
/// rescued file must be self-contained.
///
/// Numeric (attempt DESC, skeptic ASC) order so budget elision drops
/// stale attempts, never the final attempt the canonical body
/// references. Whole files only: each report's on-disk size is checked
/// against the remaining budget BEFORE reading (reports are
/// harness-uncapped and this runs under the tracker lock); the first
/// overflow writes an elision marker and stops. Best-effort.
fn append_skeptic_reports(scratch_root: &std::path::Path, dest: &std::path::Path) {
    use std::io::Write;

    let Ok(entries) = std::fs::read_dir(scratch_root) else {
        return;
    };
    let mut reports: Vec<((u32, u32), PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let (attempt, idx) =
                parse_skeptic_report_order(path.file_name()?.to_string_lossy().as_ref())?;
            Some(((attempt, idx), path))
        })
        .collect();
    reports.sort_by_key(|&((attempt, idx), _)| (std::cmp::Reverse(attempt), idx));
    if reports.is_empty() {
        return;
    }
    let Ok(mut out) = std::fs::OpenOptions::new().append(true).open(dest) else {
        return;
    };
    let mut budget = crate::session::goal_classifier::GOAL_VERIFIER_PANEL_MAX_BYTES as u64;
    for (_, report) in reports {
        let name = report.file_name().unwrap_or_default().to_string_lossy();
        let header = format!("\n\n---\n## Inlined skeptic report: {name}\n\n");
        let Ok(len) = std::fs::symlink_metadata(&report).map(|m| m.len()) else {
            continue;
        };
        if header.len() as u64 + len > budget {
            let _ = out
                .write_all(b"\n\n---\n(remaining skeptic reports elided: rescue budget reached)\n");
            return;
        }
        let Ok(body) = std::fs::read_to_string(&report) else {
            continue;
        };
        budget = budget.saturating_sub(header.len() as u64 + body.len() as u64);
        if out
            .write_all(header.as_bytes())
            .and_then(|()| out.write_all(body.as_bytes()))
            .is_err()
        {
            return;
        }
    }
}

/// The goal model's private scratch dir (`<scratch_root>/implementer`).
/// The implementer writes screenshots, temp scripts, and throwaway
/// artifacts here; the skeptics READ it to verify the claimed outputs.
pub(crate) fn implementer_scratch_dir(verifier_id: &str) -> PathBuf {
    goal_scratch_root(verifier_id).join("implementer")
}

/// Skeptic `idx`'s private scratch dir (`<scratch_root>/skeptic-<idx>`).
/// Each skeptic re-runs the verification plan into its OWN dir so N
/// skeptics never overwrite each other or the implementer's outputs.
pub(crate) fn skeptic_scratch_dir(verifier_id: &str, idx: u32) -> PathBuf {
    goal_scratch_root(verifier_id).join(format!("skeptic-{idx}"))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoalOrchestration {
    pub goal_id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub phase: GoalPhase,
    pub token_budget: Option<i64>,
    pub elapsed_ms: u64,
    pub created_at: String,
    pub current_subagent_id: Option<String>,
    pub current_subagent_role: Option<String>,
    #[serde(default)]
    pub total_worker_rounds: u32,
    #[serde(default)]
    pub total_verify_rounds: u32,
    #[serde(skip)]
    pub budget_limit_reported: bool,
    /// Session-wide total tokens recorded at goal creation. Seeds the
    /// spend accumulator (`last_session_tokens_seen`) so pre-goal usage
    /// is excluded from the goal's token count.
    #[serde(default)]
    pub token_baseline: i64,
    /// Monotonic high-water mark ratcheted by `SessionActor::goal_tokens`
    /// so wire values never decrease across compactions.
    #[serde(default)]
    pub tokens_used_high_water: i64,
    /// Cumulative parent-session tokens spent on this goal: the sum of
    /// positive per-call deltas of the session token total. Unlike a
    /// `current - baseline` difference, this can never shrink or freeze
    /// when auto-compaction reduces the context-size total. Best-effort
    /// sampling: growth fully consumed by a compaction between two
    /// `goal_tokens` calls is unobserved, and spend accrued since the
    /// last persisted snapshot is lost on crash (bounded by the snapshot
    /// cadence).
    #[serde(default)]
    pub parent_tokens_spent: i64,
    /// Session token total at the previous `SessionActor::goal_tokens`
    /// call — the anchor for the next positive delta. `None` on legacy
    /// snapshots; seeded from `token_baseline` on first use.
    #[serde(default)]
    pub last_session_tokens_seen: Option<i64>,
    pub history: Vec<GoalHistoryEntry>,

    /// Human-readable explanation set when the goal transitions to a
    /// paused state with a meaningful reason. `Blocked` and `InfraPaused`
    /// populate it (via [`GoalTracker::pause_with_message`]),
    /// but the field is orthogonal to status so future reasons can
    /// reuse it. Set only by [`GoalTracker::pause_with_message`];
    /// cleared by every transition out of a paused state —
    /// [`GoalTracker::resume`], [`GoalTracker::complete`], and
    /// [`GoalTracker::budget_limit`] all reset it to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator_blocker_key: Option<String>,
    #[serde(default)]
    pub evaluator_blocked_streak: u32,

    /// Short opaque identifier used to scope per-goal artifact paths
    /// owned by the harness. Today's consumers:
    ///
    /// * `goal_classifier.rs` — classifier details / changes diff /
    ///   per-skeptic verdict files (see the
    ///   `GOAL_CLASSIFIER_DETAILS_PATH_TEMPLATE`,
    ///   `GOAL_CLASSIFIER_CHANGES_PATH_TEMPLATE`,
    ///   `GOAL_VERIFIER_VERDICT_PATH_TEMPLATE`,
    ///   `GOAL_VERIFIER_DETAILS_PATH_TEMPLATE` consts).
    ///
    /// Generated by [`generate_verifier_id`] when the goal is created
    /// and persisted alongside the rest of the orchestration so the
    /// same id is reused across pause/resume cycles. The current
    /// model-facing template no longer references this id; the model
    /// is no longer instructed to read per-goal verdict files.
    ///
    /// Older persisted snapshots predate this field; the `serde(default)`
    /// attribute backfills a fresh id on load so verdict-file paths
    /// stay well-formed even after an upgrade.
    #[serde(default = "generate_verifier_id")]
    pub verifier_id: String,

    /// Number of times the goal-achievement classifier has been run
    /// for this goal. Reset only when the goal is recreated.
    #[serde(default)]
    pub classifier_runs_attempted: u32,
    /// Worker rounds since the last verification fired: `+1` per
    /// continuation build, reset to 0 when a classifier attempt is
    /// reserved. Drives the re-verify escalation.
    #[serde(default)]
    pub rounds_since_verify: u32,
    /// Hard cap on classifier runs for this goal. `None` means the
    /// cap has not been configured; `Some(0)` reserves the explicit
    /// "zero runs allowed" case. Mirrors the `token_budget:
    /// Option<i64>` precedent on this struct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier_max_runs: Option<u32>,
    /// Last aggregate verdict returned by the verification stage, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_classifier_verdict: Option<GoalClassifierVerdict>,
    /// Path to the most recent verification-stage details artifact on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_classifier_details_path: Option<String>,
    /// RFC3339 timestamp of the last classifier run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_classifier_at: Option<String>,
    /// Curated per-refuter gap summary (`build_gaps_summary`) from the
    /// most recent `NotAchieved` verdict. Inlined verbatim into every
    /// continuation directive until a later verdict overwrites it (an
    /// `Achieved` verdict clears it), so the freshest verifier feedback
    /// reaches the model each round rather than once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_classifier_gaps: Option<String>,
    /// First verification round's full `FINAL_RESPONSE`, replayed as the
    /// breadth anchor on later rounds so a cold skeptic panel sees the whole
    /// deliverable, not just that round's fix note. Captured once (capped);
    /// never cleared on `Achieved` — it must outlive each round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_final_response: Option<String>,
    /// Child session id of skeptic 0 (the persistent reject-gatekeeper)
    /// from the most recent N > 1 verification attempt. The next attempt
    /// resumes it (`resume_from`) so it delta-re-checks the prior gaps
    /// instead of re-analyzing cold. Cleared by [`GoalTracker::from_snapshot`]
    /// (the in-memory token records that anchor a resumed child's marginal
    /// accounting do not survive a restart), on goal completion, and never
    /// set for an N == 1 sole-judge panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skeptic0_session_id: Option<String>,
    /// Resolved skeptic index → `{model, agent_type}` assignment, frozen at
    /// the first verification panel and reused on every resume so skeptic-0
    /// (and the cold panel) keep stable models across attempts. Index `i`
    /// holds `pool[i % pool.len()]`; the vector grows (clamped) but never
    /// rewrites a committed index. Empty ⇒ all skeptics inherit the current
    /// model. Persists across snapshot save/restore exactly like
    /// `skeptic0_session_id`, and is reset on the same terminal transitions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skeptic_model_assignment: Vec<crate::util::config::GoalRoleModel>,
    /// Normalized gap fingerprint of the previous `NotAchieved`
    /// rejection (see `goal_classifier::gap_fingerprint`). Compared
    /// against the next rejection's fingerprint to detect a stuck loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_gap_fingerprint: Option<String>,
    /// Count of consecutive rejections carrying the same gap
    /// fingerprint (1 on the first occurrence of a fingerprint). Drives
    /// the stall early-exit in [`GoalTracker::record_classifier_stall`].
    #[serde(default)]
    pub classifier_stall_count: u32,
    /// Count of consecutive `NotAchieved` verifications regardless of
    /// gap content (resets to 0 on an `Achieved` verdict). Drives the
    /// stall-triggered strategist: it fires when this reaches the
    /// configured `goal_strategist_every` (N) and again at each multiple
    /// (2N, 3N, …). Distinct from `classifier_stall_count`, which only
    /// counts *identical*-fingerprint repeats — this catches whack-a-mole
    /// where each round flags a different gap.
    #[serde(default)]
    pub consecutive_not_achieved: u32,
    /// The `consecutive_not_achieved` value at which the strategist last
    /// fired. The trigger fires when `consecutive_not_achieved >=
    /// last_strategist_fired_at + N`, which is SKIP-ROBUST: the synthetic
    /// concurrent-in-flight path can bump the streak past a multiple of N
    /// without landing exactly on it, and a strict `% N == 0` check would
    /// then miss the fire. Reset to 0 with `consecutive_not_achieved`.
    #[serde(default)]
    pub last_strategist_fired_at: u32,
    /// Added to the resolved classifier cap once the strategist has fired.
    #[serde(default)]
    pub strategist_cap_bonus: u32,

    /// Path to the most recent strategist strategy note on disk
    /// (`<session_dir>/goal/strategy.md`, via
    /// [`GoalTracker::strategy_path`]). `None` until the strategist runs.
    /// Surfaced in the continuation directive so the model re-reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_strategy_path: Option<String>,
    /// Short narrative recommendation read back from the strategist's
    /// note (capped). Inlined into the continuation directive until a
    /// later strategist run overwrites it; cleared on an `Achieved`
    /// verdict (same replay convention as `last_classifier_gaps`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_strategy_recommendation: Option<String>,

    /// `git rev-parse HEAD` captured at goal creation. Used by the
    /// classifier to diff the worktree against the goal's baseline.
    /// `None` for goals created before the baseline-capture wiring
    /// landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes_baseline_commit: Option<String>,

    /// Path to the goal's plan markdown (`<session_dir>/goal/plan.md`,
    /// via [`GoalTracker::plan_path`]). `None` until a planner writes
    /// one. `is_some()` is the single source of truth for "this goal
    /// has a plan" — gates setup-time fire, the resume-retry path,
    /// and the load-time reconciler. Persisted across restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_file: Option<PathBuf>,

    /// Path to the immutable snapshot of the planner's ORIGINAL plan
    /// (`<session_dir>/goal/plan.baseline.md`, via
    /// [`GoalTracker::plan_baseline_path`]). Captured once right after the
    /// planner first writes `plan_file`; never overwritten on later attempts
    /// or restarts. The verifier diffs the CURRENT plan against it
    /// (`capture_plan_changes`) so a skeptic sees every edit the agent made to
    /// `plan.md` during the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_baseline_file: Option<PathBuf>,

    /// True once the harness created and squat-verified the scratch root AND
    /// the implementer subdir, so prompts can honestly say the dir exists.
    /// `#[serde(skip)]`: recomputed by `from_snapshot` on every reload (the
    /// sole reload path), so a persisted value would be dead-on-read — same as
    /// the recomputed/transient `live_*` fields below.
    #[serde(skip)]
    pub scratch_dir_ready: bool,

    // Transient live-progress fields (not persisted)
    #[serde(skip)]
    pub live_subagent_tokens: u64,
    /// Per-model marginal-token breakdown (model_id, tokens), sorted by
    /// tokens descending. Transient mirror of the active goal's subagent
    /// token records; `#[serde(skip)]` so legacy snapshots deserialize and
    /// it is never persisted.
    #[serde(skip)]
    pub live_tokens_by_model: Vec<(String, u64)>,
    #[serde(skip)]
    pub live_context_window: u64,
    #[serde(skip)]
    pub live_context_pct: u8,
    #[serde(skip)]
    pub live_turn_count: u32,
    #[serde(skip)]
    pub live_tool_call_count: u32,

    /// True while the goal planner subagent is running. Latched by
    /// `emit_goal_planning` and reset after the planner finishes so the
    /// "planning…" badge survives the subagent-spawn / token-accounting
    /// `GoalUpdated`s that fire mid-run. Transient (never persisted).
    #[serde(skip)]
    pub planning_in_flight: bool,

    /// True while the verification skeptic panel is running. Latched around
    /// the verification stage (mirrors `planning_in_flight`) so the
    /// "Verifying…" badge survives the token-accounting / continuation
    /// `GoalUpdated`s that fire mid-verification. Transient (never persisted).
    #[serde(skip)]
    pub verifying_in_flight: bool,
}

impl GoalOrchestration {
    /// Reset ALL strategist state in one place: the consecutive-NotAchieved
    /// streak, the last-fired marker, and the persisted recommendation +
    /// path. Coupling these means a streak reset can never leave a stale
    /// structural recommendation replaying into a clean run (a past
    /// reset/clear asymmetry across branches). Called wherever the goal
    /// starts a fresh streak (`Achieved`/`Blocked` verdicts) or ends/resumes
    /// (`complete`/`budget_limit`/`resume`).
    fn reset_strategist_fields(&mut self) {
        self.consecutive_not_achieved = 0;
        self.last_strategist_fired_at = 0;
        self.strategist_cap_bonus = 0;
        self.last_strategy_path = None;
        self.last_strategy_recommendation = None;
    }

    /// Clear the gap-fingerprint stall streak (shared reset so every caller
    /// zeroes the same fields).
    fn reset_classifier_stall_fields(&mut self) {
        self.last_gap_fingerprint = None;
        self.classifier_stall_count = 0;
    }

    fn reset_evaluator_blocker_fields(&mut self) {
        self.evaluator_blocker_key = None;
        self.evaluator_blocked_streak = 0;
    }
}

// GoalTracker (pure state machine)

#[derive(Debug)]
pub struct GoalTracker {
    orchestration: Option<GoalOrchestration>,
    session_dir: PathBuf,
    active_since: Option<Instant>,
}

impl GoalTracker {
    pub fn new(session_dir: PathBuf) -> Self {
        Self {
            orchestration: None,
            session_dir,
            active_since: None,
        }
    }

    /// Restore from a persisted snapshot.
    ///
    /// If the snapshot had an in-flight phase (Planning, Executing),
    /// reset to Idle because subagents don't survive a restart. An
    /// in-flight `Active` goal becomes `UserPaused`; other paused
    /// variants (including [`GoalStatus::InfraPaused`]) are preserved
    /// so `pause_message` and pause cause stay aligned. Clear
    /// `current_subagent_id`.
    pub fn from_snapshot(session_dir: PathBuf, mut snapshot: GoalOrchestration) -> Self {
        if matches!(snapshot.phase, GoalPhase::Planning | GoalPhase::Executing) {
            snapshot.phase = GoalPhase::Idle;
            snapshot.current_subagent_id = None;
            snapshot.current_subagent_role = None;
        }
        if snapshot.status == GoalStatus::Active {
            snapshot.status = GoalStatus::UserPaused;
        }
        // `planning_in_flight` / `verifying_in_flight` are `#[serde(skip)]` but
        // in-memory-clone callers bypass that; reset explicitly.
        snapshot.planning_in_flight = false;
        snapshot.verifying_in_flight = false;
        // Token records anchoring a resumed skeptic-0's marginal accounting
        // are in-memory only; a post-restart resume would re-count its full
        // prior cumulative as fresh spend. Cold-spawn instead.
        snapshot.skeptic0_session_id = None;
        // `verifier_id` is snapshot-controlled and embedded in paths later
        // fed to `remove_dir_all`; a non-canonical id (e.g. `/../`) could
        // escape the temp root. Enforce the pinned 12-hex form.
        if snapshot.verifier_id.len() != 12
            || !snapshot.verifier_id.chars().all(|c| c.is_ascii_hexdigit())
        {
            snapshot.verifier_id = generate_verifier_id();
        }
        // Best-effort like `create_goal`: scratch rarely survives a restart
        // but the skeptics expect it. Terminal snapshots had theirs removed
        // deliberately — don't resurrect.
        if !matches!(
            snapshot.status,
            GoalStatus::Complete | GoalStatus::BudgetLimited
        ) {
            // Subdirs only under a verified root — a squatted root must
            // not receive writes through `create_dir_all`. Recompute readiness
            // so a resumed prompt only claims the dir exists when it does.
            snapshot.scratch_dir_ready = match ensure_goal_scratch_root(&snapshot.verifier_id) {
                Ok(_) => {
                    std::fs::create_dir_all(implementer_scratch_dir(&snapshot.verifier_id)).is_ok()
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "goal restore: could not secure the scratch root; skipping scratch creation",
                    );
                    false
                }
            };
        } else {
            // The prompt must not claim a dir the terminal transition removed.
            snapshot.scratch_dir_ready = false;
        }
        let active_since = if snapshot.status == GoalStatus::Active {
            Some(Instant::now())
        } else {
            None
        };
        Self {
            orchestration: Some(snapshot),
            session_dir,
            active_since,
        }
    }

    pub fn snapshot(&self) -> Option<&GoalOrchestration> {
        self.orchestration.as_ref()
    }

    pub fn snapshot_mut(&mut self) -> Option<&mut GoalOrchestration> {
        self.orchestration.as_mut()
    }

    pub fn is_active(&self) -> bool {
        self.orchestration
            .as_ref()
            .is_some_and(|o| o.status == GoalStatus::Active)
    }

    pub fn phase(&self) -> Option<GoalPhase> {
        self.orchestration.as_ref().map(|o| o.phase)
    }

    pub fn status(&self) -> Option<GoalStatus> {
        self.orchestration.as_ref().map(|o| o.status)
    }

    pub fn current_subagent_id(&self) -> Option<&str> {
        self.orchestration
            .as_ref()
            .and_then(|o| o.current_subagent_id.as_deref())
    }

    pub fn objective(&self) -> Option<&str> {
        self.orchestration.as_ref().map(|o| o.objective.as_str())
    }

    pub fn token_budget(&self) -> Option<i64> {
        self.orchestration.as_ref().and_then(|o| o.token_budget)
    }

    fn goal_dir(&self) -> PathBuf {
        self.session_dir.join("goal")
    }

    /// Path to the goal's plan markdown (`<session_dir>/goal/plan.md`);
    /// may not exist yet.
    pub fn plan_path(&self) -> PathBuf {
        self.goal_dir().join("plan.md")
    }

    /// Path to the immutable baseline snapshot of the planner's original
    /// plan (`<session_dir>/goal/plan.baseline.md`); written once after
    /// the planner first produces `plan.md`. Sibling of [`Self::plan_path`].
    pub fn plan_baseline_path(&self) -> PathBuf {
        self.goal_dir().join("plan.baseline.md")
    }

    /// Path to the strategist's advisory note (`<session_dir>/goal/strategy.md`).
    /// The strategist writes here, NOT `plan.md`. Its `PlanGuard` snapshots
    /// `plan.md` and restores it byte-for-byte (and on cancellation),
    /// reverting ANY strategist edit. Whole-file restore is safe because the
    /// strategist runs synchronously as the sole writer (the goal turn is
    /// blocked awaiting it), so there's no concurrent implementer edit to
    /// clobber. Sibling of [`Self::plan_path`]; may not exist until the
    /// strategist first runs.
    pub fn strategy_path(&self) -> PathBuf {
        self.goal_dir().join("strategy.md")
    }

    /// Move the last classifier details file out of the scratch root
    /// (which the caller is about to remove) into the durable session
    /// goal dir and update `last_classifier_details_path`, so the
    /// "See <path>" surfaced by the achieved ack stays readable;
    /// per-skeptic reports are inlined below it
    /// ([`append_skeptic_reports`]) to keep it self-contained.
    ///
    /// The stored source path is snapshot-controlled: `..` components
    /// are rejected (`starts_with` is lexical), the source root must
    /// pass [`verify_owned_real_dir`] (a squatted root could stage an
    /// attacker file for the move), and the copy fallback is
    /// [`copy_no_follow`]. The path is stamped only after a move from
    /// the verified root succeeds; otherwise it is left unchanged.
    ///
    /// Runs under the tracker lock — bounded I/O (≤ the panel cap) on
    /// cold, one-shot transitions.
    fn rescue_classifier_details(&mut self) {
        let goal_dir = self.goal_dir();
        let Some(o) = self.orchestration.as_mut() else {
            return;
        };
        let Some(src) = o.last_classifier_details_path.as_deref() else {
            return;
        };
        let src = PathBuf::from(src);
        if src
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return;
        }
        // Only artifacts inside THIS goal's scratch root are at risk.
        let scratch_root = goal_scratch_root(&o.verifier_id);
        if !src.starts_with(&scratch_root) {
            return;
        }
        // A symlink-squatted root could stage an attacker file for the
        // rename below.
        if verify_owned_real_dir(&scratch_root).is_err() {
            return;
        }
        let Some(name) = src.file_name() else {
            return;
        };
        let dest = goal_dir.join(name);
        let _ = std::fs::create_dir_all(&goal_dir);
        if std::fs::rename(&src, &dest).is_ok() || copy_no_follow(&src, &dest).is_ok() {
            append_skeptic_reports(&scratch_root, &dest);
            o.last_classifier_details_path = Some(dest.to_string_lossy().into_owned());
        }
    }

    /// Create a new goal. Replaces any existing orchestration.
    ///
    /// `baseline_commit` is the `git rev-parse HEAD` captured at the
    /// call site (or `None` if not available). It is consumed by
    /// value — callers with `&str` should `.to_string()` at the call
    /// site rather than have the helper clone.
    pub fn create_goal(
        &mut self,
        goal_id: String,
        objective: String,
        token_budget: Option<i64>,
        token_baseline: i64,
        created_at: String,
        baseline_commit: Option<String>,
    ) {
        let _ = std::fs::create_dir_all(self.goal_dir());
        // Replacing a still-active goal: same rescue-then-remove contract
        // as the terminal transitions (the prior goal's details path may
        // already be in user-visible messages).
        if self.orchestration.is_some() {
            self.rescue_classifier_details();
            self.remove_scratch_root();
        }
        // Private per-goal scratch: the implementer dir is created up
        // front (the goal model writes throwaway artifacts here from its
        // first round); each `skeptic-<idx>` dir is created lazily when
        // that skeptic spawns. Best-effort — a creation failure degrades
        // to the model's own fallback, never blocks goal setup.
        let verifier_id = generate_verifier_id();
        // Subdirs only under a verified root (see `ensure_goal_scratch_root`).
        // Capture whether the implementer dir is truly on disk so the prompts
        // only claim "created for you" when it is.
        let scratch_dir_ready = match ensure_goal_scratch_root(&verifier_id) {
            Ok(_) => std::fs::create_dir_all(implementer_scratch_dir(&verifier_id)).is_ok(),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "goal create: could not secure the scratch root; skipping scratch creation",
                );
                false
            }
        };
        self.orchestration = Some(GoalOrchestration {
            goal_id,
            objective,
            status: GoalStatus::Active,
            phase: GoalPhase::Executing,
            token_budget,
            elapsed_ms: 0,
            created_at,
            current_subagent_id: None,
            current_subagent_role: None,
            total_worker_rounds: 0,
            total_verify_rounds: 0,
            budget_limit_reported: false,
            token_baseline,
            tokens_used_high_water: 0,
            parent_tokens_spent: 0,
            last_session_tokens_seen: Some(token_baseline),
            history: Vec::new(),
            pause_message: None,
            evaluator_blocker_key: None,
            evaluator_blocked_streak: 0,
            verifier_id,
            classifier_runs_attempted: 0,
            rounds_since_verify: 0,
            classifier_max_runs: None,
            last_classifier_verdict: None,
            last_classifier_details_path: None,
            last_classifier_at: None,
            last_classifier_gaps: None,
            first_final_response: None,
            skeptic0_session_id: None,
            skeptic_model_assignment: Vec::new(),
            last_gap_fingerprint: None,
            classifier_stall_count: 0,
            consecutive_not_achieved: 0,
            last_strategist_fired_at: 0,
            strategist_cap_bonus: 0,
            last_strategy_path: None,
            last_strategy_recommendation: None,
            changes_baseline_commit: baseline_commit,
            plan_file: None,
            plan_baseline_file: None,
            scratch_dir_ready,
            live_subagent_tokens: 0,
            live_tokens_by_model: Vec::new(),
            live_context_window: 0,
            live_context_pct: 0,
            live_turn_count: 0,
            live_tool_call_count: 0,
            planning_in_flight: false,
            verifying_in_flight: false,
        });
        self.active_since = Some(Instant::now());
        self.record_event(GoalEvent::GoalCreated, None);
    }

    pub fn set_phase(&mut self, phase: GoalPhase) {
        if let Some(o) = &mut self.orchestration {
            o.phase = phase;
        }
    }

    pub fn set_current_subagent(&mut self, id: Option<String>, role: Option<String>) {
        if let Some(o) = &mut self.orchestration {
            o.current_subagent_id = id;
            o.current_subagent_role = role;
        }
    }

    /// Pause the goal with a specific reason. Only transitions from `Active`.
    /// Flushes elapsed time and stops the timer. The `reason` selects one
    /// of the paused variants of [`GoalStatus`]. The `pause_message` field
    /// on the orchestration is NOT modified — to stash a human-readable
    /// reason alongside the pause, use [`Self::pause_with_message`].
    /// Returns `true` if the transition was applied.
    pub fn pause(&mut self, reason: GoalPauseReason) -> bool {
        self.pause_inner(reason, None)
    }

    /// Like [`Self::pause`] but also stores a human-readable `message` on
    /// [`GoalOrchestration::pause_message`]. Used by the `Verification`
    /// reason so the user-visible block reason survives until the next
    /// transition out of the paused state.
    /// Returns `true` if the transition was applied.
    pub fn pause_with_message(&mut self, reason: GoalPauseReason, message: String) -> bool {
        self.pause_inner(reason, Some(message))
    }

    fn pause_inner(&mut self, reason: GoalPauseReason, message: Option<String>) -> bool {
        let applied = if let Some(o) = &mut self.orchestration
            && o.status == GoalStatus::Active
        {
            if let Some(since) = self.active_since.take() {
                o.elapsed_ms = o
                    .elapsed_ms
                    .saturating_add(since.elapsed().as_millis() as u64);
            }
            o.status = reason.to_status();
            if message.is_some() {
                o.pause_message = message;
            }
            true
        } else {
            false
        };
        if applied {
            self.record_event(
                GoalEvent::GoalPaused,
                Some(reason.history_detail().to_owned()),
            );
        }
        applied
    }

    /// Resume a paused goal (any paused variant, including `Blocked`). Sets
    /// `active_since`, clears [`GoalOrchestration::pause_message`], and resets
    /// every per-attempt auto-pause counter — `classifier_runs_attempted`, the
    /// strategist streak/recommendation, and the gap-fingerprint stall streak —
    /// so a user re-arm starts fully fresh. Returns `true` if applied.
    pub fn resume(&mut self) -> bool {
        if let Some(o) = &mut self.orchestration
            && o.status.is_paused()
        {
            o.status = GoalStatus::Active;
            o.pause_message = None;
            o.classifier_runs_attempted = 0;
            o.rounds_since_verify = 0;
            o.reset_strategist_fields();
            o.reset_classifier_stall_fields();
            o.reset_evaluator_blocker_fields();
            self.active_since = Some(Instant::now());
            self.record_event(GoalEvent::GoalResumed, None);
            return true;
        }
        false
    }

    /// Mark the goal as complete. Accepts `Active` or any paused variant.
    /// Returns `true` if the transition was applied.
    pub fn complete(&mut self) -> bool {
        if let Some(o) = &mut self.orchestration
            && (o.status == GoalStatus::Active || o.status.is_paused())
        {
            if let Some(since) = self.active_since.take() {
                o.elapsed_ms = o
                    .elapsed_ms
                    .saturating_add(since.elapsed().as_millis() as u64);
            }
            o.status = GoalStatus::Complete;
            o.phase = GoalPhase::Idle;
            o.current_subagent_id = None;
            o.current_subagent_role = None;
            o.pause_message = None;
            // Drop the resumed reject-gatekeeper so any later goal starts
            // verification with a fresh, cold skeptic 0.
            o.skeptic0_session_id = None;
            // Sibling of skeptic 0: drop the frozen per-index model
            // assignment so a later goal re-resolves its own panel.
            o.skeptic_model_assignment.clear();
            // Drop the plan baseline alongside skeptic 0: a later goal
            // re-snapshots its own planner's original plan.
            o.plan_baseline_file = None;
            // Terminal transition: reset all strategist state so a
            // recreated/reactivated goal never inherits a stale count or note.
            o.reset_strategist_fields();
            o.reset_evaluator_blocker_fields();
            // The achieved ack points the user at the details file, so it
            // must outlive the scratch-root removal below.
            self.rescue_classifier_details();
            self.remove_scratch_root();
            self.record_event(GoalEvent::GoalCompleted, None);
            return true;
        }
        false
    }

    /// Mark the goal as budget-limited. Accepts `Active` or any paused variant.
    /// Returns `true` if the transition was applied.
    pub fn budget_limit(&mut self) -> bool {
        if let Some(o) = &mut self.orchestration
            && (o.status == GoalStatus::Active || o.status.is_paused())
        {
            if let Some(since) = self.active_since.take() {
                o.elapsed_ms = o
                    .elapsed_ms
                    .saturating_add(since.elapsed().as_millis() as u64);
            }
            o.status = GoalStatus::BudgetLimited;
            o.phase = GoalPhase::Idle;
            o.current_subagent_id = None;
            o.current_subagent_role = None;
            o.pause_message = None;
            // Symmetric with `complete`: drop the resumed reject-gatekeeper,
            // the frozen per-index model assignment, and the plan baseline on
            // every terminal goal-ending transition.
            o.skeptic0_session_id = None;
            o.skeptic_model_assignment.clear();
            o.plan_baseline_file = None;
            o.reset_strategist_fields();
            o.reset_evaluator_blocker_fields();
            // Symmetric with `complete`.
            self.rescue_classifier_details();
            self.remove_scratch_root();
            self.record_event(GoalEvent::BudgetExceeded, None);
            return true;
        }
        false
    }

    /// Clear the goal entirely (`GoalClear`). Dropping the whole
    /// orchestration also drops `plan_baseline_file` / `skeptic0_session_id`,
    /// so no per-field reset is needed here — but the on-disk scratch root
    /// outlives the struct, so rescue the surfaced details file and remove
    /// the root explicitly first (mirrors the `complete` / `budget_limit`
    /// cleanup).
    pub fn clear(&mut self) {
        self.rescue_classifier_details();
        self.remove_scratch_root();
        self.orchestration = None;
        self.active_since = None;
    }

    /// Best-effort scratch-root removal shared by every terminal
    /// transition; a distinct `verifier_id` per goal means this never
    /// touches a concurrent goal's dir.
    fn remove_scratch_root(&self) {
        if let Some(o) = &self.orchestration {
            let _ = std::fs::remove_dir_all(goal_scratch_root(&o.verifier_id));
        }
    }

    /// Overwrite the transient live-display fields from one subagent's
    /// progress tick. Single-slot, last-writer-wins by design: a display
    /// hint may flip between concurrent children; authoritative totals
    /// come from the token records.
    pub fn update_live_progress(
        &mut self,
        subagent_tokens: u64,
        tokens_by_model: Vec<(String, u64)>,
        context_window: u64,
        context_pct: u8,
        turn_count: u32,
        tool_call_count: u32,
    ) {
        if let Some(o) = &mut self.orchestration {
            o.live_subagent_tokens = subagent_tokens;
            o.live_tokens_by_model = tokens_by_model;
            o.live_context_window = context_window;
            o.live_context_pct = context_pct;
            o.live_turn_count = turn_count;
            o.live_tool_call_count = tool_call_count;
        }
    }

    /// Flush elapsed wall-clock time into `elapsed_ms`.
    pub fn account_elapsed(&mut self) {
        if let Some(o) = &mut self.orchestration
            && let Some(since) = self.active_since
        {
            o.elapsed_ms = o
                .elapsed_ms
                .saturating_add(since.elapsed().as_millis() as u64);
            self.active_since = Some(Instant::now());
        }
    }

    /// Record a `NotAchieved` rejection's gap `fingerprint` and report
    /// whether the goal has stalled — i.e. the same fingerprint has now
    /// appeared on enough consecutive rejections that the model changed
    /// nothing in the flagged gaps. The threshold is
    /// [`GOAL_CLASSIFIER_STALL_THRESHOLD`], relaxed to
    /// [`GOAL_STRATEGIST_STALL_THRESHOLD`] while a strategist restructure is
    /// in flight (cap bonus active). A fingerprint that differs from the
    /// previous one resets the streak to its first occurrence. No-op
    /// (returns `false`) without an orchestration.
    pub fn record_classifier_stall(&mut self, fingerprint: &str) -> bool {
        let Some(o) = self.orchestration.as_mut() else {
            return false;
        };
        if o.last_gap_fingerprint.as_deref() == Some(fingerprint) {
            o.classifier_stall_count = o.classifier_stall_count.saturating_add(1);
        } else {
            o.last_gap_fingerprint = Some(fingerprint.to_string());
            o.classifier_stall_count = 1;
        }
        let threshold = if o.strategist_cap_bonus > 0 {
            GOAL_STRATEGIST_STALL_THRESHOLD
        } else {
            GOAL_CLASSIFIER_STALL_THRESHOLD
        };
        o.classifier_stall_count >= threshold
    }

    pub fn record_evaluator_blocker(&mut self, blocker_key: &str) -> u32 {
        let Some(o) = self.orchestration.as_mut() else {
            return 0;
        };
        if o.evaluator_blocker_key.as_deref() == Some(blocker_key) {
            o.evaluator_blocked_streak = o.evaluator_blocked_streak.saturating_add(1);
        } else {
            o.evaluator_blocker_key = Some(blocker_key.to_owned());
            o.evaluator_blocked_streak = 1;
        }
        o.evaluator_blocked_streak
    }

    pub fn reset_evaluator_blocker(&mut self) {
        if let Some(o) = self.orchestration.as_mut() {
            o.reset_evaluator_blocker_fields();
        }
    }

    /// Undo the most recent attempt-slot reservation. Used when a
    /// rejection is routed to the `Blocked` outcome so it does not
    /// consume the retry budget the user gets back on resume.
    pub fn rollback_classifier_attempt(&mut self) {
        if let Some(o) = self.orchestration.as_mut() {
            o.classifier_runs_attempted = o.classifier_runs_attempted.saturating_sub(1);
        }
    }

    /// Clear the stall streak so the next rejection starts a fresh
    /// fingerprint comparison. Used on the `Blocked` route — a paused-for-
    /// user goal must not carry a half-built streak into its resume.
    pub fn reset_classifier_stall(&mut self) {
        if let Some(o) = self.orchestration.as_mut() {
            o.reset_classifier_stall_fields();
        }
    }

    /// Increment the consecutive-`NotAchieved` streak and return the new
    /// value. Drives the (skip-robust) strategist trigger. No-op (returns
    /// 0) without an orchestration.
    pub fn record_not_achieved_streak(&mut self) -> u32 {
        match self.orchestration.as_mut() {
            Some(o) => {
                o.consecutive_not_achieved = o.consecutive_not_achieved.saturating_add(1);
                o.consecutive_not_achieved
            }
            None => 0,
        }
    }

    /// Atomically evaluate the strategist trigger and claim a fire under a
    /// single lock: if `should_fire(consecutive_not_achieved,
    /// last_strategist_fired_at)` holds, mark the fire at the current streak
    /// (so the next fire needs another N failures) and return
    /// `Some(consecutive_not_achieved)`; otherwise leave state untouched and
    /// return `None`. Folding the check and the record into one critical
    /// section keeps them indivisible, so a streak can never double-fire the
    /// strategist. On fire it also grants the cap bonus and resets the
    /// gap-fingerprint stall streak so the restructure runs against a relaxed,
    /// freshly-measured stall window. No-op (`None`) without an orchestration.
    pub fn claim_strategist_fire(&mut self, should_fire: impl Fn(u32, u32) -> bool) -> Option<u32> {
        let o = self.orchestration.as_mut()?;
        if should_fire(o.consecutive_not_achieved, o.last_strategist_fired_at) {
            o.last_strategist_fired_at = o.consecutive_not_achieved;
            o.strategist_cap_bonus = GOAL_STRATEGIST_CAP_BONUS; // idempotent, not stacked
            o.reset_classifier_stall_fields();
            Some(o.consecutive_not_achieved)
        } else {
            None
        }
    }

    /// Revoke the cap bonus granted by [`Self::claim_strategist_fire`] when
    /// the strategist delivered no restructure. `last_strategist_fired_at`
    /// keeps the claim so the next fire still waits a full window.
    /// Deliberately conservative: the bonus is set, never stacked, so this
    /// also wipes an earlier successful fire's bonus — capping early beats
    /// running unearned rounds under a relaxed stall guard.
    pub fn revoke_strategist_cap_bonus(&mut self) {
        if let Some(o) = self.orchestration.as_mut() {
            o.strategist_cap_bonus = 0;
        }
    }

    /// Reset ALL strategist state (streak + last-fired marker + persisted
    /// recommendation). Called on an `Achieved` verdict (streak broken) and
    /// the `Blocked` route (paused for the user). Symmetric with the
    /// `complete`/`budget_limit`/`resume` resets so a paused/solved goal
    /// never replays a stale recommendation.
    pub fn reset_strategist_state(&mut self) {
        if let Some(o) = self.orchestration.as_mut() {
            o.reset_strategist_fields();
        }
    }

    /// Persist the strategist's latest output path + short recommendation
    /// so the continuation directive can inline them. No-op without an
    /// orchestration.
    pub fn record_strategy_recommendation(&mut self, path: String, recommendation: String) {
        if let Some(o) = self.orchestration.as_mut() {
            o.last_strategy_path = Some(path);
            o.last_strategy_recommendation = Some(recommendation);
        }
    }

    pub fn append_history(&mut self, entry: GoalHistoryEntry) {
        if let Some(o) = &mut self.orchestration {
            o.history.push(entry);
            // Bound the persisted timeline; drop oldest past the cap.
            let overflow = o.history.len().saturating_sub(GOAL_HISTORY_MAX);
            if overflow > 0 {
                o.history.drain(0..overflow);
            }
        }
    }

    /// Append a timestamped lifecycle history entry. The single chokepoint for
    /// every transition (create / pause / resume / complete / budget_limit) so
    /// each reaches `GoalUpdated.last_event` and no branch can forget to record.
    fn record_event(&mut self, event: GoalEvent, detail: Option<String>) {
        self.append_history(GoalHistoryEntry::now(event, detail));
    }
}

// Test helpers (shared across goal_tracker + goal_orchestrator tests)

#[cfg(test)]
pub(crate) fn make_base_orchestration() -> GoalOrchestration {
    GoalOrchestration {
        goal_id: "g-test".into(),
        objective: "test objective".into(),
        status: GoalStatus::Active,
        phase: GoalPhase::Idle,
        token_budget: None,
        elapsed_ms: 0,
        created_at: "2026-01-01T00:00:00Z".into(),
        current_subagent_id: None,
        current_subagent_role: None,
        total_worker_rounds: 0,
        total_verify_rounds: 0,
        budget_limit_reported: false,
        token_baseline: 0,
        tokens_used_high_water: 0,
        parent_tokens_spent: 0,
        last_session_tokens_seen: Some(0),
        history: Vec::new(),
        pause_message: None,
        evaluator_blocker_key: None,
        evaluator_blocked_streak: 0,
        verifier_id: generate_verifier_id(),
        classifier_runs_attempted: 0,
        rounds_since_verify: 0,
        classifier_max_runs: None,
        last_classifier_verdict: None,
        last_classifier_details_path: None,
        last_classifier_at: None,
        last_classifier_gaps: None,
        first_final_response: None,
        skeptic0_session_id: None,
        skeptic_model_assignment: Vec::new(),
        last_gap_fingerprint: None,
        classifier_stall_count: 0,
        consecutive_not_achieved: 0,
        last_strategist_fired_at: 0,
        strategist_cap_bonus: 0,
        last_strategy_path: None,
        last_strategy_recommendation: None,
        changes_baseline_commit: None,
        plan_file: None,
        plan_baseline_file: None,
        scratch_dir_ready: false,
        live_subagent_tokens: 0,
        live_tokens_by_model: Vec::new(),
        live_context_window: 0,
        live_context_pct: 0,
        live_turn_count: 0,
        live_tool_call_count: 0,
        planning_in_flight: false,
        verifying_in_flight: false,
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tracker() -> GoalTracker {
        GoalTracker::new(PathBuf::from("/tmp/test-goal-session"))
    }

    fn activate_tracker(t: &mut GoalTracker) {
        t.create_goal(
            "goal-1".into(),
            "Build a widget".into(),
            Some(100_000),
            0,
            "2026-01-01T00:00:00Z".into(),
            None,
        );
    }

    use super::make_base_orchestration;

    #[test]
    fn create_goal_activates_and_starts_timer() {
        let mut t = make_tracker();
        activate_tracker(&mut t);

        assert!(t.is_active());
        assert_eq!(t.phase(), Some(GoalPhase::Executing));
        assert_eq!(t.status(), Some(GoalStatus::Active));
        assert_eq!(t.objective(), Some("Build a widget"));
        assert_eq!(t.token_budget(), Some(100_000));
        assert!(t.active_since.is_some());
    }

    #[test]
    fn lifecycle_transitions_record_history_events() {
        // Created/paused/resumed/completed each append a history entry that
        // reaches `GoalUpdated.last_event`.
        use crate::session::goal_orchestrator::build_goal_updated;

        let mut t = make_tracker();
        activate_tracker(&mut t);
        assert!(
            matches!(
                t.snapshot().unwrap().history.last().map(|e| &e.event),
                Some(GoalEvent::GoalCreated)
            ),
            "create_goal must record GoalCreated"
        );

        assert!(t.pause(GoalPauseReason::User));
        {
            let last = t.snapshot().unwrap().history.last().unwrap();
            assert!(matches!(last.event, GoalEvent::GoalPaused));
            assert_eq!(
                last.detail.as_deref(),
                Some("user"),
                "pause records its cause as the history detail"
            );
        }

        assert!(t.resume());
        assert!(matches!(
            t.snapshot().unwrap().history.last().map(|e| &e.event),
            Some(GoalEvent::GoalResumed)
        ));

        assert!(t.complete());
        let o = t.snapshot().unwrap();
        assert!(matches!(
            o.history.last().map(|e| &e.event),
            Some(GoalEvent::GoalCompleted)
        ));
        // The latest event must surface on the wire as `last_event`.
        match build_goal_updated(o, 0, 0) {
            crate::extensions::notification::SessionUpdate::GoalUpdated { last_event, .. } => {
                assert_eq!(last_event.as_deref(), Some("goal_completed"));
            }
            other => panic!("expected GoalUpdated, got {other:?}"),
        }
    }

    #[test]
    fn premature_stop_event_maps_to_wire_string() {
        // PrematureStopDetected serializes to its wire label and carries the
        // pattern via `last_event_detail`.
        use crate::session::goal_orchestrator::build_goal_updated;

        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.append_history(GoalHistoryEntry {
            timestamp: "t".into(),
            event: GoalEvent::PrematureStopDetected,
            detail: Some("giving_up".into()),
            round: None,
            tokens_used: None,
            unmet: Vec::new(),
        });
        match build_goal_updated(t.snapshot().unwrap(), 0, 0) {
            crate::extensions::notification::SessionUpdate::GoalUpdated {
                last_event,
                last_event_detail,
                ..
            } => {
                assert_eq!(last_event.as_deref(), Some("premature_stop_detected"));
                assert_eq!(last_event_detail.as_deref(), Some("giving_up"));
            }
            other => panic!("expected GoalUpdated, got {other:?}"),
        }
    }

    #[test]
    fn append_history_caps_oldest_entries() {
        // The persisted timeline is bounded; the newest entry is retained.
        let mut t = make_tracker();
        activate_tracker(&mut t); // seeds 1 GoalCreated entry
        for i in 0..(GOAL_HISTORY_MAX + 10) {
            t.append_history(GoalHistoryEntry {
                timestamp: format!("t{i}"),
                event: GoalEvent::WorkerStarted,
                detail: None,
                round: None,
                tokens_used: None,
                unmet: Vec::new(),
            });
        }
        let o = t.snapshot().unwrap();
        assert_eq!(o.history.len(), GOAL_HISTORY_MAX, "history must be capped");
        assert_eq!(
            o.history.last().unwrap().timestamp,
            format!("t{}", GOAL_HISTORY_MAX + 10 - 1),
            "newest entry retained; oldest dropped"
        );
    }

    #[test]
    fn goal_event_unknown_string_deserializes_to_unknown() {
        // #[serde(other)] forward-compat: an unknown event from a newer shell
        // deserializes to `Unknown` instead of failing the field.
        let unknown: GoalEvent = serde_json::from_str("\"some_future_event\"").unwrap();
        assert!(matches!(unknown, GoalEvent::Unknown));
        let known: GoalEvent = serde_json::from_str("\"goal_paused\"").unwrap();
        assert!(matches!(known, GoalEvent::GoalPaused));
    }

    #[test]
    fn pause_records_cause_specific_history_detail() {
        // All six pause reasons record a distinct history `detail` (the
        // `history_detail` mapping, exercised via the real pause path).
        for (reason, expected) in [
            (GoalPauseReason::User, "user"),
            (GoalPauseReason::User, "user"),
            (GoalPauseReason::BackOff, "back_off"),
            (GoalPauseReason::NoProgress, "no_progress"),
            (GoalPauseReason::Verification, "blocked"),
            (GoalPauseReason::Infra, "infra"),
        ] {
            let mut t = make_tracker();
            activate_tracker(&mut t);
            assert!(t.pause(reason), "pause must apply for {reason:?}");
            let last = t.snapshot().unwrap().history.last().unwrap();
            assert!(matches!(last.event, GoalEvent::GoalPaused));
            assert_eq!(last.detail.as_deref(), Some(expected), "for {reason:?}");
        }
    }

    #[test]
    fn budget_limit_records_history_via_chokepoint() {
        // budget_limit records its own BudgetExceeded entry via the chokepoint,
        // like the other lifecycle transitions.
        let mut t = make_tracker();
        activate_tracker(&mut t);
        assert!(t.budget_limit());
        assert!(matches!(
            t.snapshot().unwrap().history.last().map(|e| &e.event),
            Some(GoalEvent::BudgetExceeded)
        ));
    }

    #[test]
    fn plan_path_is_session_scoped() {
        let t = GoalTracker::new(PathBuf::from("/tmp/plan-path-session-xyz"));
        assert_eq!(
            t.plan_path(),
            PathBuf::from("/tmp/plan-path-session-xyz/goal/plan.md"),
        );
    }

    #[test]
    fn create_goal_initialises_plan_file_to_none() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        assert!(t.snapshot().unwrap().plan_file.is_none());
    }

    #[test]
    fn plan_file_round_trips_through_serde_when_some() {
        let mut o = make_base_orchestration();
        let path = PathBuf::from("/tmp/plan-rt-session/goal/plan.md");
        o.plan_file = Some(path.clone());

        let json = serde_json::to_string(&o).unwrap();
        assert!(
            json.contains("\"plan_file\":\"/tmp/plan-rt-session/goal/plan.md\""),
            "plan_file must appear on the wire as a string: {json}",
        );

        let restored: GoalOrchestration = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.plan_file, Some(path));
    }

    #[test]
    fn plan_file_omitted_from_json_when_none() {
        let o = make_base_orchestration();
        assert!(o.plan_file.is_none());

        let json = serde_json::to_string(&o).unwrap();
        assert!(
            !json.contains("plan_file"),
            "plan_file=None must be skipped on serialize: {json}",
        );

        let restored: GoalOrchestration = serde_json::from_str(&json).unwrap();
        assert!(restored.plan_file.is_none());
    }

    /// A legacy snapshot written before this PR has no `plan_file`
    /// key; `#[serde(default)]` must backfill `None`.
    #[test]
    fn plan_file_backfills_none_on_legacy_snapshot() {
        const LEGACY: &str = r#"{
            "goal_id": "g-legacy",
            "objective": "legacy goal",
            "status": "active",
            "phase": "Idle",
            "token_budget": null,
            "elapsed_ms": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "current_subagent_id": null,
            "current_subagent_role": null,
            "total_worker_rounds": 0,
            "total_verify_rounds": 0,
            "token_baseline": 0,
            "history": [],
            "verifier_id": "abcdef012345"
        }"#;
        let loaded: GoalOrchestration =
            serde_json::from_str(LEGACY).expect("legacy snapshot must deserialize");
        assert!(loaded.plan_file.is_none());
    }

    /// `generate_verifier_id` must produce 12-char hex strings and
    /// fresh values on every call. The fixed length is part of the
    /// public contract — verifier file paths embed it verbatim, and
    /// drift here would silently invalidate the documented
    /// `grok-goal-<12 hex chars>` scratch-root format (and the
    /// 12-hex restore validation in `from_snapshot`).
    #[test]
    fn generate_verifier_id_is_short_hex_and_unique() {
        let a = generate_verifier_id();
        let b = generate_verifier_id();
        assert_eq!(a.len(), 12, "verifier id must be exactly 12 chars: {a}");
        assert_eq!(b.len(), 12, "verifier id must be exactly 12 chars: {b}");
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit()),
            "verifier id must be hex: {a}"
        );
        assert_ne!(a, b, "two successive ids must differ");
    }

    /// `create_goal` populates `verifier_id` and that id survives every
    /// pause/resume/complete transition unchanged. The id is the
    /// stable handle the model uses to read verdict files, so any
    /// mid-lifecycle reassignment would orphan the previously written
    /// verdicts.
    #[test]
    fn verifier_id_is_stable_across_pause_resume() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        let original = t.snapshot().unwrap().verifier_id.clone();
        assert_eq!(original.len(), 12);

        assert!(t.pause(GoalPauseReason::User));
        assert_eq!(t.snapshot().unwrap().verifier_id, original);

        assert!(t.resume());
        assert_eq!(t.snapshot().unwrap().verifier_id, original);

        assert!(t.pause_with_message(GoalPauseReason::Verification, "stuck on flaky test".into()));
        assert_eq!(t.snapshot().unwrap().verifier_id, original);

        assert!(t.resume());
        assert_eq!(t.snapshot().unwrap().verifier_id, original);

        assert!(t.complete());
        assert_eq!(t.snapshot().unwrap().verifier_id, original);
    }

    /// Serde round-trip must preserve `verifier_id`, and a legacy
    /// snapshot without the field must round-trip to a freshly
    /// generated id (not panic, not empty). This guards both the
    /// stability contract (existing goals keep their id after a
    /// shell restart) and the backwards-compat contract (pre-field
    /// snapshots load cleanly with a backfilled id).
    #[test]
    fn verifier_id_serde_roundtrip_and_backfill() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        let original_id = t.snapshot().unwrap().verifier_id.clone();

        let json = serde_json::to_string(t.snapshot().unwrap()).unwrap();
        assert!(
            json.contains(&format!("\"verifier_id\":\"{original_id}\"")),
            "serialized snapshot must include verifier_id: {json}"
        );
        let restored: GoalOrchestration = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.verifier_id, original_id);

        // Legacy snapshot — drop the verifier_id key entirely and
        // confirm serde backfills a fresh 12-char hex id.
        let legacy: serde_json::Value = {
            let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
            v.as_object_mut().unwrap().remove("verifier_id");
            v
        };
        let backfilled: GoalOrchestration =
            serde_json::from_value(legacy).expect("legacy snapshot must deserialize");
        assert_eq!(
            backfilled.verifier_id.len(),
            12,
            "backfilled id must be 12 chars: {}",
            backfilled.verifier_id
        );
        assert!(
            backfilled
                .verifier_id
                .chars()
                .all(|c| c.is_ascii_hexdigit()),
            "backfilled id must be hex: {}",
            backfilled.verifier_id
        );
    }

    #[test]
    fn record_classifier_stall_trips_on_two_consecutive_identical_fingerprints() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        assert!(
            !t.record_classifier_stall("fp-a"),
            "first occurrence of a fingerprint is not a stall"
        );
        assert!(
            t.record_classifier_stall("fp-a"),
            "the same fingerprint twice running trips the stall early-exit"
        );
        assert_eq!(t.snapshot().unwrap().classifier_stall_count, 2);
    }

    #[test]
    fn record_classifier_stall_resets_when_fingerprint_changes() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        assert!(!t.record_classifier_stall("fp-a"));
        assert!(t.record_classifier_stall("fp-a"));
        assert!(
            !t.record_classifier_stall("fp-b"),
            "a different fingerprint resets the streak to its first occurrence"
        );
        assert_eq!(t.snapshot().unwrap().classifier_stall_count, 1);
        assert!(
            t.record_classifier_stall("fp-b"),
            "the new fingerprint then trips on its own second occurrence"
        );
    }

    #[test]
    fn evaluator_blocker_streak_requires_one_stable_key() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        assert_eq!(t.record_evaluator_blocker("missing_github_access"), 1);
        assert_eq!(t.record_evaluator_blocker("missing_github_access"), 2);
        assert_eq!(t.record_evaluator_blocker("missing_slack_access"), 1);
        assert_eq!(t.record_evaluator_blocker("missing_slack_access"), 2);
        assert_eq!(t.record_evaluator_blocker("missing_slack_access"), 3);
    }

    #[test]
    fn evaluator_blocker_streak_is_persisted_and_resettable() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.record_evaluator_blocker("missing_access");
        t.record_evaluator_blocker("missing_access");
        let encoded = serde_json::to_string(t.snapshot().unwrap()).unwrap();
        let restored: GoalOrchestration = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            restored.evaluator_blocker_key.as_deref(),
            Some("missing_access")
        );
        assert_eq!(restored.evaluator_blocked_streak, 2);
        t.reset_evaluator_blocker();
        assert!(t.snapshot().unwrap().evaluator_blocker_key.is_none());
        assert_eq!(t.snapshot().unwrap().evaluator_blocked_streak, 0);
    }

    #[test]
    fn reset_classifier_stall_clears_streak_so_next_occurrence_is_first() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        // Build a streak, then reset it (as the Blocked route does).
        assert!(!t.record_classifier_stall("fp-a"));
        {
            let o = t.snapshot().unwrap();
            assert_eq!(o.classifier_stall_count, 1);
            assert_eq!(o.last_gap_fingerprint.as_deref(), Some("fp-a"));
        }
        t.reset_classifier_stall();
        {
            let o = t.snapshot().unwrap();
            assert_eq!(o.classifier_stall_count, 0);
            assert!(o.last_gap_fingerprint.is_none());
        }
        // The same fingerprint is now a fresh first occurrence — it must
        // NOT immediately trip the stall.
        assert!(
            !t.record_classifier_stall("fp-a"),
            "after reset, a repeat of the old fingerprint must not re-stall"
        );
    }

    /// Firing the strategist clears the gap-fingerprint stall streak so the
    /// restructure is measured from a clean slate.
    #[test]
    fn strategist_fire_resets_classifier_stall_streak() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        // Seed a partial stall streak (a pre-strategist gap repeat).
        assert!(!t.record_classifier_stall("fp-pre"));
        {
            let o = t.snapshot().unwrap();
            assert_eq!(o.classifier_stall_count, 1);
            assert_eq!(o.last_gap_fingerprint.as_deref(), Some("fp-pre"));
        }
        let _ = t.record_not_achieved_streak();
        assert!(t.claim_strategist_fire(|_, _| true).is_some());
        let o = t.snapshot().unwrap();
        assert_eq!(
            o.classifier_stall_count, 0,
            "fire must clear the stall count"
        );
        assert!(
            o.last_gap_fingerprint.is_none(),
            "fire must clear the stall fingerprint"
        );
    }

    /// While the strategist bonus is active, identical gap fingerprints get the
    /// relaxed stall window instead of tripping at the default threshold of 2,
    /// and the window stays bounded (still exits at its own threshold).
    #[test]
    fn relaxed_stall_threshold_applies_while_strategist_bonus_active() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        let _ = t.record_not_achieved_streak();
        assert!(t.claim_strategist_fire(|_, _| true).is_some());
        assert!(t.snapshot().unwrap().strategist_cap_bonus > 0);

        // Identical fingerprints below the relaxed threshold must NOT stall.
        for n in 1..GOAL_STRATEGIST_STALL_THRESHOLD {
            assert!(
                !t.record_classifier_stall("fp-loop"),
                "count {n} must not stall inside the relaxed window"
            );
        }
        // Bounded: the relaxed window still exits at its own threshold.
        assert!(
            t.record_classifier_stall("fp-loop"),
            "relaxed window trips at GOAL_STRATEGIST_STALL_THRESHOLD"
        );
    }

    /// Once strategist state resets (clearing the cap bonus) the stall
    /// threshold reverts to the strict default — the relaxation is scoped to
    /// the post-strategist window only, never weakening the guard outside it.
    #[test]
    fn relaxed_stall_threshold_reverts_after_strategist_reset() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        let _ = t.record_not_achieved_streak();
        assert!(t.claim_strategist_fire(|_, _| true).is_some());

        // Two identical fingerprints under the relaxed window: no stall yet.
        assert!(!t.record_classifier_stall("fp-x"));
        assert!(!t.record_classifier_stall("fp-x"));
        assert_eq!(t.snapshot().unwrap().classifier_stall_count, 2);

        // Clearing strategist state drops the bonus, restoring the strict threshold.
        t.reset_strategist_state();
        assert_eq!(t.snapshot().unwrap().strategist_cap_bonus, 0);
        assert!(
            t.record_classifier_stall("fp-x"),
            "after strategist reset the strict default threshold applies again"
        );
    }

    #[test]
    fn rollback_classifier_attempt_decrements_saturating() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.snapshot_mut().unwrap().classifier_runs_attempted = 2;
        t.rollback_classifier_attempt();
        assert_eq!(t.snapshot().unwrap().classifier_runs_attempted, 1);
        t.snapshot_mut().unwrap().classifier_runs_attempted = 0;
        t.rollback_classifier_attempt();
        assert_eq!(
            t.snapshot().unwrap().classifier_runs_attempted,
            0,
            "rollback must saturate at zero"
        );
    }

    #[test]
    fn full_lifecycle_create_to_complete() {
        let mut t = make_tracker();
        activate_tracker(&mut t);

        t.set_phase(GoalPhase::Executing);
        assert_eq!(t.phase(), Some(GoalPhase::Executing));

        t.set_current_subagent(Some("sub-1".into()), Some("worker".into()));
        assert_eq!(t.current_subagent_id(), Some("sub-1"));

        assert!(t.complete());
        assert_eq!(t.status(), Some(GoalStatus::Complete));
        assert_eq!(t.phase(), Some(GoalPhase::Idle));
        assert!(t.current_subagent_id().is_none());
        assert!(t.active_since.is_none());
    }

    #[test]
    fn pause_only_from_active() {
        let mut t = make_tracker();
        activate_tracker(&mut t);

        assert!(t.pause(GoalPauseReason::User));
        assert_eq!(t.status(), Some(GoalStatus::UserPaused));
        assert!(!t.is_active());

        assert!(t.resume());
        assert_eq!(t.status(), Some(GoalStatus::Active));
        assert!(t.is_active());
    }

    #[test]
    fn pause_from_complete_is_noop() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.complete();

        assert!(!t.pause(GoalPauseReason::User));
        assert_eq!(t.status(), Some(GoalStatus::Complete));
    }

    #[test]
    fn pause_from_budget_limited_is_noop() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.budget_limit();

        assert!(!t.pause(GoalPauseReason::User));
        assert_eq!(t.status(), Some(GoalStatus::BudgetLimited));
    }

    #[test]
    fn resume_only_from_paused_variants() {
        let mut t = make_tracker();
        activate_tracker(&mut t);

        assert!(!t.resume());
        assert_eq!(t.status(), Some(GoalStatus::Active));

        t.budget_limit();
        assert!(!t.resume());
        assert_eq!(t.status(), Some(GoalStatus::BudgetLimited));
    }

    #[test]
    fn resume_sets_active_since() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause(GoalPauseReason::User);
        assert!(t.active_since.is_none());

        t.resume();
        assert!(t.active_since.is_some());
    }

    #[test]
    fn resume_resets_classifier_runs_attempted() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        // A goal that hit the classifier cap and BackOff-paused.
        {
            let o = t.snapshot_mut().unwrap();
            o.classifier_runs_attempted = 4;
            o.classifier_max_runs = Some(4);
        }
        t.pause(GoalPauseReason::BackOff);
        assert_eq!(t.status(), Some(GoalStatus::BackOffPaused));

        assert!(t.resume());
        assert_eq!(t.status(), Some(GoalStatus::Active));
        // Counter reset for a fresh budget; cap untouched.
        assert_eq!(t.snapshot().unwrap().classifier_runs_attempted, 0);
        assert_eq!(t.snapshot().unwrap().classifier_max_runs, Some(4));
    }

    /// Resume re-arms for a fresh attempt, so it clears the stall streak too —
    /// the first post-resume round must not inherit a count that insta-pauses.
    #[test]
    fn resume_clears_classifier_stall_streak() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        // Build the streak up to the default stall threshold, then pause.
        assert!(!t.record_classifier_stall("fp-a"));
        assert!(t.record_classifier_stall("fp-a"));
        t.pause(GoalPauseReason::NoProgress);

        assert!(t.resume());
        {
            let o = t.snapshot().unwrap();
            assert_eq!(o.classifier_stall_count, 0);
            assert!(o.last_gap_fingerprint.is_none());
        }
        // A repeat of the same fingerprint now starts fresh, not at a carried count.
        assert!(!t.record_classifier_stall("fp-a"));
    }

    #[test]
    fn create_goal_initialises_strategist_streak_to_zero() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        assert_eq!(t.snapshot().unwrap().consecutive_not_achieved, 0);
    }

    #[test]
    fn record_not_achieved_streak_increments_and_returns_new_count() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        assert_eq!(t.record_not_achieved_streak(), 1);
        assert_eq!(t.record_not_achieved_streak(), 2);
        assert_eq!(t.record_not_achieved_streak(), 3);
        assert_eq!(t.snapshot().unwrap().consecutive_not_achieved, 3);
    }

    #[test]
    fn claim_strategist_fire_marks_current_streak() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        let _ = t.record_not_achieved_streak();
        let _ = t.record_not_achieved_streak();
        assert_eq!(t.claim_strategist_fire(|_, _| true), Some(2));
        let o = t.snapshot().unwrap();
        assert_eq!(
            (o.consecutive_not_achieved, o.last_strategist_fired_at),
            (2, 2)
        );
        let _ = t.record_not_achieved_streak();
        let o = t.snapshot().unwrap();
        assert_eq!(
            (o.consecutive_not_achieved, o.last_strategist_fired_at),
            (3, 2),
            "streak advances but last-fired stays until the next fire",
        );
    }

    #[test]
    fn claim_strategist_fire_skips_and_preserves_state_when_predicate_false() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        let _ = t.record_not_achieved_streak();
        assert_eq!(t.claim_strategist_fire(|_, _| false), None);
        let o = t.snapshot().unwrap();
        assert_eq!(o.last_strategist_fired_at, 0, "no fire => marker untouched");
        assert_eq!(o.strategist_cap_bonus, 0, "no fire => no cap bonus");
    }

    /// A strategist fire grants the cap bonus; reset clears it.
    #[test]
    fn strategist_fire_grants_cap_bonus_then_reset_clears_it() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        assert_eq!(t.snapshot().unwrap().strategist_cap_bonus, 0);
        let _ = t.record_not_achieved_streak();
        let _ = t.record_not_achieved_streak();
        let _ = t.claim_strategist_fire(|_, _| true);
        assert_eq!(
            t.snapshot().unwrap().strategist_cap_bonus,
            GOAL_STRATEGIST_CAP_BONUS,
        );
        t.reset_strategist_state();
        assert_eq!(t.snapshot().unwrap().strategist_cap_bonus, 0);
    }

    /// `reset_strategist_state` clears the streak, the last-fired marker,
    /// AND the persisted recommendation together — the single reset used by
    /// every branch so they can't drift.
    #[test]
    fn reset_strategist_state_clears_streak_marker_and_recommendation() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        let _ = t.record_not_achieved_streak();
        let _ = t.record_not_achieved_streak();
        let _ = t.claim_strategist_fire(|_, _| true);
        t.record_strategy_recommendation("/tmp/goal/strategy.md".into(), "split it".into());

        t.reset_strategist_state();

        let o = t.snapshot().unwrap();
        assert_eq!(o.consecutive_not_achieved, 0);
        assert_eq!(o.last_strategist_fired_at, 0);
        assert!(o.last_strategy_path.is_none());
        assert!(o.last_strategy_recommendation.is_none());
    }

    /// resume / complete / budget_limit must ALL clear the recommendation +
    /// streak + last-fired marker (symmetric with the reset, so a
    /// paused/terminal goal never replays a stale structural note).
    #[test]
    fn lifecycle_transitions_clear_all_strategist_state() {
        let seed = |t: &mut GoalTracker| {
            let _ = t.record_not_achieved_streak();
            let _ = t.claim_strategist_fire(|_, _| true);
            t.record_strategy_recommendation("/tmp/s.md".into(), "do X".into());
        };
        let assert_clean = |t: &GoalTracker| {
            let o = t.snapshot().unwrap();
            assert_eq!(o.consecutive_not_achieved, 0);
            assert_eq!(o.last_strategist_fired_at, 0);
            assert!(o.last_strategy_path.is_none());
            assert!(o.last_strategy_recommendation.is_none());
        };

        // resume
        let mut t = make_tracker();
        activate_tracker(&mut t);
        seed(&mut t);
        t.pause(GoalPauseReason::BackOff);
        assert!(t.resume());
        assert_clean(&t);

        // complete
        let mut t = make_tracker();
        activate_tracker(&mut t);
        seed(&mut t);
        assert!(t.complete());
        assert_clean(&t);

        // budget_limit
        let mut t = make_tracker();
        activate_tracker(&mut t);
        seed(&mut t);
        assert!(t.budget_limit());
        assert_clean(&t);
    }

    #[test]
    fn strategy_recommendation_record_round_trip() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.record_strategy_recommendation(
            "/tmp/goal/strategy.md".into(),
            "split the monolith".into(),
        );
        let o = t.snapshot().unwrap();
        assert_eq!(
            o.last_strategy_path.as_deref(),
            Some("/tmp/goal/strategy.md")
        );
        assert_eq!(
            o.last_strategy_recommendation.as_deref(),
            Some("split the monolith")
        );
    }

    /// Legacy snapshots predate `consecutive_not_achieved` /
    /// `last_strategy_*`; `#[serde(default)]` must backfill them so an
    /// upgrade doesn't fail to deserialize.
    #[test]
    fn strategist_fields_backfill_on_legacy_snapshot() {
        const LEGACY: &str = r#"{
            "goal_id": "g-legacy",
            "objective": "legacy goal",
            "status": "active",
            "phase": "Idle",
            "token_budget": null,
            "elapsed_ms": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "current_subagent_id": null,
            "current_subagent_role": null,
            "total_worker_rounds": 0,
            "total_verify_rounds": 0,
            "token_baseline": 0,
            "history": [],
            "verifier_id": "abcdef012345"
        }"#;
        let loaded: GoalOrchestration =
            serde_json::from_str(LEGACY).expect("legacy snapshot must deserialize");
        assert_eq!(loaded.consecutive_not_achieved, 0);
        assert_eq!(loaded.last_strategist_fired_at, 0);
        assert!(loaded.last_strategy_path.is_none());
        assert!(loaded.last_strategy_recommendation.is_none());
    }

    #[test]
    fn complete_from_active_succeeds() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.set_current_subagent(Some("sub-1".into()), Some("worker".into()));

        assert!(t.complete());
        assert_eq!(t.status(), Some(GoalStatus::Complete));
        assert!(t.current_subagent_id().is_none());
        assert!(t.snapshot().unwrap().current_subagent_role.is_none());
    }

    #[test]
    fn complete_from_paused_succeeds() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause(GoalPauseReason::User);

        assert!(t.complete());
        assert_eq!(t.status(), Some(GoalStatus::Complete));
    }

    #[test]
    fn complete_from_budget_limited_is_noop() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.budget_limit();

        assert!(!t.complete());
        assert_eq!(t.status(), Some(GoalStatus::BudgetLimited));
    }

    #[test]
    fn complete_from_complete_is_noop() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.complete();

        assert!(!t.complete());
        assert_eq!(t.status(), Some(GoalStatus::Complete));
    }

    #[test]
    fn budget_limit_from_active_succeeds() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.set_phase(GoalPhase::Executing);

        assert!(t.budget_limit());
        assert_eq!(t.status(), Some(GoalStatus::BudgetLimited));
        assert_eq!(t.phase(), Some(GoalPhase::Idle));
        assert!(t.current_subagent_id().is_none());
        assert!(t.active_since.is_none());
    }

    #[test]
    fn budget_limit_from_complete_is_noop() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.complete();

        assert!(!t.budget_limit());
        assert_eq!(t.status(), Some(GoalStatus::Complete));
    }

    #[test]
    fn pause_reason_maps_to_correct_status() {
        let mut t = make_tracker();
        activate_tracker(&mut t);

        assert!(t.pause(GoalPauseReason::User));
        assert_eq!(t.status(), Some(GoalStatus::UserPaused));

        t.resume();
        assert!(t.pause(GoalPauseReason::User));
        assert_eq!(t.status(), Some(GoalStatus::UserPaused));

        t.resume();
        assert!(t.pause(GoalPauseReason::BackOff));
        assert_eq!(t.status(), Some(GoalStatus::BackOffPaused));

        // Stall (NoProgress) maps to a status distinct from the cap (BackOff).
        t.resume();
        assert!(t.pause(GoalPauseReason::NoProgress));
        assert_eq!(t.status(), Some(GoalStatus::NoProgressPaused));

        t.resume();
        assert!(t.pause_with_message(GoalPauseReason::Infra, "Turn failed: rate limit".into()));
        assert_eq!(t.status(), Some(GoalStatus::InfraPaused));
    }

    #[test]
    fn is_paused_matches_all_paused_variants() {
        assert!(GoalStatus::UserPaused.is_paused());
        assert!(GoalStatus::UserPaused.is_paused());
        assert!(GoalStatus::BackOffPaused.is_paused());
        assert!(GoalStatus::NoProgressPaused.is_paused());
        assert!(GoalStatus::InfraPaused.is_paused());
        assert!(GoalStatus::Blocked.is_paused());
        assert!(!GoalStatus::Active.is_paused());
        assert!(!GoalStatus::Complete.is_paused());
        assert!(!GoalStatus::BudgetLimited.is_paused());
    }

    /// `NoProgressPaused` round-trips through both serde wire forms (snake_case
    /// `Serialize` + `from_wire_str`) and stays distinct from the cap pause's
    /// `back_off_paused`.
    #[test]
    fn no_progress_paused_round_trips_distinctly_from_back_off() {
        assert_eq!(
            GoalStatus::from_wire_str("no_progress_paused"),
            GoalStatus::NoProgressPaused
        );
        let json = serde_json::to_string(&GoalStatus::NoProgressPaused).unwrap();
        assert_eq!(json, "\"no_progress_paused\"");
        let back: GoalStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, GoalStatus::NoProgressPaused);
        // The cap pause keeps its own wire form — the two must not collapse.
        assert_eq!(
            GoalStatus::from_wire_str("back_off_paused"),
            GoalStatus::BackOffPaused
        );
        assert_ne!(GoalStatus::NoProgressPaused, GoalStatus::BackOffPaused);
    }

    #[test]
    fn resume_from_user_paused() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause(GoalPauseReason::User);
        assert!(t.resume());
        assert_eq!(t.status(), Some(GoalStatus::Active));
    }

    #[test]
    fn resume_from_back_off_paused() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause(GoalPauseReason::BackOff);
        assert!(t.resume());
        assert_eq!(t.status(), Some(GoalStatus::Active));
    }

    #[test]
    fn resume_from_infra_paused() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause_with_message(GoalPauseReason::Infra, "Turn failed: auth".into());
        assert_eq!(
            t.snapshot().and_then(|o| o.pause_message.clone()),
            Some("Turn failed: auth".into())
        );
        assert!(t.resume());
        assert_eq!(t.status(), Some(GoalStatus::Active));
        assert!(t.snapshot().unwrap().pause_message.is_none());
    }

    #[test]
    fn complete_from_back_off_paused() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause(GoalPauseReason::BackOff);
        assert!(t.complete());
        assert_eq!(t.status(), Some(GoalStatus::Complete));
    }

    #[test]
    fn budget_limit_from_user_paused() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause(GoalPauseReason::User);
        assert!(t.budget_limit());
        assert_eq!(t.status(), Some(GoalStatus::BudgetLimited));
    }

    #[test]
    fn budget_limit_from_back_off_paused() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause(GoalPauseReason::BackOff);
        assert!(t.budget_limit());
        assert_eq!(t.status(), Some(GoalStatus::BudgetLimited));
    }

    #[test]
    fn pause_with_verification_reason_transitions_to_blocked() {
        let mut t = make_tracker();
        activate_tracker(&mut t);

        assert!(t.pause(GoalPauseReason::Verification));
        assert_eq!(t.status(), Some(GoalStatus::Blocked));
        assert!(t.status().unwrap().is_paused());
    }

    #[test]
    fn pause_with_message_stores_message_and_transitions() {
        let mut t = make_tracker();
        activate_tracker(&mut t);

        assert!(t.pause_with_message(GoalPauseReason::Verification, "no windows sdk".into()));
        assert_eq!(t.status(), Some(GoalStatus::Blocked));
        assert_eq!(
            t.snapshot().unwrap().pause_message.as_deref(),
            Some("no windows sdk"),
        );
    }

    #[test]
    fn resume_clears_pause_message_and_returns_to_active() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause_with_message(GoalPauseReason::Verification, "X".into());
        assert_eq!(t.snapshot().unwrap().pause_message.as_deref(), Some("X"));

        assert!(t.resume());
        assert_eq!(t.status(), Some(GoalStatus::Active));
        assert!(t.snapshot().unwrap().pause_message.is_none());
    }

    #[test]
    fn pause_without_message_leaves_existing_pause_message_intact() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause_with_message(GoalPauseReason::Verification, "X".into());

        t.snapshot_mut().unwrap().status = GoalStatus::Active;
        t.active_since = Some(Instant::now());

        assert!(t.pause(GoalPauseReason::User));
        assert_eq!(t.snapshot().unwrap().pause_message.as_deref(), Some("X"));
    }

    #[test]
    fn pause_message_survives_serde_round_trip() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause_with_message(GoalPauseReason::Verification, "blocker".into());

        let json = serde_json::to_string(t.snapshot().unwrap()).unwrap();
        let restored: GoalOrchestration = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.status, GoalStatus::Blocked);
        assert_eq!(restored.pause_message.as_deref(), Some("blocker"));

        let mut t2 = make_tracker();
        activate_tracker(&mut t2);
        t2.pause_with_message(GoalPauseReason::Verification, "blocker".into());
        t2.resume();
        let json2 = serde_json::to_string(t2.snapshot().unwrap()).unwrap();
        let restored2: GoalOrchestration = serde_json::from_str(&json2).unwrap();
        assert!(restored2.pause_message.is_none());
    }

    #[test]
    fn resume_from_blocked_transitions_to_active() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause(GoalPauseReason::Verification);
        assert!(t.resume());
        assert_eq!(t.status(), Some(GoalStatus::Active));
    }

    #[test]
    fn complete_from_blocked_succeeds() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause(GoalPauseReason::Verification);
        assert!(t.complete());
        assert_eq!(t.status(), Some(GoalStatus::Complete));
    }

    #[test]
    fn budget_limit_from_blocked_succeeds() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause(GoalPauseReason::Verification);
        assert!(t.budget_limit());
        assert_eq!(t.status(), Some(GoalStatus::BudgetLimited));
    }

    #[test]
    fn pause_reason_verification_maps_to_blocked_status() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        assert!(t.pause(GoalPauseReason::Verification));
        assert_eq!(t.status(), Some(GoalStatus::Blocked));
    }

    #[test]
    fn complete_from_blocked_clears_pause_message() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause_with_message(GoalPauseReason::Verification, "X".into());
        assert_eq!(t.snapshot().unwrap().pause_message.as_deref(), Some("X"));

        assert!(t.complete());
        assert_eq!(t.status(), Some(GoalStatus::Complete));
        assert!(t.snapshot().unwrap().pause_message.is_none());
    }

    #[test]
    fn budget_limit_from_blocked_clears_pause_message() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.pause_with_message(GoalPauseReason::Verification, "Y".into());
        assert_eq!(t.snapshot().unwrap().pause_message.as_deref(), Some("Y"));

        assert!(t.budget_limit());
        assert_eq!(t.status(), Some(GoalStatus::BudgetLimited));
        assert!(t.snapshot().unwrap().pause_message.is_none());
    }

    #[test]
    fn complete_flushes_elapsed_before_clearing_timer() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        let before = t.snapshot().unwrap().elapsed_ms;
        t.complete();
        assert!(t.snapshot().unwrap().elapsed_ms >= before);
        assert!(t.active_since.is_none());
    }

    #[test]
    fn budget_limit_flushes_elapsed_before_clearing_timer() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        let before = t.snapshot().unwrap().elapsed_ms;
        t.budget_limit();
        assert!(t.snapshot().unwrap().elapsed_ms >= before);
        assert!(t.active_since.is_none());
    }

    #[test]
    fn clear_removes_orchestration() {
        let mut t = make_tracker();
        activate_tracker(&mut t);

        t.clear();
        assert!(t.snapshot().is_none());
        assert!(!t.is_active());
        assert!(t.active_since.is_none());
    }

    #[test]
    fn serde_round_trip_preserves_data() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.set_phase(GoalPhase::Executing);

        let original = t.snapshot().unwrap().clone();
        let json = serde_json::to_string(&original).unwrap();
        let restored: GoalOrchestration = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.goal_id, original.goal_id);
        assert_eq!(restored.objective, original.objective);
        assert_eq!(restored.status, original.status);
        assert_eq!(restored.phase, original.phase);
    }

    #[test]
    fn serde_skip_fields_reset_on_deserialize() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        t.update_live_progress(
            100,
            vec![("grok-4".to_owned(), 60), ("grok-3".to_owned(), 40)],
            200_000,
            50,
            3,
            10,
        );
        // The transient field is set on the live snapshot...
        assert_eq!(t.snapshot().unwrap().live_tokens_by_model.len(), 2);

        let json = serde_json::to_string(t.snapshot().unwrap()).unwrap();
        let restored: GoalOrchestration = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.live_subagent_tokens, 0);
        // ...but `#[serde(skip)]` drops it on round-trip.
        assert!(restored.live_tokens_by_model.is_empty());
        assert_eq!(restored.live_context_window, 0);
        assert_eq!(restored.live_context_pct, 0);
        assert_eq!(restored.live_turn_count, 0);
        assert_eq!(restored.live_tool_call_count, 0);
        assert!(!restored.budget_limit_reported);
    }

    #[test]
    fn legacy_pascal_case_paused_deserializes_to_user_paused() {
        let legacy = r#""Paused""#;
        let parsed: GoalStatus = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed, GoalStatus::UserPaused);
    }

    #[test]
    fn legacy_pascal_case_other_variants_deserialize() {
        for (legacy, expected) in [
            (r#""Active""#, GoalStatus::Active),
            (r#""BudgetLimited""#, GoalStatus::BudgetLimited),
            (r#""Complete""#, GoalStatus::Complete),
        ] {
            let parsed: GoalStatus = serde_json::from_str(legacy).unwrap();
            assert_eq!(parsed, expected, "legacy {legacy} must parse");
        }
    }

    #[test]
    fn legacy_infra_paused_deserializes() {
        let parsed: GoalStatus = serde_json::from_str(r#""infra_paused""#).unwrap();
        assert_eq!(parsed, GoalStatus::InfraPaused);
    }

    #[test]
    fn unknown_future_paused_status_deserializes_to_user_paused() {
        let parsed: GoalStatus = serde_json::from_str(r#""error_paused""#).unwrap();
        assert_eq!(parsed, GoalStatus::UserPaused);
    }

    /// Any unknown wire status — not just `*_paused` forms — must restore
    /// as a resumable paused goal, never an Active self-driving one.
    #[test]
    fn unknown_non_paused_status_deserializes_to_user_paused_not_active() {
        for wire in [r#""quarantined""#, r#""v9_super_active""#, r#""""#] {
            let parsed: GoalStatus = serde_json::from_str(wire).unwrap();
            assert_eq!(parsed, GoalStatus::UserPaused, "wire {wire}");
        }
        assert_eq!(
            GoalStatus::from_wire_str("not-a-status"),
            GoalStatus::UserPaused,
        );
    }

    #[test]
    fn serde_round_trip_infra_paused_with_pause_message() {
        let mut o = make_base_orchestration();
        o.status = GoalStatus::InfraPaused;
        o.pause_message = Some("Turn failed: rate limit".into());
        let json = serde_json::to_string(&o).unwrap();
        let restored: GoalOrchestration = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.status, GoalStatus::InfraPaused);
        assert_eq!(
            restored.pause_message.as_deref(),
            Some("Turn failed: rate limit")
        );
    }

    #[test]
    fn from_snapshot_resets_planning_to_idle_paused() {
        let mut orchestration = make_base_orchestration();
        orchestration.phase = GoalPhase::Planning;
        orchestration.current_subagent_id = Some("sub-old".into());
        orchestration.current_subagent_role = Some("planner".into());

        let t = GoalTracker::from_snapshot(PathBuf::from("/tmp/test"), orchestration);
        let o = t.snapshot().unwrap();
        assert_eq!(o.phase, GoalPhase::Idle);
        assert_eq!(o.status, GoalStatus::UserPaused);
        assert!(o.current_subagent_id.is_none());
        assert!(o.current_subagent_role.is_none());
        assert!(t.active_since.is_none());
    }

    #[test]
    fn from_snapshot_resets_executing() {
        let mut orchestration = make_base_orchestration();
        orchestration.phase = GoalPhase::Executing;

        let t = GoalTracker::from_snapshot(PathBuf::from("/tmp"), orchestration);
        assert_eq!(t.snapshot().unwrap().phase, GoalPhase::Idle);
        assert_eq!(t.snapshot().unwrap().status, GoalStatus::UserPaused);
    }

    #[test]
    fn from_snapshot_preserves_infra_paused_on_restart() {
        let mut orchestration = make_base_orchestration();
        orchestration.phase = GoalPhase::Executing;
        orchestration.status = GoalStatus::InfraPaused;
        orchestration.pause_message = Some("Turn failed: upstream unavailable".into());

        let t = GoalTracker::from_snapshot(PathBuf::from("/tmp"), orchestration);
        let o = t.snapshot().unwrap();
        assert_eq!(o.phase, GoalPhase::Idle);
        assert_eq!(o.status, GoalStatus::InfraPaused);
        assert_eq!(
            o.pause_message.as_deref(),
            Some("Turn failed: upstream unavailable")
        );
    }

    /// Guards that `from_snapshot` preserves `plan_file` across reloads.
    #[test]
    fn from_snapshot_preserves_plan_file_some() {
        let mut orchestration = make_base_orchestration();
        let path = PathBuf::from("/tmp/from-snapshot-session/goal/plan.md");
        orchestration.plan_file = Some(path.clone());

        let t = GoalTracker::from_snapshot(PathBuf::from("/tmp"), orchestration);
        assert_eq!(t.snapshot().unwrap().plan_file, Some(path));
    }

    /// The temp-dir scratch doesn't survive a restart; restore must
    /// recreate it so the next panel's skeptics can read claimed outputs.
    #[test]
    fn from_snapshot_recreates_implementer_scratch_dir() {
        let orchestration = make_base_orchestration();
        let scratch = implementer_scratch_dir(&orchestration.verifier_id);
        assert!(!scratch.exists(), "fresh verifier_id ⇒ no dir yet");

        let t = GoalTracker::from_snapshot(PathBuf::from("/tmp"), orchestration);
        assert!(
            scratch.is_dir(),
            "restore must recreate {} like create_goal does",
            scratch.display(),
        );
        assert!(
            t.snapshot().unwrap().scratch_dir_ready,
            "recreating the dir must mark scratch_dir_ready true",
        );
        let _ = std::fs::remove_dir_all(goal_scratch_root(&t.snapshot().unwrap().verifier_id));
    }

    /// A terminal snapshot's scratch root was deliberately removed by its
    /// transition; restore must not resurrect it.
    #[test]
    fn from_snapshot_skips_scratch_recreation_for_terminal_status() {
        for status in [GoalStatus::Complete, GoalStatus::BudgetLimited] {
            let mut orchestration = make_base_orchestration();
            orchestration.status = status;
            let scratch = implementer_scratch_dir(&orchestration.verifier_id);

            let t = GoalTracker::from_snapshot(PathBuf::from("/tmp"), orchestration);
            assert!(
                !scratch.exists(),
                "terminal status {status:?} must not recreate {}",
                scratch.display(),
            );
            assert!(
                !t.snapshot().unwrap().scratch_dir_ready,
                "terminal status {status:?} must leave scratch_dir_ready false",
            );
        }
    }

    /// Anything off the pinned 12-hex `verifier_id` form must be
    /// regenerated, never used as a path component.
    #[test]
    fn from_snapshot_regenerates_non_canonical_verifier_id() {
        for bad in ["../../../escape", "abcd", "abcdef01234Z", ""] {
            let mut orchestration = make_base_orchestration();
            orchestration.verifier_id = bad.to_string();

            let t = GoalTracker::from_snapshot(PathBuf::from("/tmp"), orchestration);
            let vid = &t.snapshot().unwrap().verifier_id;
            assert_ne!(vid, bad, "non-canonical id must be replaced");
            assert_eq!(vid.len(), 12);
            assert!(vid.chars().all(|c| c.is_ascii_hexdigit()));
            let _ = std::fs::remove_dir_all(goal_scratch_root(vid));
        }
        // A canonical id is kept verbatim.
        let orchestration = make_base_orchestration();
        let original = orchestration.verifier_id.clone();
        let t = GoalTracker::from_snapshot(PathBuf::from("/tmp"), orchestration);
        assert_eq!(t.snapshot().unwrap().verifier_id, original);
        let _ = std::fs::remove_dir_all(goal_scratch_root(&original));
    }

    /// Full raw-JSON snapshot path: an unknown wire status must come back
    /// from `from_snapshot` as a resumable `UserPaused` goal, never Active.
    #[test]
    fn from_snapshot_raw_json_unknown_status_restores_user_paused() {
        const FORWARD_VERSION: &str = r#"{
            "goal_id": "g-forward",
            "objective": "from a newer shell",
            "status": "hyper_active_v9",
            "phase": "Idle",
            "token_budget": null,
            "elapsed_ms": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "current_subagent_id": null,
            "current_subagent_role": null,
            "total_worker_rounds": 0,
            "total_verify_rounds": 0,
            "token_baseline": 0,
            "history": [],
            "verifier_id": "abcdef012345"
        }"#;
        let snapshot: GoalOrchestration =
            serde_json::from_str(FORWARD_VERSION).expect("forward-version snapshot must parse");
        let t = GoalTracker::from_snapshot(PathBuf::from("/tmp"), snapshot);
        assert_eq!(
            t.status(),
            Some(GoalStatus::UserPaused),
            "unknown status must restore paused, not self-driving",
        );
        assert!(t.active_since.is_none());
        let _ = std::fs::remove_dir_all(goal_scratch_root(&t.snapshot().unwrap().verifier_id));
    }

    /// Restoring the id would over-count a resumed skeptic-0's prior
    /// cumulative as fresh spend (token records are in-memory only).
    #[test]
    fn from_snapshot_drops_skeptic0_session_id() {
        let mut orchestration = make_base_orchestration();
        orchestration.skeptic0_session_id = Some("child-from-before-restart".into());

        let t = GoalTracker::from_snapshot(PathBuf::from("/tmp"), orchestration);
        assert!(
            t.snapshot().unwrap().skeptic0_session_id.is_none(),
            "restore must cold-spawn the next panel",
        );
        let _ = std::fs::remove_dir_all(goal_scratch_root(&t.snapshot().unwrap().verifier_id));
    }

    /// A failed strategist run revokes the cap bonus while keeping the
    /// fire claimed, so the next fire still waits a full window.
    #[test]
    fn revoke_strategist_cap_bonus_keeps_fire_claim() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        let _ = t.record_not_achieved_streak();
        assert!(t.claim_strategist_fire(|_, _| true).is_some());
        assert_eq!(
            t.snapshot().unwrap().strategist_cap_bonus,
            GOAL_STRATEGIST_CAP_BONUS,
        );

        t.revoke_strategist_cap_bonus();
        let o = t.snapshot().unwrap();
        assert_eq!(o.strategist_cap_bonus, 0, "failed fire must revoke bonus");
        assert_eq!(
            o.last_strategist_fired_at, 1,
            "the claim survives so the trigger does not retry every round",
        );
    }

    #[test]
    fn from_snapshot_idle_active_restores_paused() {
        let orchestration = make_base_orchestration(); // Idle + Active
        let t = GoalTracker::from_snapshot(PathBuf::from("/tmp"), orchestration);
        assert_eq!(t.snapshot().unwrap().status, GoalStatus::UserPaused);
        assert!(t.active_since.is_none());
    }

    #[test]
    fn from_snapshot_idle_complete_no_active_since() {
        let mut orchestration = make_base_orchestration();
        orchestration.status = GoalStatus::Complete;

        let t = GoalTracker::from_snapshot(PathBuf::from("/tmp"), orchestration);
        assert_eq!(t.snapshot().unwrap().status, GoalStatus::Complete);
        assert!(t.active_since.is_none());
    }

    #[test]
    fn from_snapshot_restore_then_resume_tracks_elapsed() {
        let mut orchestration = make_base_orchestration();
        orchestration.phase = GoalPhase::Executing;
        orchestration.elapsed_ms = 5000;

        let mut t = GoalTracker::from_snapshot(PathBuf::from("/tmp"), orchestration);
        assert_eq!(t.status(), Some(GoalStatus::UserPaused));
        assert!(t.active_since.is_none());

        assert!(t.resume());
        assert!(t.active_since.is_some());

        let before = t.snapshot().unwrap().elapsed_ms;
        assert_eq!(before, 5000);
        t.account_elapsed();
        assert!(t.snapshot().unwrap().elapsed_ms >= 5000);
    }

    /// A snapshot written by an older shell will omit all nine
    /// classifier fields (incl. the stall `last_gap_fingerprint` /
    /// `classifier_stall_count` and the persisted `last_classifier_gaps`
    /// summary added later). Deserialization must
    /// succeed and every new field must come back at its
    /// `#[serde(default)]` value. The
    /// JSON literal below doubles as a documented v0 schema contract
    /// — if a future PR breaks legacy load, this test fails *because*
    /// the documented shape no longer parses, not because the in-code
    /// serialization shape happened to drift in sync.
    #[test]
    fn classifier_fields_backwards_compat_defaults_on_legacy_snapshot() {
        const LEGACY_SNAPSHOT_JSON: &str = r#"{
            "goal_id": "g-legacy",
            "objective": "legacy goal",
            "status": "active",
            "phase": "Executing",
            "token_budget": null,
            "tokens_used": 0,
            "elapsed_ms": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "current_subagent_id": null,
            "current_subagent_role": null,
            "total_worker_rounds": 0,
            "total_verify_rounds": 0,
            "token_baseline": 0,
            "finished_subagent_tokens": 0,
            "history": [],
            "verifier_id": "abcdef012345"
        }"#;
        let legacy: GoalOrchestration =
            serde_json::from_str(LEGACY_SNAPSHOT_JSON).expect("legacy snapshot must deserialize");
        assert_eq!(legacy.goal_id, "g-legacy");
        assert_eq!(legacy.verifier_id, "abcdef012345");
        assert_eq!(legacy.classifier_runs_attempted, 0);
        assert!(legacy.classifier_max_runs.is_none());
        assert!(legacy.last_classifier_verdict.is_none());
        assert!(legacy.last_classifier_details_path.is_none());
        assert!(legacy.last_classifier_at.is_none());
        assert!(legacy.last_classifier_gaps.is_none());
        assert!(legacy.skeptic0_session_id.is_none());
        assert!(legacy.changes_baseline_commit.is_none());
        assert!(legacy.last_gap_fingerprint.is_none());
        assert_eq!(legacy.classifier_stall_count, 0);
        // New transient `#[serde(skip)]` field defaults empty for a
        // legacy snapshot that predates it.
        assert!(legacy.live_tokens_by_model.is_empty());
        // Spend-accumulator backfill: zero spent, `None` anchor so the
        // first `goal_tokens` call seeds from `token_baseline`.
        assert_eq!(legacy.parent_tokens_spent, 0);
        assert_eq!(legacy.last_session_tokens_seen, None);
    }

    /// Legacy on-disk snapshots carry the dropped `tokens_used` and
    /// `finished_subagent_tokens` fields. Verify they are silently
    /// ignored on load and the struct deserializes cleanly.
    #[test]
    fn goal_orchestration_serde_drops_legacy_tokens_used_field() {
        const LEGACY: &str = r#"{
            "goal_id": "g-old",
            "objective": "legacy",
            "status": "active",
            "phase": "Idle",
            "token_budget": null,
            "tokens_used": 123,
            "elapsed_ms": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "current_subagent_id": null,
            "current_subagent_role": null,
            "total_worker_rounds": 0,
            "total_verify_rounds": 0,
            "token_baseline": 0,
            "finished_subagent_tokens": 456,
            "history": [],
            "verifier_id": "abcdef012345"
        }"#;
        let loaded: GoalOrchestration =
            serde_json::from_str(LEGACY).expect("legacy snapshot must deserialize");
        assert_eq!(loaded.goal_id, "g-old");
        assert_eq!(loaded.token_baseline, 0);
        // Round-trip and verify the dropped keys do not reappear at
        // the top level; `tokens_used` on history entries (Option<i64>)
        // is a distinct field and `history` is empty here so the
        // simple `contains` check is sound.
        let round = serde_json::to_string(&loaded).unwrap();
        assert!(
            !round.contains("\"tokens_used\""),
            "legacy field re-serialized: {round}"
        );
        assert!(
            !round.contains("\"finished_subagent_tokens\""),
            "legacy field re-serialized: {round}"
        );
    }

    /// Populating every new field and round-tripping through serde
    /// must return identical values. This guards both the on-disk
    /// schema shape and the `Eq`-by-field semantics the verification
    /// stage orchestrator relies on.
    #[test]
    fn classifier_fields_serde_round_trip_preserves_all_fields() {
        let mut o = make_base_orchestration();
        o.classifier_runs_attempted = 2;
        o.classifier_max_runs = Some(3);
        o.last_classifier_verdict = Some(GoalClassifierVerdict::NotAchieved);
        o.last_classifier_details_path = Some("/tmp/goal-classifier-abc.md".to_string());
        o.last_classifier_at = Some("2026-05-24T12:00:00Z".to_string());
        o.last_classifier_gaps = Some("- [skeptic 0, high] still on fire".to_string());
        o.skeptic0_session_id = Some("0190abcd-skeptic0".to_string());
        o.changes_baseline_commit = Some("abc123def456".to_string());

        let json = serde_json::to_string(&o).unwrap();
        let restored: GoalOrchestration = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.classifier_runs_attempted, 2);
        assert_eq!(restored.classifier_max_runs, Some(3));
        assert_eq!(
            restored.last_classifier_verdict,
            Some(GoalClassifierVerdict::NotAchieved)
        );
        assert_eq!(
            restored.last_classifier_details_path.as_deref(),
            Some("/tmp/goal-classifier-abc.md")
        );
        assert_eq!(
            restored.last_classifier_at.as_deref(),
            Some("2026-05-24T12:00:00Z")
        );
        assert_eq!(
            restored.last_classifier_gaps.as_deref(),
            Some("- [skeptic 0, high] still on fire")
        );
        assert_eq!(
            restored.skeptic0_session_id.as_deref(),
            Some("0190abcd-skeptic0")
        );
        assert_eq!(
            restored.changes_baseline_commit.as_deref(),
            Some("abc123def456")
        );

        // Contract: `Option::None` fields must NOT serialize as
        // `"key": null`. Verify the round-trip JSON omits the keys when
        // we reset them to None on a freshly-created orchestration.
        let mut base = make_base_orchestration();
        base.classifier_max_runs = None;
        base.last_classifier_verdict = None;
        base.last_classifier_details_path = None;
        base.last_classifier_at = None;
        base.last_classifier_gaps = None;
        base.skeptic0_session_id = None;
        base.changes_baseline_commit = None;
        let json_none = serde_json::to_string(&base).unwrap();
        for key in [
            "classifier_max_runs",
            "last_classifier_verdict",
            "last_classifier_details_path",
            "last_classifier_at",
            "last_classifier_gaps",
            "skeptic0_session_id",
            "changes_baseline_commit",
        ] {
            assert!(
                !json_none.contains(key),
                "None-valued field {key} must be skipped on serialize, got: {json_none}"
            );
        }
    }

    /// `skeptic0_session_id` round-trips through serde when populated
    /// (it must persist across a snapshot save/restore within a session
    /// so the next attempt can resume skeptic 0). Mirrors the
    /// `last_classifier_gaps` round-trip contract.
    #[test]
    fn skeptic0_session_id_round_trips_through_serde() {
        let mut o = make_base_orchestration();
        o.skeptic0_session_id = Some("0190-skeptic0-child".to_string());
        let json = serde_json::to_string(&o).unwrap();
        let restored: GoalOrchestration = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.skeptic0_session_id.as_deref(),
            Some("0190-skeptic0-child"),
        );
    }

    /// Round-trips through serde so a resumed goal keeps its breadth anchor,
    /// and is skipped on serialize when `None`.
    #[test]
    fn first_final_response_round_trips_through_serde() {
        let mut o = make_base_orchestration();
        o.first_final_response = Some("Round 1: built the whole feature; 14 tests pass.".into());
        let json = serde_json::to_string(&o).unwrap();
        let restored: GoalOrchestration = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.first_final_response.as_deref(),
            Some("Round 1: built the whole feature; 14 tests pass."),
        );

        let base = make_base_orchestration();
        let json_none = serde_json::to_string(&base).unwrap();
        assert!(
            !json_none.contains("first_final_response"),
            "None-valued first_final_response must be skipped on serialize: {json_none}",
        );
    }

    /// Every terminal goal-ending transition (`complete`, `budget_limit`)
    /// clears the resumed skeptic-0 id so a later goal starts verification
    /// with a fresh, cold skeptic 0.
    #[test]
    fn terminal_transitions_clear_skeptic0_session_id() {
        for ending in ["complete", "budget_limit"] {
            let mut t = make_tracker();
            activate_tracker(&mut t);
            t.snapshot_mut().unwrap().skeptic0_session_id = Some("prior-child".to_string());
            let applied = match ending {
                "complete" => t.complete(),
                _ => t.budget_limit(),
            };
            assert!(applied, "{ending} transition must apply");
            assert!(
                t.snapshot().unwrap().skeptic0_session_id.is_none(),
                "{ending}() must clear skeptic0_session_id",
            );
        }
    }

    /// `skeptic_model_assignment` (the frozen per-index pool) round-trips
    /// through serde so the assignment survives a snapshot save/restore and
    /// stays stable across resumes.
    #[test]
    fn skeptic_model_assignment_round_trips_through_serde() {
        let mut o = make_base_orchestration();
        o.skeptic_model_assignment = vec![
            crate::util::config::GoalRoleModel {
                model: "grok-4".to_string(),
                agent_type: "general-purpose".to_string(),
            },
            crate::util::config::GoalRoleModel {
                model: "grok-4.5".to_string(),
                agent_type: "cursor".to_string(),
            },
        ];
        let json = serde_json::to_string(&o).unwrap();
        let restored: GoalOrchestration = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.skeptic_model_assignment,
            o.skeptic_model_assignment
        );
    }

    /// A legacy snapshot (no `skeptic_model_assignment` key) deserializes to
    /// an empty assignment (serde default) — all skeptics inherit.
    #[test]
    fn legacy_snapshot_without_skeptic_model_assignment_deserializes_empty() {
        const LEGACY: &str = r#"{
            "goal_id": "g-legacy",
            "objective": "legacy goal",
            "status": "active",
            "phase": "Idle",
            "token_budget": null,
            "elapsed_ms": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "current_subagent_id": null,
            "current_subagent_role": null,
            "total_worker_rounds": 0,
            "total_verify_rounds": 0,
            "token_baseline": 0,
            "history": [],
            "verifier_id": "abcdef012345"
        }"#;
        let restored: GoalOrchestration =
            serde_json::from_str(LEGACY).expect("legacy snapshot must deserialize");
        assert!(restored.skeptic_model_assignment.is_empty());
    }

    /// Sibling of `skeptic0_session_id`: every terminal goal-ending
    /// transition (`complete`, `budget_limit`) clears the frozen per-index
    /// model assignment so a later goal re-resolves its own panel.
    #[test]
    fn terminal_transitions_clear_skeptic_model_assignment() {
        for ending in ["complete", "budget_limit"] {
            let mut t = make_tracker();
            activate_tracker(&mut t);
            t.snapshot_mut().unwrap().skeptic_model_assignment =
                vec![crate::util::config::GoalRoleModel {
                    model: "grok-4".to_string(),
                    agent_type: "general-purpose".to_string(),
                }];
            let applied = match ending {
                "complete" => t.complete(),
                _ => t.budget_limit(),
            };
            assert!(applied, "{ending} transition must apply");
            assert!(
                t.snapshot().unwrap().skeptic_model_assignment.is_empty(),
                "{ending}() must clear skeptic_model_assignment",
            );
        }
    }

    /// `plan_baseline_file` is a sibling of `skeptic0_session_id` and must
    /// be cleared by the SAME terminal transitions so a later goal
    /// re-snapshots its own planner's original plan.
    #[test]
    fn terminal_transitions_clear_plan_baseline_file() {
        for ending in ["complete", "budget_limit"] {
            let mut t = make_tracker();
            activate_tracker(&mut t);
            t.snapshot_mut().unwrap().plan_baseline_file =
                Some(PathBuf::from("/tmp/sess/goal/plan.baseline.md"));
            let applied = match ending {
                "complete" => t.complete(),
                _ => t.budget_limit(),
            };
            assert!(applied, "{ending} transition must apply");
            assert!(
                t.snapshot().unwrap().plan_baseline_file.is_none(),
                "{ending}() must clear plan_baseline_file",
            );
        }
    }

    /// The scratch path helpers derive the pinned layout from
    /// `temp_dir()` + `verifier_id`: a `grok-goal-<vid>` root with an
    /// `implementer/` subdir and per-index `skeptic-<idx>/` subdirs.
    #[test]
    fn scratch_path_helpers_derive_pinned_layout() {
        let root = goal_scratch_root("vid123");
        assert_eq!(root, std::env::temp_dir().join("grok-goal-vid123"));
        assert_eq!(
            implementer_scratch_dir("vid123"),
            std::env::temp_dir()
                .join("grok-goal-vid123")
                .join("implementer"),
        );
        assert_eq!(
            skeptic_scratch_dir("vid123", 2),
            std::env::temp_dir()
                .join("grok-goal-vid123")
                .join("skeptic-2"),
        );
    }

    /// Two skeptics in the SAME goal get DISTINCT scratch dirs so their
    /// re-runs never overwrite each other (the intra-goal screenshot race).
    #[test]
    fn distinct_skeptics_get_distinct_scratch_dirs() {
        assert_ne!(skeptic_scratch_dir("vid", 0), skeptic_scratch_dir("vid", 1),);
        // ...and neither equals the implementer dir.
        assert_ne!(
            skeptic_scratch_dir("vid", 0),
            implementer_scratch_dir("vid")
        );
    }

    /// `create_goal` creates the scratch root + implementer dir up front.
    #[test]
    fn create_goal_creates_implementer_scratch_dir() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        let vid = t.snapshot().unwrap().verifier_id.clone();
        let dir = implementer_scratch_dir(&vid);
        assert!(dir.is_dir(), "implementer scratch dir must exist: {dir:?}");
        assert!(
            t.snapshot().unwrap().scratch_dir_ready,
            "create_goal must mark scratch_dir_ready true when the dir was created",
        );
        // Clean up the real on-disk dir this test created.
        let _ = std::fs::remove_dir_all(goal_scratch_root(&vid));
    }

    /// The root is 0700 from the instant it exists (atomic-mode create,
    /// no chmod window) and a re-ensure re-pins the mode.
    #[cfg(unix)]
    #[test]
    fn ensure_goal_scratch_root_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let vid = generate_verifier_id();
        let root = ensure_goal_scratch_root(&vid).unwrap();
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "scratch root must be 0700: {root:?}");
        // Idempotent re-ensure keeps (re-applies) the mode.
        let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755));
        let _ = ensure_goal_scratch_root(&vid).unwrap();
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "re-ensure must restore 0700: {root:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A pre-planted symlink root is rejected: never followed, the
    /// victim dir never chmodded, nothing created behind it.
    #[cfg(unix)]
    #[test]
    fn ensure_goal_scratch_root_rejects_preplanted_symlink_root() {
        use std::os::unix::fs::PermissionsExt;
        let vid = generate_verifier_id();
        let victim = tempfile::tempdir().unwrap();
        let victim_mode_before = std::fs::metadata(victim.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let root = goal_scratch_root(&vid);
        std::os::unix::fs::symlink(victim.path(), &root).unwrap();

        let result = ensure_goal_scratch_root(&vid);
        let _ = std::fs::remove_file(&root);

        assert!(result.is_err(), "a symlinked root must be rejected");
        let victim_mode_after = std::fs::metadata(victim.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            victim_mode_before, victim_mode_after,
            "the victim dir must not be chmodded through the symlink",
        );
        assert_eq!(
            std::fs::read_dir(victim.path()).unwrap().count(),
            0,
            "nothing may be created behind the symlink",
        );
    }

    /// A non-directory squat (regular file at the root path) is also
    /// rejected rather than written around.
    #[cfg(unix)]
    #[test]
    fn ensure_goal_scratch_root_rejects_file_squat() {
        let vid = generate_verifier_id();
        let root = goal_scratch_root(&vid);
        std::fs::write(&root, b"squat").unwrap();
        let result = ensure_goal_scratch_root(&vid);
        let _ = std::fs::remove_file(&root);
        assert!(result.is_err(), "a file squat must be rejected");
    }

    /// Starting a new goal over a still-active one removes the prior goal's
    /// scratch root (no orphan), while creating the new one.
    #[test]
    fn create_goal_over_active_removes_prior_scratch_root() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        let old_vid = t.snapshot().unwrap().verifier_id.clone();
        let old_root = goal_scratch_root(&old_vid);
        assert!(old_root.is_dir(), "prior scratch root must exist");

        // Re-create over the active goal without an intervening terminal
        // transition.
        activate_tracker(&mut t);
        let new_vid = t.snapshot().unwrap().verifier_id.clone();
        assert_ne!(old_vid, new_vid, "a fresh goal gets a fresh verifier_id");
        assert!(
            !old_root.exists(),
            "prior scratch root must be removed on create-over-active: {old_root:?}",
        );
        assert!(
            implementer_scratch_dir(&new_vid).is_dir(),
            "the new goal's implementer dir must exist",
        );
        let _ = std::fs::remove_dir_all(goal_scratch_root(&new_vid));
    }

    /// All three terminal transitions rescue the details file (the
    /// achieved ack surfaces its path AFTER `complete()`): pins the
    /// rescued location, the updated stored path, and the inlined
    /// per-skeptic reports (numeric order; see `append_skeptic_reports`).
    #[test]
    fn terminal_transitions_rescue_classifier_details_file() {
        for ending in ["complete", "budget_limit", "clear"] {
            let session = tempfile::tempdir().unwrap();
            let mut t = GoalTracker::new(session.path().to_path_buf());
            activate_tracker(&mut t);
            let vid = t.snapshot().unwrap().verifier_id.clone();
            let root = goal_scratch_root(&vid);
            let details = root.join("goal-classifier-x-1.md");
            std::fs::write(&details, "panel verdict body").unwrap();
            std::fs::write(
                root.join("goal-classifier-x-1-skeptic-0.md"),
                "SKEPTIC_ZERO_BODY",
            )
            .unwrap();
            std::fs::write(
                root.join("goal-classifier-x-1-skeptic-1.md"),
                "SKEPTIC_ONE_BODY",
            )
            .unwrap();
            t.snapshot_mut().unwrap().last_classifier_details_path =
                Some(details.to_string_lossy().into_owned());

            match ending {
                "complete" => assert!(t.complete()),
                "budget_limit" => assert!(t.budget_limit()),
                _ => t.clear(),
            }

            let rescued = session.path().join("goal").join("goal-classifier-x-1.md");
            let body = std::fs::read_to_string(&rescued).unwrap_or_else(|e| {
                panic!("{ending}: details must survive the scratch-root removal: {e}")
            });
            assert!(
                body.starts_with("panel verdict body"),
                "{ending}: canonical body leads the rescued file:\n{body}",
            );
            let s0 = body
                .find("SKEPTIC_ZERO_BODY")
                .unwrap_or_else(|| panic!("{ending}: skeptic-0 report inlined:\n{body}"));
            let s1 = body
                .find("SKEPTIC_ONE_BODY")
                .unwrap_or_else(|| panic!("{ending}: skeptic-1 report inlined:\n{body}"));
            assert!(s0 < s1, "{ending}: reports inlined in name order:\n{body}");
            assert!(
                body.contains("## Inlined skeptic report: goal-classifier-x-1-skeptic-0.md"),
                "{ending}: each inlined report carries a per-file header:\n{body}",
            );
            assert!(
                !goal_scratch_root(&vid).exists(),
                "{ending}: scratch root still removed",
            );
            if ending != "clear" {
                assert_eq!(
                    t.snapshot()
                        .unwrap()
                        .last_classifier_details_path
                        .as_deref(),
                    Some(rescued.to_string_lossy().as_ref()),
                    "{ending}: stored path must point at the rescued copy",
                );
            }
        }
    }

    /// Replacing a still-active goal (`create_goal` over-active) is the
    /// 4th root-removal site and must rescue like its terminal siblings.
    #[test]
    fn create_goal_over_active_rescues_prior_details_file() {
        let session = tempfile::tempdir().unwrap();
        let mut t = GoalTracker::new(session.path().to_path_buf());
        activate_tracker(&mut t);
        let old_vid = t.snapshot().unwrap().verifier_id.clone();
        let details = goal_scratch_root(&old_vid).join("goal-classifier-x-1.md");
        std::fs::write(&details, "prior goal verdict").unwrap();
        t.snapshot_mut().unwrap().last_classifier_details_path =
            Some(details.to_string_lossy().into_owned());

        activate_tracker(&mut t); // replace without a terminal transition

        let rescued = session.path().join("goal").join("goal-classifier-x-1.md");
        assert_eq!(
            std::fs::read_to_string(&rescued).unwrap(),
            "prior goal verdict",
            "replace path must rescue the prior goal's details file",
        );
        assert!(!goal_scratch_root(&old_vid).exists());
        let _ = std::fs::remove_dir_all(goal_scratch_root(&t.snapshot().unwrap().verifier_id));
    }

    /// Source guards: a non-scratch stored path is a no-op, and a
    /// lexically-inside `..`-escaping path is rejected unread.
    #[test]
    fn rescue_classifier_details_guards_source_path() {
        // Non-scratch path → no-op.
        let session = tempfile::tempdir().unwrap();
        let mut t = GoalTracker::new(session.path().to_path_buf());
        activate_tracker(&mut t);
        let elsewhere = tempfile::tempdir().unwrap();
        let outside = elsewhere.path().join("user-notes.md");
        std::fs::write(&outside, "not ours").unwrap();
        let outside_str = outside.to_string_lossy().into_owned();
        t.snapshot_mut().unwrap().last_classifier_details_path = Some(outside_str.clone());
        assert!(t.complete());
        assert!(outside.exists(), "non-scratch source must not be moved");
        assert_eq!(
            t.snapshot()
                .unwrap()
                .last_classifier_details_path
                .as_deref(),
            Some(outside_str.as_str()),
            "stored path must stay untouched on the no-op guard",
        );

        // `..`-escaping path (lexically under the root) → rejected.
        let session = tempfile::tempdir().unwrap();
        let mut t = GoalTracker::new(session.path().to_path_buf());
        activate_tracker(&mut t);
        let vid = t.snapshot().unwrap().verifier_id.clone();
        let victim = std::env::temp_dir().join(format!("goal-rescue-victim-{vid}.md"));
        std::fs::write(&victim, "victim body").unwrap();
        let traversal = goal_scratch_root(&vid)
            .join("..")
            .join(victim.file_name().unwrap());
        let traversal_str = traversal.to_string_lossy().into_owned();
        t.snapshot_mut().unwrap().last_classifier_details_path = Some(traversal_str.clone());
        assert!(t.complete());
        assert!(
            victim.exists(),
            "a `..`-escaping source must never be moved",
        );
        assert_eq!(
            t.snapshot()
                .unwrap()
                .last_classifier_details_path
                .as_deref(),
            Some(traversal_str.as_str()),
            "stored path must stay untouched on the traversal guard",
        );
        let _ = std::fs::remove_file(&victim);
    }

    /// A symlink-squatted root fails the rescue: the attacker-staged
    /// file is neither moved into the goal dir nor stamped.
    #[cfg(unix)]
    #[test]
    fn rescue_classifier_details_rejects_symlink_squatted_root() {
        let session = tempfile::tempdir().unwrap();
        let mut t = GoalTracker::new(session.path().to_path_buf());
        activate_tracker(&mut t);
        let vid = t.snapshot().unwrap().verifier_id.clone();
        let root = goal_scratch_root(&vid);
        std::fs::remove_dir_all(&root).unwrap();
        let attacker = tempfile::tempdir().unwrap();
        std::fs::write(
            attacker.path().join("goal-classifier-x-1.md"),
            "ATTACKER BODY",
        )
        .unwrap();
        std::os::unix::fs::symlink(attacker.path(), &root).unwrap();
        let staged = root.join("goal-classifier-x-1.md");
        let staged_str = staged.to_string_lossy().into_owned();
        t.snapshot_mut().unwrap().last_classifier_details_path = Some(staged_str.clone());

        assert!(t.complete());
        let _ = std::fs::remove_file(&root);

        assert!(
            !session
                .path()
                .join("goal")
                .join("goal-classifier-x-1.md")
                .exists(),
            "an attacker-staged file must not be blessed into the goal dir",
        );
        assert_eq!(
            t.snapshot()
                .unwrap()
                .last_classifier_details_path
                .as_deref(),
            Some(staged_str.as_str()),
            "stored path must stay unstamped on a squatted root",
        );
        assert_eq!(
            std::fs::read_to_string(attacker.path().join("goal-classifier-x-1.md")).unwrap(),
            "ATTACKER BODY",
            "the staged file must be untouched",
        );
    }

    /// Numeric latest-attempt-first inlining: budget elision drops the
    /// stale attempt, never the final one (lexical order — `-1-` before
    /// `-10-` — would invert that).
    #[test]
    fn append_skeptic_reports_prefers_latest_attempt_numerically() {
        let scratch = tempfile::tempdir().unwrap();
        let dest = scratch.path().join("canonical.md");
        std::fs::write(&dest, "canonical body").unwrap();
        // Stale attempt 1: alone larger than the whole budget.
        std::fs::write(
            scratch.path().join("goal-classifier-v-1-skeptic-0.md"),
            "s".repeat(crate::session::goal_classifier::GOAL_VERIFIER_PANEL_MAX_BYTES + 1),
        )
        .unwrap();
        // Final attempt 10: two small reports.
        std::fs::write(
            scratch.path().join("goal-classifier-v-10-skeptic-0.md"),
            "LATEST_S0_BODY",
        )
        .unwrap();
        std::fs::write(
            scratch.path().join("goal-classifier-v-10-skeptic-1.md"),
            "LATEST_S1_BODY",
        )
        .unwrap();

        append_skeptic_reports(scratch.path(), &dest);

        let body = std::fs::read_to_string(&dest).unwrap();
        let s0 = body
            .find("LATEST_S0_BODY")
            .expect("final attempt's skeptic-0 report inlined");
        let s1 = body
            .find("LATEST_S1_BODY")
            .expect("final attempt's skeptic-1 report inlined");
        assert!(s0 < s1, "skeptics ascend within an attempt:\n{body}");
        assert!(
            body.contains("(remaining skeptic reports elided"),
            "the oversized stale report must be elided:\n{body}",
        );
        assert!(
            !body.contains("ssss"),
            "the stale attempt-1 body must not crowd out the final attempt:\n{body}",
        );
    }

    /// `copy_no_follow` must refuse a pre-planted destination symlink
    /// (O_EXCL) instead of writing the rescued content through it.
    #[cfg(unix)]
    #[test]
    fn copy_no_follow_refuses_destination_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.md");
        std::fs::write(&src, "rescued body").unwrap();
        let victim = dir.path().join("victim.md");
        std::fs::write(&victim, "precious").unwrap();
        let dest = dir.path().join("dest.md");
        std::os::unix::fs::symlink(&victim, &dest).unwrap();

        assert!(
            copy_no_follow(&src, &dest).is_err(),
            "an existing destination (symlink) must fail the copy",
        );
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "precious",
            "the symlink's victim must be untouched",
        );
    }

    /// Every terminal goal-ending transition (`complete`, `budget_limit`,
    /// `clear`) removes the private scratch root from disk — mirrors
    /// `terminal_transitions_clear_plan_baseline_file`.
    #[test]
    fn terminal_transitions_remove_scratch_root() {
        for ending in ["complete", "budget_limit", "clear"] {
            let mut t = make_tracker();
            activate_tracker(&mut t);
            let vid = t.snapshot().unwrap().verifier_id.clone();
            let root = goal_scratch_root(&vid);
            // create_goal already made implementer/; assert the precondition.
            assert!(
                root.is_dir(),
                "{ending}: scratch root must exist pre-transition"
            );
            match ending {
                "complete" => {
                    assert!(t.complete());
                }
                "budget_limit" => {
                    assert!(t.budget_limit());
                }
                _ => t.clear(),
            }
            assert!(
                !root.exists(),
                "{ending}() must remove the scratch root {root:?}",
            );
        }
    }

    #[test]
    fn plan_baseline_path_is_session_scoped_sibling_of_plan() {
        let t = GoalTracker::new(PathBuf::from("/tmp/plan-baseline-session-xyz"));
        assert_eq!(
            t.plan_baseline_path(),
            PathBuf::from("/tmp/plan-baseline-session-xyz/goal/plan.baseline.md"),
        );
    }

    #[test]
    fn create_goal_initialises_plan_baseline_file_to_none() {
        let mut t = make_tracker();
        activate_tracker(&mut t);
        assert!(t.snapshot().unwrap().plan_baseline_file.is_none());
    }

    /// `plan_baseline_file` round-trips through serde when populated (it
    /// must persist across a snapshot save/restore so the baseline survives
    /// a shell restart), and is omitted from the wire when `None`.
    #[test]
    fn plan_baseline_file_round_trips_through_serde() {
        let mut o = make_base_orchestration();
        o.plan_baseline_file = Some(PathBuf::from("/tmp/sess/goal/plan.baseline.md"));
        let json = serde_json::to_string(&o).unwrap();
        assert!(
            json.contains("\"plan_baseline_file\":\"/tmp/sess/goal/plan.baseline.md\""),
            "plan_baseline_file must appear on the wire: {json}",
        );
        let restored: GoalOrchestration = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.plan_baseline_file.as_deref(),
            Some(std::path::Path::new("/tmp/sess/goal/plan.baseline.md")),
        );

        let none = make_base_orchestration();
        let json_none = serde_json::to_string(&none).unwrap();
        assert!(
            !json_none.contains("plan_baseline_file"),
            "None-valued plan_baseline_file must be skipped on serialize: {json_none}",
        );
        let restored_none: GoalOrchestration = serde_json::from_str(&json_none).unwrap();
        assert!(restored_none.plan_baseline_file.is_none());
    }

    /// The verdict enum must serialize in snake_case so it stays
    /// consistent with `GoalStatus` and the rest of the goal-tracker
    /// enums. Both variants are asserted explicitly — no wildcard.
    #[test]
    fn classifier_verdict_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&GoalClassifierVerdict::Achieved).unwrap(),
            "\"achieved\""
        );
        assert_eq!(
            serde_json::to_string(&GoalClassifierVerdict::NotAchieved).unwrap(),
            "\"not_achieved\""
        );
        // Symmetric: deserialization accepts the same shape.
        let a: GoalClassifierVerdict = serde_json::from_str("\"achieved\"").unwrap();
        let n: GoalClassifierVerdict = serde_json::from_str("\"not_achieved\"").unwrap();
        assert_eq!(a, GoalClassifierVerdict::Achieved);
        assert_eq!(n, GoalClassifierVerdict::NotAchieved);
    }

    /// `create_goal` must thread `baseline_commit` straight into
    /// `changes_baseline_commit` on the new orchestration record.
    /// Passing `None` must leave the field `None`.
    #[test]
    fn create_goal_plumbs_baseline_commit_into_orchestration() {
        let mut t = make_tracker();
        t.create_goal(
            "goal-bc".into(),
            "obj".into(),
            None,
            0,
            "2026-01-01T00:00:00Z".into(),
            Some("deadbeef".to_string()),
        );
        assert_eq!(
            t.snapshot().unwrap().changes_baseline_commit.as_deref(),
            Some("deadbeef")
        );

        let mut t2 = make_tracker();
        t2.create_goal(
            "goal-none".into(),
            "obj".into(),
            None,
            0,
            "2026-01-01T00:00:00Z".into(),
            None,
        );
        assert!(t2.snapshot().unwrap().changes_baseline_commit.is_none());
    }
}
