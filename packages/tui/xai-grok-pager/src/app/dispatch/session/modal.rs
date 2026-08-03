//! Session rename / close helpers (shared with the dashboard).
//!
//! The `/sessions` picker modal was removed; rename-via-slash and
//! dashboard close still use these dispatchers.
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::app_view::{ActiveView, AppView};
use crate::app::dispatch::ctx::{SwitchCause, show_welcome, switch_to_agent};
use crate::app::dispatch::task_result::unregister_session_effect;
/// Remove an agent and clean up all references to it:
/// `forked_from` pointers on surviving agents.
pub(in crate::app::dispatch) fn remove_agent_and_cleanup(app: &mut AppView, agent_id: AgentId) {
    let removed = app.agents.shift_remove(&agent_id);
    for agent in app.agents.values_mut() {
        if agent.session.forked_from == Some(agent_id) {
            agent.session.forked_from = None;
        }
    }
    if removed.is_some() {
        drop(removed);
        crate::memory_release::release_retained_memory_with("agent-close");
    }
}
/// Close (drop from this pager's in-memory list) the given agent.
///
/// Order matters:
/// 1. Refuse to close the only alive agent (toast "Cannot close the
///    only session -- use /home to exit"). The user has nothing to
///    fall back to inside the agent shell.
/// 2. If the closed agent is currently active, switch first to a
///    surviving peer (parent via `forked_from` if alive, else the
///    first surviving entry) using `SwitchCause::Picker`. If no peer
///    survives, fall back to Welcome (already covered by case 1 --
///    this is a defensive belt).
/// 3. Drop the agent from `app.agents` (`shift_remove` to preserve
///    insertion order on every other entry) and clear `forked_from`
///    references on surviving agents so dangling parent pointers
///    cannot resurface.
pub(in crate::app::dispatch) fn dispatch_sessions_confirm_close(
    app: &mut AppView,
    closed_id: AgentId,
) -> Vec<Effect> {
    if !app.agents.contains_key(&closed_id) {
        return vec![];
    }
    if app.agents.len() == 1 {
        app.show_toast("Cannot close the only session -- use /home to exit");
        return vec![];
    }
    if matches!(app.active_view, ActiveView::Agent(id) if id == closed_id) {
        let parent = app
            .agents
            .get(&closed_id)
            .and_then(|a| a.session.forked_from)
            .filter(|p| app.agents.contains_key(p));
        let fallback = parent.or_else(|| app.agents.keys().copied().find(|id| *id != closed_id));
        if let Some(target) = fallback {
            switch_to_agent(app, target, SwitchCause::Picker);
        } else {
            show_welcome(app);
        }
    }
    let effects = unregister_session_effect(
        app.agents
            .get(&closed_id)
            .and_then(|a| a.session.session_id.clone()),
    );
    remove_agent_and_cleanup(app, closed_id);
    effects
}
/// Rename the current session via x.ai/session/rename.
///
/// Produces Effect::RenameSession which spawns an async ACP ext request.
/// On completion, TaskResult::RenameSessionComplete shows the result.
pub(in crate::app::dispatch) fn dispatch_rename_session(
    app: &mut AppView,
    title: String,
) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };
    agent.display_name = Some(title.clone());
    vec![Effect::RenameSession {
        agent_id: id,
        session_id,
        title,
        cwd: agent.session.cwd.clone(),
    }]
}
