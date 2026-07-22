//! Session lifecycle: bind/reload/replay bookkeeping, turn activity
//! resolution, context/credit updates, and app-scoped gates.
#[cfg(test)]
use super::test_agent_view;
use super::{
    ActivePane, AgentView, InlineMediaHitAreas, InputMode, PaneAreas, PluginCtaState,
    PromptInputMode, PromptMode, REWOUND_PROMPT_ID_CAP, SELF_ORIGINATED_PROMPT_CAP, SessionReload,
};
use crate::app::agent::AgentSession;
use crate::app::app_view::InputOutcome;
use crate::scrollback::state::ScrollbackState;
use crate::scrollback::text_selection::ResolvedSelectionModel;
use crate::views::prompt_widget::PromptWidget;
use crate::views::queue_pane::QueuePane;
use crate::views::subagent_catalog_pane::SubagentCatalogPane;
use crate::views::tasks_pane::TasksPane;
use crate::views::todo_pane::TodoPane;
use ratatui::layout::Rect;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;
impl AgentView {
    /// Bind this view to a root session id, resetting the per-session
    /// reconnect cursor and both dedup highwaters (ACP + xAI) when the id
    /// actually changes — all three are meaningless against another session's
    /// event-id history (a stale cursor relies on exact-match failure for
    /// safety; a stale highwater could dedup-drop the new session's events
    /// outright).
    pub(crate) fn bind_session_id(&mut self, session_id: agent_client_protocol::SessionId) {
        if self.session.session_id.as_ref() != Some(&session_id) {
            self.last_seen_event_id = None;
            self.last_applied_event_seq = None;
            self.last_applied_xai_event_seq = None;
            self.clear_minimal_btw_lifecycle();
        }
        self.session.session_id = Some(session_id);
    }
    /// Unbind this view from its current session identity.
    pub(crate) fn unbind_session_id(&mut self) {
        if self.session.session_id.take().is_some() {
            self.clear_minimal_btw_lifecycle();
        }
    }
    /// Record a prompt id this client originated (sent to the agent as the turn
    /// driver). Used by the ACP gate to keep `attached_as_viewer` per-turn
    /// accurate. Bounded FIFO; a no-op for ids already tracked.
    pub fn note_self_originated_prompt(&mut self, prompt_id: &str) {
        if self.is_self_originated_prompt(prompt_id) {
            return;
        }
        self.self_originated_prompt_ids
            .push_back(prompt_id.to_string());
        while self.self_originated_prompt_ids.len() > SELF_ORIGINATED_PROMPT_CAP {
            self.self_originated_prompt_ids.pop_front();
        }
    }
    /// Whether `prompt_id` is a turn THIS client originated (vs. one another
    /// client drives, or a server-initiated turn).
    pub fn is_self_originated_prompt(&self, prompt_id: &str) -> bool {
        self.self_originated_prompt_ids
            .iter()
            .any(|p| p == prompt_id)
    }
    pub(crate) fn note_rewound_prompt(&mut self, prompt_id: &str) {
        if self.rewound_prompt_ids.iter().any(|p| p == prompt_id) {
            return;
        }
        self.rewound_prompt_ids.push_back(prompt_id.to_string());
        while self.rewound_prompt_ids.len() > REWOUND_PROMPT_ID_CAP {
            self.rewound_prompt_ids.pop_front();
        }
    }
    pub(crate) fn is_rewound_prompt(&self, prompt_id: &str) -> bool {
        self.rewound_prompt_ids.iter().any(|p| p == prompt_id)
    }
    /// Create a new agent view with default UI state.
    ///
    /// The prompt widget is initialized with the session's working directory.
    pub fn new(session: AgentSession, scrollback: ScrollbackState) -> Self {
        let prompt = PromptWidget::new_with_cwd(&session.cwd);
        let mut view = Self {
            session,
            scrollback,
            prompt,
            tip_typing_dismissed: false,
            todo: TodoPane::new(),
            tasks: TasksPane::new(),
            catalog: SubagentCatalogPane::new(),
            queue: QueuePane::new(),
            shared_queue: Vec::new(),
            attached_as_viewer: false,
            self_originated_prompt_ids: VecDeque::new(),
            rewound_prompt_ids: VecDeque::new(),
            last_applied_event_seq: None,
            last_applied_xai_event_seq: None,
            last_seen_event_id: None,
            session_reload: None,
            unexpected_replay_drops: 0,
            replayed_terminal_prompts: HashSet::new(),
            active_pane: ActivePane::Prompt,
            prompt_mode: PromptMode::Normal,
            prompt_input_mode: PromptInputMode::Normal,
            multiline_mode: false,
            vim_mode: crate::appearance::cache::load_vim_mode(),
            input_mode: InputMode::Vim,
            bash_turn: false,
            cron_task_id: None,
            stashed_prompt: None,
            credit_limit_stashed_prompt: None,
            reauth_stashed_prompt: None,
            active_modal: None,
            modal_buttons: Vec::new(),
            modal_hovered_key: None,
            context_state: None,
            chat_kind: false,
            app_chat_mode: false,
            credit_balance: None,
            auto_topup: None,
            goal_state: None,
            workflow_blocks: std::collections::HashMap::new(),
            workflow_runs: Vec::new(),
            workflow_run_revisions: std::collections::HashMap::new(),
            cleared_workflow_runs: std::collections::HashSet::new(),
            show_workflows: false,
            workflows_view: crate::views::workflows::WorkflowsViewState::default(),
            parked_wait_marker_for: None,
            pending_stop_hooks: None,
            last_cleared_goal_id: None,
            show_goal_detail: false,
            turn_start_ms: None,
            turn_started_at: None,
            first_activity_logged_for: None,
            turn_paused_duration: std::time::Duration::ZERO,
            self_interjection_ids: std::collections::HashSet::new(),
            last_active_at: Some(Instant::now()),
            current_branch: None,
            is_worktree: false,
            main_repo: None,
            worktree_label: None,
            activity_started_at: None,
            last_activity: None,
            pane_areas: PaneAreas::default(),
            hovered_entry: None,
            pending_text_drag: None,
            drag_selection: None,
            pending_block_drag: None,
            block_drag_selection: None,
            deferred_text_press: None,
            persistent_text_selection: None,
            table_selection_geometry: None,
            selection_created_at: None,
            last_drag_mouse: None,
            drag_autoscroll: None,
            left_mouse_down: false,
            plan_prompt_mouse_drag: false,
            last_scrollback_selection_model: ResolvedSelectionModel::default(),
            last_scrollback_selection_boundaries: Default::default(),
            last_link_overlay: Default::default(),
            frame_occluder_rects: Vec::new(),
            visible_link_map: Default::default(),
            scrollback_visible_link_count: 0,
            highlighted_link_idx: None,
            hovered_link_idx: None,
            last_pointer_on_link: false,
            last_btw_selection_model: ResolvedSelectionModel::default(),
            last_btw_area: Rect::default(),
            pending_scrollback_click: None,
            pending_link_click: None,
            media_link_paths: Vec::new(),
            media_link_paths_gen: None,
            last_mouse_pos: (0, 0),
            last_mouse_moved_at: None,
            last_click: None,
            last_text_click: None,
            last_clipboard_toast_at: None,
            last_context_click_at: None,
            hovered_prompt: false,
            hit_badge: Default::default(),
            hit_context: Default::default(),
            hit_credits: Default::default(),
            hit_todo_close: Default::default(),
            hit_bg_close: Default::default(),
            hit_subagent_close: Default::default(),
            hit_catalog_close: Default::default(),
            hit_bg_status: Default::default(),
            hit_goal_status: Default::default(),
            hit_goal_close: Default::default(),
            hit_bg_button: Default::default(),
            last_bg_click: None,
            hit_queue_close: Default::default(),
            hit_queue_badge: Default::default(),
            hit_plan_button: Default::default(),
            hit_plan_approval_status: Default::default(),
            hit_follow_indicator: Default::default(),
            hit_cwd: Default::default(),
            hit_cancel_button: Default::default(),
            hit_announcement_hide: Default::default(),
            hit_announcement_cta: Default::default(),
            hit_upgrade_cta: Default::default(),
            hit_voice_stop_button: Default::default(),
            hit_scrollbar: Default::default(),
            scrollbar_dragging: false,
            dropdown_items_area: None,
            slash_dropdown_items_area: None,
            slash_dropdown_hit: Default::default(),
            completion_dropdown_items_area: None,
            history_dropdown_area: None,
            last_prompt_click_ms: None,
            line_viewer: None,
            image_viewer: None,
            image_load_rx: None,
            video_viewer: None,
            gboom: None,
            inline_media_cache: std::collections::HashMap::new(),
            inline_media_ids: std::collections::HashMap::new(),
            inline_media_iterm_emitted: std::collections::HashMap::new(),
            next_inline_media_id: 2,
            inline_video: None,
            video_load_rx: None,
            mermaid: None,
            edit_hl: None,
            inline_media_active: false,
            last_placed_ids: HashSet::new(),
            last_terminal_size: (0, 0),
            terminal_size_stale: false,
            inline_media_hits: InlineMediaHitAreas::default(),
            extensions_modal: None,
            agents_modal: None,
            persona_detail: None,
            btw_state: None,
            minimal_btw_lifecycle: None,
            btw_focused: false,
            hit_btw_close: Default::default(),
            toast: None,
            ephemeral_tip: Default::default(),
            word_select_tip_prompt_snapshot: None,
            last_word_select_probe: None,
            sticky_toast: None,
            mode_switch_banner: None,
            session_banner_active: false,
            pinned_upgrade_cta_live: false,
            block_viewer: None,
            scrollback_search: None,
            hit_sb_copy: Default::default(),
            hit_sb_view: Default::default(),
            question_view: None,
            hit_question_scrollbar: Default::default(),
            hovered_question_item: None,
            question_scrollbar_dragging: false,
            last_question_click: None,
            inline_prompt_area: None,
            question_nav_buttons: Vec::new(),
            hovered_question_button: None,
            question_scroll_region: None,
            plan_mode_active: false,
            plan_mode_pending: None,
            deferred_session_mode: None,
            pending_extensions_fetch: false,
            in_dashboard_overlay: false,
            mcp_init_progress: None,
            acp_synced_generation: 0,
            hovered_permission_item: None,
            last_permission_click: None,
            permission_queue: VecDeque::new(),
            next_perm_req_id: 0,
            permission_stashed_prompt: None,
            plan_approval_view: None,
            latest_inline_plan_content: None,
            plan_comments: Vec::new(),
            plan_next_comment_id: 0,
            casual_commenting_range: None,
            casual_editing_comment_id: None,
            casual_stashed_prompt: None,
            cancel_turn_view: None,
            cancel_turn_buttons: Vec::new(),
            cancel_subagents_preference: None,
            cancel_trigger_hint: None,
            rewind_state: None,
            rewind_points: None,
            inline_edit: None,
            pending_inline_resubmit: None,
            jump_state: None,
            timeline_rail: None,
            timeline_hover: None,
            timeline_hover_preview: None,
            session_agent_name: None,
            subagent_sessions: HashMap::new(),
            subagent_views: HashMap::new(),
            active_subagent: None,
            is_subagent_view: false,
            hit_subagent_frame_close: Default::default(),
            sharing_enabled: false,
            billing_surface_visible: false,
            input_log: crate::input_log::InputRingBuffer::new(),
            esc_pressed_at: None,
            pending_first_prompt: None,
            pending_fork_banner: None,
            loading_placeholder_id: None,
            pending_recap_entry: None,
            display_name: None,
            generated_session_title: None,
            pending_effects: Vec::new(),
            paste_probe_in_flight: 0,
            deferred_send: None,
            pending_turn_end_reconcile: None,
            expect_send_now_cancel: None,
            optimistic_queue_ids: std::collections::HashSet::new(),
            send_now_awaiting_confirm: None,
            send_now_painted_blocks: std::collections::HashMap::new(),
            follow_without_jump_prompt_id: None,
            plugin_cta: PluginCtaState::default(),
            follow_ups: None,
            follow_up_shown_prompt_id: None,
            follow_up_chips: Vec::new(),
            hovered_follow_up_chip: None,
            follow_up_seen: HashMap::new(),
            follow_up_next_gen: 0,
            follow_up_pending: HashMap::new(),
            follow_up_pending_order: VecDeque::new(),
            pending_adoption_updates: Vec::new(),
        };
        let mode = if crate::appearance::cache::load_simple_mode() {
            InputMode::Simple
        } else {
            InputMode::Vim
        };
        view.set_input_mode(mode);
        view
    }
    /// Establish read-only child identity before a view is stored or opened.
    pub(crate) fn mark_as_subagent_view(&mut self) {
        self.is_subagent_view = true;
    }
    /// Register a child view and establish its read-only subagent identity.
    pub(crate) fn insert_subagent_view(
        &mut self,
        child_sid: String,
        mut child_view: Box<AgentView>,
    ) {
        child_view.mark_as_subagent_view();
        self.subagent_views.insert(child_sid, child_view);
    }
    /// Clear `turn_started_at` and stamp `last_active_at` to "now".
    ///
    /// Call this from every site that ends a turn (success, failure,
    /// cancellation, reconnect cleanup). Centralised so the two
    /// fields cannot drift apart at the ~10 termination call sites
    /// across `dispatch.rs` and `event_loop.rs`.
    pub fn mark_turn_finished(&mut self) {
        self.turn_started_at = None;
        self.turn_paused_duration = std::time::Duration::ZERO;
        self.last_active_at = Some(Instant::now());
    }
    /// Invalidate and clear a minimal `/btw` lifecycle at a session boundary.
    pub(crate) fn clear_minimal_btw_lifecycle(&mut self) {
        crate::minimal_api::clear_minimal_btw(self);
    }
    /// Enter a `session/load` replay window: flip `loading_replay` on and reset
    /// every field coupled to that transition together, so no site can drift
    /// (e.g. reset one coupled field but miss another). Called at every
    /// replay-window entry: the fresh/restore load ctor paths and the
    /// reconnect/fork reuse paths.
    pub(crate) fn begin_replay_window(&mut self) {
        self.clear_minimal_btw_lifecycle();
        self.session.loading_replay = true;
        self.replayed_terminal_prompts.clear();
        self.unexpected_replay_drops = 0;
        self.pending_stop_hooks = None;
        self.clear_send_now_expectation();
        self.optimistic_queue_ids.clear();
        self.send_now_awaiting_confirm = None;
        self.send_now_painted_blocks.clear();
        self.workflow_blocks.clear();
        self.workflow_run_revisions.clear();
        self.cleared_workflow_runs.clear();
        self.workflow_runs.clear();
    }
    /// Open a reconnect reload window: stash the current transcript/tracker
    /// and point the live fields at fresh state for the incoming
    /// `session/load` replay. The transcript is NOT cleared — it stays
    /// recoverable until [`finish_session_reload`](Self::finish_session_reload)
    /// decides the outcome.
    pub(crate) fn begin_session_reload(&mut self, generation: u64) {
        self.dismiss_jump_picker();
        if let Some(prev) = self.session_reload.take() {
            tracing::warn!(
                generation,
                prev_generation = prev.generation,
                "session reload superseded without finalize; restoring previous stash first"
            );
            if self.apply_reload_outcome(prev, false) {
                crate::memory_release::release_retained_memory_with("reload-supersede");
            }
        }
        while self.scrollback.in_batch() {
            self.scrollback.end_batch();
        }
        if let Some(pid) = self.loading_placeholder_id.take() {
            self.scrollback.remove_entry(pid);
        }
        if let Some(rid) = self.pending_recap_entry.take() {
            self.scrollback.remove_entry(rid);
        }
        self.session.model_switch_pending = false;
        self.pending_adoption_updates.clear();
        let fresh = self.scrollback.fresh_continuation();
        self.session_reload = Some(SessionReload {
            generation,
            scrollback: std::mem::replace(&mut self.scrollback, fresh),
            tracker: std::mem::replace(
                &mut self.session.tracker,
                crate::acp::tracker::AcpUpdateTracker::new(),
            ),
            todo: std::mem::take(&mut self.todo),
            workflow_blocks: std::mem::take(&mut self.workflow_blocks),
            workflow_runs: std::mem::take(&mut self.workflow_runs),
            workflow_run_revisions: std::mem::take(&mut self.workflow_run_revisions),
            cleared_workflow_runs: std::mem::take(&mut self.cleared_workflow_runs),
            last_seen_event_id: self.last_seen_event_id.clone(),
            last_applied_event_seq: self.last_applied_event_seq,
            last_applied_xai_event_seq: self.last_applied_xai_event_seq,
            saw_replay: false,
            saw_todo_update: false,
        });
        self.loading_placeholder_id = Some(self.scrollback.push_block(
            crate::scrollback::block::RenderBlock::system("Reloading session after reconnect..."),
        ));
        self.scrollback.begin_batch();
        self.begin_replay_window();
    }
    /// Record that an `isReplay` update applied while a reload window is open.
    /// No-op otherwise.
    pub(crate) fn mark_reload_replay_seen(&mut self) {
        if let Some(reload) = self.session_reload.as_mut() {
            reload.saw_replay = true;
        }
    }
    /// Record that a Plan update applied while a reload window is open.
    /// No-op otherwise.
    pub(crate) fn mark_reload_todo_update(&mut self) {
        if let Some(reload) = self.session_reload.as_mut() {
            reload.saw_todo_update = true;
        }
    }
    /// Start a locally-tracked turn: enter TurnRunning with the turn-scoped
    /// bookkeeping every real turn start must apply, so no caller can miss
    /// it. Deliberately NOT used by server-initiated synthetic turns
    /// (auto-wake / actor runs): they never call `start_turn`.
    pub(crate) fn start_turn_boundary(&mut self, starting_prompt_id: Option<&str>) {
        if self
            .expect_send_now_cancel
            .as_deref()
            .is_some_and(|id| Some(id) != starting_prompt_id)
        {
            self.expect_send_now_cancel = None;
        }
        self.session.start_turn(&mut self.scrollback);
    }
    /// Adopt the in-flight turn another client is driving, conveyed by the
    /// `session/load` response meta (`x.ai/runningPromptId`): enter
    /// TurnRunning and match subsequent live deltas. No user-prompt block is
    /// pushed — the turn's prompt and prior chunks arrived via the replay.
    pub(crate) fn adopt_running_prompt(&mut self, prompt_id: String) {
        self.start_turn_boundary(Some(&prompt_id));
        self.session.tracker.clear_user_echo_skip();
        self.session.current_prompt_id = Some(prompt_id.clone());
        self.turn_started_at = Some(Instant::now());
        self.scrollback.enable_follow_with_preserve();
        self.flush_pending_follow_ups(&prompt_id);
    }
    /// Finalize any open reload window as FAILED, regardless of generation.
    ///
    /// For load initiations that take over the agent (fork/worktree/restore
    /// binding a new session): the stash belongs to the superseded
    /// pre-reconnect state, and an open window would corrupt the incoming
    /// load's batch/replay bookkeeping — and defer its results. The window's
    /// pending re-init completion later no-ops (generation gone).
    pub(crate) fn abort_session_reload(&mut self) {
        if let Some(reload) = self.session_reload.take()
            && self.apply_reload_outcome(reload, false)
        {
            crate::memory_release::release_retained_memory_with("reload-abort");
        }
    }
    /// Finalize the reload window opened for `generation`.
    ///
    /// Returns `false` (untouched state) when no window with that generation
    /// is open — the agent was never reloading, or a newer reconnect already
    /// superseded it.
    pub(crate) fn finish_session_reload(&mut self, generation: u64, success: bool) -> bool {
        match self.session_reload.take() {
            Some(reload) if reload.generation == generation => {
                if self.apply_reload_outcome(reload, success) {
                    crate::memory_release::release_retained_memory_with("reload-finalize");
                }
                true
            }
            Some(other) => {
                tracing::warn!(
                    generation,
                    open_generation = other.generation,
                    "ignoring session reload finalize for a superseded generation"
                );
                self.session_reload = Some(other);
                false
            }
            None => false,
        }
    }
    /// Whether a running prompt reported on a `session/load` (resume /
    /// reconnect) is adoptable by THIS agent: the pure synthetic-turn guard
    /// ([`acp_handler::should_adopt_running_prompt`]) AND not terminal-in-replay.
    /// A turn whose durable `TurnCompleted` already arrived in this load's replay
    /// (recorded in [`Self::replayed_terminal_prompts`]) has ended; adopting it
    /// would re-strand the viewer on "Waiting…".
    ///
    /// [`acp_handler::should_adopt_running_prompt`]: crate::app::acp_handler::should_adopt_running_prompt
    pub(crate) fn should_adopt_running_prompt(&self, prompt_id: &str) -> bool {
        crate::app::acp_handler::should_adopt_running_prompt(prompt_id)
            && !self.replayed_terminal_prompts.contains(prompt_id)
            && !self.is_rewound_prompt(prompt_id)
    }
    /// Finalize a reconnect-reload window and, iff the running prompt is
    /// adoptable, adopt it. Returns whether the window finalized.
    ///
    /// Adoption is gated by [`Self::should_adopt_running_prompt`] and ordered
    /// AFTER finalize so the finalize side effect (force-idle + window resolve)
    /// always runs even when adoption is skipped for a synthetic / non-adoptable
    /// / terminal-in-replay running id. The reconnect loop in `event_loop.rs`
    /// calls this per agent.
    pub(crate) fn finalize_reload_and_maybe_adopt(
        &mut self,
        generation: u64,
        ok: bool,
        running_prompt_id: Option<String>,
    ) -> bool {
        let finalized = self.finish_session_reload(generation, ok);
        if finalized
            && let Some(pid) = running_prompt_id
            && self.should_adopt_running_prompt(&pid)
        {
            self.adopt_running_prompt(pid);
        }
        finalized
    }
    /// Resolve a closed window per the [`SessionReload`] outcome trichotomy.
    ///
    /// Returns whether a heavy transient was dropped — the stashed pre-reload
    /// scrollback (success + full replay) or the staged partial replay
    /// (failure). The success+cursor branch *reuses* the stash and moves the
    /// tail entries into it: nothing multi-MB drops, so callers must NOT
    /// purge for it (a full-arena purge there would madvise away warm pages
    /// on the most common reconnect outcome, once per open tab).
    #[must_use = "purge retained memory iff a heavy transient dropped"]
    fn apply_reload_outcome(&mut self, reload: SessionReload, success: bool) -> bool {
        if let Some(pid) = self.loading_placeholder_id.take() {
            self.scrollback.remove_entry(pid);
        }
        let dropped_heavy;
        if success && reload.saw_replay {
            self.scrollback.end_batch();
            dropped_heavy = true;
        } else if success {
            let tail = std::mem::replace(&mut self.scrollback, reload.scrollback);
            self.scrollback.append_entries_from(tail);
            self.workflow_blocks.extend(reload.workflow_blocks);
            {
                let mut live_by_id: HashMap<String, _> = std::mem::take(&mut self.workflow_runs)
                    .into_iter()
                    .map(|run| (run.run_id.clone(), run))
                    .collect();
                let mut merged = Vec::with_capacity(reload.workflow_runs.len() + live_by_id.len());
                for run in reload.workflow_runs {
                    if let Some(live) = live_by_id.remove(&run.run_id) {
                        merged.push(live);
                    } else {
                        merged.push(run);
                    }
                }
                let mut live_only: Vec<_> = live_by_id.into_values().collect();
                live_only.sort_by_key(|run| run.received_at);
                merged.extend(live_only);
                self.cleared_workflow_runs
                    .extend(reload.cleared_workflow_runs);
                merged.retain(|run| !self.cleared_workflow_runs.contains(&run.run_id));
                self.workflow_runs = merged;
            }
            for (run_id, rev) in reload.workflow_run_revisions {
                self.workflow_run_revisions
                    .entry(run_id)
                    .and_modify(|live| *live = (*live).max(rev))
                    .or_insert(rev);
            }
            if !reload.saw_todo_update {
                self.todo = reload.todo;
            }
            dropped_heavy = false;
        } else {
            let floor = self.scrollback.id_floor();
            let staging_generations = self.scrollback.invalidation_generations();
            self.scrollback = reload.scrollback;
            self.scrollback.raise_id_floor(floor);
            self.scrollback
                .raise_invalidation_floor(staging_generations);
            self.session.tracker = reload.tracker;
            self.todo = reload.todo;
            self.workflow_blocks = reload.workflow_blocks;
            self.workflow_runs = reload.workflow_runs;
            self.workflow_run_revisions = reload.workflow_run_revisions;
            self.cleared_workflow_runs = reload.cleared_workflow_runs;
            self.last_seen_event_id = reload.last_seen_event_id;
            self.last_applied_event_seq = reload.last_applied_event_seq;
            self.last_applied_xai_event_seq = reload.last_applied_xai_event_seq;
            dropped_heavy = true;
        }
        self.session.loading_replay = false;
        self.session.prompt_history_loading = false;
        self.session.tracker.clear_user_echo_skip();
        self.session.finish_turn(&mut self.scrollback);
        self.scrollback.finish_all_running();
        if let Some(id) = self.pending_recap_entry.take() {
            self.scrollback.remove_entry(id);
        }
        self.mark_turn_finished();
        self.activity_started_at = None;
        self.last_activity = None;
        self.reset_follow_ups_for_reload();
        dropped_heavy
    }
    /// Effective turn elapsed time, excluding time spent in question views.
    ///
    /// Subtracts both the accumulated `turn_paused_duration` (from previously
    /// closed question views) and the time elapsed since the current question
    /// view opened (if one is active).
    pub fn turn_elapsed(&self) -> Option<std::time::Duration> {
        let raw = self.turn_started_at?.elapsed();
        let mut paused = self.turn_paused_duration;
        if let Some(qv) = &self.question_view {
            paused += qv.opened_at.elapsed();
        }
        Some(raw.saturating_sub(paused))
    }
    /// Turn activity for the status spinner, with the implicit "no activity"
    /// gap during a running inference turn resolved into an explicit
    /// [`WaitingReason`] so the spinner names *what* we're waiting on.
    ///
    /// The tracker already returns `Waiting(TaskOutput/TasksComplete/Sleep)`,
    /// and `Waiting(Subagent)` for a foreground `task` call from the moment it's
    /// issued. This fills in the remaining gap: if no tracker activity but a
    /// foreground subagent is registered as running, it's still `Subagent`
    /// (covers any window where the task tool call has cleared but the child is
    /// live); otherwise the model itself (`Model`). Bash turns keep `None` so
    /// the status line renders its own "Running…".
    ///
    /// For `Waiting(TaskOutput { task_ids, .. })`, also resolves a display
    /// `subject` from live bg-task / subagent state (description preferred,
    /// else command) so the spinner can read `{description}…`.
    pub(crate) fn resolve_turn_activity(&self) -> Option<crate::acp::tracker::TurnActivity> {
        use crate::acp::tracker::{TurnActivity, WaitingReason};
        use crate::app::agent::AgentState;
        if let Some(activity) = self.session.turn_activity() {
            return Some(self.enrich_waiting_activity(activity));
        }
        if !matches!(self.session.state, AgentState::TurnRunning) {
            return None;
        }
        if self.bash_turn {
            return None;
        }
        let reason = if self.has_running_foreground_subagent() {
            WaitingReason::Subagent
        } else {
            WaitingReason::Model
        };
        Some(TurnActivity::Waiting(reason))
    }
    /// Fill in a `TaskOutput` wait's display subject from live task state.
    fn enrich_waiting_activity(
        &self,
        activity: crate::acp::tracker::TurnActivity,
    ) -> crate::acp::tracker::TurnActivity {
        use crate::acp::tracker::{TurnActivity, WaitingReason};
        match activity {
            TurnActivity::Waiting(WaitingReason::TaskOutput {
                task_ids, waits, ..
            }) => {
                let subject = self.subject_for_wait_tasks(&task_ids);
                TurnActivity::Waiting(WaitingReason::TaskOutput {
                    task_ids,
                    subject,
                    waits,
                })
            }
            other => other,
        }
    }
    /// Best user-facing name for the tasks being waited on.
    ///
    /// Uses the first resolvable subject. Multi-id waits always reflect the
    /// full `task_ids` length (`"first + N more"` with `N = task_ids.len()-1`)
    /// so partial resolution still reads as multi-task. Unknown ids → `None`
    /// (spinner falls back to the generic label).
    fn subject_for_wait_tasks(&self, task_ids: &[String]) -> Option<String> {
        use crate::acp::tracker::{MAX_ACTIVITY_SUBJECT_CHARS, clamp_activity_subject};
        if task_ids.is_empty() {
            return None;
        }
        let first = task_ids
            .iter()
            .find_map(|id| self.lookup_task_subject(id))?;
        if task_ids.len() == 1 {
            let first = clamp_activity_subject(&first);
            return (!first.is_empty()).then_some(first);
        }
        let n = task_ids.len() - 1;
        let suffix = format!(" + {n} more");
        let budget = MAX_ACTIVITY_SUBJECT_CHARS
            .saturating_sub(suffix.chars().count())
            .max(8);
        let base: String = first
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or(first.trim())
            .chars()
            .take(budget)
            .collect();
        if base.is_empty() {
            None
        } else {
            Some(format!("{base}{suffix}"))
        }
    }
    /// Resolve one task id to a display subject (description preferred, else
    /// a *short* command / subagent description).
    ///
    /// Long bare commands are intentionally not used as subjects — the spinner
    /// falls back to the generic `"Waiting on task output…"` instead of
    /// stuffing a wall of shell into the status line. Descriptions are kept
    /// but clamped by the caller via [`clamp_activity_subject`].
    fn lookup_task_subject(&self, task_id: &str) -> Option<String> {
        use crate::acp::tracker::MAX_ACTIVITY_SUBJECT_CHARS;
        fn first_nonempty_line(s: &str) -> &str {
            s.lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or(s)
        }
        if let Some(task) = self.session.bg_tasks.get(task_id) {
            if let Some(desc) = task
                .description
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(first_nonempty_line(desc).to_string());
            }
            let cmd = first_nonempty_line(task.command.trim());
            if !cmd.is_empty() && cmd.chars().count() <= MAX_ACTIVITY_SUBJECT_CHARS {
                return Some(cmd.to_string());
            }
        }
        if let Some(info) = self.subagent_sessions.get(task_id) {
            let desc = info.description.trim();
            if !desc.is_empty() {
                return Some(first_nonempty_line(desc).to_string());
            }
        }
        self.subagent_sessions
            .values()
            .find(|info| info.subagent_id.as_ref() == task_id)
            .and_then(|info| {
                let desc = info.description.trim();
                if desc.is_empty() {
                    None
                } else {
                    Some(first_nonempty_line(desc).to_string())
                }
            })
    }
    /// Whether a foreground subagent (`task`/`spawn_subagent`, not
    /// `run_in_background`) is currently running. The parent turn is blocked on
    /// it, so the spinner should read "Waiting on subagent…".
    fn has_running_foreground_subagent(&self) -> bool {
        self.subagent_sessions
            .values()
            .any(|s| s.is_running() && !s.is_background && s.workflow_run_id.is_none())
    }
    /// Update context state with a full snapshot from live callers.
    ///
    /// No-op for gateway/chat-kind sessions — local GetSessionInfo / sampler
    /// breakdowns must not populate the context bar (remote owns context).
    pub fn apply_full_context_info(&mut self, next: xai_grok_shell::session::ContextInfo) {
        if self.chat_kind {
            self.context_state = None;
            return;
        }
        self.context_state = Some(next);
    }
    /// Update context state from a streaming notification carrying only
    /// `used` and `total` fields.
    ///
    /// No-op for gateway/chat-kind sessions (same policy as
    /// [`Self::apply_full_context_info`]).
    pub fn apply_context_used(&mut self, used: u64, total: u64) {
        if self.chat_kind {
            self.context_state = None;
            return;
        }
        let total = if total > 0 {
            total
        } else {
            self.context_state.as_ref().map(|s| s.total).unwrap_or(0)
        };
        match self.context_state.as_mut() {
            Some(snap) => {
                snap.used = used;
                if total > 0 {
                    snap.total = total;
                }
                snap.usage_pct = xai_token_estimation::usage_percentage_u8(used, snap.total);
                snap.free_tokens = xai_token_estimation::free_tokens(snap.total, used);
            }
            None => {
                self.context_state = Some(xai_grok_shell::session::ContextInfo::from_notification(
                    used, total,
                ));
            }
        }
    }
    /// Apply Build coding-credit balance only for non-chat agents.
    /// Gateway/chat-kind sessions keep credits unset so bars/warnings stay off.
    pub fn apply_credit_balance(
        &mut self,
        balance: Option<crate::views::credit_bar::CreditBalance>,
        auto_topup: Option<crate::views::credit_bar::AutoTopupInfo>,
    ) {
        if self.chat_kind {
            self.credit_balance = None;
            self.auto_topup = None;
            return;
        }
        self.credit_balance = balance;
        self.auto_topup = auto_topup;
    }
    /// Record a key event to the input flight recorder.
    ///
    /// Zero heap allocations — stores raw `Copy` types in the ring buffer.
    /// Formatting into strings happens only during dump (`snapshot_entries`).
    pub(crate) fn record_input(
        &mut self,
        key: &crossterm::event::KeyEvent,
        outcome: &InputOutcome,
    ) {
        use crate::input_log::{ActivePaneSnapshot, OutcomeSnapshot, RawInputEntry};
        use std::time::{SystemTime, UNIX_EPOCH};
        let delta = std::mem::take(&mut self.prompt.last_input_delta);
        let pane = match self.active_pane {
            ActivePane::Scrollback => ActivePaneSnapshot::Scrollback,
            ActivePane::Todo => ActivePaneSnapshot::Todo,
            ActivePane::Queue => ActivePaneSnapshot::Queue,
            ActivePane::Prompt => ActivePaneSnapshot::Prompt,
            ActivePane::Tasks => ActivePaneSnapshot::Tasks,
            ActivePane::Catalog => ActivePaneSnapshot::Catalog,
        };
        let outcome_snap = match outcome {
            InputOutcome::Changed | InputOutcome::ArmPending { .. } => OutcomeSnapshot::Changed,
            InputOutcome::Unchanged => OutcomeSnapshot::Unchanged,
            InputOutcome::Action(_)
            | InputOutcome::ActionThenForward(_)
            | InputOutcome::ActionPair(_, _) => OutcomeSnapshot::Action,
        };
        self.input_log.push(RawInputEntry {
            wall_ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            key_code: key.code,
            key_modifiers: key.modifiers,
            key_kind: key.kind,
            active_pane: pane,
            outcome: outcome_snap,
            cursor_before: delta.cursor_before,
            cursor_after: delta.cursor_after,
            text_len_before: delta.text_len_before,
            text_len_after: delta.text_len_after,
            sel_before: delta.had_selection_before,
            sel_after: delta.had_selection_after,
            textarea_changed: delta.textarea_changed,
        });
    }
    /// Set the sharing-enabled flag on this view and propagate it to the
    /// slash-command registry so the `/share` entry stays hidden/visible in
    /// lockstep with `AgentView::sharing_enabled`. Use this instead of
    /// mutating `sharing_enabled` directly when a new agent is created or a
    /// session is loaded, so the field and registry can't drift.
    pub fn set_sharing_enabled(&mut self, enabled: bool) {
        self.sharing_enabled = enabled;
        self.prompt
            .slash_controller
            .registry_mut()
            .set_share_visible(enabled);
    }
    /// Set [`Self::billing_surface_visible`] (see the field doc) and mirror it
    /// into this agent's slash controller, so the two can't drift.
    pub fn set_billing_surface_visible(&mut self, visible: bool) {
        self.billing_surface_visible = visible;
        self.prompt
            .slash_controller
            .set_billing_surface_visible(visible);
    }
    /// Replace the restricted slash-command deny list in this agent's
    /// registry (e.g. `/usage` denied on the free / X Basic tiers). Deny
    /// wins over every `set_*_visible` gate.
    pub fn set_restricted_commands(&mut self, names: &[String]) {
        self.prompt.set_restricted_commands(names);
    }
    /// Show or hide the `/dashboard` slash command in this agent's registry.
    /// Driven by the dashboard feature flag
    /// (`crate::views::dashboard::dashboard_enabled()`) at agent-creation
    /// time — independent of leader mode.
    pub fn set_dashboard_visible(&mut self, visible: bool) {
        self.prompt
            .slash_controller
            .registry_mut()
            .set_dashboard_visible(visible);
    }
    /// Offer `/announcements` when session announcements (critical or promo) exist.
    pub fn set_has_session_announcements(&mut self, has: bool) {
        self.prompt
            .slash_controller
            .set_has_session_announcements(has);
    }
    /// One place for the app-scoped gates a new/adopted session inherits so the session-creation sites cannot drift.
    pub(crate) fn apply_app_scoped_gates(
        &mut self,
        sharing_enabled: bool,
        billing_surface_visible: bool,
        chat_mode: bool,
        screen_mode: crate::app::ScreenMode,
        announcements: &[xai_grok_announcements::RemoteAnnouncement],
        restricted_commands: &[String],
    ) {
        self.set_sharing_enabled(sharing_enabled);
        self.set_billing_surface_visible(billing_surface_visible);
        self.app_chat_mode = chat_mode;
        self.prompt.set_screen_mode(screen_mode);
        self.set_dashboard_visible(crate::views::dashboard::dashboard_enabled());
        self.set_has_session_announcements(crate::views::announcements::has_session_announcements(
            announcements,
        ));
        self.set_restricted_commands(restricted_commands);
    }
    /// Show or hide the `/recap` slash command in this agent's registry.
    pub fn set_session_recap_available(&mut self, available: bool) {
        self.prompt.set_recap_visible(available);
    }
    /// Show or hide the `/voice` slash command in this agent's registry,
    /// gated on the runtime voice gate (GA default on; kill switch may hide).
    pub fn set_voice_mode_available(&mut self, available: bool) {
        self.prompt.set_voice_visible(available);
    }
}
#[cfg(test)]
mod resolve_turn_activity_tests {
    use super::*;
    use crate::acp::tracker::{TurnActivity, WaitingReason};
    use crate::app::agent::AgentState;
    fn running_view() -> AgentView {
        let mut view = test_agent_view(Some("s1"), std::path::PathBuf::from("/tmp"));
        view.session.state = AgentState::TurnRunning;
        view
    }
    #[test]
    fn idle_turn_has_no_activity() {
        let view = test_agent_view(Some("s1"), std::path::PathBuf::from("/tmp"));
        assert_eq!(view.resolve_turn_activity(), None);
    }
    #[test]
    fn running_with_no_stream_waits_on_model() {
        let view = running_view();
        assert_eq!(
            view.resolve_turn_activity(),
            Some(TurnActivity::Waiting(WaitingReason::Model))
        );
    }
    #[test]
    fn bash_turn_stays_none() {
        let mut view = running_view();
        view.bash_turn = true;
        assert_eq!(view.resolve_turn_activity(), None);
    }
    #[test]
    fn real_activity_passes_through() {
        let mut view = running_view();
        view.session
            .set_compaction_activity(Some(TurnActivity::AutoCompacting));
        assert_eq!(
            view.resolve_turn_activity(),
            Some(TurnActivity::AutoCompacting)
        );
    }
    /// When waiting on task output, the spinner subject is the bg task's
    /// description (preferred over the raw command).
    #[test]
    fn task_output_wait_uses_bg_task_description() {
        use crate::acp::meta::NotificationMeta;
        use crate::app::agent::{BgTaskState, BgTaskStatus};
        use agent_client_protocol as acp;
        use std::sync::Arc;
        use std::time::SystemTime;
        let mut view = running_view();
        view.session.bg_tasks.insert(
            "bg-1".into(),
            BgTaskState {
                task_id: "bg-1".into(),
                tool_call_id: "tc-1".into(),
                command: "cargo test --release".into(),
                description: Some("run release tests".into()),
                cwd: String::new(),
                output_file: String::new(),
                status: BgTaskStatus::Running,
                start_time: SystemTime::now(),
                end_time: None,
                exit_code: None,
                signal: None,
                stdout: String::new(),
                stdout_line_count: 0,
                truncated: false,
                pending_kill: false,
                kill_requested_at: None,
                scrollback_entry_id: None,
                is_monitor: false,
                restored_from_replay: false,
            },
        );
        let meta = NotificationMeta::default();
        view.session.handle_update(
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(
                    acp::ToolCallId::new(Arc::from("wait-1")),
                    "get_command_or_subagent_output",
                )
                .kind(acp::ToolKind::Other)
                .status(acp::ToolCallStatus::Pending)
                .content(vec![])
                .locations(vec![]),
            ),
            &meta,
            &mut view.scrollback,
        );
        view.session.handle_update(
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                acp::ToolCallId::new(Arc::from("wait-1")),
                acp::ToolCallUpdateFields::new().raw_input(Some(serde_json::json!(
                    { "task_ids" : ["bg-1"], "timeout_ms" : 30_000, }
                ))),
            )),
            &meta,
            &mut view.scrollback,
        );
        let activity = view.resolve_turn_activity();
        assert_eq!(
            activity,
            Some(TurnActivity::Waiting(WaitingReason::TaskOutput {
                task_ids: vec!["bg-1".into()],
                subject: Some("run release tests".into()),
                waits: true,
            }))
        );
        assert_eq!(activity.as_ref().unwrap().as_label(), "waiting_task_output");
        let TurnActivity::Waiting(reason) = activity.unwrap() else {
            panic!("expected waiting activity");
        };
        assert_eq!(reason.label(), "run release tests…");
    }
    /// Without a description, a short command is used as the subject.
    #[test]
    fn task_output_wait_falls_back_to_short_command() {
        use crate::acp::meta::NotificationMeta;
        use crate::app::agent::{BgTaskState, BgTaskStatus};
        use agent_client_protocol as acp;
        use std::sync::Arc;
        use std::time::SystemTime;
        let mut view = running_view();
        view.session.bg_tasks.insert(
            "bg-2".into(),
            BgTaskState {
                task_id: "bg-2".into(),
                tool_call_id: "tc-2".into(),
                command: "sleep 30".into(),
                description: None,
                cwd: String::new(),
                output_file: String::new(),
                status: BgTaskStatus::Running,
                start_time: SystemTime::now(),
                end_time: None,
                exit_code: None,
                signal: None,
                stdout: String::new(),
                stdout_line_count: 0,
                truncated: false,
                pending_kill: false,
                kill_requested_at: None,
                scrollback_entry_id: None,
                is_monitor: false,
                restored_from_replay: false,
            },
        );
        let meta = NotificationMeta::default();
        view.session.handle_update(
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(acp::ToolCallId::new(Arc::from("wait-2")), "get_task_output")
                    .kind(acp::ToolKind::Other)
                    .status(acp::ToolCallStatus::Pending)
                    .content(vec![])
                    .raw_input(Some(serde_json::json!(
                        { "task_ids" : ["bg-2"], "timeout_ms" : 5_000, }
                    )))
                    .locations(vec![]),
            ),
            &meta,
            &mut view.scrollback,
        );
        let activity = view.resolve_turn_activity().expect("activity");
        let TurnActivity::Waiting(reason) = activity else {
            panic!("expected waiting: {activity:?}");
        };
        assert_eq!(reason.label(), "sleep 30…");
    }
    /// Multi-id waits use full task_ids.len() for "+ N more", not just resolved count.
    #[test]
    fn task_output_wait_multi_id_uses_full_task_count() {
        use crate::acp::meta::NotificationMeta;
        use crate::app::agent::{BgTaskState, BgTaskStatus};
        use agent_client_protocol as acp;
        use std::sync::Arc;
        use std::time::SystemTime;
        let mut view = running_view();
        view.session.bg_tasks.insert(
            "bg-a".into(),
            BgTaskState {
                task_id: "bg-a".into(),
                tool_call_id: "tc-a".into(),
                command: "echo a".into(),
                description: Some("alpha task".into()),
                cwd: String::new(),
                output_file: String::new(),
                status: BgTaskStatus::Running,
                start_time: SystemTime::now(),
                end_time: None,
                exit_code: None,
                signal: None,
                stdout: String::new(),
                stdout_line_count: 0,
                truncated: false,
                pending_kill: false,
                kill_requested_at: None,
                scrollback_entry_id: None,
                is_monitor: false,
                restored_from_replay: false,
            },
        );
        let meta = NotificationMeta::default();
        view.session.handle_update(
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(
                    acp::ToolCallId::new(Arc::from("wait-multi")),
                    "get_task_output",
                )
                .kind(acp::ToolKind::Other)
                .status(acp::ToolCallStatus::Pending)
                .content(vec![])
                .raw_input(Some(serde_json::json!(
                    { "task_ids" : ["bg-a", "missing-b", "missing-c"],
                    "timeout_ms" : 5_000, }
                )))
                .locations(vec![]),
            ),
            &meta,
            &mut view.scrollback,
        );
        let activity = view.resolve_turn_activity().expect("activity");
        let TurnActivity::Waiting(reason) = activity else {
            panic!("expected waiting: {activity:?}");
        };
        assert_eq!(
            reason.label(),
            "alpha task + 2 more…",
            "N more is based on full task_ids length, not resolved count"
        );
    }
    /// Long first subjects still keep the multi-task suffix after clamping.
    #[test]
    fn task_output_wait_multi_id_preserves_suffix_when_first_is_long() {
        use crate::acp::meta::NotificationMeta;
        use crate::acp::tracker::MAX_ACTIVITY_SUBJECT_CHARS;
        use crate::app::agent::{BgTaskState, BgTaskStatus};
        use agent_client_protocol as acp;
        use std::sync::Arc;
        use std::time::SystemTime;
        let long_desc = "L".repeat(80);
        let mut view = running_view();
        view.session.bg_tasks.insert(
            "bg-long".into(),
            BgTaskState {
                task_id: "bg-long".into(),
                tool_call_id: "tc-long".into(),
                command: "echo long".into(),
                description: Some(long_desc),
                cwd: String::new(),
                output_file: String::new(),
                status: BgTaskStatus::Running,
                start_time: SystemTime::now(),
                end_time: None,
                exit_code: None,
                signal: None,
                stdout: String::new(),
                stdout_line_count: 0,
                truncated: false,
                pending_kill: false,
                kill_requested_at: None,
                scrollback_entry_id: None,
                is_monitor: false,
                restored_from_replay: false,
            },
        );
        let meta = NotificationMeta::default();
        view.session.handle_update(
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(
                    acp::ToolCallId::new(Arc::from("wait-long-multi")),
                    "get_task_output",
                )
                .kind(acp::ToolKind::Other)
                .status(acp::ToolCallStatus::Pending)
                .content(vec![])
                .raw_input(Some(serde_json::json!(
                    { "task_ids" : ["bg-long", "missing-b"], "timeout_ms" :
                    5_000, }
                )))
                .locations(vec![]),
            ),
            &meta,
            &mut view.scrollback,
        );
        let activity = view.resolve_turn_activity().expect("activity");
        let TurnActivity::Waiting(reason) = activity else {
            panic!("expected waiting: {activity:?}");
        };
        let label = reason.label();
        assert!(
            label.contains(" + 1 more"),
            "multi-task suffix must survive clamp: {label}"
        );
        assert!(label.ends_with('…'));
        let body = label.strip_suffix('…').unwrap();
        assert!(
            body.chars().count() <= MAX_ACTIVITY_SUBJECT_CHARS + 20,
            "unexpectedly long body: {body}"
        );
    }
    /// get_task_output often passes subagent_id, not the child_session_id map key.
    #[test]
    fn task_output_wait_resolves_subagent_by_subagent_id() {
        use crate::acp::meta::NotificationMeta;
        use crate::app::subagent::SubagentInfo;
        use agent_client_protocol as acp;
        use std::sync::Arc;
        use std::time::Instant;
        let mut view = running_view();
        let now = Instant::now();
        view.subagent_sessions.insert(
            "child-session-xyz".into(),
            SubagentInfo {
                subagent_id: Arc::from("sub-id-42"),
                child_session_id: Arc::from("child-session-xyz"),
                description: Arc::from("explore the auth module"),
                subagent_type: Arc::from("explore"),
                persona: None,
                role: None,
                model: None,
                context_source: None,
                resumed_from: None,
                capability_mode: None,
                workflow_run_id: None,
                context_normalized: false,
                parent_prompt_id: None,
                started_at: now,
                last_progress_at: now,
                finished: false,
                status: None,
                error: None,
                duration_ms: None,
                tool_calls: None,
                turns: None,
                turn_count: None,
                tool_call_count: None,
                tokens_used: None,
                context_window_tokens: None,
                context_usage_pct: None,
                tools_used: vec![],
                error_count: None,
                activity_label: None,
                is_background: true,
                pending_kill: false,
                kill_requested_at: None,
                scrollback_entry_id: None,
                prompt: None,
                child_cwd: None,
                worktree_path: None,
                child_updates_replayed: false,
            },
        );
        let meta = NotificationMeta::default();
        view.session.handle_update(
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(
                    acp::ToolCallId::new(Arc::from("wait-sub")),
                    "get_command_or_subagent_output",
                )
                .kind(acp::ToolKind::Other)
                .status(acp::ToolCallStatus::Pending)
                .content(vec![])
                .raw_input(Some(serde_json::json!(
                    { "task_ids" : ["sub-id-42"], "timeout_ms" : 10_000, }
                )))
                .locations(vec![]),
            ),
            &meta,
            &mut view.scrollback,
        );
        let activity = view.resolve_turn_activity().expect("activity");
        let TurnActivity::Waiting(reason) = activity else {
            panic!("expected waiting: {activity:?}");
        };
        assert_eq!(reason.label(), "explore the auth module…");
    }
    /// Long bare commands are not used as subjects — keep the original label.
    #[test]
    fn task_output_wait_long_command_keeps_generic_label() {
        use crate::acp::meta::NotificationMeta;
        use crate::app::agent::{BgTaskState, BgTaskStatus};
        use agent_client_protocol as acp;
        use std::sync::Arc;
        use std::time::SystemTime;
        let long_cmd = "cargo test --release --workspace --all-features -- --nocapture".to_string();
        assert!(
            long_cmd.chars().count() > 40,
            "fixture must exceed the short-command threshold"
        );
        let mut view = running_view();
        view.session.bg_tasks.insert(
            "bg-3".into(),
            BgTaskState {
                task_id: "bg-3".into(),
                tool_call_id: "tc-3".into(),
                command: long_cmd,
                description: None,
                cwd: String::new(),
                output_file: String::new(),
                status: BgTaskStatus::Running,
                start_time: SystemTime::now(),
                end_time: None,
                exit_code: None,
                signal: None,
                stdout: String::new(),
                stdout_line_count: 0,
                truncated: false,
                pending_kill: false,
                kill_requested_at: None,
                scrollback_entry_id: None,
                is_monitor: false,
                restored_from_replay: false,
            },
        );
        let meta = NotificationMeta::default();
        view.session.handle_update(
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(acp::ToolCallId::new(Arc::from("wait-3")), "get_task_output")
                    .kind(acp::ToolKind::Other)
                    .status(acp::ToolCallStatus::Pending)
                    .content(vec![])
                    .raw_input(Some(serde_json::json!(
                        { "task_ids" : ["bg-3"], "timeout_ms" : 5_000, }
                    )))
                    .locations(vec![]),
            ),
            &meta,
            &mut view.scrollback,
        );
        let activity = view.resolve_turn_activity().expect("activity");
        let TurnActivity::Waiting(reason) = activity else {
            panic!("expected waiting: {activity:?}");
        };
        assert_eq!(
            reason.label(),
            "Waiting on task output…",
            "long command without description must not become the spinner subject"
        );
        assert_eq!(
            reason,
            WaitingReason::TaskOutput {
                task_ids: vec!["bg-3".into()],
                subject: None,
                waits: true,
            }
        );
    }
}
#[cfg(test)]
mod status_window_tests {
    use super::super::test_agent_view;
    #[test]
    fn start_turn_boundary_enters_turn_running() {
        let mut agent = test_agent_view(Some("s1"), std::path::PathBuf::from("/tmp"));
        agent.start_turn_boundary(None);
        assert!(agent.session.state.is_turn_running());
    }
    #[test]
    fn session_rebind_and_replay_invalidate_minimal_btw() {
        let mut agent = test_agent_view(Some("s1"), std::path::PathBuf::from("/tmp"));
        let old_request = crate::minimal_api::start_minimal_btw(&mut agent, "old question".into());
        agent.bind_session_id(agent_client_protocol::SessionId::new("s2"));
        assert!(agent.btw_state.is_none());
        assert!(agent.minimal_btw_lifecycle.is_none());
        assert!(!crate::minimal_api::finish_minimal_btw(
            &mut agent,
            old_request,
            Ok("old answer".into())
        ));
        assert!(agent.btw_state.is_none());
        let replay_request =
            crate::minimal_api::start_minimal_btw(&mut agent, "pre-replay question".into());
        agent.begin_replay_window();
        assert!(agent.btw_state.is_none());
        assert!(agent.minimal_btw_lifecycle.is_none());
        assert!(!crate::minimal_api::finish_minimal_btw(
            &mut agent,
            replay_request,
            Ok("pre-replay answer".into())
        ));
        assert!(agent.btw_state.is_none());
    }
}
#[cfg(test)]
mod reconnect_workflow_maps_tests {
    use super::super::test_agent_view;
    use crate::views::workflows::WorkflowRunSnapshot;
    fn wf_snapshot(run_id: &str, status: &str) -> WorkflowRunSnapshot {
        WorkflowRunSnapshot {
            run_id: run_id.to_string(),
            name: "deep-research".to_string(),
            objective: "obj".to_string(),
            status: status.to_string(),
            management_available: true,
            builtin: false,
            phases: Vec::new(),
            current_phase: None,
            agents: Vec::new(),
            agent_budget: None,
            agents_used: 0,
            agents_reserved: 0,
            agents_remaining: None,
            agent_usage_incomplete: false,
            active_agents: 0,
            elapsed_ms: 1_000,
            received_at: std::time::Instant::now(),
            pause_message: None,
            result_summary: None,
        }
    }
    #[test]
    fn cursor_reconnect_restores_stashed_workflow_run_maps() {
        let mut agent = test_agent_view(Some("s1"), std::path::PathBuf::from("/tmp"));
        agent.workflow_runs.push(wf_snapshot("wf-1", "active"));
        agent.workflow_run_revisions.insert("wf-1".to_string(), 4);
        agent.cleared_workflow_runs.insert("wf-old".to_string());
        agent.begin_session_reload(1);
        assert!(
            agent.workflow_runs.is_empty()
                && agent.workflow_run_revisions.is_empty()
                && agent.cleared_workflow_runs.is_empty(),
            "staging starts empty for all three maps"
        );
        assert!(agent.finish_session_reload(1, true));
        assert_eq!(
            agent.workflow_runs.len(),
            1,
            "run list must be restored from the stash on cursor reconnect"
        );
        assert_eq!(agent.workflow_runs[0].run_id, "wf-1");
        assert_eq!(agent.workflow_runs[0].status, "active");
        assert_eq!(
            agent.workflow_run_revisions.get("wf-1").copied(),
            Some(4),
            "revision highwater must survive so stale re-deliveries still dedupe"
        );
        assert!(
            agent.cleared_workflow_runs.contains("wf-old"),
            "clear tombstones must survive cursor reconnect"
        );
    }
    #[test]
    fn cursor_reconnect_prefers_live_workflow_maps_over_stash() {
        let mut agent = test_agent_view(Some("s1"), std::path::PathBuf::from("/tmp"));
        agent.workflow_runs.push(wf_snapshot("wf-1", "active"));
        agent
            .workflow_runs
            .push(wf_snapshot("wf-stash-only", "active"));
        agent.workflow_run_revisions.insert("wf-1".to_string(), 3);
        agent
            .workflow_run_revisions
            .insert("wf-stash-only".to_string(), 1);
        agent.cleared_workflow_runs.insert("wf-old".to_string());
        agent.begin_session_reload(1);
        agent.workflow_runs.push(wf_snapshot("wf-1", "complete"));
        agent
            .workflow_runs
            .push(wf_snapshot("wf-live-only", "active"));
        agent.workflow_run_revisions.insert("wf-1".to_string(), 5);
        agent
            .workflow_run_revisions
            .insert("wf-live-only".to_string(), 2);
        agent.cleared_workflow_runs.insert("wf-new".to_string());
        assert!(agent.finish_session_reload(1, true));
        let by_id: std::collections::HashMap<_, _> = agent
            .workflow_runs
            .iter()
            .map(|r| (r.run_id.as_str(), r.status.as_str()))
            .collect();
        assert_eq!(
            by_id.get("wf-1").copied(),
            Some("complete"),
            "live staging snapshot wins for a shared run_id"
        );
        assert_eq!(
            by_id.get("wf-stash-only").copied(),
            Some("active"),
            "stash-only runs are restored"
        );
        assert_eq!(
            by_id.get("wf-live-only").copied(),
            Some("active"),
            "live-only runs are kept"
        );
        assert_eq!(
            agent.workflow_run_revisions.get("wf-1").copied(),
            Some(5),
            "max revision per run_id"
        );
        assert_eq!(
            agent.workflow_run_revisions.get("wf-stash-only").copied(),
            Some(1)
        );
        assert_eq!(
            agent.workflow_run_revisions.get("wf-live-only").copied(),
            Some(2)
        );
        assert!(agent.cleared_workflow_runs.contains("wf-old"));
        assert!(agent.cleared_workflow_runs.contains("wf-new"));
    }
    #[test]
    fn cursor_reconnect_does_not_resurrect_cleared_runs() {
        let mut agent = test_agent_view(Some("s1"), std::path::PathBuf::from("/tmp"));
        agent.workflow_runs.push(wf_snapshot("wf-1", "active"));
        agent.workflow_runs.push(wf_snapshot("wf-keep", "active"));
        agent
            .workflow_runs
            .push(wf_snapshot("wf-stash-survivor", "active"));
        agent.workflow_run_revisions.insert("wf-1".to_string(), 2);
        agent
            .workflow_run_revisions
            .insert("wf-keep".to_string(), 1);
        agent
            .workflow_run_revisions
            .insert("wf-stash-survivor".to_string(), 1);
        agent.begin_session_reload(1);
        agent.workflow_runs.push(wf_snapshot("wf-keep", "complete"));
        agent.cleared_workflow_runs.insert("wf-1".to_string());
        assert!(agent.finish_session_reload(1, true));
        assert!(
            agent.workflow_runs.iter().all(|r| r.run_id != "wf-1"),
            "cleared-during-window runs must not reappear from the stash"
        );
        assert!(agent.cleared_workflow_runs.contains("wf-1"));
        assert_eq!(
            agent
                .workflow_runs
                .iter()
                .find(|r| r.run_id == "wf-stash-survivor")
                .map(|r| r.status.as_str()),
            Some("active"),
            "a stash-only run not cleared during the window must be restored by the merge"
        );
        assert_eq!(
            agent
                .workflow_runs
                .iter()
                .find(|r| r.run_id == "wf-keep")
                .map(|r| r.status.as_str()),
            Some("complete")
        );
    }
}
