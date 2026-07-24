//! `/changes` — review pending agent edits and accept/reject them (A2).
//!
//! The shell's hunk-tracker already records every pending change (baseline
//! diff) with accept/reject machinery behind ACP extension methods. This modal
//! is the first pager consumer: it lists pending hunks grouped by file with
//! their patches, and dispatches accept/reject per hunk, per file, or for all.
//!
//! Data flows through `Effect::FetchChanges` / `Effect::ChangesAction` (see
//! app/dispatch); this file owns the wire DTOs (the shell's ACP DTOs are
//! serialize-only), the modal state, rendering, and key handling.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use xai_grok_pager_render::render::SafeBuf;

use crate::theme::Theme;
use crate::views::modal_window::{
    ModalSizing, ModalWindowConfig, ModalWindowState, Shortcut, render_modal_window,
};

// ---------------------------------------------------------------------------
// Wire DTOs (mirror the shell's ACP payloads; camelCase on the wire)
// ---------------------------------------------------------------------------

/// One pending hunk from `x.ai/hunk-tracker/get-hunks`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HunkDto {
    pub id: String,
    pub path: String,
    pub line_info: HunkLineInfoDto,
    pub source: HunkSourceDto,
    #[serde(default)]
    pub patch: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HunkLineInfoDto {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
}

/// Tagged `{"type": "agentEdit", "promptIndex": N}` | `externalEditOnAgentFile`
/// | `external`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum HunkSourceDto {
    AgentEdit {
        #[serde(default)]
        prompt_index: usize,
    },
    ExternalEditOnAgentFile,
    External,
    /// Forward-compat sink: a source variant this pager doesn't know yet
    /// (oracle: an unknown variant must never silently drop the hunk list).
    #[serde(other)]
    Unknown,
}

impl HunkSourceDto {
    /// Short source label for the hunk row (localized by the caller's key).
    pub fn key(&self) -> (&'static str, Option<usize>) {
        match self {
            Self::AgentEdit { prompt_index } => ("changes.source.agent_turn", Some(*prompt_index)),
            Self::ExternalEditOnAgentFile => ("changes.source.external_on_agent", None),
            Self::External => ("changes.source.external", None),
            Self::Unknown => ("changes.source.unknown", None),
        }
    }

    /// Whether this hunk is the USER's external edit (not the agent's) —
    /// rejecting it reverts the user's own work on disk.
    pub fn is_external(&self) -> bool {
        matches!(self, Self::ExternalEditOnAgentFile | Self::External)
    }
}

/// One file row from `x.ai/hunk-tracker/get-files`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSummaryDto {
    pub path: String,
    #[serde(default)]
    pub is_agent_file: bool,
    #[serde(default)]
    pub staged: bool,
    pub hunk_count: usize,
    pub additions: usize,
    pub deletions: usize,
}

/// `x.ai/hunk-tracker/*-action` response.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResponseDto {
    pub success: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub affected_count: Option<usize>,
}

// ---------------------------------------------------------------------------
// Actions the modal can dispatch
// ---------------------------------------------------------------------------

/// A review action the user picked in the modal. Dispatched via
/// `Action::ChangesAction` → `Effect::ChangesAction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangesActionKind {
    AcceptHunk(String),
    RejectHunk(String),
    AcceptFile(String),
    RejectFile(String),
    AcceptAll,
    RejectAll,
}

impl ChangesActionKind {
    /// Wire verb for the ACP method.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::AcceptHunk(_) | Self::AcceptFile(_) | Self::AcceptAll => "accept",
            Self::RejectHunk(_) | Self::RejectFile(_) | Self::RejectAll => "reject",
        }
    }

    /// ACP extension method name for this action.
    pub fn method(&self) -> &'static str {
        match self {
            Self::AcceptHunk(_) | Self::RejectHunk(_) => "x.ai/hunk-tracker/hunk-action",
            Self::AcceptFile(_) | Self::RejectFile(_) => "x.ai/hunk-tracker/file-action",
            Self::AcceptAll | Self::RejectAll => "x.ai/hunk-tracker/all-action",
        }
    }

    /// Params JSON for the ACP request (session id injected by the caller).
    pub fn params(&self, session_id: &str) -> serde_json::Value {
        match self {
            Self::AcceptHunk(id) | Self::RejectHunk(id) => serde_json::json!({
                "sessionId": session_id,
                "hunkId": id,
                "action": self.verb(),
            }),
            Self::AcceptFile(path) | Self::RejectFile(path) => serde_json::json!({
                "sessionId": session_id,
                "path": path,
                "action": self.verb(),
            }),
            Self::AcceptAll | Self::RejectAll => serde_json::json!({
                "sessionId": session_id,
                "action": self.verb(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Modal state
// ---------------------------------------------------------------------------

/// Modal outcome for the app input router.
pub enum ChangesModalOutcome {
    Changed,
    Unchanged,
    Closed,
    /// Dispatch a review action (accept/reject).
    Act(ChangesActionKind),
    /// Re-fetch pending changes (manual refresh).
    Refresh,
}

/// A pending reject confirmation: which reject is armed and how many of the
/// affected hunks are the USER's external edits (rejecting those reverts the
/// user's own work — the riskiest case).
pub struct RejectConfirm {
    pub kind: ChangesActionKind,
    pub external_count: usize,
}

pub struct ChangesModalState {
    pub window: ModalWindowState,
    /// Flattened hunks in display order (grouped by file).
    pub hunks: Vec<HunkDto>,
    /// Per-file summaries (for the file header rows).
    pub files: Vec<FileSummaryDto>,
    /// Selected hunk index in `hunks` (display order).
    pub selected: usize,
    pub scroll: usize,
    /// Waiting on an action's ACP response (blocks further actions).
    pub action_in_flight: bool,
    /// Reject confirmation pending (`y` confirms, `n`/Esc cancels).
    pub confirm: Option<RejectConfirm>,
    /// Transient inline message (action errors, info).
    pub message: Option<String>,
}

impl ChangesModalState {
    pub fn new(files: Vec<FileSummaryDto>, hunks: Vec<HunkDto>) -> Self {
        Self {
            window: ModalWindowState::default(),
            hunks,
            files,
            selected: 0,
            scroll: 0,
            action_in_flight: false,
            confirm: None,
            message: None,
        }
    }

    /// Empty state (no pending changes).
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    pub fn selected_hunk(&self) -> Option<&HunkDto> {
        self.hunks.get(self.selected)
    }

    fn select_next(&mut self) {
        if !self.hunks.is_empty() {
            self.selected = (self.selected + 1).min(self.hunks.len() - 1);
        }
    }

    fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// After an action lands, the list is refetched; clamp the selection.
    pub fn clamp_selection(&mut self) {
        if self.hunks.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.hunks.len() - 1);
        }
        self.scroll = 0;
    }

    /// Arm the reject-confirmation flow for a reject action. Agent-sourced
    /// single-hunk rejects fire immediately (no confirm — the plan is the
    /// agent's own work); everything else (external hunk, file, all) asks
    /// first, because reject REWRITES THE DISK with no undo.
    fn arm_reject(&mut self, kind: ChangesActionKind) -> ChangesModalOutcome {
        let external_count = match &kind {
            ChangesActionKind::RejectHunk(id) => self
                .hunks
                .iter()
                .filter(|h| &h.id == id && h.source.is_external())
                .count(),
            ChangesActionKind::RejectFile(path) => self
                .hunks
                .iter()
                .filter(|h| &h.path == path && h.source.is_external())
                .count(),
            ChangesActionKind::RejectAll => {
                self.hunks.iter().filter(|h| h.source.is_external()).count()
            }
            _ => 0,
        };
        if let ChangesActionKind::RejectHunk(_) = kind
            && external_count == 0
        {
            self.action_in_flight = true;
            return ChangesModalOutcome::Act(kind);
        }
        self.confirm = Some(RejectConfirm {
            kind,
            external_count,
        });
        ChangesModalOutcome::Changed
    }

    /// Map a footer shortcut id (mouse click) to the same outcome the
    /// matching key would produce. `None` for non-action ids (nav hints).
    pub fn handle_shortcut_id(&mut self, id: usize) -> Option<ChangesModalOutcome> {
        if self.confirm.is_some() {
            return match id {
                0 => {
                    let confirm = self.confirm.take().expect("checked is_some");
                    self.action_in_flight = true;
                    Some(ChangesModalOutcome::Act(confirm.kind))
                }
                1 => {
                    self.confirm = None;
                    Some(ChangesModalOutcome::Changed)
                }
                _ => None,
            };
        }
        if self.action_in_flight {
            return None;
        }
        match id {
            1 if !self.is_empty() => {
                let hunk = self.hunks[self.selected].id.clone();
                self.action_in_flight = true;
                Some(ChangesModalOutcome::Act(ChangesActionKind::AcceptHunk(
                    hunk,
                )))
            }
            2 if !self.is_empty() => {
                let hunk = self.hunks[self.selected].id.clone();
                Some(self.arm_reject(ChangesActionKind::RejectHunk(hunk)))
            }
            3 if !self.is_empty() => {
                let path = self.hunks[self.selected].path.clone();
                self.action_in_flight = true;
                Some(ChangesModalOutcome::Act(ChangesActionKind::AcceptFile(
                    path,
                )))
            }
            4 if !self.is_empty() => {
                let path = self.hunks[self.selected].path.clone();
                Some(self.arm_reject(ChangesActionKind::RejectFile(path)))
            }
            5 if !self.is_empty() => {
                self.action_in_flight = true;
                Some(ChangesModalOutcome::Act(ChangesActionKind::AcceptAll))
            }
            6 if !self.is_empty() => Some(self.arm_reject(ChangesActionKind::RejectAll)),
            7 => Some(ChangesModalOutcome::Refresh),
            8 => Some(ChangesModalOutcome::Closed),
            _ => None,
        }
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> ChangesModalOutcome {
        // Reject confirmation mode: only y/n/Esc mean anything.
        if self.confirm.is_some() {
            return match key.code {
                KeyCode::Char('y') => {
                    let confirm = self.confirm.take().expect("checked is_some");
                    self.action_in_flight = true;
                    ChangesModalOutcome::Act(confirm.kind)
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.confirm = None;
                    ChangesModalOutcome::Changed
                }
                _ => ChangesModalOutcome::Unchanged,
            };
        }
        if self.action_in_flight {
            return ChangesModalOutcome::Unchanged;
        }
        match key.code {
            KeyCode::Esc => ChangesModalOutcome::Closed,
            KeyCode::Char('j') | KeyCode::Down => {
                self.select_next();
                ChangesModalOutcome::Changed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.select_prev();
                ChangesModalOutcome::Changed
            }
            // Crossterm reports Ctrl+letter as LOWERCASE Char + CONTROL
            // (oracle High: uppercase chords never fire).
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !self.is_empty() {
                    self.action_in_flight = true;
                    ChangesModalOutcome::Act(ChangesActionKind::AcceptAll)
                } else {
                    ChangesModalOutcome::Unchanged
                }
            }
            KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !self.is_empty() {
                    self.arm_reject(ChangesActionKind::RejectAll)
                } else {
                    ChangesModalOutcome::Unchanged
                }
            }
            KeyCode::Char('a') if !self.is_empty() => {
                let id = self.hunks[self.selected].id.clone();
                self.action_in_flight = true;
                ChangesModalOutcome::Act(ChangesActionKind::AcceptHunk(id))
            }
            KeyCode::Char('x') if !self.is_empty() => {
                let id = self.hunks[self.selected].id.clone();
                self.arm_reject(ChangesActionKind::RejectHunk(id))
            }
            KeyCode::Char('A') if !self.is_empty() => {
                let path = self.hunks[self.selected].path.clone();
                self.action_in_flight = true;
                ChangesModalOutcome::Act(ChangesActionKind::AcceptFile(path))
            }
            KeyCode::Char('X') if !self.is_empty() => {
                let path = self.hunks[self.selected].path.clone();
                self.arm_reject(ChangesActionKind::RejectFile(path))
            }
            KeyCode::Char('r') => ChangesModalOutcome::Refresh,
            _ => ChangesModalOutcome::Unchanged,
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Display rows: a file header followed by its hunks (display-only grouping —
/// selection moves over hunk rows only).
enum Row<'a> {
    File(&'a FileSummaryDto),
    Hunk(usize, &'a HunkDto),
}

fn build_rows<'a>(files: &'a [FileSummaryDto], hunks: &'a [HunkDto]) -> Vec<Row<'a>> {
    let mut rows = Vec::new();
    for file in files {
        rows.push(Row::File(file));
        for (idx, hunk) in hunks
            .iter()
            .enumerate()
            .filter(|(_, h)| h.path == file.path)
        {
            rows.push(Row::Hunk(idx, hunk));
        }
    }
    // Hunks whose file is missing from the summary (defensive) go last.
    for (idx, hunk) in hunks
        .iter()
        .enumerate()
        .filter(|(_, h)| !files.iter().any(|f| f.path == h.path))
    {
        rows.push(Row::Hunk(idx, hunk));
    }
    rows
}

/// Render the changes review modal.
pub fn render_changes_modal(
    buf: &mut Buffer,
    area: Rect,
    state: &mut ChangesModalState,
    theme: &Theme,
) {
    let shortcuts = build_shortcuts(state);
    let config = ModalWindowConfig {
        title: &rust_i18n::t!("changes.title"),
        tabs: None,
        shortcuts: &shortcuts,
        sizing: ModalSizing::large(),
        fold_info: None,
    };
    let Some(mca) = render_modal_window(buf, area, &mut state.window, &config, theme) else {
        return;
    };
    let inner = mca.content;

    if state.is_empty() {
        let text = Line::from(Span::styled(
            rust_i18n::t!("changes.empty"),
            Style::default().fg(theme.gray),
        ));
        buf.set_line_safe(inner.x, inner.y + inner.height / 2, &text, inner.width);
        return;
    }

    let rows = build_rows(&state.files, &state.hunks);
    // Patch preview gets the bottom ~40% (bounded), list the rest.
    let preview_h = (inner.height * 2 / 5).min(12).max(3);
    let list_h = inner.height.saturating_sub(preview_h + 1);
    let mut y = inner.y;
    let list_end = inner.y + list_h;
    // The separator line between list and patch preview (also the reserved
    // banner slot for confirm/messages).
    let sep_line_y = list_end;

    // Scroll-follow-selection (oracle: state.scroll never advanced, so a
    // selection past the first page rendered nothing). Compute the selected
    // hunk's grouped-row index and clamp the window around it.
    let selected_row = rows
        .iter()
        .position(|r| matches!(r, Row::Hunk(idx, _) if *idx == state.selected))
        .unwrap_or(0);
    let mut scroll = state.scroll;
    if selected_row < scroll {
        scroll = selected_row;
    } else if list_h > 0 && selected_row >= scroll + list_h as usize {
        scroll = selected_row + 1 - list_h as usize;
    }
    let visible = scroll..(scroll + list_h as usize);
    for (row_idx, row) in rows.iter().enumerate() {
        if y >= list_end {
            break;
        }
        if !visible.contains(&row_idx) {
            continue;
        }
        match row {
            Row::File(file) => {
                let line = Line::from(vec![
                    Span::styled(
                        format!("{}", shorten_path(&file.path, inner.width as usize / 2)),
                        Style::default()
                            .fg(theme.text_primary)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            "  +{} −{} · {}",
                            file.additions, file.deletions, file.hunk_count
                        ),
                        Style::default().fg(theme.gray_dim),
                    ),
                ]);
                buf.set_line_safe(inner.x, y, &line, inner.width);
                y += 1;
            }
            Row::Hunk(idx, hunk) => {
                let selected = *idx == state.selected;
                let (key, turn) = hunk.source.key();
                let source = match turn {
                    Some(t) => rust_i18n::t!(key, turn = t),
                    None => rust_i18n::t!(key),
                };
                let marker = if selected { "▸" } else { " " };
                let style = if selected {
                    Style::default().fg(theme.accent_user)
                } else {
                    Style::default().fg(theme.text_secondary)
                };
                let line = Line::from(vec![
                    Span::styled(format!("{marker} "), style),
                    Span::styled(
                        format!(
                            "@@ -{},{} +{},{} @@",
                            hunk.line_info.old_start,
                            hunk.line_info.old_count,
                            hunk.line_info.new_start,
                            hunk.line_info.new_count
                        ),
                        style,
                    ),
                    Span::styled(format!("  {source}"), Style::default().fg(theme.gray_dim)),
                ]);
                if selected {
                    let mut bg = Style::default().bg(theme.bg_highlight);
                    if let Some(fg) = style.fg {
                        bg = bg.fg(fg);
                    }
                    for x in inner.x..inner.x + inner.width {
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.set_style(bg);
                        }
                    }
                }
                buf.set_line_safe(inner.x, y, &line, inner.width);
                y += 1;
            }
        }
    }

    // Patch preview of the selected hunk.
    if let Some(hunk) = state.selected_hunk() {
        let sep_y = list_end;
        buf.set_line_safe(
            mca.inner_x,
            sep_y,
            &Line::from(Span::styled(
                "─".repeat(mca.inner_width as usize),
                Style::default().fg(theme.gray_dim),
            )),
            mca.inner_width,
        );
        let mut py = sep_y + 1;
        let max_py = inner.y + inner.height;
        let patch_text = hunk
            .patch
            .clone()
            .unwrap_or_else(|| rust_i18n::t!("changes.no_patch").into_owned());
        for line in patch_text.lines() {
            if py >= max_py {
                break;
            }
            let (content, color) = if line.starts_with('+') && !line.starts_with("+++") {
                (line, theme.accent_success)
            } else if line.starts_with('-') && !line.starts_with("---") {
                (line, theme.accent_error)
            } else if line.starts_with("@@") {
                (line, theme.accent_user)
            } else {
                (line, theme.gray)
            };
            buf.set_line_safe(
                inner.x,
                py,
                &Line::from(Span::styled(
                    content.to_string(),
                    Style::default().fg(color),
                )),
                inner.width,
            );
            py += 1;
        }
    }

    // Confirm banner (reject needs an explicit yes — reject REWRITES THE
    // DISK with no undo; external hunks are the USER's own edits) rendered on
    // the reserved separator line; otherwise a transient message there.
    if let Some(confirm) = &state.confirm {
        let msg = if confirm.external_count > 0 {
            rust_i18n::t!(
                "changes.confirm_reject_external",
                count = confirm.external_count
            )
        } else {
            rust_i18n::t!("changes.confirm_reject")
        };
        buf.set_line_safe(
            inner.x,
            sep_line_y,
            &Line::from(Span::styled(
                msg.into_owned(),
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            )),
            inner.width,
        );
    } else if let Some(msg) = &state.message {
        buf.set_line_safe(
            inner.x,
            sep_line_y,
            &Line::from(Span::styled(
                msg.clone(),
                Style::default().fg(theme.warning),
            )),
            inner.width,
        );
    }
}

pub(crate) fn build_shortcuts(state: &ChangesModalState) -> Vec<Shortcut<'static>> {
    if state.confirm.is_some() {
        return vec![
            Shortcut {
                label: rust_i18n::t!("footer.y_confirm"),
                clickable: true,
                id: 0,
            },
            Shortcut {
                label: rust_i18n::t!("footer.n_esc_cancel"),
                clickable: true,
                id: 1,
            },
        ];
    }
    let mut sc = vec![
        Shortcut {
            label: rust_i18n::t!("footer.nav_jk"),
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: rust_i18n::t!("changes.footer.accept_hunk"),
            clickable: !state.is_empty(),
            id: 1,
        },
        Shortcut {
            label: rust_i18n::t!("changes.footer.reject_hunk"),
            clickable: !state.is_empty(),
            id: 2,
        },
        Shortcut {
            label: rust_i18n::t!("changes.footer.accept_file"),
            clickable: !state.is_empty(),
            id: 3,
        },
        Shortcut {
            label: rust_i18n::t!("changes.footer.reject_file"),
            clickable: !state.is_empty(),
            id: 4,
        },
        Shortcut {
            label: rust_i18n::t!("changes.footer.accept_all"),
            clickable: !state.is_empty(),
            id: 5,
        },
        Shortcut {
            label: rust_i18n::t!("changes.footer.reject_all"),
            clickable: !state.is_empty(),
            id: 6,
        },
        Shortcut {
            label: rust_i18n::t!("changes.footer.refresh"),
            clickable: true,
            id: 7,
        },
        Shortcut {
            label: rust_i18n::t!("footer.esc_close"),
            clickable: true,
            id: 8,
        },
    ];
    crate::views::modal_window::push_vim_nav_search_hint(&mut sc, false);
    sc
}

/// Shorten an absolute path for display: keep the tail within `max` chars.
fn shorten_path(path: &str, max: usize) -> String {
    if max == 0 || path.chars().count() <= max {
        return path.to_string();
    }
    let tail: String = path.chars().rev().take(max.saturating_sub(1)).collect();
    format!("…{}", tail.chars().rev().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(id: &str, path: &str) -> HunkDto {
        HunkDto {
            id: id.to_string(),
            path: path.to_string(),
            line_info: HunkLineInfoDto {
                old_start: 10,
                old_count: 3,
                new_start: 10,
                new_count: 5,
            },
            source: HunkSourceDto::AgentEdit { prompt_index: 2 },
            patch: Some("@@ -10,3 +10,5 @@\n-old\n+new\n+more".to_string()),
        }
    }

    fn file(path: &str, count: usize) -> FileSummaryDto {
        FileSummaryDto {
            path: path.to_string(),
            is_agent_file: true,
            staged: false,
            hunk_count: count,
            additions: 5,
            deletions: 2,
        }
    }

    #[test]
    fn action_kind_wire_shapes() {
        let k = ChangesActionKind::AcceptHunk("h-1".into());
        assert_eq!(k.method(), "x.ai/hunk-tracker/hunk-action");
        assert_eq!(k.verb(), "accept");
        let p = k.params("sess-1");
        assert_eq!(p["sessionId"], "sess-1");
        assert_eq!(p["hunkId"], "h-1");

        let k = ChangesActionKind::RejectFile("/a/b.rs".into());
        assert_eq!(k.method(), "x.ai/hunk-tracker/file-action");
        assert_eq!(k.params("s")["action"], "reject");

        let k = ChangesActionKind::AcceptAll;
        assert_eq!(k.method(), "x.ai/hunk-tracker/all-action");
        assert!(k.params("s")["hunkId"].is_null());
    }

    #[test]
    fn navigation_clamps_and_actions() {
        let mut s = ChangesModalState::new(
            vec![file("/a.rs", 2)],
            vec![hunk("h1", "/a.rs"), hunk("h2", "/a.rs")],
        );
        s.select_next();
        s.select_next();
        assert_eq!(s.selected, 1);
        s.select_prev();
        assert_eq!(s.selected, 0);
        match s.handle_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)) {
            ChangesModalOutcome::Act(ChangesActionKind::AcceptHunk(id)) => assert_eq!(id, "h1"),
            _ => panic!("expected accept hunk"),
        }
        assert!(s.action_in_flight);
        // In flight: keys are blocked.
        assert!(matches!(
            s.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            ChangesModalOutcome::Unchanged
        ));
    }

    #[test]
    fn reject_all_requires_confirmation() {
        let mut s = ChangesModalState::new(vec![file("/a.rs", 1)], vec![hunk("h1", "/a.rs")]);
        // Crossterm reports Ctrl+letter as LOWERCASE Char + CONTROL.
        match s.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)) {
            ChangesModalOutcome::Changed => {}
            _ => panic!("expected confirm mode"),
        }
        assert!(s.confirm.is_some());
        // n cancels.
        s.handle_key(&KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(s.confirm.is_none());
        // Re-arm and confirm with y.
        s.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        match s.handle_key(&KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)) {
            ChangesModalOutcome::Act(ChangesActionKind::RejectAll) => {}
            _ => panic!("expected reject all"),
        }
    }

    #[test]
    fn external_reject_arms_confirmation_with_count() {
        let mut s = ChangesModalState::new(
            vec![file("/a.rs", 1)],
            vec![{
                let mut h = hunk("h1", "/a.rs");
                h.source = HunkSourceDto::External;
                h
            }],
        );
        // Agent-sourced single-hunk rejects fire immediately (no confirm).
        let mut agent_only =
            ChangesModalState::new(vec![file("/a.rs", 1)], vec![hunk("h1", "/a.rs")]);
        match agent_only.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)) {
            ChangesModalOutcome::Act(ChangesActionKind::RejectHunk(_)) => {}
            _ => panic!("agent hunk reject must not require confirmation"),
        }
        // External hunk: confirm with external_count = 1.
        match s.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)) {
            ChangesModalOutcome::Changed => {}
            _ => panic!("external reject must arm confirmation"),
        }
        let confirm = s.confirm.as_ref().expect("confirm armed");
        assert_eq!(confirm.external_count, 1);
    }

    #[test]
    fn rows_group_hunks_under_their_file() {
        let files = vec![file("/a.rs", 1), file("/b.rs", 1)];
        let hunks = vec![
            hunk("h1", "/a.rs"),
            hunk("h2", "/b.rs"),
            hunk("h3", "/orphan.rs"),
        ];
        let rows = build_rows(&files, &hunks);
        assert_eq!(rows.len(), 5);
        assert!(matches!(rows[0], Row::File(_)));
        assert!(matches!(rows[1], Row::Hunk(0, _)));
        assert!(matches!(rows[2], Row::File(_)));
        assert!(matches!(rows[3], Row::Hunk(1, _)));
        assert!(matches!(rows[4], Row::Hunk(2, _)));
    }
}
