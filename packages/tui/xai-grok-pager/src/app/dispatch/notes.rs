//! Feedback, remember-note, btw, and recap dispatchers.

use super::ctx::with_active_agent;
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::agent_view::{AgentView, PromptInputMode};
use crate::app::app_view::{ActiveView, AppView};
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::{SessionEvent, ToolCallBlock};
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counter for correlating async rewrite responses with the modal
/// that requested them. Prevents stale results from populating a different
/// note's review modal when the user closes and re-opens quickly.
static REWRITE_NONCE: AtomicU64 = AtomicU64::new(0);

fn next_rewrite_nonce() -> u64 {
    REWRITE_NONCE.fetch_add(1, Ordering::Relaxed)
}

/// Enter feedback mode: visual change to prompt bar (teal accent, pencil prefix).
/// No side effects — the user types feedback text and presses Enter to send.
pub(super) fn dispatch_enter_feedback_mode(app: &mut AppView) -> Vec<Effect> {
    with_active_agent(app, |agent| {
        agent.prompt_input_mode = PromptInputMode::Feedback;
        agent.prompt.set_text("");
    });
    vec![]
}

/// Enter remember mode: visual change to prompt bar (remember accent, `#` prefix).
/// No side effects — the user types a memory note and presses Enter to send.
pub(super) fn dispatch_enter_remember_mode(app: &mut AppView) -> Vec<Effect> {
    with_active_agent(app, |agent| {
        agent.prompt_input_mode = PromptInputMode::Remember;
        agent.prompt.set_text("");
    });
    vec![]
}

/// Send feedback text to the server. Shows a thank-you message immediately
/// and fires the HTTP POST as a background effect.
pub(super) fn dispatch_send_feedback(app: &mut AppView, text: String) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    agent.prompt_input_mode = PromptInputMode::Normal;
    agent.prompt.set_text("");
    // Submitting feedback retires any edit-contextual ephemeral tip.
    agent.ephemeral_tip.clear_on_submit();

    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        agent.scrollback.push_block(RenderBlock::system(
            "Please provide feedback text.".to_string(),
        ));
        return vec![];
    }

    let Some(session_id) = agent.session.session_id.clone() else {
        agent
            .scrollback
            .push_block(RenderBlock::system("No active session.".to_string()));
        return vec![];
    };

    agent.scrollback.push_block(RenderBlock::system(
        "Thanks for the feedback! The Grok Build team is on it.".to_string(),
    ));

    vec![Effect::SendFeedback {
        agent_id: id,
        session_id,
        feedback_text: trimmed,
    }]
}

/// Send a raw remember note for LLM-powered rewriting via `x.ai/memory/rewrite`.
/// Clears remember mode and prompts the LLM to reformat the note with session
/// context. Falls back to direct `SaveMemoryNote` when no session is available.
pub(super) fn dispatch_send_remember_note(app: &mut AppView, text: String) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;

    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    agent.prompt_input_mode = PromptInputMode::Normal;
    agent.prompt.set_text("");
    // Submitting a memory note retires any edit-contextual ephemeral tip.
    agent.ephemeral_tip.clear_on_submit();

    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        agent.scrollback.push_block(RenderBlock::system(
            "Please provide a memory note.".to_string(),
        ));
        return vec![];
    }

    let cwd = agent.session.cwd.clone();

    let Some(session_id) = agent.session.session_id.clone() else {
        // No session — open modal with raw content only (no LLM rewrite).
        agent.active_modal = Some(ActiveModal::RememberNoteReview {
            raw_content: trimmed.clone(),
            enhanced_content: None, // no session → no LLM rewrite, Tab disabled
            showing_enhanced: false,
            scroll: 0,
            window: crate::views::modal_window::ModalWindowState::new(),
            cached_lines: None,
            cwd,
            agent_id: id,
            rewrite_nonce: 0, // no rewrite in flight, nonce unused
        });
        return vec![];
    };

    // Open modal with raw content, LLM rewrite in flight.
    let nonce = next_rewrite_nonce();
    agent.active_modal = Some(ActiveModal::RememberNoteReview {
        raw_content: trimmed.clone(),
        enhanced_content: None,
        showing_enhanced: false,
        scroll: 0,
        window: crate::views::modal_window::ModalWindowState::new(),
        cached_lines: None,
        cwd: cwd.clone(),
        agent_id: id,
        rewrite_nonce: nonce,
    });

    let context_summary = extract_session_context(agent);

    vec![Effect::RewriteMemoryNote {
        agent_id: id,
        session_id,
        raw_text: trimmed,
        context_summary,
        nonce,
    }]
}

/// Save the currently displayed remember note from the review modal.
pub(super) fn dispatch_save_remember_note_from_modal(app: &mut AppView) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;

    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    let (content, cwd) = if let Some(ActiveModal::RememberNoteReview {
        ref raw_content,
        ref enhanced_content,
        showing_enhanced,
        ref cwd,
        ..
    }) = agent.active_modal
    {
        let text = if showing_enhanced {
            enhanced_content.as_deref().unwrap_or(raw_content)
        } else {
            raw_content
        };
        (text.trim().to_string(), cwd.clone())
    } else {
        return vec![];
    };

    agent.active_modal = None;
    agent
        .scrollback
        .push_block(RenderBlock::system("Saving memory note...".to_string()));

    vec![Effect::SaveMemoryNote {
        agent_id: id,
        text: content,
        cwd,
    }]
}

/// Extract session context for the LLM memory rewrite request.
///
/// Walks scrollback in reverse, collecting:
/// - Last 5 user prompts
/// - File paths from recent tool calls (Read, Edit, ListDir)
/// - CWD and git branch
fn extract_session_context(agent: &AgentView) -> String {
    let mut user_prompts: Vec<String> = Vec::new();
    let mut file_paths: Vec<String> = Vec::new();

    // Walk scrollback entries in reverse to collect recent context.
    let len = agent.scrollback.len();
    for i in (0..len).rev() {
        let Some(entry) = agent.scrollback.entry(i) else {
            continue;
        };
        match &entry.block {
            RenderBlock::UserPrompt(prompt) => {
                if user_prompts.len() < 5 {
                    let text = if prompt.text.len() > 200 {
                        let end = prompt
                            .text
                            .char_indices()
                            .map(|(i, _)| i)
                            .take_while(|&i| i <= 200)
                            .last()
                            .unwrap_or(0);
                        format!("{}...", &prompt.text[..end])
                    } else {
                        prompt.text.clone()
                    };
                    user_prompts.push(text);
                }
            }
            RenderBlock::ToolCall(tc) => {
                if file_paths.len() < 20 {
                    match tc {
                        ToolCallBlock::Read(b) => {
                            file_paths.push(b.path.clone());
                        }
                        ToolCallBlock::Edit(b) => {
                            file_paths.push(b.path.clone());
                        }
                        ToolCallBlock::ListDir(b) => {
                            file_paths.push(b.path.clone());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        // Stop early once we have enough context.
        if user_prompts.len() >= 5 && file_paths.len() >= 20 {
            break;
        }
    }

    let mut parts: Vec<String> = Vec::new();

    // CWD
    parts.push(format!("CWD: {}", agent.session.cwd.display()));

    // Git branch
    if let Some(ref branch) = agent.current_branch {
        parts.push(format!("Branch: {branch}"));
    }

    // Recent prompts (chronological order)
    if !user_prompts.is_empty() {
        user_prompts.reverse();
        parts.push("Recent prompts:".to_string());
        for p in &user_prompts {
            parts.push(format!("- {p}"));
        }
    }

    // Recent file paths (deduplicated, preserving first-seen order)
    if !file_paths.is_empty() {
        let mut seen = std::collections::HashSet::new();
        file_paths.retain(|p| seen.insert(p.clone()));
        parts.push("Recent files:".to_string());
        for p in &file_paths {
            parts.push(format!("- {p}"));
        }
    }

    parts.join("\n")
}

/// Send a /btw side question. Bypasses the prompt queue — works even while
/// the agent is mid-turn. Fires an ACP ext method and shows a loading overlay.
pub(super) fn dispatch_send_btw(app: &mut AppView, question: String) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let minimal = app.screen_mode.is_minimal();
    let (session_id, minimal_request_id) = {
        let Some(agent) = app.agents.get_mut(&id) else {
            return vec![];
        };
        let Some(session_id) = agent.session.session_id.clone() else {
            if minimal {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(
                        "No active session",
                    ));
            } else {
                agent.show_toast("No active session");
            }
            return vec![];
        };

        agent.prompt.set_text("");
        let minimal_request_id = if minimal {
            Some(crate::minimal_api::start_minimal_btw(
                agent,
                question.clone(),
            ))
        } else {
            agent.btw_state = Some(crate::views::btw_overlay::BtwOverlayState::Loading {
                question: question.clone(),
            });
            // Prompt keeps focus while the answer is in flight (panel focuses on Done).
            agent.btw_focused = false;
            None
        };
        (session_id, minimal_request_id)
    };

    vec![Effect::SendBtw {
        agent_id: id,
        session_id,
        question,
        minimal_request_id,
    }]
}

/// Toast when a manual `/recap` produces no summary. Empty sessions get a clear
/// empty-state message; anything else (model failure, empty summary, etc.) keeps
/// the generic failure toast.
pub(crate) fn recap_unavailable_toast(has_user_messages: bool) -> &'static str {
    if has_user_messages {
        "Couldn't generate recap"
    } else {
        "No messages yet"
    }
}

/// Whether scrollback already has a user prompt. Scans entries (not
/// `turn_count`) so it stays correct during `begin_batch`/`end_batch` session
/// load, when `push` defers `rebuild_turns` and `turn_count` can stay 0 while
/// replayed prompts are already present.
pub(crate) fn scrollback_has_user_messages(
    scrollback: &crate::scrollback::state::ScrollbackState,
) -> bool {
    scrollback
        .iter_entries()
        .any(|(_, entry)| entry.block.is_user_prompt())
}

/// Request a session recap. Bypasses the prompt queue — works even while the
/// agent is mid-turn. Fires the `x.ai/recap` ext method; the recap arrives
/// asynchronously as a `SessionRecap` notification (rendered in scrollback).
///
/// `auto` is `false` for an explicit `/recap` and `true` for the automatic
/// return-from-away recap. For the manual path we clear the prompt and, when
/// no session exists yet, surface a toast; the auto path is best-effort and
/// silently no-ops without an active session.
pub(super) fn dispatch_send_recap(app: &mut AppView, auto: bool) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    // Shell is authoritative (remote settings / config / env). Skip client requests
    // entirely when the feature is off so we never hit `x.ai/recap`.
    if !app.session_recap_available {
        if !auto {
            agent.show_toast("Session recap is not enabled");
        }
        return vec![];
    }

    let Some(session_id) = agent.session.session_id.clone() else {
        if !auto {
            agent.show_toast("No active session");
        }
        return vec![];
    };

    if !auto {
        agent.prompt.set_text("");
        // Nothing to summarize yet — show a clear empty-state toast instead of
        // a spinner that ends in "Couldn't generate recap".
        //
        // Skip the short-circuit while session replay is still loading (prompts
        // may not have arrived yet). Prefer an entry scan over `turn_count()`
        // so mid-batch resume (deferred `rebuild_turns`) still sees history.
        if !agent.session.loading_replay && !scrollback_has_user_messages(&agent.scrollback) {
            agent.show_toast(recap_unavailable_toast(false));
            return vec![];
        }
        // Show an immediate loading block with the animated "running" sidebar so
        // the user has feedback that a recap is being generated. The
        // `SessionRecap` handler fills this entry in and stops the animation.
        // Reuse an existing in-flight loading block instead of stacking spinners
        // when `/recap` is pressed repeatedly.
        let already_loading = agent.pending_recap_entry.is_some_and(|eid| {
            agent
                .scrollback
                .get_by_id(eid)
                .is_some_and(|entry| entry.is_running)
        });
        if !already_loading {
            let entry_id =
                agent
                    .scrollback
                    .push(crate::scrollback::entry::ScrollbackEntry::running(
                        RenderBlock::session_event(SessionEvent::Recap {
                            summary: String::new(),
                            auto: false,
                        }),
                    ));
            agent.pending_recap_entry = Some(entry_id);
        }
    } else {
        // Retry backoff only — do not consume the away period on dispatch.
        // The shell often no-ops auto recap until ≥3 min since the last main
        // turn; mark_recap_shown runs when any SessionRecap arrives (auto or
        // manual `/recap`).
        app.notification_service
            .focus_tracker
            .note_auto_recap_attempt();
    }

    vec![Effect::SendRecap { session_id, auto }]
}

// TaskResult handlers.

pub(super) fn handle_memory_note_saved(
    app: &mut AppView,
    agent_id: AgentId,
    result: Result<(), String>,
) -> Vec<Effect> {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        match result {
            Ok(()) => {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Memory saved to {}",
                        crate::util::display_user_grok_path("memory/MEMORY.md")
                    )));
            }
            Err(error) => {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Couldn't save memory note: {error}"
                    )));
            }
        }
    }
    vec![]
}

pub(super) fn handle_btw_response(
    app: &mut AppView,
    agent_id: AgentId,
    result: Result<String, String>,
    minimal_request_id: Option<uuid::Uuid>,
) -> Vec<Effect> {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        use crate::views::btw_overlay::BtwOverlayState;
        if let Some(request_id) = minimal_request_id {
            crate::minimal_api::finish_minimal_btw(agent, request_id, result);
            return vec![];
        }
        let question = match &agent.btw_state {
            Some(BtwOverlayState::Loading { question }) => question.clone(),
            _ => String::new(),
        };
        match result {
            Ok(response) => {
                // Answer arrived: show it (until Esc) and focus the panel
                // so Up/Down scroll it until the user returns to the prompt.
                agent.btw_state = Some(BtwOverlayState::done(question, response));
                agent.btw_focused = true;
            }
            Err(error) => {
                // Error stays until Esc; nothing to scroll, keep prompt focus.
                agent.btw_state = Some(BtwOverlayState::Error { question, error });
                agent.btw_focused = false;
            }
        }
    }
    vec![]
}
