use async_trait::async_trait;

/// Input supplied when the host starts a turn.
pub struct TurnStartInput {
    /// True when the harness produced the turn (auto-wake, drain, cron, continuation), not the user.
    pub synthetic: bool,
}

impl TurnStartInput {
    pub fn new(synthetic: bool) -> Self {
        TurnStartInput { synthetic }
    }
}

/// Input supplied when the host completes a turn.
pub struct TurnDoneInput;

/// Why the host aborted the turn instead of completing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnAbortReason {
    /// The client went away mid-turn.
    Disconnected,
    /// The user interrupted the turn before it completed.
    Interrupted,
}

/// Input supplied when the host aborts a turn.
pub struct TurnAbortInput {
    pub reason: TurnAbortReason,
}

impl TurnAbortInput {
    pub fn new(reason: TurnAbortReason) -> Self {
        TurnAbortInput { reason }
    }
}

/// Input supplied when the host observes an error for a turn.
pub struct TurnErrorInput<'a> {
    pub message: &'a str,
}

#[async_trait]
pub trait TurnLifecycleContributor: Send + Sync {
    async fn on_turn_start(&self, _input: &TurnStartInput) {}

    async fn on_turn_done(&self, _input: &TurnDoneInput) {}

    async fn on_turn_abort(&self, _input: &TurnAbortInput) {}

    async fn on_turn_error(&self, _input: &TurnErrorInput<'_>) {}
}
