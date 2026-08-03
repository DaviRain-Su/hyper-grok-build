use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;
use crate::views::prompt_widget::StashedPrompt;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RewindPointInfo {
    #[serde(alias = "promptIndex")]
    pub prompt_index: usize,
    #[serde(default, alias = "createdAt")]
    pub created_at: String,
    #[serde(default, alias = "numFileSnapshots")]
    pub num_file_snapshots: usize,
    #[serde(default, alias = "promptPreview")]
    pub prompt_preview: Option<String>,
    #[serde(default, alias = "hasFileChanges")]
    pub has_file_changes: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RewindPointsResponse {
    #[serde(alias = "rewindPoints")]
    pub rewind_points: Vec<RewindPointInfo>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RewindResponse {
    pub success: bool,
    #[serde(alias = "targetPromptIndex")]
    pub target_prompt_index: usize,
    #[serde(default, alias = "revertedFiles")]
    pub reverted_files: Vec<String>,
    #[serde(default, alias = "cleanFiles")]
    pub clean_files: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<RewindConflictInfo>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default, alias = "promptText")]
    pub prompt_text: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RewindConflictInfo {
    pub path: String,
    #[serde(alias = "conflictType")]
    pub conflict_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindMode {
    All,
    ConversationOnly,
    FilesOnly,
}

impl RewindMode {
    pub fn wire_value(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::ConversationOnly => "conversation_only",
            Self::FilesOnly => "files_only",
        }
    }

    pub fn display(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::ConversationOnly => "conversation only",
            Self::FilesOnly => "files only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewindPhase {
    Loading,
    Picker {
        points: Vec<RewindPointInfo>,
        selected: usize,
    },
    CancelOffer {
        active_idx: usize,
    },
    ModeSelect {
        target_prompt_index: usize,
        has_file_changes: bool,
        /// Whether the "File changes only" row exists at all. `false` in the
        /// inline edit-and-resubmit context, where the conversation rewind is
        /// a given and the only question is whether to also revert files.
        offer_files_only: bool,
        active_idx: usize,
    },
    Previewing {
        target_prompt_index: usize,
        mode: RewindMode,
    },
    Confirm {
        target_prompt_index: usize,
        mode: RewindMode,
        clean_files: Vec<String>,
        conflicts: Vec<ConflictDisplay>,
        active_idx: usize,
        prompt_preview: Option<String>,
    },
    ConversationOnlyConfirm {
        target_prompt_index: usize,
        active_idx: usize,
        prompt_preview: Option<String>,
    },
    Executing {
        target_prompt_index: usize,
        mode: RewindMode,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictDisplay {
    pub path: String,
    pub label: &'static str,
}

impl ConflictDisplay {
    pub fn from_conflict(c: &RewindConflictInfo) -> Self {
        let label = match c.conflict_type.as_str() {
            "deleted_externally" => "deleted",
            "created_externally" => "added",
            "modified_externally" => "modified",
            _ => "conflict",
        };
        Self {
            path: c.path.clone(),
            label,
        }
    }
}

#[derive(Debug)]
pub struct RewindState {
    pub phase: RewindPhase,
    pub anchor_entry_idx: usize,
    pub stashed_draft: Option<StashedPrompt>,
    pub selected_prompt_index: Option<usize>,
}

impl RewindState {
    pub fn new_cancel_offer(
        anchor: usize,
        draft: Option<StashedPrompt>,
        selected_prompt_index: Option<usize>,
    ) -> Self {
        Self {
            phase: RewindPhase::CancelOffer { active_idx: 0 },
            anchor_entry_idx: anchor,
            stashed_draft: draft,
            selected_prompt_index,
        }
    }

    pub fn new_mode_select(
        anchor: usize,
        target_prompt_index: usize,
        has_file_changes: bool,
        offer_files_only: bool,
        draft: Option<StashedPrompt>,
    ) -> Self {
        Self {
            phase: RewindPhase::ModeSelect {
                target_prompt_index,
                has_file_changes,
                offer_files_only,
                active_idx: 0,
            },
            anchor_entry_idx: anchor,
            stashed_draft: draft,
            selected_prompt_index: None,
        }
    }
}

pub enum RewindInput {
    Dismissed,
    CancelTurnThenProceed,
    SelectMode(RewindMode, usize),
    Confirm(usize, RewindMode),
    BackToModeSelect,
    DismissError,
    ConversationOnlyConfirm(usize),
    PickerSelect(usize),
    MoveUp,
    MoveDown,
    ConfirmCursor,
    Consumed,
}

const MODE_SELECT_OPTIONS: usize = 3;
const CANCEL_OFFER_OPTIONS: usize = 2;
const CONFIRM_OPTIONS: usize = 2;

fn mode_for_idx(idx: usize) -> RewindMode {
    match idx {
        0 => RewindMode::All,
        1 => RewindMode::ConversationOnly,
        _ => RewindMode::FilesOnly,
    }
}

pub fn handle_rewind_key(state: &RewindState, key: &KeyEvent) -> RewindInput {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return RewindInput::Consumed;
    }
    match &state.phase {
        RewindPhase::Picker { points, selected } => match key.code {
            KeyCode::Char('j') | KeyCode::Down => RewindInput::MoveDown,
            KeyCode::Char('k') | KeyCode::Up => RewindInput::MoveUp,
            KeyCode::Enter => {
                if let Some(p) = points.get(*selected) {
                    RewindInput::PickerSelect(p.prompt_index)
                } else {
                    RewindInput::Consumed
                }
            }
            KeyCode::Esc => RewindInput::Dismissed,
            _ => RewindInput::Consumed,
        },
        RewindPhase::CancelOffer { .. } => match key.code {
            KeyCode::Char('y') => RewindInput::CancelTurnThenProceed,
            KeyCode::Char('n') => RewindInput::Dismissed,
            KeyCode::Char('j') | KeyCode::Down => RewindInput::MoveDown,
            KeyCode::Char('k') | KeyCode::Up => RewindInput::MoveUp,
            KeyCode::Enter => RewindInput::ConfirmCursor,
            KeyCode::Esc => RewindInput::Dismissed,
            _ => RewindInput::Consumed,
        },
        RewindPhase::ModeSelect {
            target_prompt_index,
            has_file_changes,
            offer_files_only,
            ..
        } => {
            let idx = *target_prompt_index;
            match key.code {
                KeyCode::Char('a') => RewindInput::SelectMode(RewindMode::All, idx),
                // Two-row inline variant letters its rows a/b; 'c' stays as
                // an alias so classic-flow muscle memory keeps working.
                KeyCode::Char('b') if !*offer_files_only => {
                    RewindInput::SelectMode(RewindMode::ConversationOnly, idx)
                }
                KeyCode::Char('c') => RewindInput::SelectMode(RewindMode::ConversationOnly, idx),
                KeyCode::Char('f') if *offer_files_only && *has_file_changes => {
                    RewindInput::SelectMode(RewindMode::FilesOnly, idx)
                }
                KeyCode::Char('j') | KeyCode::Down => RewindInput::MoveDown,
                KeyCode::Char('k') | KeyCode::Up => RewindInput::MoveUp,
                KeyCode::Enter => RewindInput::ConfirmCursor,
                KeyCode::Esc => RewindInput::Dismissed,
                _ => RewindInput::Consumed,
            }
        }
        RewindPhase::Confirm {
            target_prompt_index,
            mode,
            ..
        } => match key.code {
            KeyCode::Char('y') => RewindInput::Confirm(*target_prompt_index, *mode),
            KeyCode::Char('j') | KeyCode::Down => RewindInput::MoveDown,
            KeyCode::Char('k') | KeyCode::Up => RewindInput::MoveUp,
            KeyCode::Enter => RewindInput::ConfirmCursor,
            // Esc dismisses every phase; Backspace is the "back" gesture.
            KeyCode::Esc => RewindInput::Dismissed,
            KeyCode::Backspace => RewindInput::BackToModeSelect,
            _ => RewindInput::Consumed,
        },
        RewindPhase::ConversationOnlyConfirm {
            target_prompt_index,
            ..
        } => match key.code {
            KeyCode::Char('y') => RewindInput::ConversationOnlyConfirm(*target_prompt_index),
            KeyCode::Char('j') | KeyCode::Down => RewindInput::MoveDown,
            KeyCode::Char('k') | KeyCode::Up => RewindInput::MoveUp,
            KeyCode::Enter => RewindInput::ConfirmCursor,
            KeyCode::Esc => RewindInput::Dismissed,
            KeyCode::Backspace => RewindInput::BackToModeSelect,
            _ => RewindInput::Consumed,
        },
        RewindPhase::Error { .. } => match key.code {
            KeyCode::Esc | KeyCode::Enter => RewindInput::DismissError,
            _ => RewindInput::Consumed,
        },
        RewindPhase::Loading | RewindPhase::Previewing { .. } => match key.code {
            KeyCode::Esc => RewindInput::Dismissed,
            _ => RewindInput::Consumed,
        },
        RewindPhase::Executing { .. } => RewindInput::Consumed,
    }
}

pub fn move_cursor(phase: &mut RewindPhase, delta: i32) {
    match phase {
        RewindPhase::Picker { points, selected } => {
            if points.is_empty() {
                return;
            }
            let max = points.len() as i32 - 1;
            let new = (*selected as i32 + delta).clamp(0, max);
            *selected = new as usize;
        }
        RewindPhase::CancelOffer { active_idx } => {
            let new = (*active_idx as i32 + delta).clamp(0, CANCEL_OFFER_OPTIONS as i32 - 1);
            *active_idx = new as usize;
        }
        RewindPhase::ModeSelect {
            active_idx,
            has_file_changes,
            offer_files_only,
            ..
        } => {
            let max = if *offer_files_only && *has_file_changes {
                MODE_SELECT_OPTIONS - 1
            } else {
                MODE_SELECT_OPTIONS - 2
            };
            let new = (*active_idx as i32 + delta).clamp(0, max as i32);
            *active_idx = new as usize;
        }
        RewindPhase::Confirm { active_idx, .. } => {
            let new = (*active_idx as i32 + delta).clamp(0, CONFIRM_OPTIONS as i32 - 1);
            *active_idx = new as usize;
        }
        RewindPhase::ConversationOnlyConfirm { active_idx, .. } => {
            let new = (*active_idx as i32 + delta).clamp(0, CONFIRM_OPTIONS as i32 - 1);
            *active_idx = new as usize;
        }
        _ => {}
    }
}

pub fn confirm_cursor(phase: &RewindPhase) -> RewindInput {
    match phase {
        RewindPhase::CancelOffer { active_idx } => match active_idx {
            0 => RewindInput::CancelTurnThenProceed,
            _ => RewindInput::Dismissed,
        },
        RewindPhase::ModeSelect {
            target_prompt_index,
            active_idx,
            ..
        } => {
            let mode = mode_for_idx(*active_idx);
            RewindInput::SelectMode(mode, *target_prompt_index)
        }
        RewindPhase::Confirm {
            target_prompt_index,
            mode,
            active_idx,
            ..
        } => match active_idx {
            0 => RewindInput::Confirm(*target_prompt_index, *mode),
            _ => RewindInput::BackToModeSelect,
        },
        RewindPhase::ConversationOnlyConfirm {
            target_prompt_index,
            active_idx,
            ..
        } => match active_idx {
            0 => RewindInput::ConversationOnlyConfirm(*target_prompt_index),
            _ => RewindInput::BackToModeSelect,
        },
        _ => RewindInput::Consumed,
    }
}

/// Hit-test a screen position against the rewind overlay's clickable rows.
///
/// Returns the logical cursor index under `(col, row)` for the current
/// phase, or `None` if the position is not on a selectable row.
///
/// IMPORTANT: the row geometry here mirrors `render_rewind_overlay`. Keep
/// this, `render_rewind_overlay`, and `rewind_overlay_height` in sync when
/// changing layout.
pub fn rewind_row_at(phase: &RewindPhase, area: Rect, col: u16, row: u16) -> Option<usize> {
    if area.height == 0 || area.width < 10 {
        return None;
    }
    if col < area.x || col >= area.x + area.width {
        return None;
    }
    if row < area.y || row >= area.y + area.height {
        return None;
    }
    match phase {
        RewindPhase::Picker { points, selected } => crate::views::overlay_list::ListOverlay {
            len: points.len(),
            selected: *selected,
        }
        .row_at(area, col, row),
        RewindPhase::CancelOffer { .. } => match row.checked_sub(area.y + 3) {
            Some(0) => Some(0),
            Some(1) => Some(1),
            _ => None,
        },
        RewindPhase::ModeSelect {
            has_file_changes,
            offer_files_only,
            ..
        } => match row.checked_sub(area.y + 2) {
            Some(0) => Some(0),
            Some(1) => Some(1),
            // With the row hidden entirely (inline resubmit), the overlay is
            // one row shorter and there is nothing at index 2 to hit.
            Some(2) if *offer_files_only && *has_file_changes => Some(2),
            _ => None,
        },
        RewindPhase::Confirm {
            clean_files,
            conflicts,
            ..
        } => {
            let clean_rows = clean_files.len().min(5) + if clean_files.len() > 5 { 1 } else { 0 };
            let conflict_rows = conflicts.len().min(5) + if conflicts.len() > 5 { 1 } else { 0 };
            let gap = if !clean_files.is_empty() || !conflicts.is_empty() {
                1
            } else {
                0
            };
            let r0 = area.y + 2 + clean_rows as u16 + conflict_rows as u16 + gap;
            match row.checked_sub(r0) {
                Some(0) => Some(0),
                Some(1) => Some(1),
                _ => None,
            }
        }
        RewindPhase::ConversationOnlyConfirm { .. } => match row.checked_sub(area.y + 3) {
            Some(0) => Some(0),
            Some(1) => Some(1),
            _ => None,
        },
        RewindPhase::Error { .. } => {
            if row == area.y + 3 {
                Some(0)
            } else {
                None
            }
        }
        RewindPhase::Loading | RewindPhase::Previewing { .. } | RewindPhase::Executing { .. } => {
            None
        }
    }
}

/// Move the overlay cursor/selection to `idx` (used by mouse hover/click).
/// Returns `true` if the stored cursor changed.
pub fn set_rewind_cursor(phase: &mut RewindPhase, idx: usize) -> bool {
    match phase {
        RewindPhase::Picker { points, selected } => {
            if points.is_empty() {
                return false;
            }
            let new = idx.min(points.len() - 1);
            if *selected != new {
                *selected = new;
                true
            } else {
                false
            }
        }
        RewindPhase::CancelOffer { active_idx } => {
            let new = idx.min(CANCEL_OFFER_OPTIONS - 1);
            if *active_idx != new {
                *active_idx = new;
                true
            } else {
                false
            }
        }
        RewindPhase::ModeSelect {
            active_idx,
            has_file_changes,
            offer_files_only,
            ..
        } => {
            let max = if *offer_files_only && *has_file_changes {
                MODE_SELECT_OPTIONS - 1
            } else {
                MODE_SELECT_OPTIONS - 2
            };
            let new = idx.min(max);
            if *active_idx != new {
                *active_idx = new;
                true
            } else {
                false
            }
        }
        RewindPhase::Confirm { active_idx, .. } => {
            let new = idx.min(CONFIRM_OPTIONS - 1);
            if *active_idx != new {
                *active_idx = new;
                true
            } else {
                false
            }
        }
        RewindPhase::ConversationOnlyConfirm { active_idx, .. } => {
            let new = idx.min(CONFIRM_OPTIONS - 1);
            if *active_idx != new {
                *active_idx = new;
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

/// The activation input for the current cursor position — equivalent to
/// pressing Enter on the focused row. Used by mouse-click handling.
pub fn rewind_activate(phase: &RewindPhase) -> RewindInput {
    match phase {
        RewindPhase::Picker { points, selected } => points
            .get(*selected)
            .map(|p| RewindInput::PickerSelect(p.prompt_index))
            .unwrap_or(RewindInput::Consumed),
        RewindPhase::Error { .. } => RewindInput::DismissError,
        other => confirm_cursor(other),
    }
}

pub fn rewind_overlay_height(phase: &RewindPhase, screen_h: u16) -> u16 {
    let content = match phase {
        RewindPhase::Loading => 2,
        RewindPhase::Picker { points, selected } => {
            return crate::views::overlay_list::ListOverlay {
                len: points.len(),
                selected: *selected,
            }
            .height(screen_h);
        }
        RewindPhase::CancelOffer { .. } => 5,
        // One row shorter when the "File changes only" row is hidden
        // (inline edit-and-resubmit context).
        RewindPhase::ModeSelect {
            offer_files_only, ..
        } => {
            if *offer_files_only {
                5
            } else {
                4
            }
        }
        RewindPhase::Previewing { .. } | RewindPhase::Executing { .. } => 2,
        RewindPhase::Confirm {
            clean_files,
            conflicts,
            ..
        } => {
            let file_lines = clean_files.len().min(5) + conflicts.len().min(5);
            let extra_clean = if clean_files.len() > 5 { 1 } else { 0 };
            let extra_conflicts = if conflicts.len() > 5 { 1 } else { 0 };
            let gap = if file_lines > 0 { 1 } else { 0 };
            (4 + file_lines + extra_clean + extra_conflicts + gap) as u16
        }
        RewindPhase::ConversationOnlyConfirm { .. } => 5,
        RewindPhase::Error { .. } => 4,
    };
    content + 1
}

pub fn render_rewind_overlay(buf: &mut Buffer, area: Rect, phase: &RewindPhase, focused: bool) {
    if area.height == 0 || area.width < 10 {
        return;
    }

    let theme = Theme::current();
    let bg = theme.bg_light;

    buf.set_style(area, Style::default().bg(bg));

    let accent_style = Style::default().fg(theme.accent_user);
    for row in area.y..area.y + area.height {
        if let Some(cell) = buf.cell_mut((area.x, row)) {
            cell.set_symbol(crate::glyphs::accent_bar());
            cell.set_style(accent_style);
        }
    }

    let content_x = area.x + 3;
    let content_w = area.width.saturating_sub(5);

    let title_style = Style::default()
        .fg(theme.accent_user)
        .add_modifier(Modifier::BOLD);

    match phase {
        RewindPhase::Loading => {
            let y = area.y + 1;
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(
                    "Loading rewind points...",
                    Style::default().fg(theme.gray),
                )),
                content_w,
            );
        }
        RewindPhase::Picker { points, selected } => {
            // Shared list-overlay chrome + row geometry (also used by /jump).
            // It applies the unfocus dim itself, so return before the shared
            // blend at the bottom of this function.
            crate::views::overlay_list::ListOverlay {
                len: points.len(),
                selected: *selected,
            }
            .render(buf, area, "Rewind to which turn?", focused, |i, ctx| {
                let point = &points[i];
                let dot_style = Style::default().fg(theme.gray).bg(ctx.row_bg);
                let preview: String = crate::render::line_utils::truncate_str(
                    point.prompt_preview.as_deref().unwrap_or("(no preview)"),
                    ctx.content_width.saturating_sub(8) as usize,
                );
                let text_style = Style::default()
                    .fg(theme.text_primary)
                    .bg(ctx.row_bg)
                    .add_modifier(if ctx.is_cursor {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    });
                let meta_style = Style::default().fg(theme.gray).bg(ctx.row_bg);

                let file_info = if point.has_file_changes {
                    format!(" \u{00B7} {} files", point.num_file_snapshots)
                } else {
                    String::new()
                };

                Line::from(vec![
                    Span::styled("\u{00B7} ", dot_style),
                    Span::styled(preview, text_style),
                    Span::styled(file_info, meta_style),
                ])
            });
            return;
        }
        RewindPhase::CancelOffer { active_idx } => {
            let mut y = area.y + 1;
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled("A turn is currently running.", title_style)),
                content_w,
            );
            y += 1;
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(
                    "Would you like to cancel it before rewinding?",
                    Style::default().fg(theme.gray),
                )),
                content_w,
            );
            y += 1;
            render_radio_row(
                buf,
                content_x,
                y,
                content_w,
                'y',
                "Cancel turn and rewind",
                true,
                *active_idx == 0,
                focused,
                &theme,
            );
            y += 1;
            render_radio_row(
                buf,
                content_x,
                y,
                content_w,
                'n',
                "Let it finish",
                true,
                *active_idx == 1,
                focused,
                &theme,
            );
        }
        RewindPhase::ModeSelect {
            has_file_changes,
            offer_files_only,
            active_idx,
            ..
        } => {
            let mut y = area.y + 1;
            // Inline edit-and-resubmit: the conversation rewind is a given —
            // the only question is whether files come along.
            let title = if *offer_files_only {
                "What do you want to rewind?"
            } else {
                "Resubmit from here \u{2014} what should be rewound?"
            };
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(title, title_style)),
                content_w,
            );
            y += 1;
            render_radio_row(
                buf,
                content_x,
                y,
                content_w,
                'a',
                "Both conversation and file changes",
                true,
                *active_idx == 0,
                focused,
                &theme,
            );
            y += 1;
            render_radio_row(
                buf,
                content_x,
                y,
                content_w,
                // Sequential lettering in the two-row inline variant; the
                // mnemonic 'c' only reads right with the 'f' row present.
                if *offer_files_only { 'c' } else { 'b' },
                "Conversation only",
                true,
                *active_idx == 1,
                focused,
                &theme,
            );
            if *offer_files_only {
                y += 1;
                render_radio_row(
                    buf,
                    content_x,
                    y,
                    content_w,
                    'f',
                    "File changes only",
                    *has_file_changes,
                    *active_idx == 2,
                    focused,
                    &theme,
                );
            }
        }
        RewindPhase::Previewing { .. } => {
            let y = area.y + 1;
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(
                    "Previewing file changes...",
                    Style::default().fg(theme.gray),
                )),
                content_w,
            );
        }
        RewindPhase::Executing { .. } => {
            let y = area.y + 1;
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(
                    "Rewinding...",
                    Style::default().fg(theme.gray),
                )),
                content_w,
            );
        }
        RewindPhase::Confirm {
            clean_files,
            conflicts,
            mode,
            active_idx,
            prompt_preview,
            ..
        } => {
            let mut y = area.y + 1;
            let file_total = clean_files.len() + conflicts.len();
            let preview_text = prompt_preview.as_deref().unwrap_or("this turn");
            let (prefix, suffix) = match mode {
                RewindMode::All => {
                    if file_total > 0 {
                        (
                            "Rewind file changes and conversation to \u{201C}",
                            format!("\u{201D}? ({file_total} files)"),
                        )
                    } else {
                        (
                            "Rewind file changes and conversation to \u{201C}",
                            "\u{201D}?".to_string(),
                        )
                    }
                }
                RewindMode::ConversationOnly => (
                    "Rewind conversation only to \u{201C}",
                    "\u{201D}?".to_string(),
                ),
                RewindMode::FilesOnly => {
                    if file_total > 0 {
                        (
                            "Rewind file changes only to \u{201C}",
                            format!("\u{201D}? ({file_total} files)"),
                        )
                    } else {
                        (
                            "Rewind file changes only to \u{201C}",
                            "\u{201D}?".to_string(),
                        )
                    }
                }
            };
            let chrome = prefix.chars().count() + suffix.chars().count();
            let max_preview = (content_w as usize).saturating_sub(chrome + 1);
            let preview_trunc: String = if preview_text.chars().count() > max_preview {
                let truncated: String = preview_text
                    .chars()
                    .take(max_preview.saturating_sub(1))
                    .collect();
                format!("{truncated}\u{2026}")
            } else {
                preview_text.to_string()
            };
            let title = format!("{prefix}{preview_trunc}{suffix}");
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(title, title_style)),
                content_w,
            );
            y += 1;

            for (i, path) in clean_files.iter().enumerate() {
                if i >= 5 {
                    let more = format!("+{} more", clean_files.len() - 5);
                    buf.set_line(
                        content_x,
                        y,
                        &Line::from(Span::styled(more, Style::default().fg(theme.gray))),
                        content_w,
                    );
                    y += 1;
                    break;
                }
                buf.set_line(
                    content_x,
                    y,
                    &Line::from(Span::styled(
                        path.to_string(),
                        Style::default().fg(theme.gray),
                    )),
                    content_w,
                );
                y += 1;
            }
            for (i, conflict) in conflicts.iter().enumerate() {
                if i >= 5 {
                    let more = format!("+{} more", conflicts.len() - 5);
                    buf.set_line(
                        content_x,
                        y,
                        &Line::from(Span::styled(more, Style::default().fg(theme.gray))),
                        content_w,
                    );
                    y += 1;
                    break;
                }
                let line_text = format!("! {} ({})", conflict.path, conflict.label);
                buf.set_line(
                    content_x,
                    y,
                    &Line::from(Span::styled(line_text, Style::default().fg(theme.warning))),
                    content_w,
                );
                y += 1;
            }

            if !clean_files.is_empty() || !conflicts.is_empty() {
                y += 1;
            }

            render_radio_row(
                buf,
                content_x,
                y,
                content_w,
                'y',
                "Confirm rewind",
                true,
                *active_idx == 0,
                focused,
                &theme,
            );
            y += 1;
            render_radio_row(
                buf,
                content_x,
                y,
                content_w,
                '\x08',
                "Back",
                true,
                *active_idx == 1,
                focused,
                &theme,
            );
        }
        RewindPhase::ConversationOnlyConfirm {
            active_idx,
            prompt_preview,
            ..
        } => {
            let mut y = area.y + 1;
            let preview_text = prompt_preview.as_deref().unwrap_or("this turn");
            let prefix = "Rewind conversation only to \u{201C}";
            let suffix = "\u{201D}?";
            let chrome = prefix.chars().count() + suffix.chars().count();
            let max_preview = (content_w as usize).saturating_sub(chrome + 1);
            let preview_trunc: String = if preview_text.chars().count() > max_preview {
                let truncated: String = preview_text
                    .chars()
                    .take(max_preview.saturating_sub(1))
                    .collect();
                format!("{truncated}\u{2026}")
            } else {
                preview_text.to_string()
            };
            let title = format!("{prefix}{preview_trunc}{suffix}");
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(title, title_style)),
                content_w,
            );
            y += 1;
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(
                    "File effects from removed turns will be orphaned.",
                    Style::default().fg(theme.warning),
                )),
                content_w,
            );
            y += 1;
            render_radio_row(
                buf,
                content_x,
                y,
                content_w,
                'y',
                "Confirm rewind",
                true,
                *active_idx == 0,
                focused,
                &theme,
            );
            y += 1;
            render_radio_row(
                buf,
                content_x,
                y,
                content_w,
                '\x08',
                "Back",
                true,
                *active_idx == 1,
                focused,
                &theme,
            );
        }
        RewindPhase::Error { message } => {
            let mut y = area.y + 1;
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(
                    "Rewind failed",
                    Style::default()
                        .fg(theme.accent_error)
                        .add_modifier(Modifier::BOLD),
                )),
                content_w,
            );
            y += 1;
            let truncated: String = message.chars().take(content_w as usize).collect();
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(
                    truncated,
                    Style::default().fg(theme.text_primary),
                )),
                content_w,
            );
            y += 1;
            render_radio_row(
                buf, content_x, y, content_w, '\x1b', "Dismiss", true, true, focused, &theme,
            );
        }
    }

    // Unfocus dim: when the prompt area is unfocused (e.g. user moved
    // to scrollback), blend foregrounds toward `bg_light` so the panel
    // visually recedes. Mirrors the unfocused prompt widget pattern
    // (`prompt_widget.rs:1948`).
    if !focused {
        crate::render::color::blend_area(buf, area, Some((bg, 0.66)), None);
    }
}

/// Visible label for sentinel-encoded keys (`Esc`, `Bksp`).
fn key_label(key: char) -> String {
    match key {
        '\x1b' => "Esc".into(),
        '\x08' => "Bksp".into(),
        other => other.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_radio_row(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    w: u16,
    key: char,
    label: &str,
    enabled: bool,
    is_cursor: bool,
    panel_focused: bool,
    theme: &Theme,
) {
    let bg = if is_cursor && panel_focused {
        theme.bg_visual
    } else {
        theme.bg_light
    };

    let row_rect = Rect {
        x: x.saturating_sub(1),
        y,
        width: w + 2,
        height: 1,
    };
    buf.set_style(row_rect, Style::default().bg(bg));

    if !enabled {
        let dim_style = Style::default().fg(theme.gray_dim).bg(bg);
        let key_display = key_label(key);
        let line = Line::from(vec![
            Span::styled(format!("{key_display:<4}"), dim_style),
            Span::styled("(\u{25CB}) ", dim_style),
            Span::styled(label.to_string(), dim_style),
        ]);
        buf.set_line(x, y, &line, w);
        return;
    }

    // Contract: callers MUST push rows in `active_idx` order and place
    // any disabled rows at the tail of a phase, because the mouse

    let marker = if is_cursor {
        crate::glyphs::filled_dot()
    } else {
        "\u{25CB}"
    };
    let key_display = key_label(key);

    let num_style = Style::default().fg(theme.accent_user).bg(bg);
    let marker_style = if is_cursor {
        Style::default().fg(theme.accent_user).bg(bg)
    } else {
        Style::default().fg(theme.gray).bg(bg)
    };
    let label_style = Style::default()
        .fg(theme.text_primary)
        .bg(bg)
        .add_modifier(if is_cursor {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });

    let line = Line::from(vec![
        Span::styled(format!("{key_display:<4}"), num_style),
        Span::styled(format!("({marker}) "), marker_style),
        Span::styled(label.to_string(), label_style),
    ]);
    buf.set_line(x, y, &line, w);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyModifiers};

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        }
    }

    fn point(prompt_index: usize) -> RewindPointInfo {
        RewindPointInfo {
            prompt_index,
            created_at: String::new(),
            num_file_snapshots: 0,
            prompt_preview: Some(format!("turn {prompt_index}")),
            has_file_changes: false,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        }
    }

    fn confirm_state() -> RewindState {
        RewindState {
            phase: RewindPhase::Confirm {
                target_prompt_index: 3,
                mode: RewindMode::All,
                clean_files: vec![],
                conflicts: vec![],
                active_idx: 0,
                prompt_preview: None,
            },
            anchor_entry_idx: 0,
            stashed_draft: None,
            selected_prompt_index: Some(3),
        }
    }

    fn conv_only_state() -> RewindState {
        RewindState {
            phase: RewindPhase::ConversationOnlyConfirm {
                target_prompt_index: 3,
                active_idx: 0,
                prompt_preview: None,
            },
            anchor_entry_idx: 0,
            stashed_draft: None,
            selected_prompt_index: Some(3),
        }
    }

    #[test]
    fn picker_row_hit_test_maps_to_point_index() {
        let phase = RewindPhase::Picker {
            points: vec![point(0), point(1), point(2)],
            selected: 0,
        };
        // Title is at y+1; rows start at y+2.
        assert_eq!(rewind_row_at(&phase, area(), 5, 1), None);
        assert_eq!(rewind_row_at(&phase, area(), 5, 2), Some(0));
        assert_eq!(rewind_row_at(&phase, area(), 5, 4), Some(2));
        // Past the last point.
        assert_eq!(rewind_row_at(&phase, area(), 5, 5), None);
        // Outside the overlay horizontally.
        assert_eq!(rewind_row_at(&phase, area(), 99, 2), None);
    }

    #[test]
    fn cancel_offer_rows() {
        let phase = RewindPhase::CancelOffer { active_idx: 0 };
        assert_eq!(rewind_row_at(&phase, area(), 5, 3), Some(0));
        assert_eq!(rewind_row_at(&phase, area(), 5, 4), Some(1));
        assert_eq!(rewind_row_at(&phase, area(), 5, 5), None);
    }

    #[test]
    fn mode_select_skips_disabled_files_row() {
        let with_files = RewindPhase::ModeSelect {
            target_prompt_index: 7,
            has_file_changes: true,
            offer_files_only: true,
            active_idx: 0,
        };
        assert_eq!(rewind_row_at(&with_files, area(), 5, 2), Some(0));
        assert_eq!(rewind_row_at(&with_files, area(), 5, 3), Some(1));
        assert_eq!(rewind_row_at(&with_files, area(), 5, 4), Some(2));

        let no_files = RewindPhase::ModeSelect {
            target_prompt_index: 7,
            has_file_changes: false,
            offer_files_only: true,
            active_idx: 0,
        };
        // The "file changes only" row is disabled and not clickable.
        assert_eq!(rewind_row_at(&no_files, area(), 5, 4), None);
    }

    /// Inline edit-and-resubmit: the "File changes only" row does not exist
    /// at all — the mouse hit-test has nothing at index 2 even when the
    /// point has file changes, and the overlay is one row shorter.
    #[test]
    fn mode_select_without_files_only_offer_has_no_third_row() {
        let inline = RewindPhase::ModeSelect {
            target_prompt_index: 7,
            has_file_changes: true,
            offer_files_only: false,
            active_idx: 0,
        };
        assert_eq!(rewind_row_at(&inline, area(), 5, 2), Some(0));
        assert_eq!(rewind_row_at(&inline, area(), 5, 3), Some(1));
        assert_eq!(rewind_row_at(&inline, area(), 5, 4), None, "row hidden");

        let classic = RewindPhase::ModeSelect {
            target_prompt_index: 7,
            has_file_changes: true,
            offer_files_only: true,
            active_idx: 0,
        };
        assert_eq!(
            rewind_overlay_height(&inline, 40) + 1,
            rewind_overlay_height(&classic, 40),
            "hidden row shrinks the overlay by exactly one row"
        );
    }

    /// The cursor can never land on the hidden files-only index: both the
    /// keyboard move and the mouse set-cursor clamp to "Conversation only".
    #[test]
    fn cursor_cannot_reach_hidden_files_only_row() {
        let mut phase = RewindPhase::ModeSelect {
            target_prompt_index: 0,
            has_file_changes: true,
            offer_files_only: false,
            active_idx: 0,
        };
        move_cursor(&mut phase, 1);
        move_cursor(&mut phase, 1);
        move_cursor(&mut phase, 1);
        if let RewindPhase::ModeSelect { active_idx, .. } = phase {
            assert_eq!(active_idx, 1, "keyboard clamps below the hidden row");
        } else {
            panic!("expected mode select");
        }

        set_rewind_cursor(&mut phase, 2);
        if let RewindPhase::ModeSelect { active_idx, .. } = phase {
            assert_eq!(active_idx, 1, "mouse clamps below the hidden row");
        } else {
            panic!("expected mode select");
        }
    }

    /// 'f' is ignored when the files-only row is hidden, even though the
    /// point has file changes (it would be selectable in the classic flow).
    #[test]
    fn f_key_ignored_when_files_only_row_hidden() {
        let state = RewindState {
            phase: RewindPhase::ModeSelect {
                target_prompt_index: 3,
                has_file_changes: true,
                offer_files_only: false,
                active_idx: 0,
            },
            anchor_entry_idx: 0,
            stashed_draft: None,
            selected_prompt_index: Some(3),
        };
        assert!(matches!(
            handle_rewind_key(&state, &key(KeyCode::Char('f'))),
            RewindInput::Consumed
        ));

        // Classic flow: same point, row offered → 'f' selects FilesOnly.
        let classic = RewindState::new_mode_select(0, 3, true, true, None);
        assert!(matches!(
            handle_rewind_key(&classic, &key(KeyCode::Char('f'))),
            RewindInput::SelectMode(RewindMode::FilesOnly, 3)
        ));
    }

    /// The two-row inline variant letters its rows a/b: 'b' selects
    /// conversation-only there ('c' stays as an alias), while the classic
    /// three-row popup ignores 'b' and keeps the mnemonic 'c'.
    #[test]
    fn inline_mode_select_letters_rows_a_b() {
        let inline = RewindState {
            phase: RewindPhase::ModeSelect {
                target_prompt_index: 3,
                has_file_changes: true,
                offer_files_only: false,
                active_idx: 0,
            },
            anchor_entry_idx: 0,
            stashed_draft: None,
            selected_prompt_index: Some(3),
        };
        assert!(matches!(
            handle_rewind_key(&inline, &key(KeyCode::Char('b'))),
            RewindInput::SelectMode(RewindMode::ConversationOnly, 3)
        ));
        assert!(matches!(
            handle_rewind_key(&inline, &key(KeyCode::Char('c'))),
            RewindInput::SelectMode(RewindMode::ConversationOnly, 3)
        ));

        let classic = RewindState::new_mode_select(0, 3, true, true, None);
        assert!(matches!(
            handle_rewind_key(&classic, &key(KeyCode::Char('b'))),
            RewindInput::Consumed
        ));
    }

    #[test]
    fn confirm_radio_rows_track_file_line_count() {
        let no_files = RewindPhase::Confirm {
            target_prompt_index: 1,
            mode: RewindMode::All,
            clean_files: vec![],
            conflicts: vec![],
            active_idx: 0,
            prompt_preview: None,
        };
        // No file lines, no gap: radios immediately after title.
        assert_eq!(rewind_row_at(&no_files, area(), 5, 2), Some(0));
        assert_eq!(rewind_row_at(&no_files, area(), 5, 3), Some(1));

        let with_files = RewindPhase::Confirm {
            target_prompt_index: 1,
            mode: RewindMode::All,
            clean_files: vec!["a.rs".into(), "b.rs".into()],
            conflicts: vec![],
            active_idx: 0,
            prompt_preview: None,
        };
        // title(1) + 2 file rows + 1 gap → radios at y+5/y+6.
        assert_eq!(rewind_row_at(&with_files, area(), 5, 5), Some(0));
        assert_eq!(rewind_row_at(&with_files, area(), 5, 6), Some(1));
    }

    #[test]
    fn error_dismiss_row() {
        let phase = RewindPhase::Error {
            message: "boom".into(),
        };
        assert_eq!(rewind_row_at(&phase, area(), 5, 3), Some(0));
        assert_eq!(rewind_row_at(&phase, area(), 5, 2), None);
    }

    #[test]
    fn non_interactive_phases_have_no_rows() {
        for phase in [
            RewindPhase::Loading,
            RewindPhase::Previewing {
                target_prompt_index: 0,
                mode: RewindMode::All,
            },
            RewindPhase::Executing {
                target_prompt_index: 0,
                mode: RewindMode::All,
            },
        ] {
            for row in 0..10 {
                assert_eq!(rewind_row_at(&phase, area(), 5, row), None);
            }
        }
    }

    #[test]
    fn set_cursor_moves_and_clamps() {
        let mut phase = RewindPhase::Picker {
            points: vec![point(0), point(1)],
            selected: 0,
        };
        assert!(set_rewind_cursor(&mut phase, 1));
        assert!(!set_rewind_cursor(&mut phase, 1)); // no change
        // Clamp out-of-range to last point (already at last → no change).
        assert!(!set_rewind_cursor(&mut phase, 99));
        if let RewindPhase::Picker { selected, .. } = phase {
            assert_eq!(selected, 1);
        } else {
            panic!("expected picker");
        }

        let mut mode = RewindPhase::ModeSelect {
            target_prompt_index: 0,
            has_file_changes: false,
            offer_files_only: true,
            active_idx: 0,
        };
        // FilesOnly index clamps to 1 when files row is disabled.
        set_rewind_cursor(&mut mode, 2);
        if let RewindPhase::ModeSelect { active_idx, .. } = mode {
            assert_eq!(active_idx, 1);
        } else {
            panic!("expected mode select");
        }
    }

    #[test]
    fn activate_matches_enter_semantics() {
        let picker = RewindPhase::Picker {
            points: vec![point(10), point(20)],
            selected: 1,
        };
        assert!(matches!(
            rewind_activate(&picker),
            RewindInput::PickerSelect(20)
        ));

        let error = RewindPhase::Error {
            message: "x".into(),
        };
        assert!(matches!(rewind_activate(&error), RewindInput::DismissError));

        let mode = RewindPhase::ModeSelect {
            target_prompt_index: 3,
            has_file_changes: true,
            offer_files_only: true,
            active_idx: 1,
        };
        assert!(matches!(
            rewind_activate(&mode),
            RewindInput::SelectMode(RewindMode::ConversationOnly, 3)
        ));

        let confirm = RewindPhase::Confirm {
            target_prompt_index: 4,
            mode: RewindMode::All,
            clean_files: vec![],
            conflicts: vec![],
            active_idx: 1,
            prompt_preview: None,
        };
        assert!(matches!(
            rewind_activate(&confirm),
            RewindInput::BackToModeSelect
        ));
    }

    #[test]
    fn esc_dismisses_from_confirm_phase() {
        let state = confirm_state();
        assert!(
            matches!(
                handle_rewind_key(&state, &key(KeyCode::Esc)),
                RewindInput::Dismissed
            ),
            "Esc in Confirm should fully dismiss — this is an \
             intentional UX change from the previous behavior where \
             Esc went BackToModeSelect, which forced users to press \
             Esc multiple times to exit"
        );
    }

    #[test]
    fn backspace_goes_back_to_mode_select_from_confirm() {
        let state = confirm_state();
        assert!(matches!(
            handle_rewind_key(&state, &key(KeyCode::Backspace)),
            RewindInput::BackToModeSelect
        ));
    }

    #[test]
    fn esc_dismisses_from_conversation_only_confirm() {
        let state = conv_only_state();
        assert!(matches!(
            handle_rewind_key(&state, &key(KeyCode::Esc)),
            RewindInput::Dismissed
        ));
    }

    #[test]
    fn backspace_goes_back_from_conversation_only_confirm() {
        let state = conv_only_state();
        assert!(matches!(
            handle_rewind_key(&state, &key(KeyCode::Backspace)),
            RewindInput::BackToModeSelect
        ));
    }

    #[test]
    fn esc_dismisses_from_picker_and_other_phases() {
        // Picker
        let s = RewindState {
            phase: RewindPhase::Picker {
                points: vec![],
                selected: 0,
            },
            anchor_entry_idx: 0,
            stashed_draft: None,
            selected_prompt_index: None,
        };
        assert!(matches!(
            handle_rewind_key(&s, &key(KeyCode::Esc)),
            RewindInput::Dismissed
        ));

        // ModeSelect
        let s = RewindState::new_mode_select(0, 1, true, true, None);
        assert!(matches!(
            handle_rewind_key(&s, &key(KeyCode::Esc)),
            RewindInput::Dismissed
        ));

        // CancelOffer
        let s = RewindState::new_cancel_offer(0, None, None);
        assert!(matches!(
            handle_rewind_key(&s, &key(KeyCode::Esc)),
            RewindInput::Dismissed
        ));
    }

    #[test]
    fn key_label_renders_special_sentinels() {
        assert_eq!(key_label('\x1b'), "Esc");
        assert_eq!(key_label('\x08'), "Bksp");
        assert_eq!(key_label('y'), "y");
        assert_eq!(key_label('a'), "a");
    }
}
