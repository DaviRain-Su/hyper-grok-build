//! `/live` — toggle the Codex Live voice session. Starts a real-time
//! bidirectional voice session bound to the active agent session; Esc/Ctrl+C
//! or `/live` again stops it. Not written to `config.toml`.
//!
//! Session-scoped (requires an agent screen with a bound ACP session). If the
//! active AgentView has no bound ACP session, the start is deferred until
//! `CreateSession` completes.
//!
//! Mutually exclusive with `/voice` dictation — starting `/live` stops `/voice`
//! and vice versa.
//!
//! Gated by the `codex-live` feature and the runtime gate
//! (`GROK_CODEX_LIVE` / requirements / config). Independent of the xAI
//! `/voice` subscription tier.

#![cfg(feature = "codex-live")]

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// `/live` — toggle the Codex Live voice session.
pub struct LiveCommand;

impl SlashCommand for LiveCommand {
    fn name(&self) -> &str {
        "live"
    }

    fn description(&self) -> &str {
        "Codex Live voice session (Space: mute, Esc: stop)"
    }

    fn usage(&self) -> &str {
        "/live"
    }

    /// Session-scoped: requires an agent screen with a bound ACP session.
    fn session_scoped(&self) -> bool {
        true
    }

    fn offered_when_session_less(&self) -> bool {
        false
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        // Toggle: starts the Live session, or stops it if already active.
        CommandResult::Action(Action::LiveToggle)
    }
}
