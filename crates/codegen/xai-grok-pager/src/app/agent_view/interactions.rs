//! Blocking interaction surfaces: permission prompts, the question view,
//! and the cancel-turn confirm flow (keys, mouse, and submit paths).
#[cfg(test)]
use super::paste::paste_key_tests;
#[cfg(test)]
use super::test_fixtures;
use super::{
    AgentPane, AgentView, MULTI_CLICK_TIMEOUT_MS, PeekAnswerOutcome, question_visible_h,
    translate_local_submit,
};
#[cfg(test)]
use crate::actions::ActionRegistry;
use crate::app::actions::Action;
use crate::app::app_view::InputOutcome;
use crate::key;
use crate::views::modal::CancelTurnChoice;
use crate::views::prompt_widget::{EnterOutcome, PromptEvent};
use crate::views::question_view::QUESTION_VIEW_HPAD;
#[cfg(test)]
use crossterm::event::Event;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::time::Instant;
impl AgentView {
    /// Handle key input when the permission view is active.
    ///
    /// Two modes (mirrors question view pattern):
    /// - **Options**: j/k navigate, Enter selects, 1..9 shortcuts,
    ///   `<`/`>` expand/contract bash selection, Esc/Ctrl-C cancel front.
    /// - **FollowupInput**: all keys go to PromptWidget; Esc exits back
    ///   to Options; Enter submits followup as RejectOnce response.
    pub(super) fn handle_permission_key(&mut self, key: &KeyEvent) -> InputOutcome {
        use crate::views::permission_view::PermissionFocus;
        let Some(perm) = self.permission_queue.front_mut() else {
            return InputOutcome::Unchanged;
        };
        if key!('f', CONTROL).matches(key) && !perm.description.is_empty() {
            perm.args_expanded = !perm.args_expanded;
            return InputOutcome::Changed;
        }
        match perm.focus {
            PermissionFocus::FollowupInput => {
                if key.code == KeyCode::Esc {
                    perm.focus = PermissionFocus::Options;
                    return InputOutcome::Changed;
                }
                if key!('c', CONTROL).matches(key) {
                    return InputOutcome::Action(Action::PermissionCancel);
                }
                match self.prompt.route_enter(key) {
                    EnterOutcome::NewlineInserted => return InputOutcome::Changed,
                    EnterOutcome::Submit => {
                        let text = self.prompt.text().to_string();
                        return InputOutcome::Action(Action::PermissionFollowup(text));
                    }
                    EnterOutcome::PassThrough => {}
                }
                match self.prompt.handle_key(key) {
                    PromptEvent::Edited => InputOutcome::Changed,
                    PromptEvent::Ignored => InputOutcome::Changed,
                }
            }
            PermissionFocus::Options => {
                if key!('c', CONTROL).matches(key) {
                    return InputOutcome::Action(Action::PermissionCancel);
                }
                if key.code == KeyCode::Tab {
                    self.active_pane = AgentPane::Scrollback;
                    return InputOutcome::Changed;
                }
                if key.code == KeyCode::Char('j') || key.code == KeyCode::Down {
                    if perm.active_idx + 1 < perm.options.len() {
                        perm.active_idx += 1;
                    }
                    return InputOutcome::Changed;
                }
                if key.code == KeyCode::Char('k') || key.code == KeyCode::Up {
                    perm.active_idx = perm.active_idx.saturating_sub(1);
                    return InputOutcome::Changed;
                }
                if key.code == KeyCode::Enter {
                    if let Some(opt) = perm.options.get(perm.active_idx) {
                        return InputOutcome::Action(Action::PermissionSelect(
                            opt.option_id.clone(),
                        ));
                    }
                    return InputOutcome::Changed;
                }
                if let KeyCode::Char(ch @ '1'..='9') = key.code {
                    let idx = (ch as u8 - b'1') as usize;
                    if let Some(opt) = perm.options.get(idx) {
                        return InputOutcome::Action(Action::PermissionSelect(
                            opt.option_id.clone(),
                        ));
                    }
                    return InputOutcome::Changed;
                }
                if key!('o', CONTROL).matches(key) {
                    return InputOutcome::Action(Action::SetYoloMode(!self.session.is_yolo()));
                }
                let is_right = key.code == KeyCode::Right || key.code == KeyCode::Char('>');
                let is_left = key.code == KeyCode::Left || key.code == KeyCode::Char('<');
                if (is_right || is_left) && perm.has_adjustable_scope() {
                    if let Some(ref mut scope) = perm.mcp_scope {
                        if is_left && scope.server_prefix.is_some() {
                            scope.selected = crate::views::permission_view::McpScope::Server;
                        } else if is_right {
                            scope.selected = crate::views::permission_view::McpScope::Tool;
                        }
                    } else if is_right
                        && let Some(ref h) = perm.bash_highlights
                        && perm.bash_selection_count < h.highlighted_words.len()
                    {
                        perm.bash_selection_count += 1;
                    } else if is_left && perm.bash_selection_count > 1 {
                        perm.bash_selection_count -= 1;
                    }
                    let on_scoped_row = perm.options.get(perm.active_idx).is_some_and(|o| {
                        matches!(
                            o.kind,
                            agent_client_protocol::PermissionOptionKind::AllowAlways
                                | agent_client_protocol::PermissionOptionKind::RejectAlways
                        )
                    });
                    if !on_scoped_row
                        && let Some(idx) = perm.options.iter().position(|o| {
                            o.kind == agent_client_protocol::PermissionOptionKind::AllowAlways
                        })
                    {
                        perm.active_idx = idx;
                    }
                    return InputOutcome::Changed;
                }
                if let Some(opt) = perm.options.get(perm.active_idx)
                    && opt.kind == agent_client_protocol::PermissionOptionKind::RejectOnce
                    && crate::input::key::is_text_input_key(key)
                    && matches!(key.code, KeyCode::Char(c) if !c.is_ascii_digit())
                {
                    perm.focus = PermissionFocus::FollowupInput;
                    let _ = self.prompt.handle_key(key);
                    return InputOutcome::Changed;
                }
                InputOutcome::Changed
            }
        }
    }
    pub(super) fn handle_cancel_turn_key(&mut self, key: &KeyEvent) -> InputOutcome {
        let Some(ctv) = self.cancel_turn_view.as_mut() else {
            return InputOutcome::Unchanged;
        };
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.cancel_trigger_hint = Some(crate::app::actions::CancelTrigger::CtrlC);
            return InputOutcome::Action(Action::CancelTurn);
        }
        if key.code == KeyCode::Tab && key.modifiers.is_empty() {
            self.active_pane = AgentPane::Scrollback;
            return InputOutcome::Changed;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                ctv.active_idx = (ctv.active_idx + 1).min(CancelTurnChoice::ALL.len() - 1);
                InputOutcome::Changed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                ctv.active_idx = ctv.active_idx.saturating_sub(1);
                InputOutcome::Changed
            }
            KeyCode::Enter => {
                let choice = CancelTurnChoice::ALL[ctv.active_idx];
                InputOutcome::Action(Action::CancelTurnChoice(choice))
            }
            KeyCode::Char(c @ '1'..='4') => {
                let idx = (c as usize) - ('1' as usize);
                let choice = CancelTurnChoice::ALL[idx];
                InputOutcome::Action(Action::CancelTurnChoice(choice))
            }
            KeyCode::Esc => {
                self.suppress_rewind_arm(std::time::Instant::now());
                InputOutcome::Action(Action::CancelTurnChoice(CancelTurnChoice::ContinueToRun))
            }
            _ => InputOutcome::Unchanged,
        }
    }
    /// Mouse handler for the cancel-turn panel. `Moved` moves the
    /// cursor onto the pointed row; `Down(Left)` dispatches the row's
    /// `CancelTurnChoice`. All other events are consumed.
    pub(super) fn handle_cancel_turn_mouse(&mut self, mouse: &MouseEvent) -> InputOutcome {
        if self.cancel_turn_view.is_none() {
            return InputOutcome::Unchanged;
        }
        let hit_idx = self
            .cancel_turn_buttons
            .iter()
            .enumerate()
            .find(|(_, rect)| rect.contains((mouse.column, mouse.row).into()))
            .map(|(idx, _)| idx);
        match mouse.kind {
            MouseEventKind::Moved => {
                let Some(idx) = hit_idx else {
                    return InputOutcome::Unchanged;
                };
                if let Some(ctv) = self.cancel_turn_view.as_mut()
                    && ctv.active_idx != idx
                {
                    ctv.active_idx = idx;
                    return InputOutcome::Changed;
                }
                InputOutcome::Unchanged
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(idx) = hit_idx
                    && idx < CancelTurnChoice::ALL.len()
                {
                    if let Some(ctv) = self.cancel_turn_view.as_mut() {
                        ctv.active_idx = idx;
                    }
                    let choice = CancelTurnChoice::ALL[idx];
                    return InputOutcome::Action(Action::CancelTurnChoice(choice));
                }
                InputOutcome::Unchanged
            }
            _ => InputOutcome::Unchanged,
        }
    }
    /// Handle key input when the question view is active.
    ///
    /// Two modes:
    /// - **Navigation**: j/k move cursor, Space toggles, Enter advances or
    ///   edits freeform, h/l/[/] cycle questions, 1-9/a-f jump+toggle,
    ///   n next, s skip, Shift-X kill (only explicit way to dismiss).
    /// - **InputMode**: all keys go to the prompt widget; Esc exits input mode.
    pub(super) fn handle_question_key(&mut self, key: &KeyEvent) -> InputOutcome {
        use crate::views::question_view::{QuestionFocus, QuestionSelection};
        let Some(ref mut qv) = self.question_view else {
            return InputOutcome::Unchanged;
        };
        match qv.focus {
            QuestionFocus::InputMode => {
                if key.code == KeyCode::Esc {
                    if self.prompt.file_search_visible() {
                        self.prompt.file_search.clear_context();
                    } else {
                        let idx = qv.active_tab;
                        let text = self.prompt.text().to_string();
                        let has_text = !text.trim().is_empty();
                        if let Some(slot) = qv.per_question_freeform.get_mut(idx) {
                            *slot = text;
                        }
                        if has_text {
                            if let Some(sel) = qv.per_question_freeform_selected.get_mut(idx) {
                                *sel = true;
                            }
                            if let Some(crate::views::question_view::QuestionSelection::Single(
                                sel,
                            )) = qv.selections.get_mut(idx)
                            {
                                *sel = None;
                            }
                        } else {
                            if let Some(sel) = qv.per_question_freeform_selected.get_mut(idx) {
                                *sel = false;
                            }
                            if let Some(slot) = qv.per_question_freeform.get_mut(idx) {
                                slot.clear();
                            }
                        }
                        qv.focus = QuestionFocus::Navigation;
                    }
                    return InputOutcome::Changed;
                }
                if key!('f', CONTROL).matches(key) {
                    qv.fullscreen = !qv.fullscreen;
                    return InputOutcome::Changed;
                }
                if key!('y', CONTROL).matches(key) {
                    return self.dismiss_question_view();
                }
                if key!('c', CONTROL).matches(key) {
                    qv.focus = QuestionFocus::Navigation;
                    return InputOutcome::Changed;
                }
                match self.prompt.route_enter(key) {
                    EnterOutcome::NewlineInserted => return InputOutcome::Changed,
                    EnterOutcome::Submit => {
                        let idx = qv.active_tab;
                        let text = self.prompt.text().to_string();
                        let has_text = !text.trim().is_empty();
                        if let Some(slot) = qv.per_question_freeform.get_mut(idx) {
                            *slot = text;
                        }
                        if has_text {
                            if let Some(sel) = qv.per_question_freeform_selected.get_mut(idx) {
                                *sel = true;
                            }
                            if let Some(crate::views::question_view::QuestionSelection::Single(
                                sel,
                            )) = qv.selections.get_mut(idx)
                            {
                                *sel = None;
                            }
                        } else {
                            if let Some(sel) = qv.per_question_freeform_selected.get_mut(idx) {
                                *sel = false;
                            }
                            if let Some(slot) = qv.per_question_freeform.get_mut(idx) {
                                slot.clear();
                            }
                        }
                        qv.focus = QuestionFocus::Navigation;
                        let last = qv.questions.len().saturating_sub(1);
                        if qv.active_tab < last {
                            self.swap_question_freeform();
                            if let Some(ref mut qv) = self.question_view {
                                qv.next_question();
                            }
                            self.load_question_freeform();
                            self.ensure_question_cursor_visible();
                        } else {
                            return self.submit_question_answers(false);
                        }
                        return InputOutcome::Changed;
                    }
                    EnterOutcome::PassThrough => {}
                }
                match self.prompt.handle_key(key) {
                    PromptEvent::Edited => {
                        if let Some(req) = self.prompt.pending_viewer_request.take() {
                            self.open_line_viewer(&req.path, req.initial_range);
                        }
                        InputOutcome::Changed
                    }
                    PromptEvent::Ignored => InputOutcome::Changed,
                }
            }
            QuestionFocus::Navigation => {
                if key!('y', CONTROL).matches(key) {
                    return self.dismiss_question_view();
                }
                if key!('c', CONTROL).matches(key) {
                    return self.submit_question_answers(true);
                }
                if key!('f', CONTROL).matches(key) {
                    qv.fullscreen = !qv.fullscreen;
                    return InputOutcome::Changed;
                }
                let mut needs_scroll_update = false;
                let mut needs_switch_question: Option<bool> = None;
                if qv.is_on_freeform_row()
                    && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
                    && matches!(key.code, KeyCode::Char(c) if c != ' ')
                {
                    let text = qv.activate_freeform_input();
                    self.prompt.set_text(&text);
                    let _ = self.prompt.handle_key(key);
                    return InputOutcome::Changed;
                }
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down
                        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::CONTROL =>
                    {
                        let max = qv.total_items(qv.active_tab).saturating_sub(1);
                        let cur = qv.cursor();
                        if cur < max {
                            qv.set_cursor(cur + 1);
                            needs_scroll_update = true;
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up
                        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::CONTROL =>
                    {
                        let cur = qv.cursor();
                        if cur > 0 {
                            qv.set_cursor(cur - 1);
                            needs_scroll_update = true;
                        }
                    }
                    KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
                        let max = qv.total_items(qv.active_tab).saturating_sub(1);
                        let half = (max / 2).max(1);
                        qv.set_cursor((qv.cursor() + half).min(max));
                        needs_scroll_update = true;
                    }
                    KeyCode::PageDown => {
                        let max = qv.total_items(qv.active_tab).saturating_sub(1);
                        let page = max.max(1);
                        qv.set_cursor((qv.cursor() + page).min(max));
                        needs_scroll_update = true;
                    }
                    KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                        let half = (qv.total_items(qv.active_tab) / 2).max(1);
                        qv.set_cursor(qv.cursor().saturating_sub(half));
                        needs_scroll_update = true;
                    }
                    KeyCode::PageUp => {
                        let page = qv.total_items(qv.active_tab).saturating_sub(1).max(1);
                        qv.set_cursor(qv.cursor().saturating_sub(page));
                        needs_scroll_update = true;
                    }
                    KeyCode::Char('g') if key.modifiers.is_empty() => {
                        qv.set_cursor(0);
                        needs_scroll_update = true;
                    }
                    KeyCode::Char('G') if key.modifiers == KeyModifiers::SHIFT => {
                        let max = qv.total_items(qv.active_tab).saturating_sub(1);
                        qv.set_cursor(max);
                        needs_scroll_update = true;
                    }
                    KeyCode::Char(' ') => {
                        if qv.is_on_freeform_row() {
                            let text = qv.activate_freeform_input();
                            self.prompt.set_text(&text);
                        } else {
                            let active = qv.active_tab;
                            let cursor = qv.cursor();
                            qv.toggle_option(active, cursor);
                            if matches!(
                                qv.selections.get(active),
                                Some(crate::views::question_view::QuestionSelection::Single(
                                    Some(_)
                                ))
                            ) && let Some(sel) =
                                qv.per_question_freeform_selected.get_mut(active)
                            {
                                *sel = false;
                            }
                        }
                    }
                    KeyCode::Enter => {
                        if qv.is_on_freeform_row() {
                            let text = qv.activate_freeform_input();
                            self.prompt.set_text(&text);
                        } else {
                            let cursor = qv.cursor();
                            let active = qv.active_tab;
                            qv.select_option(active, cursor);
                            if let Some(sel) = qv.per_question_freeform_selected.get_mut(active) {
                                *sel = false;
                            }
                            let last = qv.questions.len().saturating_sub(1);
                            if qv.active_tab < last {
                                needs_switch_question = Some(true);
                            } else {
                                return self.submit_question_answers(false);
                            }
                        }
                    }
                    KeyCode::Char('z') if key.modifiers.is_empty() => {
                        if !qv.no_freeform {
                            let freeform_idx = qv.total_items(qv.active_tab).saturating_sub(1);
                            qv.set_cursor(freeform_idx);
                            let text = qv.activate_freeform_input();
                            self.prompt.set_text(&text);
                        }
                    }
                    KeyCode::Char('l') | KeyCode::Char(']') | KeyCode::Right
                        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::CONTROL =>
                    {
                        if qv.questions.len() > 1 {
                            needs_switch_question = Some(true);
                        }
                    }
                    KeyCode::Char('h') | KeyCode::Char('[') | KeyCode::Left
                        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::CONTROL =>
                    {
                        if qv.questions.len() > 1 {
                            needs_switch_question = Some(false);
                        }
                    }
                    KeyCode::Char(c)
                        if key.modifiers.is_empty()
                            && crate::views::question_view::option_index_for_key(c).is_some() =>
                    {
                        let idx = crate::views::question_view::option_index_for_key(c).unwrap();
                        let active = qv.active_tab;
                        let opt_count = qv
                            .questions
                            .get(active)
                            .map(|q| q.options.len())
                            .unwrap_or(0);
                        if idx < opt_count {
                            qv.set_cursor(idx);
                            qv.select_option(active, idx);
                            if let Some(sel) = qv.per_question_freeform_selected.get_mut(active) {
                                *sel = false;
                            }
                            let last = qv.questions.len().saturating_sub(1);
                            if qv.active_tab < last {
                                needs_switch_question = Some(true);
                            } else {
                                return self.submit_question_answers(false);
                            }
                        }
                    }
                    KeyCode::Char('y') if key.modifiers.is_empty() => {
                        if !qv.is_on_freeform_row() {
                            let cursor = qv.cursor();
                            let active = qv.active_tab;
                            if let Some(question) = qv.questions.get(active)
                                && let Some(option) = question.options.get(cursor)
                            {
                                let mut text =
                                    crate::views::question_view::normalize_label(&option.label);
                                if !option.description.is_empty() {
                                    text.push('\n');
                                    text.push_str(&option.description);
                                }
                                self.copy_to_clipboard(&text);
                            }
                        }
                    }
                    KeyCode::Esc => {
                        if matches!(
                            qv.local_kind,
                            Some(
                                crate::views::question_view::LocalQuestionKind::ProjectSelect { .. }
                            )
                        ) {
                            return self.submit_question_answers(true);
                        }
                        let active = qv.active_tab;
                        if let Some(sel) = qv.selections.get_mut(active) {
                            match sel {
                                QuestionSelection::Multi(set) => {
                                    set.clear();
                                }
                                QuestionSelection::Single(opt) => {
                                    *opt = None;
                                }
                            }
                        }
                        if let Some(sel) = qv.per_question_freeform_selected.get_mut(active) {
                            *sel = false;
                        }
                    }
                    KeyCode::Tab => {
                        self.swap_question_freeform();
                        self.active_pane = AgentPane::Scrollback;
                        return InputOutcome::Changed;
                    }
                    KeyCode::Char('X') if key.modifiers == KeyModifiers::SHIFT => {
                        return self.submit_question_answers(true);
                    }
                    _ => {}
                }
                if let Some(forward) = needs_switch_question {
                    self.last_question_click = None;
                    self.swap_question_freeform();
                    if let Some(ref mut qv) = self.question_view {
                        if forward {
                            qv.next_question();
                        } else {
                            qv.prev_question();
                        }
                    }
                    self.load_question_freeform();
                    needs_scroll_update = true;
                }
                if needs_scroll_update {
                    self.ensure_question_cursor_visible();
                }
                InputOutcome::Changed
            }
        }
    }
    /// Handle mouse events when the question view is active.
    ///
    /// Scroll wheel scrolls the options list. Clicks on option rows move
    /// cursor and toggle/select. Everything else is consumed (modal-ish).
    pub(super) fn handle_question_mouse(&mut self, mouse: &MouseEvent) -> InputOutcome {
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            for &(key_ch, rect) in &self.question_nav_buttons {
                if rect.contains((mouse.column, mouse.row).into()) {
                    if let Some(ref mut qv) = self.question_view
                        && qv.focus == crate::views::question_view::QuestionFocus::InputMode
                    {
                        let idx = qv.active_tab;
                        let text = self.prompt.text().to_string();
                        let has_text = !text.trim().is_empty();
                        if let Some(slot) = qv.per_question_freeform.get_mut(idx) {
                            *slot = text;
                        }
                        if has_text
                            && let Some(sel) = qv.per_question_freeform_selected.get_mut(idx)
                        {
                            *sel = true;
                        }
                        if has_text
                            && let Some(crate::views::question_view::QuestionSelection::Single(sel)) =
                                qv.selections.get_mut(idx)
                        {
                            *sel = None;
                        }
                        qv.focus = crate::views::question_view::QuestionFocus::Navigation;
                    }
                    let key_event = KeyEvent::new(
                        if key_ch == '\n' {
                            KeyCode::Enter
                        } else {
                            KeyCode::Char(key_ch)
                        },
                        KeyModifiers::NONE,
                    );
                    return self.handle_question_key(&key_event);
                }
            }
        }
        let Some(ref mut qv) = self.question_view else {
            return InputOutcome::Changed;
        };
        match mouse.kind {
            MouseEventKind::Moved => {
                let item = self.question_item_at(mouse.column, mouse.row);
                let btn = self
                    .question_nav_buttons
                    .iter()
                    .find(|(_, rect)| rect.contains((mouse.column, mouse.row).into()))
                    .map(|(ch, _)| *ch);
                let changed =
                    item != self.hovered_question_item || btn != self.hovered_question_button;
                self.hovered_question_item = item;
                self.hovered_question_button = btn;
                if changed {
                    InputOutcome::Changed
                } else {
                    InputOutcome::Unchanged
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let delta: i32 = if matches!(mouse.kind, MouseEventKind::ScrollDown) {
                    1
                } else {
                    -1
                };
                let over_inline = qv.focus == crate::views::question_view::QuestionFocus::InputMode
                    && self
                        .inline_prompt_area
                        .is_some_and(|r| r.contains((mouse.column, mouse.row).into()));
                if over_inline {
                    let event = MouseEvent {
                        kind: mouse.kind,
                        column: mouse.column,
                        row: mouse.row,
                        modifiers: mouse.modifiers,
                    };
                    let _ = self.prompt.handle_mouse(&event);
                } else if let Some((scroll_top, scroll_bottom)) = self.question_scroll_region
                    && mouse.row >= scroll_top
                    && mouse.row < scroll_bottom
                {
                    self.apply_question_scroll(delta);
                }
                InputOutcome::Changed
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.last_click = None;
                self.last_text_click = None;
                self.pending_scrollback_click = None;
                if self
                    .hit_question_scrollbar
                    .contains(mouse.column, mouse.row)
                {
                    self.question_scrollbar_dragging = true;
                    self.apply_question_scrollbar_click(mouse.row);
                    return InputOutcome::Changed;
                }
                if qv.focus == crate::views::question_view::QuestionFocus::InputMode {
                    if self
                        .prompt
                        .textarea_area()
                        .contains((mouse.column, mouse.row).into())
                    {
                        let _ = self.prompt.handle_mouse(mouse);
                        return InputOutcome::Changed;
                    }
                    let idx = qv.active_tab;
                    let text = self.prompt.text().to_string();
                    let has_text = !text.trim().is_empty();
                    if let Some(slot) = qv.per_question_freeform.get_mut(idx) {
                        *slot = text;
                    }
                    if has_text && let Some(sel) = qv.per_question_freeform_selected.get_mut(idx) {
                        *sel = true;
                    }
                    if has_text
                        && let Some(crate::views::question_view::QuestionSelection::Single(sel)) =
                            qv.selections.get_mut(idx)
                    {
                        *sel = None;
                    }
                    qv.focus = crate::views::question_view::QuestionFocus::Navigation;
                }
                let prompt_area = self.pane_areas.prompt;
                let footer_h = 3u16;
                let question_area_bottom =
                    prompt_area.y + prompt_area.height.saturating_sub(footer_h);
                let sticky_freeform_y = question_area_bottom.saturating_sub(1);
                let on_sticky_freeform = !qv.no_freeform
                    && qv.focus != crate::views::question_view::QuestionFocus::InputMode
                    && mouse.row == sticky_freeform_y
                    && mouse.column >= prompt_area.x
                    && mouse.column < prompt_area.x + prompt_area.width;
                if on_sticky_freeform {
                    let freeform_idx = qv
                        .questions
                        .get(qv.active_tab)
                        .map(|q| q.options.len())
                        .unwrap_or(0);
                    let active_tab = qv.active_tab;
                    qv.set_cursor(freeform_idx);
                    let was_selected = qv
                        .per_question_freeform_selected
                        .get(active_tab)
                        .copied()
                        .unwrap_or(false);
                    if was_selected {
                        if let Some(sel) = qv.per_question_freeform_selected.get_mut(active_tab) {
                            *sel = false;
                        }
                    } else {
                        if let Some(sel) = qv.per_question_freeform_selected.get_mut(active_tab) {
                            *sel = true;
                        }
                        if let Some(crate::views::question_view::QuestionSelection::Single(sel)) =
                            qv.selections.get_mut(active_tab)
                        {
                            *sel = None;
                        }
                        let text = qv
                            .per_question_freeform
                            .get(active_tab)
                            .cloned()
                            .unwrap_or_default();
                        self.prompt.set_text(&text);
                        qv.focus = crate::views::question_view::QuestionFocus::InputMode;
                    }
                    return InputOutcome::Changed;
                }
                let hit_idx = qv.questions.get(qv.active_tab).and_then(|question| {
                    let content_w = prompt_area.width.saturating_sub(QUESTION_VIEW_HPAD) as usize;
                    let scroll = qv
                        .per_question_scroll
                        .get(qv.active_tab)
                        .copied()
                        .unwrap_or(0);
                    crate::views::question_view::item_index_at_screen_row(
                        question,
                        prompt_area,
                        content_w,
                        scroll,
                        mouse.row,
                        qv.focused_preview(),
                        qv.fullscreen,
                        qv.cached_desc_cap,
                        qv.cached_preview_cap,
                        qv.cursor(),
                    )
                });
                let option_count = qv
                    .questions
                    .get(qv.active_tab)
                    .map(|question| question.options.len());
                if let Some((idx, option_count)) = hit_idx.zip(option_count) {
                    if qv.no_freeform && idx >= option_count {
                        return InputOutcome::Changed;
                    }
                    let active_tab = qv.active_tab;
                    qv.set_cursor(idx);
                    if idx < option_count {
                        let now = Instant::now();
                        let is_double_click =
                            self.last_question_click.is_some_and(|(t, prev_idx)| {
                                prev_idx == idx
                                    && now.duration_since(t).as_millis() < MULTI_CLICK_TIMEOUT_MS
                            });
                        if is_double_click {
                            self.last_question_click = None;
                            qv.select_option(active_tab, idx);
                            if let Some(sel) = qv.per_question_freeform_selected.get_mut(active_tab)
                            {
                                *sel = false;
                            }
                            let last = qv.questions.len().saturating_sub(1);
                            if qv.active_tab < last {
                                self.swap_question_freeform();
                                if let Some(ref mut qv) = self.question_view {
                                    qv.next_question();
                                }
                                self.load_question_freeform();
                                self.ensure_question_cursor_visible();
                                return InputOutcome::Changed;
                            } else {
                                return self.submit_question_answers(false);
                            }
                        }
                        self.last_question_click = Some((now, idx));
                        qv.toggle_option(active_tab, idx);
                        if matches!(
                            qv.selections.get(active_tab),
                            Some(crate::views::question_view::QuestionSelection::Single(
                                Some(_)
                            ))
                        ) && let Some(sel) =
                            qv.per_question_freeform_selected.get_mut(active_tab)
                        {
                            *sel = false;
                        }
                    } else {
                        self.last_question_click = None;
                        use crate::views::question_view::QuestionFocus;
                        let tab = qv.active_tab;
                        let text = qv
                            .per_question_freeform
                            .get(tab)
                            .cloned()
                            .unwrap_or_default();
                        self.prompt.set_text(&text);
                        qv.focus = QuestionFocus::InputMode;
                    }
                }
                InputOutcome::Changed
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.question_scrollbar_dragging {
                    self.apply_question_scrollbar_click(mouse.row);
                } else if qv.focus == crate::views::question_view::QuestionFocus::InputMode {
                    let _ = self.prompt.handle_mouse(mouse);
                }
                InputOutcome::Changed
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.question_scrollbar_dragging = false;
                if qv.focus == crate::views::question_view::QuestionFocus::InputMode {
                    let _ = self.prompt.handle_mouse(mouse);
                }
                InputOutcome::Changed
            }
            _ => InputOutcome::Changed,
        }
    }
    /// Apply a scroll delta to the question view options.
    pub(super) fn apply_question_scroll(&mut self, delta: i32) {
        let Some(ref mut qv) = self.question_view else {
            return;
        };
        let Some(question) = qv.questions.get(qv.active_tab) else {
            return;
        };
        let content_w = self
            .pane_areas
            .prompt
            .width
            .saturating_sub(QUESTION_VIEW_HPAD) as usize;
        let visible_h = question_visible_h(
            self.question_scroll_region,
            self.pane_areas.prompt.height,
            question,
            content_w,
            qv.focused_preview(),
            qv.fullscreen,
            qv.cached_desc_cap,
            qv.cached_preview_cap,
            1 - qv.phantom_freeform_h(),
        );
        let current_scroll = qv
            .per_question_scroll
            .get(qv.active_tab)
            .copied()
            .unwrap_or(0);
        let new_scroll = crate::views::question_view::scroll_offset_for_item_delta(
            question,
            content_w,
            current_scroll,
            delta,
            visible_h,
            qv.cursor(),
            qv.phantom_freeform_h(),
        );
        if let Some(s) = qv.per_question_scroll.get_mut(qv.active_tab) {
            *s = new_scroll;
        }
    }
    /// Apply a scrollbar click/drag at the given screen row for the question view.
    ///
    /// Uses [`scrollbar_click_to_offset`] (same math as `list_pane` and the
    /// scrollback scrollbar) so click/drag is the exact inverse of the thumb.
    fn apply_question_scrollbar_click(&mut self, screen_y: u16) {
        use crate::render::scrollbar::{ScrollbarClickResult, scrollbar_click_to_offset};
        let Some(sb) = self.hit_question_scrollbar.rect else {
            return;
        };
        if sb.height == 0 {
            return;
        }
        let content_w = self
            .pane_areas
            .prompt
            .width
            .saturating_sub(QUESTION_VIEW_HPAD) as usize;
        let Some(ref mut qv) = self.question_view else {
            return;
        };
        let Some(question) = qv.questions.get(qv.active_tab) else {
            return;
        };
        let total_h =
            crate::views::question_view::total_options_height(question, content_w, qv.cursor())
                .saturating_sub(qv.phantom_freeform_h());
        let visible_h = question_visible_h(
            self.question_scroll_region,
            self.pane_areas.prompt.height,
            question,
            content_w,
            qv.focused_preview(),
            qv.fullscreen,
            qv.cached_desc_cap,
            qv.cached_preview_cap,
            1 - qv.phantom_freeform_h(),
        );
        let cell_index = screen_y.saturating_sub(sb.y);
        let result = scrollbar_click_to_offset(cell_index, sb.height, total_h, visible_h);
        let max_scroll = total_h.saturating_sub(visible_h);
        if let Some(s) = qv.per_question_scroll.get_mut(qv.active_tab) {
            match result {
                ScrollbarClickResult::Top => *s = 0,
                ScrollbarClickResult::Bottom => *s = max_scroll,
                ScrollbarClickResult::Offset(offset) => {
                    *s = (offset as u16).min(max_scroll);
                }
            }
        }
    }
    fn ensure_question_cursor_visible(&mut self) {
        let content_w = self
            .pane_areas
            .prompt
            .width
            .saturating_sub(QUESTION_VIEW_HPAD) as usize;
        let Some(ref mut qv) = self.question_view else {
            return;
        };
        let Some(question) = qv.questions.get(qv.active_tab) else {
            return;
        };
        let visible_h = question_visible_h(
            self.question_scroll_region,
            self.pane_areas.prompt.height,
            question,
            content_w,
            qv.focused_preview(),
            qv.fullscreen,
            qv.cached_desc_cap,
            qv.cached_preview_cap,
            1 - qv.phantom_freeform_h(),
        );
        qv.ensure_cursor_visible(visible_h, content_w);
    }
    fn question_item_at(&self, col: u16, row: u16) -> Option<usize> {
        let qv = self.question_view.as_ref()?;
        let prompt_area = self.pane_areas.prompt;
        if prompt_area.area() == 0 || !prompt_area.contains((col, row).into()) {
            return None;
        }
        let question = qv.questions.get(qv.active_tab)?;
        let content_w = prompt_area.width.saturating_sub(QUESTION_VIEW_HPAD) as usize;
        let scroll = qv
            .per_question_scroll
            .get(qv.active_tab)
            .copied()
            .unwrap_or(0);
        if let Some((scroll_top, scroll_bottom)) = self.question_scroll_region {
            if row >= scroll_top && row < scroll_bottom {
                let visual_line = (row - scroll_top) + scroll;
                let options_only_h: u16 = crate::views::question_view::total_options_height(
                    question,
                    content_w,
                    qv.cursor(),
                )
                .saturating_sub(1);
                if visual_line >= options_only_h {
                    return None;
                }
                return Some(crate::views::question_view::item_index_at_visual_line(
                    question,
                    content_w,
                    visual_line,
                    qv.cursor(),
                ));
            }
            let is_input_mode = qv.focus == crate::views::question_view::QuestionFocus::InputMode;
            if !is_input_mode && !qv.no_freeform && row == scroll_bottom {
                return Some(question.options.len());
            }
            return None;
        }
        crate::views::question_view::item_index_at_screen_row(
            question,
            prompt_area,
            content_w,
            scroll,
            row,
            qv.focused_preview(),
            qv.fullscreen,
            qv.cached_desc_cap,
            qv.cached_preview_cap,
            qv.cursor(),
        )
        .filter(|&idx| !qv.no_freeform || idx < question.options.len())
    }
    /// Save the current prompt text into `per_question_freeform[active_tab]`
    /// and load the text for the new `active_tab` into the prompt widget.
    /// Call this BEFORE changing `active_tab`.
    fn swap_question_freeform(&mut self) {
        let Some(ref mut qv) = self.question_view else {
            return;
        };
        let old = qv.active_tab;
        if let Some(slot) = qv.per_question_freeform.get_mut(old) {
            *slot = self.prompt.text().to_string();
        }
    }
    /// Load the freeform text for the current `active_tab` into the prompt.
    /// Call this AFTER changing `active_tab`.
    fn load_question_freeform(&mut self) {
        let Some(ref qv) = self.question_view else {
            return;
        };
        let new_text = qv
            .per_question_freeform
            .get(qv.active_tab)
            .map(|s| s.as_str())
            .unwrap_or("");
        self.prompt.set_text(new_text);
    }
    /// Dismiss (hide) the question view without submitting answers.
    ///
    /// Restores the original prompt text that was stashed when the question
    /// view opened, so typed "additional context" doesn't leak into the
    /// main prompt. Also clears any stashed (tab-hidden) question view.
    fn dismiss_question_view(&mut self) -> InputOutcome {
        let is_doctor_fix = self.question_view.as_ref().is_some_and(|qv| {
            matches!(
                qv.local_kind,
                Some(crate::views::question_view::LocalQuestionKind::DoctorFix { .. })
            )
        });
        if is_doctor_fix {
            return self.submit_question_answers(true);
        }
        if let Some(qv) = self.question_view.take() {
            self.turn_paused_duration += qv.opened_at.elapsed();
            self.prompt.restore(qv.stashed_prompt);
        }
        self.cleanup_question_state();
        InputOutcome::Changed
    }
    /// Retract an interaction modal (permission / question / plan-approval) that
    /// another connected client already resolved.
    ///
    /// In a shared (leader-hosted) session the agent broadcasts the interactive
    /// reverse-request to every pane and resolves first-answer-wins; when any
    /// pane answers, the agent broadcasts `InteractionResolved{tool_call_id}` and
    /// every other pane calls this to drop its copy. Returns `true` if a modal
    /// was dismissed (so the caller redraws). Idempotent: a `tool_call_id` this
    /// pane isn't showing is a silent no-op — including on the pane that
    /// answered, which already cleared its own modal locally. Dropping a
    /// dismissed modal's `response_tx` is harmless: the agent has already
    /// resolved, so any late response for that id is ignored by its gateway.
    pub(crate) fn dismiss_resolved_interaction(&mut self, tool_call_id: &str) -> bool {
        if self
            .question_view
            .as_ref()
            .is_some_and(|qv| qv.tool_call_id == tool_call_id)
        {
            let _ = self.dismiss_question_view();
            return true;
        }
        if self
            .plan_approval_view
            .as_ref()
            .is_some_and(|pav| pav.tool_call_id == tool_call_id)
        {
            let mut pav = self
                .plan_approval_view
                .take()
                .expect("plan_approval_view is Some (just checked)");
            pav.send_stale_cancel();
            self.latest_inline_plan_content = None;
            self.plan_next_comment_id = pav.next_comment_id;
            self.prompt.restore(pav.stashed_prompt);
            self.line_viewer = None;
            self.casual_commenting_range = None;
            self.casual_editing_comment_id = None;
            return true;
        }
        if let Some(pos) = self
            .permission_queue
            .iter()
            .position(|p| p.request.request.tool_call.tool_call_id.0.as_ref() == tool_call_id)
        {
            let was_front = pos == 0;
            let _ = self.permission_queue.remove(pos);
            if was_front {
                super::dispatch::resolve_permission_queue_transition(self);
            }
            return true;
        }
        false
    }
    /// Test-only access to [`submit_question_answers`] so dispatch tests
    /// can verify the full submit/cancel pipeline (including
    /// `prompt.restore` and `cleanup_question_state`) for local
    /// questions, not just the inner `translate_local_submit` shim.
    #[cfg(test)]
    pub(crate) fn submit_question_answers_for_test(&mut self, skipped: bool) -> InputOutcome {
        self.submit_question_answers(skipped)
    }
    #[cfg(test)]
    pub(crate) fn handle_question_key_for_test(&mut self, key: &KeyEvent) -> InputOutcome {
        self.handle_question_key(key)
    }
    fn submit_question_answers(&mut self, skipped: bool) -> InputOutcome {
        use xai_grok_tools::implementations::grok_build::ask_user_question::AskUserQuestionExtResponse;
        self.swap_question_freeform();
        let Some(mut qv) = self.question_view.take() else {
            return InputOutcome::Changed;
        };
        self.turn_paused_duration += qv.opened_at.elapsed();
        if let Some(kind) = qv.local_kind.take() {
            let is_doctor_fix = matches!(
                kind,
                crate::views::question_view::LocalQuestionKind::DoctorFix { .. }
            );
            let outcome = if skipped && is_doctor_fix {
                let crate::views::question_view::LocalQuestionKind::DoctorFix { target, .. } = kind
                else {
                    unreachable!("doctor fix checked above")
                };
                InputOutcome::Action(Action::DoctorFixCancelled(target))
            } else {
                translate_local_submit(&qv, kind, skipped)
            };
            self.prompt.restore(qv.stashed_prompt);
            self.cleanup_question_state();
            return outcome;
        }
        let response = if skipped {
            AskUserQuestionExtResponse::Cancelled
        } else {
            qv.build_accepted_response()
        };
        qv.send_ext_response(response);
        self.prompt.restore(qv.stashed_prompt);
        self.cleanup_question_state();
        let action = if skipped {
            "interview_skip"
        } else {
            "interview_submit"
        };
        xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::PlanSubmit {
            action: action.to_string(),
        });
        InputOutcome::Changed
    }
    /// Map a screen position to a permission option index.
    ///
    /// Uses the prompt area and permission chrome height to determine which
    /// option row the mouse is over. Returns `None` if outside the options.
    pub(super) fn permission_item_at(&self, _col: u16, row: u16) -> Option<usize> {
        let perm = self.permission_queue.front()?;
        let prompt_area = self.pane_areas.prompt;
        if prompt_area.area() == 0 {
            return None;
        }
        let content_w = prompt_area.width.saturating_sub(QUESTION_VIEW_HPAD) as usize;
        let chrome_h = crate::views::permission_view::permission_chrome_height_pub(
            perm,
            content_w,
            prompt_area.height,
        );
        let options_start_y = prompt_area.y + chrome_h;
        if row < options_start_y {
            return None;
        }
        let idx = (row - options_start_y) as usize;
        if idx < perm.options.len() {
            Some(idx)
        } else {
            None
        }
    }
    /// Clean up question-related visual state after the question view is
    /// dismissed (submit, cancel, or replacement).
    fn cleanup_question_state(&mut self) {
        self.hovered_question_item = None;
        self.question_scrollbar_dragging = false;
        self.hit_question_scrollbar.clear();
        self.inline_prompt_area = None;
        self.last_question_click = None;
    }
    /// Answer the ACTIVE question of this agent's pending
    /// `AskUserQuestion` from the dashboard peek panel.
    ///
    /// Mirrors the agent view's own Enter handling but sources the
    /// freeform text from the peek (a `freeform` argument) instead of
    /// this view's prompt: `option_idx` selects an option; `None` with
    /// non-empty `freeform` records the "Other" free-text answer. When
    /// more questions remain it advances to the next one
    /// ([`PeekAnswerOutcome::Advanced`]); on the last question it builds +
    /// sends the accepted ext-response, restores the stashed prompt, and
    /// clears question state ([`PeekAnswerOutcome::Submitted`]). Only
    /// valid for an ext ask (`None` `local_kind`); an empty "Other" or a
    /// non-ext question is a [`PeekAnswerOutcome::NoOp`].
    pub(crate) fn dashboard_answer_question(
        &mut self,
        option_idx: Option<usize>,
        freeform: String,
    ) -> PeekAnswerOutcome {
        use crate::views::question_view::QuestionSelection;
        let Some(mut qv) = self.question_view.take() else {
            return PeekAnswerOutcome::NoOp;
        };
        if qv.local_kind.is_some() {
            self.question_view = Some(qv);
            return PeekAnswerOutcome::NoOp;
        }
        let active = qv.active_tab;
        match option_idx {
            Some(idx) => {
                qv.select_option(active, idx);
                if let Some(slot) = qv.per_question_freeform_selected.get_mut(active) {
                    *slot = false;
                }
            }
            None => {
                if freeform.trim().is_empty() {
                    self.question_view = Some(qv);
                    return PeekAnswerOutcome::NoOp;
                }
                if let Some(slot) = qv.per_question_freeform.get_mut(active) {
                    *slot = freeform;
                }
                if let Some(slot) = qv.per_question_freeform_selected.get_mut(active) {
                    *slot = true;
                }
                if let Some(QuestionSelection::Single(sel)) = qv.selections.get_mut(active) {
                    *sel = None;
                }
            }
        }
        if active + 1 < qv.questions.len() {
            qv.next_question();
            self.question_view = Some(qv);
            return PeekAnswerOutcome::Advanced;
        }
        self.turn_paused_duration += qv.opened_at.elapsed();
        let response = qv.build_accepted_response();
        qv.send_ext_response(response);
        self.prompt.restore(qv.stashed_prompt);
        self.cleanup_question_state();
        PeekAnswerOutcome::Submitted
    }
}
#[cfg(test)]
mod cancel_turn_mouse_tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::agent::{AgentId, AgentSession, AgentState};
    use crate::app::app_view::InputOutcome;
    use crate::scrollback::state::ScrollbackState;
    use crate::views::modal::{CancelTurnChoice, CancelTurnViewState};
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;
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
    /// Panel with one synthetic Rect per choice, stacked at y=10.
    fn setup_panel(agent: &mut AgentView) {
        agent.cancel_turn_view = Some(CancelTurnViewState {
            active_idx: 0,
            running_count: 2,
        });
        agent.cancel_turn_buttons.clear();
        for (i, _) in CancelTurnChoice::ALL.iter().enumerate() {
            agent.cancel_turn_buttons.push(Rect {
                x: 5,
                y: 10 + i as u16,
                width: 40,
                height: 1,
            });
        }
    }
    fn down(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: crossterm::event::KeyModifiers::empty(),
        }
    }
    fn moved(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Moved,
            column: col,
            row,
            modifiers: crossterm::event::KeyModifiers::empty(),
        }
    }
    #[test]
    fn click_first_row_dispatches_stop_running() {
        let mut agent = make_agent();
        setup_panel(&mut agent);
        let outcome = agent.handle_cancel_turn_mouse(&down(10, 10));
        match outcome {
            InputOutcome::Action(Action::CancelTurnChoice(c)) => {
                assert_eq!(c, CancelTurnChoice::StopRunning);
            }
            other => panic!("expected CancelTurnChoice(StopRunning), got {other:?}"),
        }
        assert_eq!(agent.cancel_turn_view.as_ref().unwrap().active_idx, 0);
    }
    #[test]
    fn click_third_row_dispatches_always_stop() {
        let mut agent = make_agent();
        setup_panel(&mut agent);
        let outcome = agent.handle_cancel_turn_mouse(&down(10, 12));
        match outcome {
            InputOutcome::Action(Action::CancelTurnChoice(c)) => {
                assert_eq!(c, CancelTurnChoice::AlwaysStop);
            }
            other => panic!("expected CancelTurnChoice(AlwaysStop), got {other:?}"),
        }
        assert_eq!(agent.cancel_turn_view.as_ref().unwrap().active_idx, 2);
    }
    #[test]
    fn click_outside_rows_consumes_event_without_action() {
        let mut agent = make_agent();
        setup_panel(&mut agent);
        let outcome = agent.handle_cancel_turn_mouse(&down(10, 50));
        assert!(matches!(outcome, InputOutcome::Unchanged));
        assert_eq!(agent.cancel_turn_view.as_ref().unwrap().active_idx, 0);
    }
    #[test]
    fn hover_moves_cursor_to_pointed_row() {
        let mut agent = make_agent();
        setup_panel(&mut agent);
        assert_eq!(agent.cancel_turn_view.as_ref().unwrap().active_idx, 0);
        let outcome = agent.handle_cancel_turn_mouse(&moved(10, 11));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(agent.cancel_turn_view.as_ref().unwrap().active_idx, 1);
        let outcome = agent.handle_cancel_turn_mouse(&moved(10, 13));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(agent.cancel_turn_view.as_ref().unwrap().active_idx, 3);
        let outcome = agent.handle_cancel_turn_mouse(&moved(15, 13));
        assert!(matches!(outcome, InputOutcome::Unchanged));
        assert_eq!(agent.cancel_turn_view.as_ref().unwrap().active_idx, 3);
        let outcome = agent.handle_cancel_turn_mouse(&moved(10, 50));
        assert!(matches!(outcome, InputOutcome::Unchanged));
        assert_eq!(agent.cancel_turn_view.as_ref().unwrap().active_idx, 3);
    }
    #[test]
    fn mouse_event_ignored_when_panel_closed() {
        let mut agent = make_agent();
        let outcome = agent.handle_cancel_turn_mouse(&down(10, 10));
        assert!(matches!(outcome, InputOutcome::Unchanged));
    }
    #[test]
    fn esc_confirm_refreshes_expired_rewind_grace() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use std::time::Instant;
        let mut agent = make_agent();
        agent.session.state = AgentState::TurnRunning;
        setup_panel(&mut agent);
        agent.rewind_suppress_deadline = Some(Instant::now());
        let outcome =
            agent.handle_cancel_turn_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(
                outcome,
                InputOutcome::Action(Action::CancelTurnChoice(CancelTurnChoice::ContinueToRun))
            ),
            "panel Esc must confirm the parent-turn cancel, got {outcome:?}"
        );
        assert!(
            agent.rewind_arm_suppressed(Instant::now()),
            "the Esc-confirmed cancel must refresh the post-cancel grace"
        );
    }
}
#[cfg(test)]
mod permission_mouse_tests {
    use super::test_fixtures::make_agent;
    use super::*;
    use crate::app::app_view::InputOutcome;
    use agent_client_protocol as acp;
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;
    use std::sync::Arc;
    use std::time::Duration;
    const OPTIONS_START_Y: u16 = 23;
    fn option(id: &str, kind: acp::PermissionOptionKind) -> acp::PermissionOption {
        acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from(id)),
            id.to_string(),
            kind,
        )
    }
    fn setup_permission(agent: &mut AgentView) {
        let mut perm = super::paste_key_tests::make_followup_permission_state();
        perm.focus = crate::views::permission_view::PermissionFocus::Options;
        perm.options = vec![
            option("opt-allow-once", acp::PermissionOptionKind::AllowOnce),
            option("opt-allow-always", acp::PermissionOptionKind::AllowAlways),
            option("opt-reject-once", acp::PermissionOptionKind::RejectOnce),
        ];
        agent.permission_queue.push_back(perm);
        agent.pane_areas.prompt = Rect::new(0, 20, 80, 10);
        assert_eq!(agent.permission_item_at(10, OPTIONS_START_Y), Some(0));
    }
    /// Option-row hit targets track the planned-args rows in both toggle
    /// states (hit-testing and render share the row-budget fn).
    #[test]
    fn permission_item_at_tracks_args_rows_collapsed_and_expanded() {
        let mut agent = make_agent();
        setup_permission(&mut agent);
        agent.pane_areas.prompt = Rect::new(0, 20, 80, 20);
        agent.permission_queue.front_mut().unwrap().description =
            (0..10).map(|i| format!("\"k{i}\": {i},")).collect();
        assert_eq!(agent.permission_item_at(10, 27), None, "chrome row");
        assert_eq!(agent.permission_item_at(10, 28), Some(0));
        assert_eq!(agent.permission_item_at(10, 30), Some(2));
        agent.permission_queue.front_mut().unwrap().args_expanded = true;
        assert_eq!(agent.permission_item_at(10, 28), None, "now a chrome row");
        assert_eq!(agent.permission_item_at(10, 33), Some(0));
        assert_eq!(agent.permission_item_at(10, 35), Some(2));
    }
    fn click_at(agent: &mut AgentView, registry: &ActionRegistry, row: u16) -> InputOutcome {
        agent.handle_input(
            &Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 10,
                row,
                modifiers: crossterm::event::KeyModifiers::empty(),
            }),
            registry,
        )
    }
    fn click_row(agent: &mut AgentView, registry: &ActionRegistry, idx: u16) -> InputOutcome {
        click_at(agent, registry, OPTIONS_START_Y + idx)
    }
    #[test]
    fn double_click_on_permission_row_submits_that_row() {
        let mut agent = make_agent();
        setup_permission(&mut agent);
        let registry = ActionRegistry::defaults();
        let first = click_row(&mut agent, &registry, 1);
        assert!(matches!(first, InputOutcome::Changed));
        assert_eq!(agent.permission_queue.front().unwrap().active_idx, 1);
        let second = click_row(&mut agent, &registry, 1);
        match second {
            InputOutcome::Action(Action::PermissionSelect(id)) => {
                assert_eq!(id.0.as_ref(), "opt-allow-always");
            }
            other => panic!("expected PermissionSelect, got {other:?}"),
        }
        assert!(agent.last_permission_click.is_none());
    }
    #[test]
    fn clicks_on_two_different_rows_select_but_do_not_submit() {
        let mut agent = make_agent();
        setup_permission(&mut agent);
        let registry = ActionRegistry::defaults();
        let first = click_row(&mut agent, &registry, 0);
        assert!(matches!(first, InputOutcome::Changed));
        assert_eq!(agent.permission_queue.front().unwrap().active_idx, 0);
        let second = click_row(&mut agent, &registry, 1);
        assert!(
            matches!(second, InputOutcome::Changed),
            "click on a different row must select, not submit; got {second:?}"
        );
        assert_eq!(agent.permission_queue.front().unwrap().active_idx, 1);
        assert!(matches!(agent.last_permission_click, Some((_, 1))));
    }
    #[test]
    fn slow_second_click_on_same_row_does_not_submit() {
        let mut agent = make_agent();
        setup_permission(&mut agent);
        let registry = ActionRegistry::defaults();
        let first = click_row(&mut agent, &registry, 1);
        assert!(matches!(first, InputOutcome::Changed));
        let stale = Instant::now() - Duration::from_millis(MULTI_CLICK_TIMEOUT_MS as u64 + 50);
        agent.last_permission_click = Some((stale, 1));
        let second = click_row(&mut agent, &registry, 1);
        assert!(
            matches!(second, InputOutcome::Changed),
            "second click after the double-click window must not submit; got {second:?}"
        );
        assert_eq!(agent.permission_queue.front().unwrap().active_idx, 1);
        assert!(matches!(agent.last_permission_click, Some((_, 1))));
    }
    #[test]
    fn click_on_chrome_between_row_clicks_does_not_submit() {
        let mut agent = make_agent();
        setup_permission(&mut agent);
        let registry = ActionRegistry::defaults();
        let first = click_row(&mut agent, &registry, 1);
        assert!(matches!(first, InputOutcome::Changed));
        let chrome = click_at(&mut agent, &registry, OPTIONS_START_Y - 1);
        assert!(matches!(chrome, InputOutcome::Changed));
        assert!(agent.last_permission_click.is_none());
        let third = click_row(&mut agent, &registry, 1);
        assert!(
            matches!(third, InputOutcome::Changed),
            "row click after an intervening chrome click must not submit; got {third:?}"
        );
        assert!(matches!(agent.last_permission_click, Some((_, 1))));
    }
}
#[cfg(test)]
mod permission_scope_key_tests {
    use super::test_fixtures::make_agent;
    use super::*;
    use agent_client_protocol as acp;
    use std::sync::Arc;
    fn option(id: &str, kind: acp::PermissionOptionKind) -> acp::PermissionOption {
        acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from(id)),
            id.to_string(),
            kind,
        )
    }
    /// Bash permission with both scoped rows and a 3-word primary command,
    /// mirroring the prompter's `[allow-always, once, reject, reject-always]`.
    fn setup_bash_permission(agent: &mut AgentView) {
        let mut perm = super::paste_key_tests::make_followup_permission_state();
        perm.focus = crate::views::permission_view::PermissionFocus::Options;
        perm.options = vec![
            option(
                "allow-always-command",
                acp::PermissionOptionKind::AllowAlways,
            ),
            option("allow-once", acp::PermissionOptionKind::AllowOnce),
            option("reject-once", acp::PermissionOptionKind::RejectOnce),
            option(
                "reject-always-command",
                acp::PermissionOptionKind::RejectAlways,
            ),
        ];
        perm.bash_highlights = Some(
            xai_grok_workspace::permission::bash_command_splitting::BashCommandHighlights {
                prefix: vec![],
                highlighted_words: vec!["cargo".into(), "test".into(), "--workspace".into()],
                suffix: vec![],
            },
        );
        perm.bash_selection_count = 2;
        agent.permission_queue.push_back(perm);
    }
    /// ←/→ on the "Never allow" (RejectAlways) row must adjust the scope in
    /// place — never yank the cursor onto the AllowAlways row, where Enter
    /// would persist a whitelist for the words the user was narrowing a deny
    /// for.
    #[test]
    fn scope_keys_keep_cursor_on_reject_always_row() {
        let mut agent = make_agent();
        setup_bash_permission(&mut agent);
        agent.permission_queue.front_mut().unwrap().active_idx = 3;
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::empty());
        let outcome = agent.handle_permission_key(&left);
        assert!(matches!(outcome, InputOutcome::Changed));
        let perm = agent.permission_queue.front().unwrap();
        assert_eq!(perm.active_idx, 3, "cursor must stay on the deny row");
        assert_eq!(perm.bash_selection_count, 1, "← must still narrow scope");
    }
    /// From a non-scoped row ←/→ still jump the cursor to the AllowAlways
    /// row (the discoverability affordance).
    #[test]
    fn scope_keys_still_jump_from_neutral_row() {
        let mut agent = make_agent();
        setup_bash_permission(&mut agent);
        agent.permission_queue.front_mut().unwrap().active_idx = 1;
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::empty());
        let outcome = agent.handle_permission_key(&right);
        assert!(matches!(outcome, InputOutcome::Changed));
        let perm = agent.permission_queue.front().unwrap();
        assert_eq!(perm.active_idx, 0, "cursor jumps to the AllowAlways row");
        assert_eq!(perm.bash_selection_count, 3, "→ must still expand scope");
    }
    /// Ctrl-F toggles args expansion in both focus modes, only when args
    /// exist.
    #[test]
    fn ctrl_f_toggles_args_expansion_when_args_present() {
        let mut agent = make_agent();
        setup_bash_permission(&mut agent);
        let ctrl_f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        agent.handle_permission_key(&ctrl_f);
        assert!(
            !agent.permission_queue.front().unwrap().args_expanded,
            "Ctrl-F must be a no-op without args"
        );
        agent.permission_queue.front_mut().unwrap().description =
            vec!["{".into(), "  \"k\": 1".into(), "}".into()];
        let outcome = agent.handle_permission_key(&ctrl_f);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(agent.permission_queue.front().unwrap().args_expanded);
        agent.permission_queue.front_mut().unwrap().focus =
            crate::views::permission_view::PermissionFocus::FollowupInput;
        let outcome = agent.handle_permission_key(&ctrl_f);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(!agent.permission_queue.front().unwrap().args_expanded);
    }
}
#[cfg(test)]
mod question_no_freeform_tests {
    //! Freeform ("Other") gating for `no_freeform` question modals — e.g.
    //! the SuperGrok upsell. Regression tests for the bug where clicking
    //! under the last option of the upsell selected the (hidden) freeform
    //! row and let the user type into a modal that offers no free text.
    use super::super::test_fixtures::make_agent;
    use crate::actions::ActionRegistry;
    use crate::app::agent_view::AgentView;
    use crate::views::prompt_widget::StashedPrompt;
    use crate::views::question_view::{QuestionFocus, QuestionSelection, QuestionViewState};
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use xai_grok_tools::implementations::grok_build::ask_user_question::{
        Question, QuestionOption,
    };
    /// Fixed options, single-select — shaped like the free-usage upsell.
    fn upsell_question() -> Question {
        let opt = |label: &str, desc: &str| QuestionOption {
            label: label.into(),
            description: desc.into(),
            preview: None,
            id: None,
        };
        Question {
            question: "You hit your free usage limit.".into(),
            options: vec![
                opt("Upgrade to SuperGrok", "For everyday coding"),
                opt("Upgrade to SuperGrok Heavy", "Highest usage limits"),
            ],
            multi_select: Some(false),
            id: None,
        }
    }
    fn open_question(agent: &mut AgentView, no_freeform: bool) {
        let state = QuestionViewState::new(
            "tc-upsell".into(),
            vec![upsell_question()],
            StashedPrompt::default(),
        );
        agent.question_view = Some(if no_freeform {
            state.with_no_freeform()
        } else {
            state
        });
    }
    /// Draw one 80x30 frame so `pane_areas` and `question_scroll_region`
    /// hold the real rendered layout the mouse handler hit-tests against.
    fn draw_frame(agent: &mut AgentView) {
        let area = Rect::new(0, 0, 80, 30);
        let reg = ActionRegistry::defaults();
        let bundle = crate::app::bundle::BundleState::default();
        let mut buf = Buffer::empty(area);
        let mut scratch = crate::scrollback::render::ScratchBuffer::new();
        agent.last_terminal_size = (80, 30);
        agent.draw(
            area,
            &mut buf,
            &reg,
            &mut scratch,
            None,
            false,
            crate::app::agent_view::BannerSlotParams::none(),
            &bundle,
            false,
            &mut Vec::new(),
            crate::app::agent_view::AppRenderParams::default(),
        );
    }
    fn down(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }
    fn moved(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Moved,
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }
    fn qv(agent: &AgentView) -> &QuestionViewState {
        agent.question_view.as_ref().expect("question view open")
    }
    /// Clicking the empty rows under the last option (option gap, footer)
    /// must be inert on a `no_freeform` modal: no InputMode, no freeform
    /// selection, no cursor move.
    #[test]
    fn click_below_last_option_is_inert_when_no_freeform() {
        let mut agent = make_agent();
        open_question(&mut agent, true);
        draw_frame(&mut agent);
        let (_, scroll_bottom) = agent.question_scroll_region.expect("scroll region set");
        let col = agent.pane_areas.prompt.x + 5;
        let pane_bottom = agent.pane_areas.prompt.y + agent.pane_areas.prompt.height;
        for row in scroll_bottom..pane_bottom {
            let _ = agent.handle_question_mouse(&down(col, row));
            let state = qv(&agent);
            assert_eq!(
                state.focus,
                QuestionFocus::Navigation,
                "row {row}: click below options must not enter InputMode"
            );
            assert!(
                !state.per_question_freeform_selected[0],
                "row {row}: freeform must not get selected"
            );
            assert!(
                matches!(state.selections[0], QuestionSelection::Single(None)),
                "row {row}: no option may get selected"
            );
            assert_eq!(state.cursor(), 0, "row {row}: cursor must not move");
        }
    }
    /// The last option row of a `no_freeform` modal occupies the screen row
    /// that hosts the sticky freeform row on regular modals — clicking it
    /// must toggle that option, not freeform.
    #[test]
    fn click_last_option_row_toggles_option_when_no_freeform() {
        let mut agent = make_agent();
        open_question(&mut agent, true);
        draw_frame(&mut agent);
        let (_, scroll_bottom) = agent.question_scroll_region.expect("scroll region set");
        let col = agent.pane_areas.prompt.x + 5;
        let last_option_row = scroll_bottom - 1;
        let _ = agent.handle_question_mouse(&down(col, last_option_row));
        let state = qv(&agent);
        assert_eq!(state.focus, QuestionFocus::Navigation);
        assert_eq!(state.cursor(), 1, "cursor lands on the last option");
        assert!(
            matches!(state.selections[0], QuestionSelection::Single(Some(1))),
            "click selects the last option, got {:?}",
            state.selections[0]
        );
        assert!(!state.per_question_freeform_selected[0]);
    }
    /// Hovering the rows below the options must not highlight the
    /// (nonexistent) freeform row on a `no_freeform` modal.
    #[test]
    fn hover_below_last_option_is_inert_when_no_freeform() {
        let mut agent = make_agent();
        open_question(&mut agent, true);
        draw_frame(&mut agent);
        let (_, scroll_bottom) = agent.question_scroll_region.expect("scroll region set");
        let col = agent.pane_areas.prompt.x + 5;
        let _ = agent.handle_question_mouse(&moved(col, scroll_bottom));
        assert_eq!(
            agent.hovered_question_item, None,
            "no phantom freeform hover below the options"
        );
    }
    /// The `z` shortcut (jump to freeform) must be inert on a `no_freeform`
    /// modal.
    #[test]
    fn z_key_is_inert_when_no_freeform() {
        let mut agent = make_agent();
        open_question(&mut agent, true);
        draw_frame(&mut agent);
        let z = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE);
        let _ = agent.handle_question_key(&z);
        let state = qv(&agent);
        assert_eq!(state.focus, QuestionFocus::Navigation);
        assert_eq!(state.cursor(), 0, "z must not move the cursor");
        assert!(!state.per_question_freeform_selected[0]);
    }
    /// Control group: on a regular modal (freeform present) the sticky
    /// freeform row sits one row below the options and clicking it still
    /// selects freeform and enters InputMode, and `z` still works.
    #[test]
    fn freeform_modal_click_and_z_still_enter_input_mode() {
        let mut agent = make_agent();
        open_question(&mut agent, false);
        draw_frame(&mut agent);
        let (_, scroll_bottom) = agent.question_scroll_region.expect("scroll region set");
        let col = agent.pane_areas.prompt.x + 5;
        let _ = agent.handle_question_mouse(&down(col, scroll_bottom));
        {
            let state = qv(&agent);
            assert_eq!(
                state.focus,
                QuestionFocus::InputMode,
                "clicking the sticky freeform row enters InputMode"
            );
            assert!(state.per_question_freeform_selected[0]);
        }
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let _ = agent.handle_question_key(&esc);
        assert_eq!(qv(&agent).focus, QuestionFocus::Navigation);
        let z = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE);
        let _ = agent.handle_question_key(&z);
        assert_eq!(qv(&agent).focus, QuestionFocus::InputMode);
    }
}
