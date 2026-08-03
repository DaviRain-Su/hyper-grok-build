//! Rewind picker: anchor syncing, dim ranges, and key/mouse handling.
use super::AgentView;
use crate::app::actions::Action;
use crate::app::app_view::InputOutcome;
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
impl AgentView {
    pub(super) fn sync_rewind_anchor_to_picker(&mut self) {
        let prompt_index = {
            let Some(ref rw) = self.rewind_state else {
                return;
            };
            let crate::views::rewind::RewindPhase::Picker {
                ref points,
                selected,
            } = rw.phase
            else {
                return;
            };
            let Some(point) = points.get(selected) else {
                return;
            };
            point.prompt_index
        };
        let entry_idx = crate::app::dispatch::find_user_prompt_entry_for_shell_index(
            &self.scrollback,
            prompt_index,
        );
        if let Some(ref mut rw) = self.rewind_state {
            rw.anchor_entry_idx = entry_idx.unwrap_or(0);
        }
        if let Some(idx) = entry_idx {
            self.scrollback.scroll_to_entry_center(idx);
        }
    }
    pub(super) fn rewind_dim_from_entry(&self) -> Option<usize> {
        let rw = self.rewind_state.as_ref()?;
        match &rw.phase {
            crate::views::rewind::RewindPhase::Picker { .. }
            | crate::views::rewind::RewindPhase::ModeSelect { .. }
            | crate::views::rewind::RewindPhase::Previewing { .. }
            | crate::views::rewind::RewindPhase::Confirm { .. }
            | crate::views::rewind::RewindPhase::ConversationOnlyConfirm { .. }
            | crate::views::rewind::RewindPhase::Executing { .. } => Some(rw.anchor_entry_idx),
            crate::views::rewind::RewindPhase::Loading
            | crate::views::rewind::RewindPhase::CancelOffer { .. }
            | crate::views::rewind::RewindPhase::Error { .. } => None,
        }
    }
    /// Refresh the scrollback's "awaiting user input" marks so the renderer
    /// can swap the running-spinner bullet for a pulsing-circle bullet on
    /// tool entries that are blocked on a permission prompt or
    /// `ask_user_question`.
    ///
    /// Recomputed every frame because the queue/question state is fully
    /// owned by `AgentView` and changes asynchronously; doing a fresh
    /// clear+rebuild keeps the mark and the view of record from drifting
    /// out of sync (e.g. on Cancelled requests we never observe a
    /// matching "pop" event).
    ///
    /// Cheap: O(entries) for the clear plus O(permission_queue +
    /// question_view) lookups via the tracker, both tiny in practice.
    ///
    /// Called once per frame from `AgentView::draw` in the full TUI; minimal
    /// mode bypasses that draw path, so its commit pass
    /// ([`crate::minimal::commit::commit_active`]) calls this itself to keep a
    /// tool blocked on a permission/question out of the committed frontier.
    pub(crate) fn sync_pending_user_input_marks(&mut self) {
        self.scrollback.clear_all_pending_user_input();
        for perm in &self.permission_queue {
            let tc_id = perm.request.request.tool_call.tool_call_id.0.as_ref();
            if let Some(entry_id) = self.session.tracker.pending_tool_entry_id(tc_id) {
                self.scrollback.set_pending_user_input(entry_id, true);
            }
        }
        if let Some(qv) = self.question_view.as_ref()
            && let Some(entry_id) = self.session.tracker.pending_tool_entry_id(&qv.tool_call_id)
        {
            self.scrollback.set_pending_user_input(entry_id, true);
        }
    }
    pub(super) fn handle_rewind_key(&mut self, key: &KeyEvent) -> InputOutcome {
        let Some(ref state) = self.rewind_state else {
            return InputOutcome::Unchanged;
        };
        let input = crate::views::rewind::handle_rewind_key(state, key);
        match input {
            crate::views::rewind::RewindInput::MoveUp => {
                if let Some(ref mut rw) = self.rewind_state {
                    crate::views::rewind::move_cursor(&mut rw.phase, -1);
                    self.sync_rewind_anchor_to_picker();
                }
                InputOutcome::Changed
            }
            crate::views::rewind::RewindInput::MoveDown => {
                if let Some(ref mut rw) = self.rewind_state {
                    crate::views::rewind::move_cursor(&mut rw.phase, 1);
                    self.sync_rewind_anchor_to_picker();
                }
                InputOutcome::Changed
            }
            crate::views::rewind::RewindInput::ConfirmCursor => {
                let Some(ref state) = self.rewind_state else {
                    return InputOutcome::Unchanged;
                };
                let resolved = crate::views::rewind::confirm_cursor(&state.phase);
                Self::rewind_input_to_outcome(resolved)
            }
            other => Self::rewind_input_to_outcome(other),
        }
    }
    /// Map a terminal `RewindInput` (one that doesn't itself move the cursor)
    /// to the corresponding `InputOutcome`. Shared by the key and mouse paths
    /// so the two can't drift.
    fn rewind_input_to_outcome(input: crate::views::rewind::RewindInput) -> InputOutcome {
        use crate::views::rewind::RewindInput;
        match input {
            RewindInput::Dismissed => InputOutcome::Action(Action::RewindDismiss),
            RewindInput::CancelTurnThenProceed => InputOutcome::Action(Action::RewindCancelOffer),
            RewindInput::SelectMode(mode, target) => {
                InputOutcome::Action(Action::RewindSelectMode(mode, target))
            }
            RewindInput::Confirm(target, mode) => {
                InputOutcome::Action(Action::RewindConfirm(target, mode))
            }
            RewindInput::BackToModeSelect => InputOutcome::Action(Action::RewindBackToModeSelect),
            RewindInput::DismissError => InputOutcome::Action(Action::RewindDismissError),
            RewindInput::ConversationOnlyConfirm(target) => {
                InputOutcome::Action(Action::RewindConversationOnlyConfirm(target))
            }
            RewindInput::PickerSelect(prompt_index) => {
                InputOutcome::Action(Action::RewindPickerSelect(prompt_index))
            }
            RewindInput::MoveUp
            | RewindInput::MoveDown
            | RewindInput::ConfirmCursor
            | RewindInput::Consumed => InputOutcome::Changed,
        }
    }
    /// Mouse handler for the rewind overlay. `Moved` moves the cursor
    /// (`selected` for picker, `active_idx` for radio phases) and syncs
    /// the scrollback preview on the picker. `Down(Left)` either
    /// dispatches a synthesized key (radio) or `PickerSelect` (picker).
    /// Mouse handler for the rewind overlay. `Moved` moves the cursor
    /// to the row under the pointer; `Down(Left)` moves the cursor then
    /// activates that row (Enter-equivalent). Geometry comes from
    /// `rewind_row_at`, which mirrors `render_rewind_overlay`'s layout.
    pub(super) fn handle_rewind_mouse(&mut self, mouse: &MouseEvent) -> InputOutcome {
        use crate::views::rewind::{rewind_activate, rewind_row_at, set_rewind_cursor};
        let Some(rw) = self.rewind_state.as_mut() else {
            return InputOutcome::Unchanged;
        };
        let area = self.pane_areas.prompt;
        let Some(idx) = rewind_row_at(&rw.phase, area, mouse.column, mouse.row) else {
            return InputOutcome::Unchanged;
        };
        match mouse.kind {
            MouseEventKind::Moved => {
                if set_rewind_cursor(&mut rw.phase, idx) {
                    InputOutcome::Changed
                } else {
                    InputOutcome::Unchanged
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                set_rewind_cursor(&mut rw.phase, idx);
                let is_picker =
                    matches!(rw.phase, crate::views::rewind::RewindPhase::Picker { .. });
                let activated = rewind_activate(&rw.phase);
                if is_picker {
                    self.sync_rewind_anchor_to_picker();
                }
                Self::rewind_input_to_outcome(activated)
            }
            _ => InputOutcome::Unchanged,
        }
    }
}
#[cfg(test)]
mod sync_rewind_anchor_to_picker_tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::agent::{AgentId, AgentSession, AgentState};
    use crate::scrollback::block::RenderBlock;
    use crate::scrollback::blocks::UserPromptBlock;
    use crate::scrollback::state::ScrollbackState;
    fn make_agent() -> AgentView {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        AgentView::new(
            AgentSession {
                id: AgentId(0),
                acp_tx: tx,
                session_id: None,
                models: ModelState::default(),
                state: AgentState::Idle,
                tracker: crate::acp::tracker::AcpUpdateTracker::new(),
                cwd: std::path::PathBuf::from("/tmp"),
                is_worktree: false,
                forked_from: None,
                pending_prompts: std::collections::VecDeque::new(),
                next_queue_id: 0,
                yolo_mode: false,
                auto_mode: false,
                prompt_history: Vec::new(),
                prompt_history_loading: false,
                loading_replay: false,
                restore_degree: None,
                rate_limited: false,
                model_incompatible: false,
                credit_limit_blocked: false,
                free_usage_blocked: false,
                available_commands: Vec::new(),
                available_commands_generation: 0,
                available_tools: None,
                model_switch_pending: false,
                user_model_preference: None,
                deferred_model_switch: None,
                bg_tasks: std::collections::BTreeMap::new(),
                bg_tool_call_to_task: std::collections::HashMap::new(),
                scheduled_tasks: std::collections::HashMap::new(),
                in_flight_prompt: None,
                compact_held_prompt: None,
                current_prompt_id: None,
                created_via_new: false,
            },
            ScrollbackState::new(),
        )
    }
    fn user_block(text: &str, pi: Option<usize>) -> RenderBlock {
        let mut b = UserPromptBlock::new(text);
        b.prompt_index = pi;
        RenderBlock::UserPrompt(b)
    }
    fn run_with_indices(prompt_indices: [Option<usize>; 3]) -> (AgentView, usize, usize, usize) {
        let mut agent = make_agent();
        let alpha = agent
            .scrollback
            .push_block(user_block("alpha", prompt_indices[0]));
        agent.scrollback.push_block(RenderBlock::agent_message("a"));
        let bravo = agent
            .scrollback
            .push_block(user_block("bravo", prompt_indices[1]));
        agent.scrollback.push_block(RenderBlock::agent_message("b"));
        let charlie = agent
            .scrollback
            .push_block(user_block("charlie", prompt_indices[2]));
        agent.scrollback.push_block(RenderBlock::agent_message("c"));
        let alpha_idx = agent.scrollback.index_of_id(alpha).unwrap();
        let bravo_idx = agent.scrollback.index_of_id(bravo).unwrap();
        let charlie_idx = agent.scrollback.index_of_id(charlie).unwrap();
        (agent, alpha_idx, bravo_idx, charlie_idx)
    }
    fn set_selected(agent: &mut AgentView, sel: usize) {
        use crate::views::rewind::RewindPhase;
        if let Some(rw) = agent.rewind_state.as_mut()
            && let RewindPhase::Picker { selected, .. } = &mut rw.phase
        {
            *selected = sel;
        }
    }
    fn install_picker(agent: &mut AgentView) {
        use crate::views::rewind::{RewindPhase, RewindPointInfo, RewindState};
        let pt = |pi: usize, preview: &str| RewindPointInfo {
            prompt_index: pi,
            created_at: String::new(),
            num_file_snapshots: 0,
            has_file_changes: false,
            prompt_preview: Some(preview.into()),
        };
        let points = vec![pt(2, "charlie"), pt(1, "bravo"), pt(0, "alpha")];
        agent.rewind_state = Some(RewindState {
            phase: RewindPhase::Picker {
                points,
                selected: 0,
            },
            anchor_entry_idx: 0,
            stashed_draft: None,
            selected_prompt_index: None,
        });
    }
    #[test]
    fn anchor_tracks_each_picker_row_when_prompt_index_is_set() {
        let (mut agent, alpha_idx, bravo_idx, charlie_idx) =
            run_with_indices([Some(0), Some(1), Some(2)]);
        install_picker(&mut agent);
        agent.sync_rewind_anchor_to_picker();
        assert_eq!(
            agent.rewind_state.as_ref().unwrap().anchor_entry_idx,
            charlie_idx,
            "selected=0 → charlie"
        );
        set_selected(&mut agent, 1);
        agent.sync_rewind_anchor_to_picker();
        assert_eq!(
            agent.rewind_state.as_ref().unwrap().anchor_entry_idx,
            bravo_idx,
            "selected=1 → bravo"
        );
        set_selected(&mut agent, 2);
        agent.sync_rewind_anchor_to_picker();
        assert_eq!(
            agent.rewind_state.as_ref().unwrap().anchor_entry_idx,
            alpha_idx,
            "selected=2 → alpha"
        );
    }
    #[test]
    fn anchor_tracks_each_picker_row_when_prompt_index_is_missing() {
        let (mut agent, alpha_idx, bravo_idx, charlie_idx) = run_with_indices([None, None, None]);
        install_picker(&mut agent);
        agent.sync_rewind_anchor_to_picker();
        assert_eq!(
            agent.rewind_state.as_ref().unwrap().anchor_entry_idx,
            charlie_idx,
            "fallback: selected=0 → charlie"
        );
        set_selected(&mut agent, 1);
        agent.sync_rewind_anchor_to_picker();
        assert_eq!(
            agent.rewind_state.as_ref().unwrap().anchor_entry_idx,
            bravo_idx,
            "fallback: selected=1 → bravo (regression: was alpha before fix)"
        );
        set_selected(&mut agent, 2);
        agent.sync_rewind_anchor_to_picker();
        assert_eq!(
            agent.rewind_state.as_ref().unwrap().anchor_entry_idx,
            alpha_idx,
            "fallback: selected=2 → alpha"
        );
    }
}
