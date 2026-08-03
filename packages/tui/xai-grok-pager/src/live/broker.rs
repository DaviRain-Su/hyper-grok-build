//! `LiveDelegationBroker` — observes accepted, non-replay ACP ingress by
//! exact `SessionId`+`prompt_id` and correlates it with registered Live
//! delegations to:
//!
//! - accumulate `AgentMessageChunk` text per delegation,
//! - flush the accumulated assistant segment as 500-byte-safe commentary at
//!   tool boundaries,
//! - emit the terminal assistant segment wrapped as `"Agent Final Message":`
//!   and send `CompleteDelegation` exactly once,
//! - unify `TurnCompleted`, `PromptResponse` errors, and cancel/failure
//!   ordering with an idempotent terminal state,
//! - ignore foreign prompts, replay, old generations, and inactive unrelated
//!   agents,
//! - never send raw tool output / secrets,
//! - never block ACP handlers (all sends are `try_send`).
//!
//! The broker is a pure data structure — it does not touch the ACP stream
//! directly. The ACP handler calls into it with observed events, and the broker
//! returns actions (commentary to flush, final message to send) that the
//! caller executes via the `cmd_tx` channel.

use super::prompts;
use super::state::{DelegationEntry, Generation};
use super::{LiveCommand, LiveContextChannel};

/// The agent id type.
use super::state::AgentId;

/// Maximum byte length for a single commentary chunk (500-byte-safe, matching
/// the voice core's `CONTEXT_CHUNK_BYTES`).
const COMMENTARY_CHUNK_MAX: usize = 500;

/// A commentary segment to flush as `AppendDelegationContext`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentaryFlush {
    pub delegation_id: String,
    pub text: String,
}

/// A terminal final message to send as `CompleteDelegation`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalFinal {
    pub delegation_id: String,
    pub final_message: String,
}

/// The broker's decision after observing an event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrokerDecision {
    /// Commentary segments to flush (in order).
    pub commentary: Vec<CommentaryFlush>,
    /// Terminal final messages to send (at most one per delegation).
    pub terminal: Vec<TerminalFinal>,
    /// Whether the delegation should be marked terminal in the registry.
    pub mark_terminal: Vec<String>,
}

impl BrokerDecision {
    fn new() -> Self {
        Self::default()
    }
}

/// The accumulated assistant text for a single delegation.
#[derive(Debug, Clone, Default)]
struct DelegationAccumulator {
    /// The accumulated assistant segment text (raw, before wrapping).
    assistant_text: String,
    /// Whether the terminal has already been sent (idempotent).
    terminal_sent: bool,
}

/// The `LiveDelegationBroker` — one per active Live session.
#[derive(Debug, Default)]
pub struct LiveDelegationBroker {
    /// Per-delegation accumulators, keyed by `(generation, delegation_id)`.
    accumulators: std::collections::HashMap<(Generation, String), DelegationAccumulator>,
    /// The generation of the current Live session.
    generation: Generation,
    /// The bound session id.
    session_id: Option<String>,
    /// The bound agent id.
    agent_id: Option<AgentId>,
}

impl LiveDelegationBroker {
    /// Create a new broker for the given generation.
    pub fn new(generation: Generation) -> Self {
        Self {
            generation,
            ..Default::default()
        }
    }

    /// Bind the broker to a session + agent.
    pub fn bind(&mut self, session_id: String, agent_id: AgentId) {
        self.session_id = Some(session_id);
        self.agent_id = Some(agent_id);
    }

    /// Reserve a new delegation id for this generation before dispatch.
    ///
    /// Returns `true` only for the first `delegation.created` carrying this id.
    /// A duplicate frame returns `false`, allowing the event handler to avoid
    /// submitting the same coding task twice.
    pub fn register_delegation(&mut self, delegation_id: String) -> bool {
        use std::collections::hash_map::Entry;

        match self.accumulators.entry((self.generation, delegation_id)) {
            Entry::Vacant(entry) => {
                entry.insert(DelegationAccumulator::default());
                true
            }
            Entry::Occupied(_) => false,
        }
    }

    /// Observe an accepted, non-replay `AgentMessageChunk` for the bound
    /// session + a registered delegation's `prompt_id`.
    ///
    /// Returns commentary/terminal decisions. The caller executes them via
    /// `cmd_tx.try_send`.
    ///
    /// - `session_id`: the ACP session id of the ingress (must match the
    ///   broker's bound session).
    /// - `prompt_id`: the prompt id of the ingress (must match a registered
    ///   delegation's prompt_id).
    /// - `text`: the assistant message chunk text.
    /// - `is_tool_boundary`: whether this chunk arrived at a tool boundary
    ///   (e.g. right before/after a `ToolCallUpdate`).
    /// - `delegations`: the runtime's delegation registry.
    pub fn observe_chunk(
        &mut self,
        session_id: &str,
        prompt_id: &str,
        text: &str,
        is_tool_boundary: bool,
        delegations: &std::collections::HashMap<(Generation, String), DelegationEntry>,
    ) -> BrokerDecision {
        let mut decision = BrokerDecision::new();

        // Ignore foreign sessions.
        if self.session_id.as_deref() != Some(session_id) {
            return decision;
        }

        // Find the delegation for this prompt_id in the current generation.
        let delegation_id = match self.find_delegation_by_prompt_id(prompt_id, delegations) {
            Some(id) => id,
            None => return decision, // foreign / unregistered prompt
        };

        let key = (self.generation, delegation_id.clone());
        let acc = self.accumulators.entry(key).or_default();

        // Accumulate the assistant text.
        acc.assistant_text.push_str(text);

        if is_tool_boundary && !acc.assistant_text.is_empty() {
            // At a tool boundary, flush the accumulated assistant text as
            // 500-byte-safe commentary (the assistant's commentary before/after
            // the tool call).
            let chunks = split_500_byte_safe(&acc.assistant_text);
            for chunk in chunks {
                decision.commentary.push(CommentaryFlush {
                    delegation_id: delegation_id.clone(),
                    text: chunk,
                });
            }
            acc.assistant_text.clear();
        }

        decision
    }

    /// Observe a `TurnCompleted` (or `PromptResponse` success) for the bound
    /// session + a registered delegation's `prompt_id`.
    ///
    /// Emits the terminal final message (last assistant segment wrapped as
    /// `"Agent Final Message":`) and marks the delegation terminal. Idempotent:
    /// if the terminal was already sent, this is a no-op.
    pub fn observe_turn_completed(
        &mut self,
        session_id: &str,
        prompt_id: &str,
        delegations: &std::collections::HashMap<(Generation, String), DelegationEntry>,
    ) -> BrokerDecision {
        let mut decision = BrokerDecision::new();

        if self.session_id.as_deref() != Some(session_id) {
            return decision;
        }

        let delegation_id = match self.find_delegation_by_prompt_id(prompt_id, delegations) {
            Some(id) => id,
            None => return decision,
        };

        let key = (self.generation, delegation_id.clone());
        let acc = self.accumulators.entry(key).or_default();

        if acc.terminal_sent {
            return decision; // idempotent
        }

        // The terminal segment is the accumulated assistant text.
        let final_text = std::mem::take(&mut acc.assistant_text);
        let wrapped = prompts::wrap_agent_final_message(&final_text);
        decision.terminal.push(TerminalFinal {
            delegation_id: delegation_id.clone(),
            final_message: wrapped,
        });
        decision.mark_terminal.push(delegation_id);
        acc.terminal_sent = true;

        decision
    }

    /// Observe a `PromptResponse` error / cancel / failure for the bound
    /// session + a registered delegation's `prompt_id`.
    ///
    /// Marks the delegation terminal without a final message (idempotent).
    pub fn observe_failure(
        &mut self,
        session_id: &str,
        prompt_id: &str,
        delegations: &std::collections::HashMap<(Generation, String), DelegationEntry>,
    ) -> BrokerDecision {
        let mut decision = BrokerDecision::new();

        if self.session_id.as_deref() != Some(session_id) {
            return decision;
        }

        let delegation_id = match self.find_delegation_by_prompt_id(prompt_id, delegations) {
            Some(id) => id,
            None => return decision,
        };

        let key = (self.generation, delegation_id.clone());
        let acc = self.accumulators.entry(key).or_default();

        if acc.terminal_sent {
            return decision; // idempotent
        }

        decision.mark_terminal.push(delegation_id);
        acc.terminal_sent = true;

        decision
    }

    /// Observe a cancel/failure that should complete ALL active delegations
    /// (e.g. session disconnect, live stop). Marks all non-terminal delegations
    /// terminal without a final message.
    pub fn observe_cancel_all(
        &mut self,
        _delegations: &std::collections::HashMap<(Generation, String), DelegationEntry>,
    ) -> BrokerDecision {
        let mut decision = BrokerDecision::new();
        for ((generation, id), acc) in self.accumulators.iter_mut() {
            if *generation != self.generation || acc.terminal_sent {
                continue;
            }
            acc.terminal_sent = true;
            decision.mark_terminal.push(id.clone());
        }
        decision
    }

    /// Find the delegation id for a given prompt_id in the current generation.
    fn find_delegation_by_prompt_id(
        &self,
        prompt_id: &str,
        delegations: &std::collections::HashMap<(Generation, String), DelegationEntry>,
    ) -> Option<String> {
        for ((generation, _), entry) in delegations {
            if *generation == self.generation && entry.prompt_id == prompt_id && !entry.terminal {
                return Some(entry.delegation_id.clone());
            }
        }
        None
    }

    /// Clear all accumulators (called on teardown).
    pub fn clear(&mut self) {
        self.accumulators.clear();
        self.session_id = None;
        self.agent_id = None;
    }

    /// Whether a delegation has been completed (terminal sent).
    pub fn is_delegation_complete(&self, generation: Generation, delegation_id: &str) -> bool {
        self.accumulators
            .get(&(generation, delegation_id.to_string()))
            .is_some_and(|acc| acc.terminal_sent)
    }
}

/// Split text into 500-byte-safe chunks (never splits a multi-byte UTF-8
/// sequence). Each chunk is at most `COMMENTARY_CHUNK_MAX` bytes.
fn split_500_byte_safe(text: &str) -> Vec<String> {
    if text.len() <= COMMENTARY_CHUNK_MAX {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + COMMENTARY_CHUNK_MAX).min(text.len());
        // Walk back to a char boundary.
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(text[start..end].to_string());
        start = end;
    }
    chunks
}

/// Convert a [`BrokerDecision`] into [`LiveCommand`]s for the pipeline.
/// Commentary uses `AppendDelegationContext` with the `Commentary` channel;
/// terminal uses `CompleteDelegation` (no channel — the voice core omits it
/// for final messages).
pub fn decision_to_commands(decision: &BrokerDecision) -> Vec<LiveCommand> {
    let mut cmds = Vec::new();
    for commentary in &decision.commentary {
        cmds.push(LiveCommand::AppendDelegationContext {
            delegation_id: commentary.delegation_id.clone(),
            text: commentary.text.clone(),
            channel: LiveContextChannel::Commentary,
        });
    }
    for terminal in &decision.terminal {
        cmds.push(LiveCommand::CompleteDelegation {
            delegation_id: terminal.delegation_id.clone(),
            text: terminal.final_message.clone(),
        });
    }
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(
        generation: Generation,
        del_id: &str,
        pid: &str,
        terminal: bool,
    ) -> DelegationEntry {
        DelegationEntry {
            generation,
            delegation_id: del_id.to_string(),
            agent_id: crate::app::agent::AgentId(0),
            session_id: "sess-1".to_string(),
            prompt_id: pid.to_string(),
            terminal,
        }
    }

    fn make_delegations(
        entries: &[DelegationEntry],
    ) -> std::collections::HashMap<(Generation, String), DelegationEntry> {
        entries
            .iter()
            .map(|e| ((e.generation, e.delegation_id.clone()), e.clone()))
            .collect()
    }

    #[test]
    fn foreign_session_ignored() {
        let mut broker = LiveDelegationBroker::new(1);
        broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
        broker.register_delegation("del-1".to_string());
        let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);
        let decision = broker.observe_chunk("sess-other", "pid-1", "hello", false, &delegations);
        assert!(decision.commentary.is_empty());
        assert!(decision.terminal.is_empty());
    }

    #[test]
    fn foreign_prompt_ignored() {
        let mut broker = LiveDelegationBroker::new(1);
        broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
        broker.register_delegation("del-1".to_string());
        let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);
        let decision = broker.observe_chunk("sess-1", "pid-other", "hello", false, &delegations);
        assert!(decision.commentary.is_empty());
    }

    #[test]
    fn old_generation_ignored() {
        let mut broker = LiveDelegationBroker::new(2);
        broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
        broker.register_delegation("del-1".to_string());
        let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);
        let decision = broker.observe_chunk("sess-1", "pid-1", "hello", false, &delegations);
        assert!(decision.commentary.is_empty());
    }

    #[test]
    fn terminal_delegation_ignored() {
        let mut broker = LiveDelegationBroker::new(1);
        broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
        broker.register_delegation("del-1".to_string());
        let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", true)]);
        let decision = broker.observe_chunk("sess-1", "pid-1", "hello", false, &delegations);
        assert!(decision.commentary.is_empty());
    }

    #[test]
    fn tool_boundary_flushes_commentary() {
        let mut broker = LiveDelegationBroker::new(1);
        broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
        broker.register_delegation("del-1".to_string());
        let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);

        broker.observe_chunk("sess-1", "pid-1", "I will now ", false, &delegations);
        broker.observe_chunk("sess-1", "pid-1", "edit the file.", false, &delegations);

        let decision = broker.observe_chunk("sess-1", "pid-1", "", true, &delegations);
        assert_eq!(decision.commentary.len(), 1);
        assert_eq!(decision.commentary[0].delegation_id, "del-1");
        assert_eq!(decision.commentary[0].text, "I will now edit the file.");
    }

    #[test]
    fn terminal_exactly_once() {
        let mut broker = LiveDelegationBroker::new(1);
        broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
        broker.register_delegation("del-1".to_string());
        let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);

        broker.observe_chunk("sess-1", "pid-1", "Done!", false, &delegations);

        let d1 = broker.observe_turn_completed("sess-1", "pid-1", &delegations);
        assert_eq!(d1.terminal.len(), 1);
        assert!(
            d1.terminal[0]
                .final_message
                .starts_with("\"Agent Final Message\":")
        );
        assert_eq!(d1.mark_terminal, vec!["del-1".to_string()]);

        let d2 = broker.observe_turn_completed("sess-1", "pid-1", &delegations);
        assert!(d2.terminal.is_empty());
        assert!(d2.mark_terminal.is_empty());
    }

    #[test]
    fn signal_reordering_terminal_wins() {
        let mut broker = LiveDelegationBroker::new(1);
        broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
        broker.register_delegation("del-1".to_string());
        let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);

        broker.observe_chunk("sess-1", "pid-1", "result", false, &delegations);
        let d1 = broker.observe_turn_completed("sess-1", "pid-1", &delegations);
        assert_eq!(d1.terminal.len(), 1);

        let d2 = broker.observe_failure("sess-1", "pid-1", &delegations);
        assert!(d2.terminal.is_empty());
        assert!(d2.mark_terminal.is_empty());
    }

    #[test]
    fn cancel_all_completes_non_terminal() {
        let mut broker = LiveDelegationBroker::new(1);
        broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
        broker.register_delegation("del-1".to_string());
        broker.register_delegation("del-2".to_string());
        let delegations = make_delegations(&[
            make_entry(1, "del-1", "pid-1", false),
            make_entry(1, "del-2", "pid-2", false),
        ]);

        let d = broker.observe_cancel_all(&delegations);
        assert_eq!(d.mark_terminal.len(), 2);
    }

    #[test]
    fn split_500_byte_safe_respects_char_boundaries() {
        assert_eq!(split_500_byte_safe("hello"), vec!["hello"]);

        let long = "a".repeat(600);
        let chunks = split_500_byte_safe(&long);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 500);
        assert_eq!(chunks[1].len(), 100);

        let multi = "é".repeat(300); // 600 bytes, 300 chars
        let chunks = split_500_byte_safe(&multi);
        assert!(chunks[0].len() <= 500);
        assert!(chunks[0].chars().all(|c| c == 'é'));
    }

    #[test]
    fn repeated_delegation_ids() {
        let mut broker = LiveDelegationBroker::new(1);
        broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
        broker.register_delegation("del-1".to_string());
        broker.register_delegation("del-2".to_string());
        let delegations = make_delegations(&[
            make_entry(1, "del-1", "pid-1", false),
            make_entry(1, "del-2", "pid-2", false),
        ]);

        let d1t = broker.observe_turn_completed("sess-1", "pid-1", &delegations);
        assert_eq!(d1t.terminal.len(), 1);
        assert_eq!(d1t.terminal[0].delegation_id, "del-1");

        let d2t = broker.observe_turn_completed("sess-1", "pid-2", &delegations);
        assert_eq!(d2t.terminal.len(), 1);
        assert_eq!(d2t.terminal[0].delegation_id, "del-2");
    }

    #[test]
    fn exact_prompt_correlation() {
        let mut broker = LiveDelegationBroker::new(1);
        broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
        broker.register_delegation("del-1".to_string());
        broker.register_delegation("del-2".to_string());
        let delegations = make_delegations(&[
            make_entry(1, "del-1", "pid-1", false),
            make_entry(1, "del-2", "pid-2", false),
        ]);

        let d = broker.observe_chunk("sess-1", "pid-1", "text for del-1", true, &delegations);
        assert_eq!(d.commentary.len(), 1);
        assert_eq!(d.commentary[0].delegation_id, "del-1");

        let d = broker.observe_chunk("sess-1", "pid-2", "text for del-2", true, &delegations);
        assert_eq!(d.commentary.len(), 1);
        assert_eq!(d.commentary[0].delegation_id, "del-2");
    }

    #[test]
    fn decision_to_commands_uses_commentary_channel() {
        let decision = BrokerDecision {
            commentary: vec![CommentaryFlush {
                delegation_id: "del-1".to_string(),
                text: "hello".to_string(),
            }],
            terminal: vec![TerminalFinal {
                delegation_id: "del-1".to_string(),
                final_message: "\"Agent Final Message\":\n\ndone".to_string(),
            }],
            mark_terminal: vec!["del-1".to_string()],
        };
        let cmds = decision_to_commands(&decision);
        assert_eq!(cmds.len(), 2);
        match &cmds[0] {
            LiveCommand::AppendDelegationContext {
                channel,
                delegation_id,
                text,
            } => {
                assert_eq!(*channel, LiveContextChannel::Commentary);
                assert_eq!(delegation_id, "del-1");
                assert_eq!(text, "hello");
            }
            _ => panic!("expected AppendDelegationContext"),
        }
        match &cmds[1] {
            LiveCommand::CompleteDelegation {
                delegation_id,
                text,
            } => {
                assert_eq!(delegation_id, "del-1");
                assert!(text.starts_with("\"Agent Final Message\":"));
            }
            _ => panic!("expected CompleteDelegation"),
        }
    }
}
