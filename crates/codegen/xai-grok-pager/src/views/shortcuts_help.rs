//! All-shortcuts cheatsheet modal (Ctrl+. / Ctrl+X).
//!
//! Registry-driven: `build_entries(registry)` pulls every `ActionDef` from
//! `ActionRegistry`, groups them by `Category` in onboarding-friendly order
//! (Essentials → Panes → Scrollback Navigation → View → Prompt → Agent),
//! and includes alt-key bindings inline. Search filters against key display,
//! description, and label.
//!
//! Two ways to read a binding's help: pattern A expands an inline help line under
//! the selected hint (e/Space/l/h/arrows); pattern B opens an in-modal man-style
//! detail page on Enter, where Esc (or h/Left/Backspace) returns to the browse list.
//! Section headers collapse/expand; close via Esc in browse or Ctrl+./Ctrl+X.
//! Rendered via `ModalWindow` chrome (same appearance as the command palette).
//!
//! Entry points from `AgentView`:
//! - `build_entries(registry)` + `build_initial_picker_state` →
//!   `ActiveModal::ShortcutsHelp`
//! - `handle_input` / `handle_mouse` for key/mouse dispatch
//! - Rendering is done inline in `AgentView` via `render_modal_window` +
//!   `render_picker_in_modal`.

use std::borrow::Cow;

use crate::actions::{ActionDef, ActionId, ActionRegistry, Category, When};
use crate::input::key::KeyShortcut;
use crate::views::picker::{PickerConfig, PickerOutcome, PickerState, handle_picker_input};
use crate::views::shortcuts_bar::HintItem;

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/// Key for pattern-A inline expand state (`expanded_ids`).
///
/// Registry rows use [`ExpandKey::Action`]; display-only rows that ship
/// `long_help` (e.g. paste) use [`ExpandKey::Pseudo`] with a stable label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExpandKey {
    Action(ActionId),
    Pseudo(&'static str),
}

/// One row in the all-shortcuts cheatsheet.
///
/// Headers are non-selectable section dividers; Hints are the actual key
/// bindings and are selectable / dispatchable on Enter.
pub enum ShortcutsHelpEntry {
    SectionHeader {
        label: &'static str,
        category_idx: usize,
        entry_count: usize,
    },
    Hint {
        item: HintItem,
        dimmed: bool,
        /// Registry action for expand/detail; `None` for display-only pseudo-rows.
        action_id: Option<ActionId>,
        /// Man-style help shown under the expanded row; falls back to the description.
        long_help: Option<&'static str>,
    },
}

impl ShortcutsHelpEntry {
    pub fn is_hint(&self) -> bool {
        matches!(self, Self::Hint { .. })
    }

    pub fn is_section_header(&self) -> bool {
        matches!(self, Self::SectionHeader { .. })
    }
}

// ---------------------------------------------------------------------------
// Modal state construction
// ---------------------------------------------------------------------------

/// Category display order and labels for the cheatsheet.
const CATEGORY_ORDER: &[(Category, &str)] = &[
    (Category::GettingStarted, "Essentials"),
    (Category::Input, "Input"),
    (Category::ConversationNav, "Conversation Navigation"),
    (Category::ConversationAction, "Conversation Actions"),
    (Category::Panels, "Panels"),
    (Category::Session, "Session"),
    (Category::Dashboard, "Dashboard"),
];

pub fn default_collapsed() -> std::collections::HashSet<usize> {
    (1..CATEGORY_ORDER.len()).collect()
}

// Man-page body for the paste pseudo-row (Enter detail). Keep claims that
// hold on every host (agent + dashboard); non-image file paths are agent-only.
#[cfg(target_os = "windows")]
const PASTE_LONG_HELP: &str = "\
Pastes clipboard images into the prompt as chips, and plain text as typed.\n\
Prefer Ctrl+V. Use Alt+V as a fallback when Ctrl+V fails (some terminals or \
configs drop image clipboards; older Windows Terminal versions only pasted \
text).\n\
You can also drag an image file from Explorer into the prompt.";
#[cfg(target_os = "macos")]
const PASTE_LONG_HELP: &str = "\
Pastes clipboard images into the prompt as chips, and plain text as typed.\n\
Use Ctrl+V for screenshots, browser \"Copy Image\", and file-manager image \
copies (many terminals swallow Cmd+V and never deliver it to the TUI).\n\
You can also drag an image file into the prompt.";
#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
const PASTE_LONG_HELP: &str = "\
Pastes clipboard images into the prompt as chips, and plain text as typed.\n\
Use Ctrl+V for screenshots, browser \"Copy Image\", and file-manager image \
copies.\n\
You can also drag an image file into the prompt.";

/// Build the entries vector for the modal, grouped by category.
///
/// All registered actions are included, grouped by category. Actions
/// whose `When` context is not in `active_contexts` are dimmed.
pub fn build_entries(
    active_contexts: &[When],
    registry: &ActionRegistry,
    vim_mode: bool,
) -> Vec<ShortcutsHelpEntry> {
    let mut entries: Vec<ShortcutsHelpEntry> = Vec::new();

    // Keys the dashboard session-overlay claims while it is up. The
    // overlay intercept consults `When::DashboardOverlay` before
    // forwarding a key to the agent, so a lit row from another context
    // advertising one of these keys would be lying (e.g. the
    // cheatsheet's Ctrl+X alt is shadowed by the overlay stop).
    let overlay_claimed: std::collections::HashSet<KeyShortcut> =
        if active_contexts.contains(&When::DashboardOverlay) {
            registry
                .all()
                .iter()
                .filter(|d| d.context == When::DashboardOverlay)
                .map(|d| d.default_key)
                .collect()
        } else {
            std::collections::HashSet::new()
        };

    for (cat_idx, &(cat, label)) in CATEGORY_ORDER.iter().enumerate() {
        // Dedup per category on the default key, preferring the def
        // whose `When` context is active: `DashboardStop` (list) and
        // `DashboardOverlayStop` (overlay) share Ctrl+X and category,
        // and whichever matches the current surface must win
        // regardless of registration order.
        let mut seen_in_cat: std::collections::HashMap<KeyShortcut, usize> =
            std::collections::HashMap::new();
        let defs: Vec<&ActionDef> = registry
            .all()
            .iter()
            .filter(|d| d.category == cat)
            .collect();
        if defs.is_empty() {
            continue;
        }
        let header_idx = entries.len();
        entries.push(ShortcutsHelpEntry::SectionHeader {
            label,
            category_idx: cat_idx,
            entry_count: 0,
        });
        for def in defs {
            // Slash-only actions with no real keybinding (e.g. `/voice`'s
            // EnableVoiceMode) don't belong in a keyboard cheatsheet.
            if def.default_key == crate::key!(Null) && def.alt_keys.is_empty() {
                continue;
            }
            // The voice chord (`Ctrl+Space`) is hidden when the voice gate is
            // off (remote kill switch / `GROK_VOICE_MODE=0`). Unlike the old
            // `Ctrl+Shift+M`, `Ctrl+Space` decodes the same with or without the
            // Kitty keyboard protocol (it just toggles instead of hold-to-talk
            // without it), so it's shown on every terminal once the gate is on.
            // EnableVoiceMode is slash-only and already dropped above.
            if def.id == crate::actions::ActionId::VoiceToggle && !crate::app::voice_mode_enabled()
            {
                continue;
            }
            let mut item = def.hint();
            if !def.alt_keys.is_empty() {
                item.keys.extend_from_slice(&def.alt_keys);
                // Alt keys can be terminal-encoding variants of the SAME
                // physical chord (Shift+Tab arrives as `BackTab`,
                // `BackTab`+SHIFT, or `Tab`+SHIFT depending on the
                // terminal). Collapse keys that render identically so the
                // row doesn't read "Shift+Tab / Shift+Tab / Shift+Tab".
                let mut seen_displays = std::collections::HashSet::new();
                item.keys
                    .retain(|k| seen_displays.insert(k.display_pretty()));
                item.custom_display = None;
            }
            // In non-vim mode, suppress bare-letter / Shift+letter keys
            // from any scrollback-context binding. If the row has at least
            // one non-vim key left (e.g. an arrow alt), show only those —
            // they still work, so don't dim. If every key was a vim key,
            // hide the row entirely (the binding is genuinely inert when
            // vim mode is off).
            if !vim_mode && def.context == When::ScrollbackFocused {
                let has_non_vim = item.keys.iter().any(|k| !k.is_letter_or_shift_letter());
                if has_non_vim {
                    item.keys.retain(|k| !k.is_letter_or_shift_letter());
                    // When we strip the default_key but keep an alt, the
                    // custom_display string (e.g. "Shift+l/h") no longer
                    // matches what's shown; drop it so the keys render
                    // verbatim.
                    item.custom_display = None;
                } else {
                    continue;
                }
            }
            let dimmed = !active_contexts.contains(&def.context);
            // Strip overlay-claimed keys from lit rows of other
            // contexts (the overlay intercept shadows them). Dimmed
            // rows already say "not applicable here", so they keep
            // their keys for discoverability.
            if !dimmed
                && def.context != When::DashboardOverlay
                && item.keys.iter().any(|k| overlay_claimed.contains(k))
            {
                item.keys.retain(|k| !overlay_claimed.contains(k));
                if item.keys.is_empty() {
                    // Every key is shadowed — the binding is
                    // genuinely unreachable inside the overlay.
                    continue;
                }
                // The custom display no longer matches the surviving
                // keys; render them verbatim.
                item.custom_display = None;
            }
            // Identical row for both arms; `item` moves in, `dimmed`/`def.id` are Copy.
            let hint = ShortcutsHelpEntry::Hint {
                item,
                dimmed,
                action_id: Some(def.id),
                long_help: def.long_help,
            };
            match seen_in_cat.entry(def.default_key) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(entries.len());
                    entries.push(hint);
                }
                std::collections::hash_map::Entry::Occupied(slot) => {
                    // Same key already rendered in this category —
                    // replace it only when the earlier row is dimmed
                    // and this one is lit (active context wins).
                    let prior = &mut entries[*slot.get()];
                    if !dimmed && matches!(prior, ShortcutsHelpEntry::Hint { dimmed: true, .. }) {
                        *prior = hint;
                    }
                }
            }
        }
        // Scrollback search (`/`) has no registered ActionDef yet — vim-only,
        // handled inline; surface it here for discoverability.
        if vim_mode && cat == Category::ConversationNav {
            let mut item = HintItem::new(crate::key!('/'), "search");
            item.description = Some("Search scrollback".into());
            let dimmed = !active_contexts.contains(&When::ScrollbackFocused);
            entries.push(ShortcutsHelpEntry::Hint {
                item,
                dimmed,
                action_id: None,
                long_help: None,
            });
        }
        // Paste is handled by `is_paste_key`, not the registry. Ctrl+V always;
        // Windows also Alt+V as a fallback. Super/Cmd omitted — many terminals
        // swallow it. Lit on the agent prompt and the dashboard (both paste).
        if cat == Category::Input {
            let mut item = HintItem::new(crate::key!('v', CONTROL), "paste");
            item.description = Some("Paste images (and text) from the clipboard".into());
            #[cfg(target_os = "windows")]
            item.keys.push(crate::key!('v', ALT));
            let dimmed = !active_contexts.contains(&When::PromptFocused)
                && !active_contexts.contains(&When::DashboardFocused);
            entries.push(ShortcutsHelpEntry::Hint {
                item,
                dimmed,
                action_id: None,
                long_help: Some(PASTE_LONG_HELP),
            });
        }
        let count = entries.len() - header_idx - 1;
        if count == 0 {
            // Every action in this category got filtered out (e.g. all
            // scrollback vim-only bindings in non-vim mode); drop the
            // empty header rather than render a dead section.
            entries.pop();
        } else if let Some(ShortcutsHelpEntry::SectionHeader { entry_count, .. }) =
            entries.get_mut(header_idx)
        {
            *entry_count = count;
        }
    }
    entries
}

/// Build the initial `PickerState` for the modal. Width/height are wider
/// than the default Floating popup so the cheatsheet has room for the
/// key + label columns.
pub fn build_initial_picker_state(entries: &[ShortcutsHelpEntry]) -> PickerState {
    use crate::views::picker::{PickerMode, PopupConfig};
    let mut state = PickerState::with_mode(PickerMode::Popup(PopupConfig {
        width_pct: 0.6,
        height_pct: 0.7,
        min_width: 60,
        min_height: 16,
    }));
    state.selected = entries.iter().position(|e| e.is_hint()).unwrap_or(0);
    state
}

// ---------------------------------------------------------------------------
// Search filtering
// ---------------------------------------------------------------------------

/// Filter ShortcutsHelp entries by search query.
///
/// Returns the original-index list of entries that pass the filter.
/// Section headers are kept only when at least one hint in their section
/// matches; this mirrors the palette's `filter_palette_entries` behavior.
pub fn filter_entries(
    entries: &[ShortcutsHelpEntry],
    query: &str,
    hide_dimmed: bool,
    collapsed: &std::collections::HashSet<usize>,
) -> Vec<usize> {
    let searching = !query.is_empty();
    if !searching && !hide_dimmed && collapsed.is_empty() {
        return (0..entries.len()).collect();
    }
    let q = query.to_lowercase();
    let mut result: Vec<usize> = Vec::new();
    let mut pending_header: Option<usize> = None;
    let mut section_has_match = false;
    let mut current_section_collapsed = false;
    for (i, entry) in entries.iter().enumerate() {
        match entry {
            ShortcutsHelpEntry::SectionHeader { category_idx, .. } => {
                if let Some(h) = pending_header.take()
                    && (section_has_match || current_section_collapsed)
                {
                    result.push(h);
                }
                pending_header = Some(i);
                section_has_match = false;
                current_section_collapsed = !searching && collapsed.contains(category_idx);
            }
            ShortcutsHelpEntry::Hint {
                item: h, dimmed, ..
            } => {
                if current_section_collapsed {
                    continue;
                }
                if hide_dimmed && *dimmed {
                    continue;
                }
                let key_text = hint_key_display(h);
                let key_pretty = hint_key_pretty(h);
                let desc = hint_description(h);
                let q_matches = q.is_empty()
                    || h.label.to_lowercase().contains(&q)
                    || key_text.to_lowercase().contains(&q)
                    || key_pretty.to_lowercase().contains(&q)
                    || desc.to_lowercase().contains(&q);
                if q_matches {
                    if let Some(idx) = pending_header.take() {
                        result.push(idx);
                    }
                    section_has_match = true;
                    result.push(i);
                }
            }
        }
    }
    if let Some(h) = pending_header
        && (section_has_match || current_section_collapsed)
    {
        result.push(h);
    }
    result
}

fn hint_key_display(h: &HintItem) -> String {
    if let Some(d) = h.custom_display {
        d.to_string()
    } else {
        h.keys
            .iter()
            .map(|k| k.display())
            .collect::<Vec<_>>()
            .join("/")
    }
}

/// Pretty key display for the cheatsheet modal.
///
/// Uses `custom_display` when set (for special representations like
/// "Esc Esc" that can't be derived from the key list), otherwise renders
/// the actual keys with pretty formatting (e.g. "Ctrl+Q", "Tab / i / Space").
fn hint_key_pretty(h: &HintItem) -> String {
    if let Some(d) = h.custom_display {
        return d.to_string();
    }
    h.keys
        .iter()
        .map(|k| k.display_pretty())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Get the long description for a hint, falling back to the short label.
pub fn entry_display(entries: &[ShortcutsHelpEntry], idx: usize) -> (String, String) {
    match entries.get(idx) {
        Some(ShortcutsHelpEntry::Hint { item: h, .. }) => (hint_description(h), hint_key_pretty(h)),
        Some(ShortcutsHelpEntry::SectionHeader { label, .. }) => {
            ((*label).to_string(), String::new())
        }
        None => (String::new(), String::new()),
    }
}

fn hint_description(h: &HintItem) -> String {
    h.description
        .as_ref()
        .map(|d| d.to_string())
        .unwrap_or_else(|| {
            // Capitalize the short label as a fallback.
            let label = h.label.as_ref();
            let mut chars = label.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
            }
        })
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn selected_original_entry<'a>(
    filtered: &[usize],
    entries: &'a [ShortcutsHelpEntry],
    selected: usize,
) -> Option<&'a ShortcutsHelpEntry> {
    filtered.get(selected).and_then(|&i| entries.get(i))
}

fn non_selectable_mask(filtered: &[usize], _entries: &[ShortcutsHelpEntry]) -> Vec<bool> {
    filtered.iter().map(|_| false).collect()
}

fn picker_config(non_sel: &[bool]) -> PickerConfig<'_> {
    PickerConfig {
        title: None,
        show_search_hint: false,
        expandable: false,
        esc_clears_query: true,
        shortcuts: None,
        pending_hint: None,
        non_selectable: non_sel,
        non_selectable_clickable: &[],
        shortcuts_area: None,
        tabs: None,
        active_tab: 0,
        filter_label: None,
        filter_key_hint: None,
        filter_active: false,
        action_keys: &[],
        disable_search: false,
        compact_bottom_bar: false,
        search_only_on_slash: false,
        vim_normal_first: crate::appearance::cache::load_vim_mode(),
    }
}

// ---------------------------------------------------------------------------
// Input dispatch
// ---------------------------------------------------------------------------

/// Outcome of an input event delivered to the cheatsheet modal.
///
/// The caller is responsible for mutating `AgentView` state — closing the
/// modal, re-dispatching a synthesized key into `handle_input`, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutsHelpOutcome {
    /// User asked to close the modal (Esc in browse, Ctrl+./Ctrl+X, [x] click).
    Close,
    /// Toggle the filter (show all vs hide dimmed).
    ToggleFilter,
    /// Toggle a section's collapsed state (by category index).
    ToggleSection(usize),
    /// Toggle inline help expand for a hint row (registry or long_help pseudo).
    ToggleExpand(ExpandKey),
    /// Visual state changed (selection, hover, or detail enter/scroll/back) — redraw.
    Changed,
    /// Nothing changed.
    Unchanged,
}

/// Browse list vs in-modal man-style detail (pattern B, same modal chrome).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ShortcutsHelpMode {
    #[default]
    Browse,
    Detail {
        title: String,
        keys_line: String,
        body: String,
        dimmed_note: bool,
        scroll: u16,
    },
}

impl ShortcutsHelpMode {
    pub fn is_browse(&self) -> bool {
        matches!(self, Self::Browse)
    }

    pub fn is_detail(&self) -> bool {
        !self.is_browse()
    }
}

/// Build detail mode state from a cheatsheet entry (title/keys/body for the man page).
///
/// Registry rows always open. Pseudo-rows (`action_id: None`) open only when they
/// ship `long_help` so list-only rows like scrollback search stay browse-only.
pub fn detail_from_entry(entry: &ShortcutsHelpEntry) -> Option<ShortcutsHelpMode> {
    let ShortcutsHelpEntry::Hint {
        item,
        dimmed,
        action_id,
        long_help,
    } = entry
    else {
        return None;
    };
    if action_id.is_none() && long_help.is_none() {
        return None;
    }
    let title = item
        .description
        .as_deref()
        .unwrap_or(item.label.as_ref())
        .to_string();
    let keys_line = item
        .custom_display
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            item.keys
                .iter()
                .map(|k| k.display())
                .collect::<Vec<_>>()
                .join(" / ")
        });
    // Body prefers long_help; falls back to the one-line description.
    let body = long_help
        .as_deref()
        .or(item.description.as_deref())
        .unwrap_or(item.label.as_ref())
        .to_string();
    Some(ShortcutsHelpMode::Detail {
        title,
        keys_line,
        body,
        dimmed_note: *dimmed,
        scroll: 0,
    })
}

/// Open the detail page for `entry`, dropping any committed search so Esc from
/// detail returns to an unfiltered browse and closes with one more press.
fn enter_detail(state: &mut PickerState, entry: &ShortcutsHelpEntry) -> Option<ShortcutsHelpMode> {
    let detail = detail_from_entry(entry)?;
    state.set_query("");
    state.search_active = false;
    Some(detail)
}

/// Footer shortcuts while viewing a shortcut detail page.
pub fn modal_footer_detail() -> Vec<crate::views::modal_window::Shortcut<'static>> {
    use crate::views::modal_window::Shortcut;
    vec![
        Shortcut {
            label: "Esc back",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "\u{2191}/\u{2193} scroll",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Ctrl+./X close",
            clickable: false,
            id: 0,
        },
    ]
}

/// Paint the in-modal detail page (title, keys, body) into the content rect.
#[allow(clippy::too_many_arguments)]
pub fn render_detail_body<'a>(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    title: &'a str,
    keys_line: &'a str,
    body: &'a str,
    dimmed_note: bool,
    scroll: u16,
    theme: &crate::theme::Theme,
) {
    use crate::render::wrapping::word_wrap_lines;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Paragraph, Widget};

    if area.width == 0 || area.height == 0 {
        return;
    }
    // Borrow from the owned detail payload — no allocation while building the rows.
    let mut lines: Vec<Line<'a>> = Vec::new();
    lines.push(Line::from(Span::styled(
        title,
        Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    )));
    if !keys_line.is_empty() {
        lines.push(Line::from(Span::styled(
            keys_line,
            Style::default().fg(theme.gray_bright),
        )));
    }
    // Skip the body when it merely repeats the title (no long_help yet) to avoid a duplicate line.
    if !body.is_empty() && body != title {
        lines.push(Line::from(""));
        // Blank line between paragraphs so the detail page reads as spaced blocks; the inline expand (arrows) keeps them tight.
        for (i, para) in body.split('\n').enumerate() {
            if i > 0 {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                para,
                Style::default().fg(theme.text_primary),
            )));
        }
    }
    if dimmed_note {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "(not active in current context)",
            Style::default().fg(theme.gray_dim),
        )));
    }
    // Pre-wrap to the content width so scrolling counts wrapped rows, not logical lines.
    let wrapped = word_wrap_lines(lines, area.width as usize);
    // Clamp the displayed offset so over-scroll shows the last wrapped row, never a blank body.
    let max_scroll = wrapped.len().saturating_sub(area.height as usize);
    let skip = (scroll as usize).min(max_scroll);
    // Rows are already wrapped to width, so render verbatim (no second wrap pass).
    let visible: Vec<Line<'static>> = wrapped.into_iter().skip(skip).collect();
    Paragraph::new(visible).render(area, buf);
}

/// Render the detail page (pattern B) with its modal chrome + footer. Shared by
/// both hosts so the chrome orchestration lives in one place (like `CheatsheetRows`).
pub fn render_detail(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    window: &mut crate::views::modal_window::ModalWindowState,
    mode: &ShortcutsHelpMode,
    theme: &crate::theme::Theme,
    compact: bool,
) {
    use crate::views::modal_window as mw;
    let ShortcutsHelpMode::Detail {
        title,
        keys_line,
        body,
        dimmed_note,
        scroll,
    } = mode
    else {
        return;
    };
    let footer = modal_footer_detail();
    let modal_config = mw::ModalWindowConfig {
        title: "Keyboard Shortcuts",
        tabs: None,
        shortcuts: &footer,
        sizing: modal_sizing(compact),
        fold_info: None,
    };
    if let Some(mca) = mw::render_modal_window(buf, area, window, &modal_config, theme) {
        render_detail_body(
            buf,
            mca.content,
            title,
            keys_line,
            body,
            *dimmed_note,
            *scroll,
            theme,
        );
    }
}

/// Help line(s) shown under an expanded hint: prefers the action's `long_help`,
/// falling back to the palette description. Callers split on `\n` for multi-line.
pub fn hint_inline_help(entry: &ShortcutsHelpEntry) -> Option<&str> {
    match entry {
        ShortcutsHelpEntry::Hint {
            item, long_help, ..
        } => long_help.as_deref().or(item.description.as_deref()),
        _ => None,
    }
}

/// Expand key for pattern A (e/Space/l/→). Registry rows use their ActionId;
/// pseudo-rows with `long_help` and a static label use [`ExpandKey::Pseudo`].
pub fn expand_key(entry: &ShortcutsHelpEntry) -> Option<ExpandKey> {
    match entry {
        ShortcutsHelpEntry::Hint {
            action_id: Some(id),
            ..
        } => Some(ExpandKey::Action(*id)),
        ShortcutsHelpEntry::Hint {
            action_id: None,
            long_help: Some(_),
            item,
            ..
        } => match item.label {
            Cow::Borrowed(s) => Some(ExpandKey::Pseudo(s)),
            Cow::Owned(_) => None,
        },
        _ => None,
    }
}

/// Whether this hint can participate in inline expand (registry-backed rows only).
/// Prefer [`expand_key`] for new code; kept for call sites that need an ActionId.
pub fn hint_expand_action_id(entry: &ShortcutsHelpEntry) -> Option<crate::actions::ActionId> {
    match expand_key(entry) {
        Some(ExpandKey::Action(id)) => Some(id),
        _ => None,
    }
}

/// Flip `value`'s membership in `set`: insert when absent, remove when present.
/// Shared by both modal hosts for the section-collapse and inline-expand toggles.
pub fn toggle_membership<T: Eq + std::hash::Hash>(
    set: &mut std::collections::HashSet<T>,
    value: T,
) {
    if !set.remove(&value) {
        set.insert(value);
    }
}

/// Dispatch a key event to the cheatsheet picker. Mutates `state`.
///
/// When `mode` is `Detail`, keys scroll the man page or return to browse; global
/// close chords still dismiss the whole modal.
pub fn handle_input(
    key: &crossterm::event::KeyEvent,
    entries: &[ShortcutsHelpEntry],
    state: &mut PickerState,
    hide_dimmed: bool,
    collapsed: &std::collections::HashSet<usize>,
    expanded_ids: &std::collections::HashSet<ExpandKey>,
    mode: &mut ShortcutsHelpMode,
) -> ShortcutsHelpOutcome {
    use crossterm::event::{Event, KeyCode, KeyModifiers};

    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('.') | KeyCode::Char('x'))
    {
        return ShortcutsHelpOutcome::Close;
    }

    if mode.is_detail() {
        // Back-to-browse keys handled before borrowing `scroll` so we can replace `mode`.
        // Vim keys (h/j/k/g) are intentionally NOT bound here — vim modal bindings are owned separately.
        if matches!(key.code, KeyCode::Esc | KeyCode::Left | KeyCode::Backspace) {
            *mode = ShortcutsHelpMode::Browse;
            return ShortcutsHelpOutcome::Changed;
        }
        if let ShortcutsHelpMode::Detail { scroll, .. } = mode {
            return match key.code {
                KeyCode::Down | KeyCode::PageDown => {
                    *scroll = scroll.saturating_add(1);
                    ShortcutsHelpOutcome::Changed
                }
                KeyCode::Up | KeyCode::PageUp => {
                    *scroll = scroll.saturating_sub(1);
                    ShortcutsHelpOutcome::Changed
                }
                KeyCode::Home => {
                    *scroll = 0;
                    ShortcutsHelpOutcome::Changed
                }
                _ => ShortcutsHelpOutcome::Unchanged,
            };
        }
        return ShortcutsHelpOutcome::Unchanged;
    }

    let searching = state.search_active || !state.query().is_empty();
    let vim_mode = crate::appearance::cache::load_vim_mode();

    if !searching {
        // `i` mirrors the vim-nav pickers' "press i to search" affordance.
        if key.code == KeyCode::Char('/')
            || (key.code == KeyCode::Char('i') && key.modifiers.is_empty())
        {
            state.search_active = true;
            return ShortcutsHelpOutcome::Changed;
        }
        if key.code == KeyCode::Char('f') {
            return ShortcutsHelpOutcome::ToggleFilter;
        }
        let filtered = filter_entries(entries, state.query(), hide_dimmed, collapsed);
        if let Some(ShortcutsHelpEntry::SectionHeader { category_idx, .. }) =
            selected_original_entry(&filtered, entries, state.selected)
        {
            let is_collapsed = collapsed.contains(category_idx);
            let toggle = match key.code {
                KeyCode::Char('e') | KeyCode::Char(' ') | KeyCode::Enter => true,
                KeyCode::Right => is_collapsed,
                KeyCode::Char('l') if vim_mode && key.modifiers.is_empty() => is_collapsed,
                KeyCode::Char('E') | KeyCode::Left => !is_collapsed,
                KeyCode::Char('h') if vim_mode && key.modifiers.is_empty() => !is_collapsed,
                _ => false,
            };
            if toggle {
                return ShortcutsHelpOutcome::ToggleSection(*category_idx);
            }
        } else if let Some(entry) = selected_original_entry(&filtered, entries, state.selected)
            && let Some(key_id) = expand_key(entry)
        {
            let is_expanded = expanded_ids.contains(&key_id);
            let toggle = match key.code {
                KeyCode::Char('e') | KeyCode::Char(' ') | KeyCode::Right => true,
                KeyCode::Char('l') if vim_mode && key.modifiers.is_empty() => true,
                KeyCode::Char('E') | KeyCode::Left => is_expanded,
                KeyCode::Char('h') if vim_mode && key.modifiers.is_empty() => is_expanded,
                _ => false,
            };
            if toggle {
                return ShortcutsHelpOutcome::ToggleExpand(key_id);
            }
        }
        // Enter on a registry hint opens in-modal detail (pattern B); section handled above.
        if key.code == KeyCode::Enter {
            if let Some(entry) = selected_original_entry(&filtered, entries, state.selected)
                && let Some(detail) = detail_from_entry(entry)
            {
                *mode = detail;
                return ShortcutsHelpOutcome::Changed;
            }
            return ShortcutsHelpOutcome::Unchanged;
        }
        if matches!(
            key.code,
            KeyCode::Char('h')
                | KeyCode::Char('j')
                | KeyCode::Char('k')
                | KeyCode::Char('l')
                | KeyCode::Down
                | KeyCode::Up
                | KeyCode::Char('g')
                | KeyCode::Char('G')
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Esc
                | KeyCode::Tab
        ) {
            let non_sel: Vec<bool> = non_selectable_mask(&filtered, entries);
            let config = picker_config(&non_sel);
            let ev = Event::Key(*key);
            return match handle_picker_input(&ev, state, filtered.len(), &config) {
                PickerOutcome::Selected(_) | PickerOutcome::Closed => ShortcutsHelpOutcome::Close,
                PickerOutcome::Unchanged => ShortcutsHelpOutcome::Unchanged,
                PickerOutcome::Changed | PickerOutcome::QueryChanged => {
                    ShortcutsHelpOutcome::Changed
                }
                _ => ShortcutsHelpOutcome::Changed,
            };
        }
        return ShortcutsHelpOutcome::Unchanged;
    }

    if key.code == KeyCode::Esc {
        state.set_query("");
        state.search_active = false;
        state.selected = 0;
        return ShortcutsHelpOutcome::Changed;
    }

    let filtered = filter_entries(entries, state.query(), hide_dimmed, collapsed);
    let non_sel: Vec<bool> = non_selectable_mask(&filtered, entries);
    let config = picker_config(&non_sel);

    let ev = Event::Key(*key);
    match handle_picker_input(&ev, state, filtered.len(), &config) {
        PickerOutcome::Selected(idx) => {
            state.search_active = false;
            match selected_original_entry(&filtered, entries, idx) {
                Some(ShortcutsHelpEntry::SectionHeader { category_idx, .. }) => {
                    ShortcutsHelpOutcome::ToggleSection(*category_idx)
                }
                Some(entry) => {
                    if let Some(detail) = enter_detail(state, entry) {
                        *mode = detail;
                        ShortcutsHelpOutcome::Changed
                    } else {
                        ShortcutsHelpOutcome::Unchanged
                    }
                }
                None => ShortcutsHelpOutcome::Unchanged,
            }
        }
        PickerOutcome::Closed => ShortcutsHelpOutcome::Close,
        PickerOutcome::Unchanged => ShortcutsHelpOutcome::Unchanged,
        PickerOutcome::Changed | PickerOutcome::QueryChanged => ShortcutsHelpOutcome::Changed,
        _ => ShortcutsHelpOutcome::Changed,
    }
}

/// Dispatch a mouse event to the cheatsheet picker. Mutates `state`.
pub fn handle_mouse(
    mouse: &crossterm::event::MouseEvent,
    entries: &[ShortcutsHelpEntry],
    state: &mut PickerState,
    hide_dimmed: bool,
    collapsed: &std::collections::HashSet<usize>,
    mode: &mut ShortcutsHelpMode,
) -> ShortcutsHelpOutcome {
    if mode.is_detail() {
        use crossterm::event::MouseEventKind;
        if let ShortcutsHelpMode::Detail { scroll, .. } = mode {
            return match mouse.kind {
                MouseEventKind::ScrollDown => {
                    *scroll = scroll.saturating_add(1);
                    ShortcutsHelpOutcome::Changed
                }
                MouseEventKind::ScrollUp => {
                    *scroll = scroll.saturating_sub(1);
                    ShortcutsHelpOutcome::Changed
                }
                _ => ShortcutsHelpOutcome::Unchanged,
            };
        }
    }

    let filtered = filter_entries(entries, state.query(), hide_dimmed, collapsed);
    let non_sel: Vec<bool> = non_selectable_mask(&filtered, entries);
    let config = picker_config(&non_sel);

    let ev = crossterm::event::Event::Mouse(*mouse);
    match handle_picker_input(&ev, state, filtered.len(), &config) {
        PickerOutcome::Selected(idx) => {
            // Clicking a section header toggles it; hint opens detail (pattern B).
            if let Some(ShortcutsHelpEntry::SectionHeader { category_idx, .. }) =
                selected_original_entry(&filtered, entries, idx)
            {
                ShortcutsHelpOutcome::ToggleSection(*category_idx)
            } else if let Some(entry) = selected_original_entry(&filtered, entries, idx) {
                // enter_detail drops the committed search so click matches the keyboard path.
                if let Some(detail) = enter_detail(state, entry) {
                    *mode = detail;
                    ShortcutsHelpOutcome::Changed
                } else {
                    ShortcutsHelpOutcome::Unchanged
                }
            } else {
                ShortcutsHelpOutcome::Unchanged
            }
        }
        PickerOutcome::Closed => ShortcutsHelpOutcome::Close,
        PickerOutcome::Unchanged => ShortcutsHelpOutcome::Unchanged,
        PickerOutcome::Changed | PickerOutcome::QueryChanged => ShortcutsHelpOutcome::Changed,
        _ => ShortcutsHelpOutcome::Changed,
    }
}

// ---------------------------------------------------------------------------
// Modal rendering + chrome integration
// ---------------------------------------------------------------------------

/// Footer hints painted along the bottom border of the cheatsheet
/// modal. Identical visual vocabulary for the agent view and the
/// dashboard so muscle memory ports across surfaces.
pub fn modal_footer(filter_active: bool) -> Vec<crate::views::modal_window::Shortcut<'static>> {
    use crate::views::modal_window::Shortcut;
    let mut shortcuts = vec![
        Shortcut {
            label: "\u{2191}/\u{2193} nav",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: if filter_active {
                "f show all"
            } else {
                "f filter"
            },
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "e/Space/\u{2192} expand",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "\u{2190} collapse",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Enter details",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "/ search",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Esc close",
            clickable: false,
            id: 0,
        },
    ];
    // Append the `i search` alias last for vim users (matching the other pickers).
    crate::views::modal_window::push_vim_nav_search_hint(&mut shortcuts, false);
    shortcuts
}

/// Modal-window sizing for the cheatsheet. The `compact` knob lets
/// callers honour the user's compact-prompt setting (smaller
/// margins + tighter padding) without re-deriving the sizing rules.
pub fn modal_sizing(compact: bool) -> crate::views::modal_window::ModalSizing {
    crate::views::modal_window::ModalSizing {
        width_pct: 0.70,
        max_width: 80,
        min_width: 44,
        v_margin: 4,
        h_pad: 2,
        v_pad: 1,
        footer_lines: 2,
    }
    .with_compact(compact)
}

/// Per-row kind captured during [`CheatsheetRows::build`] so the borrowed
/// picker rows need only the owned buffers, not the source `entries`.
enum CheatsheetRowKind {
    Header {
        is_collapsed: bool,
    },
    Hint {
        dimmed: bool,
        expand: Option<ExpandKey>,
    },
    Other,
}

/// Owned per-frame buffers backing the cheatsheet picker rows, shared by both
/// modal hosts (agent inline render + dashboard [`render_modal`]). The
/// [`crate::views::picker::PickerEntry`] list from [`Self::picker_entries`]
/// borrows these buffers, so this value must outlive the render call.
pub struct CheatsheetRows {
    row_strs: Vec<(String, String)>,
    // Inline-help per row, newlines collapsed to spaces so the collapsible view renders one
    // wrap-flowed block; empty string when the row has no help. Owned (it's a transform of the source).
    help_text: Vec<String>,
    kinds: Vec<CheatsheetRowKind>,
}

impl CheatsheetRows {
    /// Build the row buffers for the current filter/collapse state. Both hosts
    /// call this so the row/expand construction lives in exactly one place.
    pub fn build(
        entries: &[ShortcutsHelpEntry],
        query: &str,
        filter_active: bool,
        collapsed_sections: &std::collections::HashSet<usize>,
    ) -> Self {
        let filtered = filter_entries(entries, query, filter_active, collapsed_sections);
        let mut row_strs = Vec::with_capacity(filtered.len());
        let mut help_text = Vec::with_capacity(filtered.len());
        let mut kinds = Vec::with_capacity(filtered.len());
        for &i in &filtered {
            match entries.get(i) {
                Some(ShortcutsHelpEntry::SectionHeader {
                    label,
                    entry_count,
                    category_idx,
                }) => {
                    let is_collapsed = collapsed_sections.contains(category_idx);
                    let display = if is_collapsed {
                        format!("{label} ({entry_count})")
                    } else {
                        (*label).to_string()
                    };
                    row_strs.push((display, String::new()));
                    help_text.push(String::new());
                    kinds.push(CheatsheetRowKind::Header { is_collapsed });
                }
                Some(entry @ ShortcutsHelpEntry::Hint { dimmed, .. }) => {
                    row_strs.push(entry_display(entries, i));
                    // Collapse newlines to spaces so the collapsible view shows one wrap-flowed block (no hard breaks).
                    let help = hint_inline_help(entry)
                        .map(|s| s.replace('\n', " "))
                        .unwrap_or_default();
                    help_text.push(help);
                    kinds.push(CheatsheetRowKind::Hint {
                        dimmed: *dimmed,
                        expand: expand_key(entry),
                    });
                }
                _ => {
                    row_strs.push(entry_display(entries, i));
                    help_text.push(String::new());
                    kinds.push(CheatsheetRowKind::Other);
                }
            }
        }
        Self {
            row_strs,
            help_text,
            kinds,
        }
    }

    /// Borrowed views of the per-row inline help, in row order. The caller holds
    /// these so the picker's description slices can borrow them across the render.
    pub fn help_refs(&self) -> Vec<&str> {
        self.help_text.iter().map(String::as_str).collect()
    }

    /// Build the borrowed picker rows, reading selection + expand state. The
    /// returned list borrows `self` and `help` (from [`Self::help_refs`]), so it
    /// lives only as long as both.
    pub fn picker_entries<'a>(
        &'a self,
        state: &PickerState,
        expanded_ids: &std::collections::HashSet<ExpandKey>,
        help: &'a [&'a str],
    ) -> Vec<crate::views::picker::PickerEntry<'a>> {
        use crate::views::picker::{PickerEntry, PickerRow};
        debug_assert_eq!(
            help.len(),
            self.kinds.len(),
            "help must be 1:1 with rows (pass CheatsheetRows::help_refs)"
        );
        self.kinds
            .iter()
            .enumerate()
            .map(|(idx, kind)| {
                let selected = state.hovered == Some(idx)
                    || (state.hovered.is_none() && idx == state.selected);
                match kind {
                    CheatsheetRowKind::Header { is_collapsed } => PickerEntry::Row(PickerRow {
                        label: self.row_strs[idx].0.as_str(),
                        right_label: "",
                        selected,
                        expanded: !is_collapsed,
                        fields: &[],
                        description_lines: &[],
                        summary_lines: &[],
                        dimmed: false,
                        indent: 0,
                        badge: "",
                        badge_color: None,
                        collapsible: true,
                        underline_last_desc: false,
                    }),
                    CheatsheetRowKind::Hint { dimmed, expand } => {
                        let is_expanded =
                            expand.map(|id| expanded_ids.contains(&id)).unwrap_or(false);
                        let description_lines: &[&str] = if is_expanded && !help[idx].is_empty() {
                            std::slice::from_ref(&help[idx])
                        } else {
                            &[]
                        };
                        PickerEntry::Row(PickerRow {
                            label: self.row_strs[idx].0.as_str(),
                            right_label: self.row_strs[idx].1.as_str(),
                            selected,
                            expanded: is_expanded,
                            fields: &[],
                            description_lines,
                            summary_lines: &[],
                            dimmed: *dimmed,
                            indent: 1,
                            badge: "",
                            badge_color: None,
                            collapsible: false,
                            underline_last_desc: false,
                        })
                    }
                    CheatsheetRowKind::Other => PickerEntry::Row(PickerRow {
                        label: self.row_strs[idx].0.as_str(),
                        right_label: self.row_strs[idx].1.as_str(),
                        selected: false,
                        expanded: false,
                        fields: &[],
                        description_lines: &[],
                        summary_lines: &[],
                        dimmed: false,
                        indent: 0,
                        badge: "",
                        badge_color: None,
                        collapsible: false,
                        underline_last_desc: false,
                    }),
                }
            })
            .collect()
    }
}

/// Render the cheatsheet modal in full (chrome + picker content).
///
/// Pulled out of `AgentView::draw` so the dashboard can paint the
/// exact same modal without re-plumbing `ModalWindowConfig` /
/// picker-inner glue. The agent view continues to drive its own
/// modal via `views::modal::ActiveModal::ShortcutsHelp`; this
/// function consumes the same fields by reference.
///
/// The signature mirrors the destructured `ActiveModal::ShortcutsHelp`
/// fields one-to-one so callers can splat them directly — packing
/// these into a wrapper struct would force every call site to
/// build an intermediate just to take it apart again at the
/// chrome / picker boundary.
#[allow(clippy::too_many_arguments)]
pub fn render_modal(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    entries: &[ShortcutsHelpEntry],
    state: &mut PickerState,
    window: &mut crate::views::modal_window::ModalWindowState,
    filter_active: bool,
    collapsed_sections: &std::collections::HashSet<usize>,
    expanded_ids: &std::collections::HashSet<ExpandKey>,
    mode: &ShortcutsHelpMode,
    theme: &crate::theme::Theme,
    compact: bool,
) {
    use crate::views::modal_window as mw;
    use crate::views::picker::{self, PickerHitAreas};
    use ratatui::layout::Rect;

    // Detail screen reuses the same modal chrome with a different footer.
    if mode.is_detail() {
        render_detail(buf, area, window, mode, theme, compact);
        return;
    }

    let rows = CheatsheetRows::build(entries, state.query(), filter_active, collapsed_sections);
    let help_refs = rows.help_refs();
    let picker_entries = rows.picker_entries(state, expanded_ids, &help_refs);
    let non_sel: Vec<bool> = vec![false; picker_entries.len()];
    let footer = modal_footer(filter_active);
    let modal_config = mw::ModalWindowConfig {
        title: "Keyboard Shortcuts",
        tabs: None,
        shortcuts: &footer,
        sizing: modal_sizing(compact),
        fold_info: None,
    };
    let Some(mca) = mw::render_modal_window(buf, area, window, &modal_config, theme) else {
        return;
    };
    let content_area = mca.content;
    let inner_x = mca.inner_x;
    let inner_width = mca.inner_width;
    let searching = state.search_active || !state.query().is_empty();
    let show_search_hint = !searching;

    picker::render_picker_search_bar(
        buf,
        content_area.x,
        content_area.y,
        content_area.width,
        theme,
        state,
        searching,
        show_search_hint,
        Some(theme.bg_base),
    );
    let sep_y = content_area.y + 1;
    if sep_y < content_area.y + content_area.height {
        picker::render_divider(buf, inner_x, sep_y, inner_width, theme, Some(theme.bg_base));
    }
    let entries_start_y = sep_y + 1;
    let search_bar_rect = Rect::new(content_area.x, content_area.y, content_area.width, 1);
    let entries_area = Rect {
        x: content_area.x,
        y: entries_start_y,
        width: content_area.width,
        height: content_area
            .height
            .saturating_sub(entries_start_y.saturating_sub(content_area.y)),
    };
    let content_hit = picker::render_picker_content_with_scrollbar_x(
        buf,
        entries_area,
        theme,
        state,
        &picker_entries,
        &non_sel,
        &[],
        Some(theme.bg_base),
        false,
        inner_x + inner_width - 1,
    );
    state.hit_areas = Some(PickerHitAreas {
        close_button: Rect::default(),
        search_bar: search_bar_rect,
        item_rects: content_hit.item_rects,
        entry_indices: content_hit.entry_indices,
        tab_rects: vec![],
        filter_rect: None,
    });
}

/// Outcome of routing a key through the cheatsheet's
/// chrome + picker pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalKeyOutcome {
    /// User asked to close the modal (Esc in browse, Ctrl+./Ctrl+X,
    /// or the close chrome button).
    Close,
    /// `f` was pressed — caller should flip `filter_active`.
    ToggleFilter,
    /// User toggled a section header (collapse / expand).
    ToggleSection(usize),
    /// Toggle inline help for a hint row (registry or long_help pseudo).
    ToggleExpand(ExpandKey),
    /// Visual state changed (cursor, query, scroll, or detail enter/back).
    Changed,
    /// Nothing changed.
    Unchanged,
}

/// Route a key through the cheatsheet's modal-window chrome + the
/// picker `handle_input`. Mirrors the agent view's per-modal
/// handler so the dashboard can reuse the exact same key
/// semantics. Caller owns `filter_active` / `collapsed_sections`
/// so the result mutations stay local to the wrapping struct.
///
/// Args follow the same one-to-one shape as the field set behind
/// `ActiveModal::ShortcutsHelp` so dashboards and agents can call
/// it via plain destructuring instead of building / unpacking a
/// wrapper struct.
#[allow(clippy::too_many_arguments)]
pub fn handle_modal_key(
    key: &crossterm::event::KeyEvent,
    entries: &[ShortcutsHelpEntry],
    state: &mut PickerState,
    window: &mut crate::views::modal_window::ModalWindowState,
    filter_active: bool,
    collapsed_sections: &std::collections::HashSet<usize>,
    expanded_ids: &std::collections::HashSet<ExpandKey>,
    mode: &mut ShortcutsHelpMode,
    compact: bool,
) -> ModalKeyOutcome {
    use crate::views::modal_window as mw;
    use crossterm::event::KeyCode;

    let searching = state.search_active || !state.query().is_empty();
    if mode.is_browse() && searching && key.code == KeyCode::Esc {
        state.set_query("");
        state.search_active = false;
        state.selected = 0;
        return ModalKeyOutcome::Changed;
    }
    let footer = if mode.is_detail() {
        modal_footer_detail()
    } else {
        modal_footer(filter_active)
    };
    let chrome_cfg = mw::ModalWindowConfig {
        title: "Keyboard Shortcuts",
        tabs: None,
        shortcuts: &footer,
        sizing: modal_sizing(compact),
        fold_info: None,
    };
    // Detail owns Esc (back to browse); skip chrome so it doesn't close the modal.
    if mode.is_browse() {
        match mw::handle_modal_key(window, key, &chrome_cfg) {
            mw::ModalWindowOutcome::CloseRequested => return ModalKeyOutcome::Close,
            mw::ModalWindowOutcome::Unhandled => {}
            _ => return ModalKeyOutcome::Changed,
        }
    }
    match handle_input(
        key,
        entries,
        state,
        filter_active,
        collapsed_sections,
        expanded_ids,
        mode,
    ) {
        ShortcutsHelpOutcome::Close => ModalKeyOutcome::Close,
        ShortcutsHelpOutcome::ToggleFilter => ModalKeyOutcome::ToggleFilter,
        ShortcutsHelpOutcome::ToggleSection(idx) => ModalKeyOutcome::ToggleSection(idx),
        ShortcutsHelpOutcome::ToggleExpand(id) => ModalKeyOutcome::ToggleExpand(id),
        ShortcutsHelpOutcome::Changed => ModalKeyOutcome::Changed,
        ShortcutsHelpOutcome::Unchanged => ModalKeyOutcome::Unchanged,
    }
}

pub fn handle_paste(
    text: &str,
    state: &mut PickerState,
    mode: &ShortcutsHelpMode,
) -> ShortcutsHelpOutcome {
    if mode.is_detail() || !state.search_active {
        return ShortcutsHelpOutcome::Unchanged;
    }
    match state.paste_query(text) {
        crate::input::line_editor::LineEditOutcome::TextChanged => {
            state.selected = 0;
            state.selection_hidden = false;
            state.scroll_offset = None;
            ShortcutsHelpOutcome::Changed
        }
        crate::input::line_editor::LineEditOutcome::HandledNoChange
        | crate::input::line_editor::LineEditOutcome::CursorChanged => {
            ShortcutsHelpOutcome::Changed
        }
        crate::input::line_editor::LineEditOutcome::Unhandled => ShortcutsHelpOutcome::Unchanged,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key;

    struct VimModeGuard(bool);

    impl VimModeGuard {
        fn set(enabled: bool) -> Self {
            let previous = crate::appearance::cache::load_vim_mode();
            crate::appearance::cache::set_vim_mode(enabled);
            Self(previous)
        }
    }

    impl Drop for VimModeGuard {
        fn drop(&mut self) {
            crate::appearance::cache::set_vim_mode(self.0);
        }
    }

    fn header(label: &'static str, idx: usize, count: usize) -> ShortcutsHelpEntry {
        ShortcutsHelpEntry::SectionHeader {
            label,
            category_idx: idx,
            entry_count: count,
        }
    }

    fn hint(label: &'static str, k: KeyShortcut) -> ShortcutsHelpEntry {
        ShortcutsHelpEntry::Hint {
            item: HintItem::new(k, label),
            dimmed: false,
            action_id: None,
            long_help: None,
        }
    }

    fn no_collapsed() -> std::collections::HashSet<usize> {
        std::collections::HashSet::new()
    }

    fn no_expanded() -> std::collections::HashSet<ExpandKey> {
        std::collections::HashSet::new()
    }

    fn browse_mode() -> ShortcutsHelpMode {
        ShortcutsHelpMode::Browse
    }

    fn hint_with_action(
        label: &'static str,
        k: KeyShortcut,
        action_id: crate::actions::ActionId,
    ) -> ShortcutsHelpEntry {
        let mut item = HintItem::new(k, label);
        item.description = Some(std::borrow::Cow::Borrowed(label));
        ShortcutsHelpEntry::Hint {
            item,
            dimmed: false,
            action_id: Some(action_id),
            long_help: None,
        }
    }

    /// `DashboardCycleMode` carries Shift+Tab three times (the terminal
    /// encoding variants `BackTab` / `BackTab`+SHIFT / `Tab`+SHIFT).
    /// The cheatsheet must collapse identically-rendered keys instead
    /// of showing "Shift+Tab / Shift+Tab / Shift+Tab".
    #[test]
    fn build_entries_dedupes_identically_rendered_alt_keys() {
        let registry = crate::actions::ActionRegistry::defaults();
        let entries = build_entries(&[When::DashboardFocused], &registry, false);
        let item = entries
            .iter()
            .find_map(|e| match e {
                ShortcutsHelpEntry::Hint { item, .. }
                    if item.description.as_deref() == Some("Cycle dispatch mode") =>
                {
                    Some(item)
                }
                _ => None,
            })
            .expect("DashboardCycleMode must be listed");
        assert_eq!(
            hint_key_pretty(item),
            "Shift+Tab",
            "encoding-variant alt keys must collapse to one display",
        );
    }

    #[test]
    fn filter_empty_query_returns_all_indices() {
        let entries = vec![
            header("Nav", 0, 2),
            hint("send", key!(Enter)),
            hint("nav", key!('j')),
            header("App", 1, 1),
            hint("quit", key!('q', CONTROL)),
        ];
        assert_eq!(
            filter_entries(&entries, "", false, &no_collapsed()),
            vec![0, 1, 2, 3, 4]
        );
    }

    #[test]
    fn filter_keeps_header_when_section_has_match() {
        let entries = vec![
            header("Nav", 0, 2),
            hint("send", key!(Enter)),
            hint("nav", key!('j')),
            header("App", 1, 1),
            hint("quit", key!('q', CONTROL)),
        ];
        assert_eq!(
            filter_entries(&entries, "send", false, &no_collapsed()),
            vec![0, 1]
        );
    }

    #[test]
    fn filter_drops_header_when_section_empty() {
        let entries = vec![
            header("Nav", 0, 1),
            hint("send", key!(Enter)),
            header("App", 1, 1),
            hint("quit", key!('q', CONTROL)),
        ];
        assert_eq!(
            filter_entries(&entries, "quit", false, &no_collapsed()),
            vec![2, 3]
        );
    }

    #[test]
    fn filter_matches_against_key_display() {
        let entries = vec![
            header("Nav", 0, 2),
            hint("send", key!(Enter)),
            hint("nav", key!('j')),
        ];
        assert_eq!(
            filter_entries(&entries, "enter", false, &no_collapsed()),
            vec![0, 1]
        );
    }

    #[test]
    fn filter_keeps_both_headers_when_both_sections_match() {
        let entries = vec![
            header("Nav", 0, 1),
            hint("nav", key!('j')),
            header("App", 1, 1),
            hint("new session", key!('n', CONTROL)),
        ];
        let result = filter_entries(&entries, "n", false, &no_collapsed());
        assert!(result.contains(&0));
        assert!(result.contains(&1));
        assert!(result.contains(&2));
        assert!(result.contains(&3));
    }

    #[test]
    fn collapsed_section_shows_header_only() {
        let entries = vec![
            header("Nav", 0, 2),
            hint("send", key!(Enter)),
            hint("nav", key!('j')),
            header("App", 1, 1),
            hint("quit", key!('q', CONTROL)),
        ];
        let mut collapsed = std::collections::HashSet::new();
        collapsed.insert(0);
        let result = filter_entries(&entries, "", false, &collapsed);
        assert_eq!(result, vec![0, 3, 4]);
    }

    #[test]
    fn search_forces_collapsed_sections_open() {
        let entries = vec![
            header("Nav", 0, 1),
            hint("nav", key!('j')),
            header("App", 1, 1),
            hint("quit", key!('q', CONTROL)),
        ];
        let mut collapsed = std::collections::HashSet::new();
        collapsed.insert(0);
        let result = filter_entries(&entries, "nav", false, &collapsed);
        assert!(
            result.contains(&1),
            "search should find nav in collapsed section"
        );
    }

    fn all_contexts() -> Vec<When> {
        vec![
            When::ScrollbackFocused,
            When::PromptFocused,
            When::AgentScreen,
            When::Always,
        ]
    }

    #[test]
    fn build_entries_groups_by_category() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);

        let headers: Vec<&str> = entries
            .iter()
            .filter_map(|e| match e {
                ShortcutsHelpEntry::SectionHeader { label, .. } => Some(*label),
                _ => None,
            })
            .collect();
        assert!(headers.contains(&"Essentials"));
        assert!(headers.contains(&"Conversation Navigation"));
        assert!(headers.contains(&"Panels"));
    }

    #[test]
    fn mouse_reporting_shortcut_absent_by_default() {
        // Opt-in via config.toml; default registry must not advertise it.
        let registry = ActionRegistry::defaults();
        assert!(registry.find(ActionId::ToggleMouseCapture).is_none());
        let entries = build_entries(&all_contexts(), &registry, true);
        let has_row = entries.iter().any(|e| {
            matches!(
                            e,
                            ShortcutsHelpEntry::Hint { item, .. }
            if item.label == "mouse reporting"
                        )
        });
        assert!(
            !has_row,
            "mouse reporting must not appear when config-disabled"
        );
    }

    #[test]
    fn mouse_reporting_shortcut_is_under_panels_when_enabled() {
        let registry = ActionRegistry::defaults_with_config(true);
        let def = registry
            .find(ActionId::ToggleMouseCapture)
            .expect("ToggleMouseCapture action must be registered when config-enabled");
        assert_eq!(def.category, Category::Panels);
        assert_eq!(def.label, "mouse reporting");
        assert_eq!(
            def.description,
            "Toggle mouse reporting (native copy/paste)",
        );

        let entries = build_entries(&all_contexts(), &registry, true);
        let mut in_panels = false;
        let mut in_essentials = false;
        let mut seen = false;
        for entry in &entries {
            match entry {
                ShortcutsHelpEntry::SectionHeader { label, .. } => {
                    in_panels = *label == "Panels";
                    in_essentials = *label == "Essentials";
                }
                ShortcutsHelpEntry::Hint { item, .. } => {
                    if item.label == "mouse reporting" {
                        assert!(
                            in_panels,
                            "mouse reporting row must be in Panels, not Essentials"
                        );
                        assert!(
                            !in_essentials,
                            "mouse reporting must not appear under Essentials"
                        );
                        assert_eq!(
                            item.description.as_deref(),
                            Some("Toggle mouse reporting (native copy/paste)"),
                        );
                        let key_text = hint_key_pretty(item);
                        assert!(
                            key_text.contains("Ctrl+r") || key_text.contains("Ctrl+R"),
                            "expected Ctrl+r in key display, got {key_text:?}"
                        );
                        seen = true;
                    }
                }
            }
        }
        assert!(
            seen,
            "mouse reporting row must be present in shortcuts help when enabled"
        );
    }

    #[test]
    fn build_entries_deduplicates_within_category() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);

        let mut current_cat_keys: std::collections::HashSet<KeyShortcut> =
            std::collections::HashSet::new();
        for entry in &entries {
            match entry {
                ShortcutsHelpEntry::SectionHeader { .. } => {
                    current_cat_keys.clear();
                }
                ShortcutsHelpEntry::Hint { item: h, .. } => {
                    if let Some(&k) = h.keys.first() {
                        assert!(
                            current_cat_keys.insert(k),
                            "duplicate key {:?} within same category",
                            k.display()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn build_entries_includes_new_pane_actions() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);

        let has_todos = entries.iter().any(|e| {
            matches!(
                            e,
                            ShortcutsHelpEntry::Hint { item, .. }
            if item.label == "todos"
                        )
        });
        let has_sessions = entries.iter().any(|e| {
            matches!(
                            e,
                            ShortcutsHelpEntry::Hint { item, .. }
            if item.label == "sessions"
                        )
        });
        let has_queue = entries.iter().any(|e| {
            matches!(
                            e,
                            ShortcutsHelpEntry::Hint { item, .. }
            if item.label == "queue"
                        )
        });
        assert!(has_todos, "should include toggle todos");
        assert!(has_sessions, "should include open sessions");
        assert!(has_queue, "should include toggle queue");
    }

    fn has_scrollback_search(entries: &[ShortcutsHelpEntry]) -> bool {
        entries.iter().any(|e| {
            matches!(
                e,
                ShortcutsHelpEntry::Hint { item, .. }
                    if item.label == "search" && item.keys.iter().any(|k| k.display() == "/")
            )
        })
    }

    #[test]
    fn build_entries_includes_scrollback_search_in_vim_mode() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);
        assert!(
            has_scrollback_search(&entries),
            "vim cheatsheet should list / search"
        );
    }

    #[test]
    fn build_entries_omits_scrollback_search_in_simple_mode() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, false);
        assert!(
            !has_scrollback_search(&entries),
            "simple mode does not bind / to search, so it must not be listed"
        );
    }

    #[test]
    fn build_entries_includes_paste() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);
        let entry = entries
            .iter()
            .find(|e| {
                matches!(
                                    e,
                                    ShortcutsHelpEntry::Hint {
                                        item,
                                        action_id: None,
                                        ..
                                    }
                if item.label == "paste"
                                )
            })
            .expect("cheatsheet should list paste");
        let ShortcutsHelpEntry::Hint {
            item, long_help, ..
        } = entry
        else {
            unreachable!();
        };
        assert!(item.keys.iter().any(|k| *k == key!('v', CONTROL)));
        assert!(
            item.description
                .as_deref()
                .is_some_and(|d| d.to_lowercase().contains("image")),
            "description should mention image for search"
        );
        assert_eq!(*long_help, Some(PASTE_LONG_HELP));
        assert!(
            PASTE_LONG_HELP.contains('\n'),
            "paste long_help should be multi-line man-style"
        );
        #[cfg(target_os = "windows")]
        assert!(item.keys.iter().any(|k| *k == key!('v', ALT)));
        #[cfg(not(target_os = "windows"))]
        assert!(!item.keys.iter().any(|k| *k == key!('v', ALT)));
    }

    fn paste_is_dimmed(entries: &[ShortcutsHelpEntry]) -> Option<bool> {
        entries.iter().find_map(|e| match e {
            ShortcutsHelpEntry::Hint {
                item,
                dimmed,
                action_id: None,
                ..
            } if item.label == "paste" => Some(*dimmed),
            _ => None,
        })
    }

    #[test]
    fn build_entries_dims_paste_outside_prompt_and_dashboard() {
        let registry = ActionRegistry::defaults();
        assert_eq!(
            paste_is_dimmed(&build_entries(
                &[When::ScrollbackFocused, When::AgentScreen, When::Always],
                &registry,
                true,
            )),
            Some(true),
            "paste dimmed when neither prompt nor dashboard is active"
        );
        assert_eq!(
            paste_is_dimmed(&build_entries(
                &[When::PromptFocused, When::AgentScreen, When::Always],
                &registry,
                true,
            )),
            Some(false),
            "paste lit when prompt is focused"
        );
        // Dashboard host opens the cheatsheet with only DashboardFocused + Always
        // and handles paste itself — must not dim a working shortcut.
        assert_eq!(
            paste_is_dimmed(&build_entries(
                &[When::DashboardFocused, When::Always],
                &registry,
                true,
            )),
            Some(false),
            "paste lit on the dashboard host"
        );
    }

    #[test]
    fn build_entries_dims_out_of_context_actions() {
        let registry = ActionRegistry::defaults();
        let prompt_contexts = vec![When::PromptFocused, When::AgentScreen, When::Always];
        let entries = build_entries(&prompt_contexts, &registry, true);

        let nav_dimmed = entries.iter().any(|e| {
            matches!(
                            e,
                            ShortcutsHelpEntry::Hint { item, dimmed: true, .. }
            if item.label == "nav"
                        )
        });
        assert!(
            nav_dimmed,
            "scrollback nav should be dimmed when prompt is focused"
        );

        let quit_bright = entries.iter().any(|e| {
            matches!(
                            e,
                            ShortcutsHelpEntry::Hint { item, dimmed: false, .. }
            if item.label == "quit"
                        )
        });
        assert!(quit_bright, "quit should not be dimmed (When::Always)");

        let cancel_bright = entries.iter().any(|e| {
            matches!(
                            e,
                            ShortcutsHelpEntry::Hint { item, dimmed: false, .. }
            if item.label == "cancel"
                        )
        });
        assert!(
            cancel_bright,
            "cancel should not be dimmed (When::AgentScreen)"
        );
    }

    #[test]
    fn build_entries_dims_both_pane_contexts_from_side_pane() {
        let registry = ActionRegistry::defaults();
        let todo_contexts = vec![When::AgentScreen, When::Always];
        let entries = build_entries(&todo_contexts, &registry, true);

        let send_dimmed = entries.iter().any(|e| {
            matches!(
                            e,
                            ShortcutsHelpEntry::Hint { item, dimmed: true, .. }
            if item.label == "send"
                        )
        });
        assert!(
            send_dimmed,
            "send should be dimmed from todo pane (PromptFocused)"
        );

        let nav_dimmed = entries.iter().any(|e| {
            matches!(
                            e,
                            ShortcutsHelpEntry::Hint { item, dimmed: true, .. }
            if item.label == "nav"
                        )
        });
        assert!(
            nav_dimmed,
            "nav should be dimmed from todo pane (ScrollbackFocused)"
        );
    }

    /// The dashboard LIST and the session OVERLAY dim each other's shortcuts:
    /// on the list the overlay-scoped shortcuts (`When::DashboardOverlay`,
    /// e.g. "prev session") are dimmed while the list shortcuts
    /// (`When::DashboardFocused`, e.g. "pin") are lit; inside the overlay it's
    /// the inverse. (Dashboard actions are registered under `cfg(test)`.)
    #[test]
    fn build_entries_dims_dashboard_list_vs_overlay() {
        let registry = ActionRegistry::defaults();
        let dimmed_of = |entries: &[ShortcutsHelpEntry], label: &str| -> Option<bool> {
            entries.iter().find_map(|e| match e {
                ShortcutsHelpEntry::Hint { item, dimmed, .. } if item.label == label => {
                    Some(*dimmed)
                }
                _ => None,
            })
        };

        // Dashboard LIST: list shortcuts lit, overlay shortcuts dimmed.
        let list = build_entries(&[When::DashboardFocused, When::Always], &registry, true);
        assert_eq!(
            dimmed_of(&list, "pin"),
            Some(false),
            "list `pin` must be lit on the dashboard list",
        );
        assert_eq!(
            dimmed_of(&list, "prev session"),
            Some(true),
            "overlay `prev session` must be dimmed on the dashboard list",
        );

        // Session OVERLAY (details): overlay shortcuts lit, list shortcuts dimmed.
        let overlay = build_entries(
            &[When::AgentScreen, When::Always, When::DashboardOverlay],
            &registry,
            true,
        );
        assert_eq!(
            dimmed_of(&overlay, "prev session"),
            Some(false),
            "overlay `prev session` must be lit inside the overlay",
        );
        assert_eq!(
            dimmed_of(&overlay, "pin"),
            Some(true),
            "list `pin` must be dimmed inside the overlay",
        );
    }

    /// `DashboardStop` (list) and `DashboardOverlayStop` (overlay) share
    /// Ctrl+X and the Dashboard category. The per-category dedup must keep
    /// whichever matches the active surface — lit — instead of always
    /// keeping the first-registered (list) def. And inside the overlay the
    /// `ShortcutsHelp` row must drop its shadowed Ctrl+X alt (the overlay
    /// stop owns the key there) while keeping its other binding.
    #[test]
    fn build_entries_overlay_stop_wins_dedup_and_shadows_cheatsheet_ctrl_x() {
        let registry = ActionRegistry::defaults();
        let ctrl_x = crate::key!('x', CONTROL);
        let stop_rows = |entries: &[ShortcutsHelpEntry]| -> Vec<(String, bool)> {
            entries
                .iter()
                .filter_map(|e| match e {
                    ShortcutsHelpEntry::Hint { item, dimmed, .. } if item.label == "stop" => {
                        Some((
                            item.description.as_deref().unwrap_or_default().to_string(),
                            *dimmed,
                        ))
                    }
                    _ => None,
                })
                .collect()
        };
        let stop_id = |entries: &[ShortcutsHelpEntry]| -> Option<ActionId> {
            entries
                .iter()
                .find_map(|e| match e {
                    ShortcutsHelpEntry::Hint {
                        item, action_id, ..
                    } if item.label == "stop" => Some(*action_id),
                    _ => None,
                })
                .flatten()
        };
        let help_keys = |entries: &[ShortcutsHelpEntry]| -> Vec<KeyShortcut> {
            entries
                .iter()
                .find_map(|e| match e {
                    ShortcutsHelpEntry::Hint { item, .. } if item.label == "shortcuts" => {
                        Some(item.keys.clone())
                    }
                    _ => None,
                })
                .expect("the ShortcutsHelp row must be present")
        };

        // Dashboard LIST: the list stop survives, lit; the cheatsheet
        // row keeps Ctrl+X (no overlay up).
        let list = build_entries(&[When::DashboardFocused, When::Always], &registry, true);
        assert_eq!(
            stop_rows(&list),
            vec![("Stop / Close agent".to_string(), false)],
            "the dashboard list must show exactly the list `stop`, lit",
        );
        assert_eq!(
            stop_id(&list),
            Some(ActionId::DashboardStop),
            "the lit list `stop` is inserted first and never replaced — keeps DashboardStop",
        );
        assert!(
            help_keys(&list).contains(&ctrl_x),
            "without an overlay the cheatsheet row keeps its Ctrl+X binding",
        );

        // Session OVERLAY: the overlay stop survives, lit; the
        // cheatsheet row drops the shadowed Ctrl+X but keeps Ctrl+.
        let overlay = build_entries(
            &[When::AgentScreen, When::Always, When::DashboardOverlay],
            &registry,
            true,
        );
        assert_eq!(
            stop_rows(&overlay),
            vec![(
                "Stop agent, close session (back to dashboard)".to_string(),
                false
            )],
            "the overlay must show exactly the overlay `stop`, lit",
        );
        assert_eq!(
            stop_id(&overlay),
            Some(ActionId::DashboardOverlayStop),
            "the lit overlay `stop` replaces the dimmed list row — carries DashboardOverlayStop",
        );
        let keys = help_keys(&overlay);
        assert!(
            !keys.contains(&ctrl_x),
            "inside the overlay the cheatsheet row must drop the shadowed Ctrl+X",
        );
        assert!(
            !keys.is_empty(),
            "the cheatsheet row must keep its non-shadowed binding (Ctrl+.)",
        );
    }

    #[test]
    fn initial_state_selects_first_hint_not_header() {
        let entries = vec![
            header("Nav", 0, 2),
            hint("send", key!(Enter)),
            hint("nav", key!('j')),
        ];
        let state = build_initial_picker_state(&entries);
        assert_eq!(state.selected, 1, "selected should land on first Hint");
    }

    // ── handle_input tests ───────────────────────────────────────

    fn make_key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    /// Helper: set up entries + state with selected on a section header.
    fn setup_on_header() -> (Vec<ShortcutsHelpEntry>, PickerState) {
        let entries = vec![
            header("Nav", 0, 2),
            hint("send", key!(Enter)),
            hint("nav", key!('j')),
        ];
        let mut state = build_initial_picker_state(&entries);
        state.selected = 0; // select the header
        (entries, state)
    }

    #[test]
    fn space_on_section_header_toggles() {
        let (entries, mut state) = setup_on_header();
        let mut mode = browse_mode();
        let result = handle_input(
            &make_key(crossterm::event::KeyCode::Char(' ')),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(result, ShortcutsHelpOutcome::ToggleSection(0));
    }

    #[test]
    fn enter_on_section_header_toggles() {
        let (entries, mut state) = setup_on_header();
        let mut mode = browse_mode();
        let result = handle_input(
            &make_key(crossterm::event::KeyCode::Enter),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(result, ShortcutsHelpOutcome::ToggleSection(0));
    }

    #[test]
    fn enter_on_hint_without_action_id_is_unchanged() {
        // Pseudo/legacy hints have no action_id — Enter does not close or open detail.
        let entries = vec![header("Nav", 0, 1), hint("send", key!(Enter))];
        let mut state = build_initial_picker_state(&entries);
        state.selected = 1; // select the hint
        let mut mode = browse_mode();
        let result = handle_input(
            &make_key(crossterm::event::KeyCode::Enter),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(result, ShortcutsHelpOutcome::Unchanged);
        assert!(mode.is_browse());
    }

    #[test]
    fn enter_on_registry_hint_opens_detail() {
        use crate::actions::ActionId;
        let entries = vec![
            header("Nav", 0, 1),
            hint_with_action("send", key!(Enter), ActionId::SendPrompt),
        ];
        let mut state = build_initial_picker_state(&entries);
        state.selected = 1;
        let mut mode = browse_mode();
        let result = handle_input(
            &make_key(crossterm::event::KeyCode::Enter),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(result, ShortcutsHelpOutcome::Changed);
        assert!(
            mode.is_detail(),
            "Enter on a registry hint must switch mode to Detail"
        );
    }

    /// Opening detail from an active search clears the query so a later Esc closes
    /// the modal directly (back -> close), not back -> clear-query -> close.
    #[test]
    fn enter_from_search_opens_detail_and_clears_query() {
        use crate::actions::ActionId;
        let entries = vec![
            header("Nav", 0, 1),
            hint_with_action("send", key!(Enter), ActionId::SendPrompt),
        ];
        let mut state = build_initial_picker_state(&entries);
        // Active search matching the hint, selection on the matching row.
        state.set_query("send");
        state.search_active = true;
        state.selected = 1;
        let mut mode = browse_mode();
        let result = handle_input(
            &make_key(crossterm::event::KeyCode::Enter),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(result, ShortcutsHelpOutcome::Changed);
        assert!(mode.is_detail(), "Enter from search opens the detail page");
        assert!(
            state.query().is_empty(),
            "opening detail clears the search query"
        );
        assert!(!state.search_active, "opening detail clears search_active");
    }

    /// Mouse parity with the keyboard path: clicking a hint while searching opens
    /// detail AND drops the committed query (so Esc from detail closes next press).
    #[test]
    fn click_from_search_opens_detail_and_clears_query() {
        use crate::actions::ActionId;
        use crate::views::picker::PickerHitAreas;
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;
        let entries = vec![
            header("Nav", 0, 1),
            hint_with_action("send", key!(Enter), ActionId::SendPrompt),
        ];
        let mut state = build_initial_picker_state(&entries);
        // Active search that still matches the hint row.
        state.set_query("send");
        state.search_active = true;
        // Map a click at row 2 to the hint's position in the filtered view.
        let filtered = filter_entries(&entries, state.query(), false, &no_collapsed());
        let hint_pos = filtered
            .iter()
            .position(|&i| matches!(entries[i], ShortcutsHelpEntry::Hint { .. }))
            .expect("hint present in the filtered view");
        state.hit_areas = Some(PickerHitAreas {
            close_button: Rect::default(),
            search_bar: Rect::default(),
            item_rects: vec![Rect::new(0, 2, 20, 1)],
            entry_indices: vec![hint_pos],
            tab_rects: vec![],
            filter_rect: None,
        });
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        let mut mode = browse_mode();
        let result = handle_mouse(
            &click,
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &mut mode,
        );
        assert_eq!(result, ShortcutsHelpOutcome::Changed);
        assert!(mode.is_detail(), "clicking a hint from search opens detail");
        assert!(
            state.query().is_empty(),
            "click-open detail clears the search query"
        );
        assert!(
            !state.search_active,
            "click-open detail clears search_active"
        );
    }

    /// The browse footer advertises the detail action so pattern B is discoverable.
    #[test]
    fn modal_footer_advertises_detail() {
        let footer = modal_footer(false);
        assert!(
            footer.iter().any(|s| s.label.contains("details")),
            "browse footer must advertise Enter details"
        );
    }

    /// Wiring check: the cheatsheet footer carries the shared `i search` hint
    /// under vim and keeps `/ search` regardless. The gate is covered centrally
    /// by `modal_window::tests::vim_nav_search_hint_only_in_vim_nav_mode`.
    #[test]
    fn modal_footer_advertises_i_search_under_vim() {
        let _vim_mode = VimModeGuard::set(true);
        let footer = modal_footer(false);
        assert!(
            footer.iter().any(|s| s.label == "i search"),
            "vim-mode cheatsheet footer must advertise `i search`"
        );
        assert!(
            footer.iter().any(|s| s.label == "/ search"),
            "`/ search` must remain regardless of vim-mode"
        );
    }

    /// Host path: Enter on a registry hint enters Detail (not Close) via the
    /// chrome + picker pipeline both hosts share.
    #[test]
    fn handle_modal_key_enter_on_hint_enters_detail() {
        use crate::actions::ActionId;
        let entries = vec![
            header("Nav", 0, 1),
            hint_with_action("send", key!(Enter), ActionId::SendPrompt),
        ];
        let mut state = build_initial_picker_state(&entries);
        state.selected = 1;
        let mut window = crate::views::modal_window::ModalWindowState::default();
        let mut mode = browse_mode();
        let outcome = handle_modal_key(
            &make_key(crossterm::event::KeyCode::Enter),
            &entries,
            &mut state,
            &mut window,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
            false,
        );
        assert_ne!(
            outcome,
            ModalKeyOutcome::Close,
            "Enter on a hint must not close"
        );
        assert_eq!(outcome, ModalKeyOutcome::Changed);
        assert!(mode.is_detail(), "Enter enters the detail page");
    }

    /// Over-scrolling a detail body clamps to the last lines instead of paging
    /// into an all-blank page.
    #[test]
    fn render_detail_body_clamps_overscroll() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let theme = crate::theme::Theme::current();
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        // Body taller than the 4-row viewport; a huge offset must still land on the end.
        let body = "L1\nL2\nL3\nL4\nL5\nZqxlast";
        render_detail_body(
            &mut buf,
            area,
            "Title",
            "Enter",
            body,
            false,
            u16::MAX,
            &theme,
        );
        let mut out = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
        }
        assert!(
            out.contains("Zqxlast"),
            "over-scroll must clamp to show the last line, got: {out:?}"
        );
    }

    /// When the body merely repeats the title (no long_help yet) it must render
    /// once; a distinct body (populated long_help) must still render below the title.
    #[test]
    fn render_detail_body_omits_body_equal_to_title() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let theme = crate::theme::Theme::current();
        let collect = |title: &str, body: &str| -> String {
            let area = Rect::new(0, 0, 40, 10);
            let mut buf = Buffer::empty(area);
            render_detail_body(&mut buf, area, title, "Enter", body, false, 0, &theme);
            let mut out = String::new();
            for y in area.y..area.y + area.height {
                for x in area.x..area.x + area.width {
                    if let Some(cell) = buf.cell((x, y)) {
                        out.push_str(cell.symbol());
                    }
                }
            }
            out
        };
        assert_eq!(
            collect("Zqxtitle", "Zqxtitle").matches("Zqxtitle").count(),
            1,
            "body equal to title must not be rendered twice"
        );
        let distinct = collect("Zqxtitle", "Zqxbody");
        assert_eq!(distinct.matches("Zqxtitle").count(), 1);
        assert!(
            distinct.contains("Zqxbody"),
            "a distinct body must still render below the title"
        );
    }

    /// Every action that ships `long_help` carries man-style copy that is present
    /// and genuinely distinct from its one-line description. Iterating the whole
    /// registry catches a future description-echo on ANY populated action.
    #[test]
    fn populated_long_help_is_distinct_and_man_style() {
        let registry = ActionRegistry::defaults();
        let populated: Vec<&crate::actions::ActionDef> = registry
            .all()
            .iter()
            .filter(|d| d.long_help.is_some())
            .collect();
        // Sanity floor so an accidental data wipe fails the test rather than passing vacuously.
        assert!(
            populated.len() >= 12,
            "expected the priority long_help set to stay populated, got {}",
            populated.len()
        );
        for def in populated {
            let long = def.long_help.expect("filtered to Some above");
            assert_ne!(
                long, def.description,
                "{:?} long_help must differ from its description (no echo)",
                def.id
            );
            assert!(
                long.contains('\n'),
                "{:?} long_help should be multi-line man-style copy",
                def.id
            );
        }
    }

    /// `detail_from_entry` surfaces the action's `long_help` as the detail body
    /// (not the description), proving the populated copy reaches the screen.
    #[test]
    fn detail_from_entry_uses_long_help_for_body() {
        let registry = ActionRegistry::defaults();
        let def = registry
            .find(ActionId::ShortcutsHelp)
            .expect("ShortcutsHelp is registered");
        let expected = def.long_help.expect("ShortcutsHelp has long_help");
        let entries = build_entries(&all_contexts(), &registry, true);
        let entry = entries
            .iter()
            .find(|e| hint_expand_action_id(e) == Some(ActionId::ShortcutsHelp))
            .expect("ShortcutsHelp row is present");
        let ShortcutsHelpMode::Detail { body, .. } =
            detail_from_entry(entry).expect("registry hint yields a detail")
        else {
            panic!("expected Detail mode");
        };
        assert_eq!(
            body, expected,
            "detail body must surface the action's long_help"
        );
        assert_ne!(
            body, def.description,
            "detail body must be the long_help, not the description"
        );
    }

    /// Scroll clamp counts WRAPPED rows: a body that wraps well past the viewport
    /// can scroll to its last wrapped row (a logical-line clamp could not reach it).
    #[test]
    fn render_detail_body_scroll_is_wrap_aware() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let theme = crate::theme::Theme::current();
        // Narrow + short: one logical body line that wraps into many rows.
        let area = Rect::new(0, 0, 20, 4);
        let mut buf = Buffer::empty(area);
        let body = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo ZZEND";
        render_detail_body(
            &mut buf,
            area,
            "Title",
            "Enter",
            body,
            false,
            u16::MAX,
            &theme,
        );
        let mut out = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
        }
        assert!(
            out.contains("ZZEND"),
            "wrap-aware clamp must scroll to the last wrapped row, got: {out:?}"
        );
    }

    /// The detail page (Enter) paints a blank line between paragraphs so wrapped
    /// text reads as spaced blocks. The inline expand (arrows) is a separate path
    /// and stays tight.
    #[test]
    fn render_detail_body_spaces_paragraphs_with_blank_line() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let theme = crate::theme::Theme::current();
        let area = Rect::new(0, 0, 40, 8);
        let mut buf = Buffer::empty(area);
        render_detail_body(
            &mut buf,
            area,
            "Title",
            "Enter",
            "First paragraph.\nSecond paragraph.",
            false,
            0,
            &theme,
        );
        let rows: Vec<String> = (area.y..area.y + area.height)
            .map(|y| {
                (area.x..area.x + area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        let first = rows
            .iter()
            .position(|r| r.contains("First paragraph."))
            .expect("first paragraph present");
        let second = rows
            .iter()
            .position(|r| r.contains("Second paragraph."))
            .expect("second paragraph present");
        assert_eq!(
            second,
            first + 2,
            "paragraphs must be separated by exactly one blank row, rows: {rows:?}"
        );
        assert!(
            rows[first + 1].is_empty(),
            "the row between paragraphs must be blank, got {:?}",
            rows[first + 1]
        );
    }

    /// Search has no long_help — Enter stays in browse.
    #[test]
    fn enter_on_search_pseudo_row_does_not_open_detail() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);
        let idx = entries
            .iter()
            .position(|e| {
                matches!(
                    e,
                    ShortcutsHelpEntry::Hint { item, action_id: None, .. }
                        if item.label == "search"
                )
            })
            .expect("vim-mode entries include the `/`-search pseudo-row");
        let mut state = build_initial_picker_state(&entries);
        state.selected = idx;
        let mut mode = browse_mode();
        let out = handle_input(
            &make_key(crossterm::event::KeyCode::Enter),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(out, ShortcutsHelpOutcome::Unchanged);
        assert!(
            mode.is_browse(),
            "search pseudo-row Enter must not open detail"
        );
    }

    /// Paste ships long_help — Enter opens the man-page detail view.
    #[test]
    fn enter_on_paste_pseudo_row_opens_detail() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);
        let idx = entries
            .iter()
            .position(|e| {
                matches!(
                                    e,
                                    ShortcutsHelpEntry::Hint {
                                        item,
                                        action_id: None,
                                        long_help: Some(_),
                                        ..
                                    }
                if item.label == "paste"
                                )
            })
            .expect("paste pseudo-row with long_help");
        assert_eq!(
            detail_from_entry(&entries[idx])
                .and_then(|m| match m {
                    ShortcutsHelpMode::Detail { body, .. } => Some(body),
                    _ => None,
                })
                .as_deref(),
            Some(PASTE_LONG_HELP)
        );
        let mut state = build_initial_picker_state(&entries);
        state.selected = idx;
        let mut mode = browse_mode();
        let out = handle_input(
            &make_key(crossterm::event::KeyCode::Enter),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(out, ShortcutsHelpOutcome::Changed);
        match &mode {
            ShortcutsHelpMode::Detail {
                body, keys_line, ..
            } => {
                assert_eq!(body, PASTE_LONG_HELP);
                assert!(
                    keys_line.to_ascii_lowercase().contains("ctrl+v"),
                    "detail keys should list Ctrl+V, got {keys_line:?}"
                );
            }
            ShortcutsHelpMode::Browse => panic!("paste Enter must open detail"),
        }
    }

    #[test]
    fn esc_in_detail_returns_to_browse() {
        let mut mode = ShortcutsHelpMode::Detail {
            title: "Send".into(),
            keys_line: "Enter".into(),
            body: "Send the message".into(),
            dimmed_note: false,
            scroll: 0,
        };
        let entries: Vec<ShortcutsHelpEntry> = vec![];
        let mut state = PickerState::default();
        let result = handle_input(
            &make_key(crossterm::event::KeyCode::Esc),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(result, ShortcutsHelpOutcome::Changed);
        assert!(mode.is_browse(), "Esc in detail must return to browse");
    }

    /// Vim keys (h/j/k/g) are intentionally NOT bound in detail mode — vim modal
    /// bindings are owned separately. Arrows/Home scroll; Esc/Left/Backspace go back.
    #[test]
    fn detail_mode_ignores_vim_keys() {
        use crossterm::event::KeyCode;
        let entries: Vec<ShortcutsHelpEntry> = vec![];
        let mut state = PickerState::default();
        let scroll_of = |m: &ShortcutsHelpMode| match m {
            ShortcutsHelpMode::Detail { scroll, .. } => *scroll,
            _ => u16::MAX,
        };
        let detail = || ShortcutsHelpMode::Detail {
            title: "Send".into(),
            keys_line: "Enter".into(),
            body: "line one\nline two".into(),
            dimmed_note: false,
            scroll: 0,
        };
        // h/j/k/g are inert in detail: no scroll, no back.
        for code in [
            KeyCode::Char('h'),
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('g'),
        ] {
            let mut mode = detail();
            let out = handle_input(
                &make_key(code),
                &entries,
                &mut state,
                false,
                &no_collapsed(),
                &no_expanded(),
                &mut mode,
            );
            assert!(mode.is_detail(), "{code:?} must not leave detail mode");
            assert_eq!(
                scroll_of(&mode),
                0,
                "{code:?} must not scroll the detail body"
            );
            assert_eq!(
                out,
                ShortcutsHelpOutcome::Unchanged,
                "{code:?} must be inert in detail, got {out:?}"
            );
        }
        // Non-vim keys still work: Down scrolls, Left returns to browse.
        let mut mode = detail();
        let _ = handle_input(
            &make_key(KeyCode::Down),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(scroll_of(&mode), 1, "Down scrolls the detail body");
        let _ = handle_input(
            &make_key(KeyCode::Left),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert!(mode.is_browse(), "Left returns to browse");
    }

    /// Host path: chrome must not intercept Esc while in detail (would close the
    /// modal); it returns to browse and keeps the modal open.
    #[test]
    fn handle_modal_key_esc_in_detail_is_back_not_close() {
        let entries = vec![header("Nav", 0, 1), hint("send", key!(Enter))];
        let mut state = build_initial_picker_state(&entries);
        let mut window = crate::views::modal_window::ModalWindowState::default();
        let collapsed = no_collapsed();
        let mut mode = ShortcutsHelpMode::Detail {
            title: "Send".into(),
            keys_line: "Enter".into(),
            body: "Send the message".into(),
            dimmed_note: false,
            scroll: 0,
        };
        let outcome = handle_modal_key(
            &make_key(crossterm::event::KeyCode::Esc),
            &entries,
            &mut state,
            &mut window,
            false,
            &collapsed,
            &no_expanded(),
            &mut mode,
            false,
        );
        assert_ne!(
            outcome,
            ModalKeyOutcome::Close,
            "Esc in detail must not close"
        );
        assert_eq!(outcome, ModalKeyOutcome::Changed);
        assert!(mode.is_browse(), "Esc in detail returns to browse");
    }

    #[test]
    fn esc_in_browse_closes_via_picker() {
        let entries = vec![header("Nav", 0, 1), hint("send", key!(Enter))];
        let mut state = build_initial_picker_state(&entries);
        let mut mode = browse_mode();
        let result = handle_input(
            &make_key(crossterm::event::KeyCode::Esc),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(result, ShortcutsHelpOutcome::Close);
        assert!(mode.is_browse());
    }

    #[test]
    fn ctrl_dot_closes_from_detail_mode() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut mode = ShortcutsHelpMode::Detail {
            title: "Send".into(),
            keys_line: "Enter".into(),
            body: "body".into(),
            dimmed_note: false,
            scroll: 3,
        };
        let entries: Vec<ShortcutsHelpEntry> = vec![];
        let mut state = PickerState::default();
        let key = KeyEvent::new(KeyCode::Char('.'), KeyModifiers::CONTROL);
        let result = handle_input(
            &key,
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(result, ShortcutsHelpOutcome::Close);
        // mode is unchanged by handle_input; caller clears the modal.
        assert!(mode.is_detail());
    }

    #[test]
    fn ctrl_x_closes_from_browse_mode() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let entries = vec![header("Nav", 0, 1), hint("send", key!(Enter))];
        let mut state = build_initial_picker_state(&entries);
        let mut mode = browse_mode();
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        let result = handle_input(
            &key,
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(result, ShortcutsHelpOutcome::Close);
    }

    #[test]
    fn vim_i_enters_search_and_printables_type_afterward() {
        let _vim_mode = VimModeGuard::set(true);
        let (entries, mut state) = setup_on_header();
        assert!(!state.search_active);
        let mut mode = browse_mode();
        let enter_search = handle_input(
            &make_key(crossterm::event::KeyCode::Char('i')),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(enter_search, ShortcutsHelpOutcome::Changed);
        assert!(state.search_active, "`i` must activate cheatsheet search");
        assert!(state.query().is_empty(), "`i` must not enter search text");

        let type_j = handle_input(
            &make_key(crossterm::event::KeyCode::Char('j')),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(type_j, ShortcutsHelpOutcome::Changed);
        assert_eq!(state.query(), "j", "printables must type in active search");
    }

    // ── vim_mode tests ───────────────────────────────────────────

    #[test]
    fn vim_mode_jk_navigate_without_starting_search() {
        let _vim_mode = VimModeGuard::set(true);
        let entries = vec![
            header("Nav", 0, 3),
            hint("send", key!(Enter)),
            hint("next", key!('n')),
            hint("quit", key!('q', CONTROL)),
        ];
        let mut state = build_initial_picker_state(&entries);
        let mut mode = browse_mode();

        let down = handle_input(
            &make_key(crossterm::event::KeyCode::Char('j')),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(down, ShortcutsHelpOutcome::Changed);
        assert_eq!(state.selected, 2, "`j` must select the next row");
        assert!(state.query().is_empty(), "`j` must not enter search text");
        assert!(!state.search_active, "`j` must leave search inactive");

        let up = handle_input(
            &make_key(crossterm::event::KeyCode::Char('k')),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(up, ShortcutsHelpOutcome::Changed);
        assert_eq!(state.selected, 1, "`k` must select the previous row");
        assert!(state.query().is_empty(), "`k` must not enter search text");
        assert!(!state.search_active, "`k` must leave search inactive");
    }

    #[test]
    fn non_vim_hjkl_start_search() {
        let _vim_mode = VimModeGuard::set(false);
        let (entries, state) = setup_on_header();

        for ch in ['h', 'j', 'k', 'l'] {
            let mut state = state.clone();
            let collapsed = if ch == 'l' {
                std::collections::HashSet::from([0])
            } else {
                no_collapsed()
            };
            let mut mode = browse_mode();
            let result = handle_input(
                &make_key(crossterm::event::KeyCode::Char(ch)),
                &entries,
                &mut state,
                false,
                &collapsed,
                &no_expanded(),
                &mut mode,
            );
            assert_eq!(
                result,
                ShortcutsHelpOutcome::Changed,
                "non-vim `{ch}` must start search"
            );
            assert_eq!(state.query(), ch.to_string(), "non-vim `{ch}` must type");
        }
    }

    /// In non-vim mode, `j/k` row should drop the `j` key and show only
    /// the `Down` alt — `Down` still works and the row should not be dimmed.
    #[test]
    fn build_entries_vim_off_keeps_arrow_alt_without_vim_key() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, false);

        let nav = entries
            .iter()
            .find_map(|e| match e {
                ShortcutsHelpEntry::Hint { item, dimmed, .. } if item.label == "nav" => {
                    Some((item, *dimmed))
                }
                _ => None,
            })
            .expect("nav (SelectNext) row should be present in non-vim mode");
        let (item, dimmed) = nav;
        assert!(!dimmed, "nav row with Down alt should not be dimmed");
        assert!(
            item.keys.iter().all(|k| !k.is_letter_or_shift_letter()),
            "non-vim cheatsheet must not advertise letter keys; got {:?}",
            item.keys.iter().map(|k| k.display()).collect::<Vec<_>>()
        );
        assert!(
            !item.keys.is_empty(),
            "row must retain at least one (non-vim) key"
        );
    }

    /// In non-vim mode, scrollback bindings that have NO non-vim alt
    /// (e.g. `g` GotoTop, `y` CopyBlockContent) should be hidden from the
    /// cheatsheet entirely.
    #[test]
    fn build_entries_vim_off_hides_vim_only_rows() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, false);

        for label in ["top", "btm", "copy", "copy cmd"] {
            let present = entries.iter().any(|e| {
                matches!(
                                    e,
                                    ShortcutsHelpEntry::Hint { item, .. }
                if item.label == label
                                )
            });
            assert!(
                !present,
                "{label:?} (vim-only) should be hidden from cheatsheet when vim_mode=false"
            );
        }
    }

    /// Vim mode ON: both vim key and arrow alt should be visible on the
    /// same row.
    #[test]
    fn build_entries_vim_on_shows_both_vim_and_arrow_keys() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);

        let nav_keys: Vec<String> = entries
            .iter()
            .find_map(|e| match e {
                ShortcutsHelpEntry::Hint { item, .. } if item.label == "nav" => {
                    Some(item.keys.iter().map(|k| k.display().to_string()).collect())
                }
                _ => None,
            })
            .expect("nav row should be present in vim mode");
        let nav_keys_joined = nav_keys.join(" ");
        assert!(
            nav_keys_joined.contains('j') || nav_keys_joined.contains('J'),
            "vim mode should show `j` key for nav: {nav_keys:?}"
        );
        assert!(
            nav_keys_joined.contains('↓') || nav_keys_joined.to_lowercase().contains("down"),
            "vim mode should also show arrow alt: {nav_keys:?}"
        );
    }

    /// Asserts that the cheatsheet row for `label` advertises `expected_key`
    /// (primary or alt). Used by the Windows-fallback regressions below.
    fn assert_cheatsheet_row_has_key(
        entries: &[ShortcutsHelpEntry],
        label: &str,
        expected_key: &str,
    ) {
        let keys: Vec<String> = entries
            .iter()
            .find_map(|e| match e {
                ShortcutsHelpEntry::Hint { item, .. } if item.label == label => {
                    Some(item.keys.iter().map(|k| k.display()).collect())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("{label:?} row not found in cheatsheet"));
        assert!(
            keys.iter().any(|k| k == expected_key),
            "{label} cheatsheet row missing {expected_key}; got {keys:?}"
        );
    }

    #[test]
    fn build_entries_surfaces_interject_ctrl_i_fallback() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);
        // Action label is compact "send now" wording (interject under the hood).
        assert_cheatsheet_row_has_key(&entries, "send now", "Ctrl+i");
    }

    #[test]
    fn build_entries_surfaces_queue_ctrl_apostrophe_fallback() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);
        assert_cheatsheet_row_has_key(&entries, "queue", "Ctrl+'");
    }

    /// A section whose entries are all filtered out should have its
    /// header dropped, not rendered as a dead row.
    #[test]
    fn build_entries_vim_off_drops_empty_section_headers() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, false);

        for entry in &entries {
            if let ShortcutsHelpEntry::SectionHeader {
                entry_count, label, ..
            } = entry
            {
                assert!(
                    *entry_count > 0,
                    "section {label:?} has 0 entries — should have been dropped"
                );
            }
        }
    }

    #[test]
    fn build_entries_sets_action_id_on_registry_hints() {
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);
        let shortcuts_id = entries.iter().find_map(|e| match e {
            ShortcutsHelpEntry::Hint {
                item,
                action_id: Some(id),
                ..
            } if item.description.as_deref() == Some("Keyboard shortcuts") => Some(*id),
            _ => None,
        });
        assert_eq!(
            shortcuts_id,
            Some(ActionId::ShortcutsHelp),
            "registry-backed hints must carry their ActionId for expand/detail"
        );

        // Registry rows carry ActionId; search + paste are display-only.
        let search_key = key!('/');
        let paste_key = key!('v', CONTROL);
        for entry in &entries {
            let ShortcutsHelpEntry::Hint {
                item, action_id, ..
            } = entry
            else {
                continue;
            };
            let is_pseudo = (item.label == "search" && item.keys.contains(&search_key))
                || (item.label == "paste" && item.keys.contains(&paste_key));
            if is_pseudo {
                assert!(
                    action_id.is_none(),
                    "pseudo-row {:?} must stay display-only",
                    item.label
                );
            } else {
                assert!(
                    action_id.is_some(),
                    "registry-backed hint {:?} lost its action_id",
                    item.label
                );
            }
        }
    }

    #[test]
    fn toggle_expand_outcome_for_hint_right_key() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);
        let mut state = build_initial_picker_state(&entries);
        // Select first non-header row (Essentials section is first header at 0).
        state.selected = 1;
        let mut mode = browse_mode();
        let key = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        let out = handle_input(
            &key,
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert!(
            matches!(out, ShortcutsHelpOutcome::ToggleExpand(_)),
            "Right on hint row should toggle inline expand, got {out:?}"
        );
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        let collapsed = handle_input(
            &left,
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert!(
            !matches!(collapsed, ShortcutsHelpOutcome::ToggleExpand(_)),
            "Left on a collapsed hint must be inert, got {collapsed:?}"
        );
        assert!(mode.is_browse(), "Left must not leave browse mode");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let out_enter = handle_input(
            &enter,
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(
            out_enter,
            ShortcutsHelpOutcome::Changed,
            "Enter on a registry hint opens detail (pattern B), got {out_enter:?}"
        );
        assert!(mode.is_detail(), "Enter switches mode to Detail directly");
    }

    #[test]
    fn vim_h_collapses_only_expanded_action_hints() {
        use crate::actions::ActionId;
        let _vim_mode = VimModeGuard::set(true);
        let entries = vec![
            header("Nav", 0, 1),
            hint_with_action("send", key!(Enter), ActionId::SendPrompt),
        ];
        let mut state = build_initial_picker_state(&entries);
        state.selected = 1;
        let mut mode = browse_mode();

        let collapsed = handle_input(
            &make_key(crossterm::event::KeyCode::Char('h')),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(
            collapsed,
            ShortcutsHelpOutcome::Unchanged,
            "vim h on a collapsed action hint must be inert"
        );
        assert!(state.query().is_empty(), "vim h must not enter search text");

        let key_id = ExpandKey::Action(ActionId::SendPrompt);
        let expanded = std::collections::HashSet::from([key_id]);
        let collapse = handle_input(
            &make_key(crossterm::event::KeyCode::Char('h')),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &expanded,
            &mut mode,
        );
        assert_eq!(
            collapse,
            ShortcutsHelpOutcome::ToggleExpand(key_id),
            "vim h must collapse an expanded action hint"
        );
        assert!(state.query().is_empty(), "vim h must not enter search text");
    }

    #[test]
    fn search_pseudo_row_does_not_expand() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);
        let search_idx = entries
            .iter()
            .position(|e| {
                matches!(
                    e,
                    ShortcutsHelpEntry::Hint { item, action_id: None, .. }
                        if item.label == "search"
                )
            })
            .expect("vim-mode entries include the `/`-search pseudo-row");
        for code in [KeyCode::Right, KeyCode::Char('e'), KeyCode::Char(' ')] {
            let mut state = build_initial_picker_state(&entries);
            state.selected = search_idx;
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            let mut mode = ShortcutsHelpMode::Browse;
            let out = handle_input(
                &key,
                &entries,
                &mut state,
                false,
                &no_collapsed(),
                &no_expanded(),
                &mut mode,
            );
            assert_eq!(
                out,
                ShortcutsHelpOutcome::Unchanged,
                "search pseudo-row must stay inert for {code:?}, got {out:?}"
            );
        }
    }

    #[test]
    fn vim_l_expands_and_h_collapses_paste() {
        use crossterm::event::KeyCode;
        let _vim_mode = VimModeGuard::set(true);
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);
        let paste_idx = entries
            .iter()
            .position(|e| {
                matches!(
                                    e,
                                    ShortcutsHelpEntry::Hint {
                                        item,
                                        action_id: None,
                                        long_help: Some(_),
                                        ..
                                    }
                if item.label == "paste"
                                )
            })
            .expect("paste pseudo-row with long_help");
        let key_id = ExpandKey::Pseudo("paste");
        assert_eq!(expand_key(&entries[paste_idx]), Some(key_id));
        let mut state = build_initial_picker_state(&entries);
        state.selected = paste_idx;
        let mut mode = ShortcutsHelpMode::Browse;
        let expand = handle_input(
            &make_key(KeyCode::Char('l')),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
        );
        assert_eq!(
            expand,
            ShortcutsHelpOutcome::ToggleExpand(key_id),
            "vim l must expand the paste pseudo-row"
        );
        assert!(state.query().is_empty(), "vim l must not enter search text");

        let expanded = std::collections::HashSet::from([key_id]);
        let collapse = handle_input(
            &make_key(KeyCode::Char('h')),
            &entries,
            &mut state,
            false,
            &no_collapsed(),
            &expanded,
            &mut mode,
        );
        assert_eq!(
            collapse,
            ShortcutsHelpOutcome::ToggleExpand(key_id),
            "vim h must collapse the expanded paste pseudo-row"
        );
        assert!(state.query().is_empty(), "vim h must not enter search text");
    }

    /// `handle_modal_key` (chrome + picker pipeline) maps the hint-row expand to
    /// `ModalKeyOutcome::ToggleExpand` so dashboards get identical semantics.
    #[test]
    fn handle_modal_key_maps_toggle_expand() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);
        let mut state = build_initial_picker_state(&entries);
        state.selected = 1;
        let mut window = crate::views::modal_window::ModalWindowState::default();
        let key = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        let mut mode = ShortcutsHelpMode::Browse;
        let out = handle_modal_key(
            &key,
            &entries,
            &mut state,
            &mut window,
            false,
            &no_collapsed(),
            &no_expanded(),
            &mut mode,
            false,
        );
        assert!(
            matches!(out, ModalKeyOutcome::ToggleExpand(_)),
            "Right on a hint row must map to ModalKeyOutcome::ToggleExpand, got {out:?}"
        );
    }

    /// `handle_modal_key` forwards `expanded_ids` through the chrome pipeline so
    /// the dashboard host's Left-collapse works. A *populated* expanded set is
    /// required to exercise the wiring — the `→` test above passes regardless of
    /// the set, so it can't catch a dropped `expanded_ids` forward.
    #[test]
    fn handle_modal_key_left_collapses_expanded_hint() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let registry = ActionRegistry::defaults();
        let entries = build_entries(&all_contexts(), &registry, true);
        let mut state = build_initial_picker_state(&entries);
        state.selected = 1;
        let key_id = expand_key(&entries[1]).expect("row 1 is expandable");
        let expanded = std::collections::HashSet::from([key_id]);
        let mut window = crate::views::modal_window::ModalWindowState::default();
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        let mut mode = ShortcutsHelpMode::Browse;
        let out = handle_modal_key(
            &key,
            &entries,
            &mut state,
            &mut window,
            false,
            &no_collapsed(),
            &expanded,
            &mut mode,
            false,
        );
        assert_eq!(
            out,
            ModalKeyOutcome::ToggleExpand(key_id),
            "Left on an expanded hint must map to ModalKeyOutcome::ToggleExpand (collapse), got {out:?}"
        );
    }

    /// A row's `long_help` renders as an inline line only while its id is
    /// expanded, and is absent otherwise.
    #[test]
    fn render_modal_shows_long_help_only_when_expanded() {
        use crate::actions::ActionId;
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        // long_help differs from label/description so the expanded line is detectable.
        let mut item = HintItem::new(key!('q', CONTROL), "quit");
        item.description = Some("Quit the app".into());
        let entries = vec![
            ShortcutsHelpEntry::SectionHeader {
                label: "Essentials",
                category_idx: 0,
                entry_count: 1,
            },
            ShortcutsHelpEntry::Hint {
                item,
                dimmed: false,
                action_id: Some(ActionId::Quit),
                long_help: Some("Zqxhelpline"),
            },
        ];
        let theme = crate::theme::Theme::current();
        let area = Rect::new(0, 0, 100, 40);
        let render = |expanded: &std::collections::HashSet<ExpandKey>| -> String {
            let mut state = build_initial_picker_state(&entries);
            let mut window = crate::views::modal_window::ModalWindowState::default();
            let mut buf = Buffer::empty(area);
            render_modal(
                &mut buf,
                area,
                &entries,
                &mut state,
                &mut window,
                false,
                &no_collapsed(),
                expanded,
                &ShortcutsHelpMode::Browse,
                &theme,
                false,
            );
            let mut out = String::new();
            for y in area.y..area.y + area.height {
                for x in area.x..area.x + area.width {
                    if let Some(cell) = buf.cell((x, y)) {
                        out.push_str(cell.symbol());
                    }
                }
            }
            out
        };
        let mut expanded = std::collections::HashSet::new();
        expanded.insert(ExpandKey::Action(ActionId::Quit));
        assert!(
            render(&expanded).contains("Zqxhelpline"),
            "expanded hint must render its long_help line"
        );
        assert!(
            !render(&std::collections::HashSet::new()).contains("Zqxhelpline"),
            "collapsed hint must not render the long_help line"
        );
    }

    /// The collapsible (inline expand) view collapses newlines to spaces so the
    /// help renders as one wrap-flowed block with no hard breaks — unlike the
    /// detail page (Enter), which spaces paragraphs out with blank lines.
    #[test]
    fn cheatsheet_rows_inline_help_joins_newlines_with_spaces() {
        use crate::actions::ActionId;
        let mut item = HintItem::new(key!('q', CONTROL), "quit");
        item.description = Some("Quit the app".into());
        let entries = vec![
            ShortcutsHelpEntry::SectionHeader {
                label: "Essentials",
                category_idx: 0,
                entry_count: 1,
            },
            ShortcutsHelpEntry::Hint {
                item,
                dimmed: false,
                action_id: Some(ActionId::Quit),
                long_help: Some("First line.\nSecond line."),
            },
        ];
        let rows = CheatsheetRows::build(&entries, "", false, &no_collapsed());
        let help = rows.help_refs();
        assert_eq!(
            help[1], "First line. Second line.",
            "inline help must join newlines with spaces"
        );
        assert!(
            !help[1].contains('\n'),
            "collapsible help must not contain newlines, got {:?}",
            help[1]
        );
    }

    /// A hint with neither long_help nor description has empty inline help, so an
    /// expanded row must render no description line (no stray blank inline row).
    #[test]
    fn inline_expand_with_no_help_renders_no_description_line() {
        use crate::actions::ActionId;
        use crate::views::picker::PickerEntry;
        // HintItem::new leaves `description` unset; no long_help either.
        let item = HintItem::new(key!('q', CONTROL), "quit");
        let entries = vec![
            ShortcutsHelpEntry::SectionHeader {
                label: "Essentials",
                category_idx: 0,
                entry_count: 1,
            },
            ShortcutsHelpEntry::Hint {
                item,
                dimmed: false,
                action_id: Some(ActionId::Quit),
                long_help: None,
            },
        ];
        let rows = CheatsheetRows::build(&entries, "", false, &no_collapsed());
        let help = rows.help_refs();
        assert_eq!(
            help[1], "",
            "a hint with no help source has empty inline help"
        );
        let mut state = build_initial_picker_state(&entries);
        state.selected = 1;
        let expanded = std::collections::HashSet::from([ExpandKey::Action(ActionId::Quit)]);
        let picker_entries = rows.picker_entries(&state, &expanded, &help);
        let PickerEntry::Row(row) = &picker_entries[1] else {
            panic!("row 1 must be a hint row");
        };
        assert!(row.expanded, "row is expanded");
        assert!(
            row.description_lines.is_empty(),
            "empty help must render no description line even when expanded"
        );
    }
}
