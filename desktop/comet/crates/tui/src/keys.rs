//! Key → [`Action`] mapping, kept pure so the whole keymap is unit-testable
//! without a terminal.
//!
//! The binding style is "modeless with a text field": navigation keys are
//! single letters everywhere *except* the composer, where they are literal
//! characters and navigation moves to modifiers. That's the same split every
//! chat TUI converges on, and it's why [`map`] takes the focus — the same
//! `KeyEvent` means different things depending on where the caret is.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Which pane has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Transcript,
    Composer,
}

impl Focus {
    /// Tab order.
    pub fn next(self) -> Self {
        match self {
            Focus::Sidebar => Focus::Transcript,
            Focus::Transcript => Focus::Composer,
            Focus::Composer => Focus::Sidebar,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Focus::Sidebar => Focus::Composer,
            Focus::Transcript => Focus::Sidebar,
            Focus::Composer => Focus::Transcript,
        }
    }
}

/// Everything a key press can ask the app to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Leave the viewport. The engine keeps running.
    Quit,
    ToggleHelp,
    CloseOverlay,
    /// Open the context menu for the row under the cursor.
    ContextMenu,
    /// Open the model picker for the open session.
    PickModel,
    /// Open the branch picker (drafts only).
    PickRef,
    /// Open the "run in" picker (drafts only).
    PickCheckout,
    /// Open the effort picker for the active model.
    PickReasoning,
    /// Move the selection inside a floating panel.
    OverlayStep(isize),
    /// Activate the floating panel's selection.
    OverlayConfirm,
    /// A keystroke aimed at a floating panel's text input.
    OverlayEdit(Edit),
    Focus(Focus),
    FocusNext,
    FocusPrevious,
    ToggleSidebar,
    /// Force an immediate reconnect attempt instead of waiting out the backoff.
    Reconnect,

    // Sidebar
    ListUp,
    ListDown,
    ListTop,
    ListBottom,
    /// Enter on a sidebar row.
    Open,
    NewSession,
    /// Archive or unarchive the session under the cursor.
    ToggleArchive,
    /// Show or hide archived sessions. Without this, an archived row is
    /// invisible and therefore impossible to unarchive.
    ToggleShowArchived,

    /// Select the tab `n` places away (wrapping).
    CycleTab(isize),
    /// Select the 1-based tab.
    SelectTab(usize),

    // Transcript
    ScrollUp(u16),
    ScrollDown(u16),
    PageUp,
    PageDown,
    ScrollTop,
    ScrollBottom,

    // Composer
    Send,
    Interrupt,
    Edit(Edit),
}

/// A composer mutation. Separated from [`Action`] so the app can apply the whole
/// family with one `match` on the buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    Insert(char),
    Paste(String),
    Newline,
    Backspace,
    Delete,
    DeleteWordBack,
    DeleteToLineStart,
    DeleteToLineEnd,
    Left,
    Right,
    WordLeft,
    WordRight,
    Up,
    Down,
    Home,
    End,
    BufferStart,
    BufferEnd,
}

/// Map a key press. `overlay` is true while the help overlay is up, which
/// swallows everything but dismissal.
///
/// `composer_empty` decides Ctrl-D: on an empty prompt it means "I'm done"
/// (quit, as in a shell); with text it would be a destructive surprise, so it
/// is ignored.
pub fn map(focus: Focus, overlay: bool, composer_empty: bool, key: KeyEvent) -> Option<Action> {
    // Terminals with the kitty keyboard protocol report releases too; acting on
    // both would double every keystroke.
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if overlay {
        return map_overlay(ctrl, alt, key);
    }

    // ---- bindings that mean the same thing in every pane ----
    match key.code {
        KeyCode::Char('c') if ctrl => return Some(Action::Quit),
        KeyCode::Char('x') if ctrl => return Some(Action::Interrupt),
        KeyCode::Char('b') if ctrl => return Some(Action::ToggleSidebar),
        // Composer chips: Alt-chords so they work mid-prompt, which is exactly
        // when you want them — the choices belong to the message you are about
        // to send.
        KeyCode::Char('m') if alt => return Some(Action::PickModel),
        KeyCode::Char('r') if alt => return Some(Action::PickRef),
        KeyCode::Char('w') if alt => return Some(Action::PickCheckout),
        KeyCode::Char('e') if alt => return Some(Action::PickReasoning),
        KeyCode::Tab => return Some(Action::FocusNext),
        KeyCode::BackTab => return Some(Action::FocusPrevious),
        // Paging the transcript from anywhere, including mid-prompt: reading
        // back while composing is the common case.
        KeyCode::PageUp => return Some(Action::PageUp),
        KeyCode::PageDown => return Some(Action::PageDown),
        // Tab strip. Alt-chords because they are unbound in every pane,
        // including the composer, where a bare digit is text.
        KeyCode::Left if alt => return Some(Action::CycleTab(-1)),
        KeyCode::Right if alt => return Some(Action::CycleTab(1)),
        KeyCode::Char(ch) if alt && ch.is_ascii_digit() && ch != '0' => {
            return Some(Action::SelectTab(ch as usize - '0' as usize));
        }
        _ => {}
    }

    match focus {
        Focus::Composer => map_composer(ctrl, alt, composer_empty, key),
        Focus::Sidebar => map_sidebar(ctrl, shift, key),
        Focus::Transcript => map_transcript(ctrl, shift, key),
    }
}

/// Keys while a floating panel is up. It owns the keyboard: navigation moves
/// inside it, Esc dismisses, and printable characters go to its text input if
/// it has one — otherwise they are swallowed, so a stray letter can't act on
/// the shell behind the panel.
fn map_overlay(ctrl: bool, alt: bool, key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('c') if ctrl => Some(Action::Quit),
        KeyCode::Esc => Some(Action::CloseOverlay),
        KeyCode::Enter => Some(Action::OverlayConfirm),
        KeyCode::Up => Some(Action::OverlayStep(-1)),
        KeyCode::Down => Some(Action::OverlayStep(1)),
        KeyCode::Tab => Some(Action::OverlayStep(1)),
        KeyCode::BackTab => Some(Action::OverlayStep(-1)),
        KeyCode::Backspace => Some(Action::OverlayEdit(Edit::Backspace)),
        KeyCode::Delete => Some(Action::OverlayEdit(Edit::Delete)),
        KeyCode::Left => Some(Action::OverlayEdit(Edit::Left)),
        KeyCode::Right => Some(Action::OverlayEdit(Edit::Right)),
        KeyCode::Home => Some(Action::OverlayEdit(Edit::Home)),
        KeyCode::End => Some(Action::OverlayEdit(Edit::End)),
        KeyCode::Char('w') if ctrl => Some(Action::OverlayEdit(Edit::DeleteWordBack)),
        KeyCode::Char('u') if ctrl => Some(Action::OverlayEdit(Edit::DeleteToLineStart)),
        KeyCode::Char(ch) if !ctrl && !alt => Some(Action::OverlayEdit(Edit::Insert(ch))),
        _ => None,
    }
}

fn map_composer(ctrl: bool, alt: bool, empty: bool, key: KeyEvent) -> Option<Action> {
    match key.code {
        // Enter sends; Alt-Enter and Ctrl-J insert a newline. Terminals without
        // the kitty protocol cannot report Shift-Enter at all, which is why the
        // newline binding is a modifier that always survives.
        KeyCode::Enter if alt || ctrl => Some(Action::Edit(Edit::Newline)),
        KeyCode::Enter => Some(Action::Send),
        KeyCode::Char('j') if ctrl => Some(Action::Edit(Edit::Newline)),
        KeyCode::Char('d') if ctrl && empty => Some(Action::Quit),
        KeyCode::Char('d') if ctrl => None,

        KeyCode::Esc => Some(Action::Focus(Focus::Transcript)),

        KeyCode::Char('w') if ctrl => Some(Action::Edit(Edit::DeleteWordBack)),
        KeyCode::Char('u') if ctrl => Some(Action::Edit(Edit::DeleteToLineStart)),
        KeyCode::Char('k') if ctrl => Some(Action::Edit(Edit::DeleteToLineEnd)),
        KeyCode::Char('a') if ctrl => Some(Action::Edit(Edit::Home)),
        KeyCode::Char('e') if ctrl => Some(Action::Edit(Edit::End)),
        KeyCode::Char('b') if alt => Some(Action::Edit(Edit::WordLeft)),
        KeyCode::Char('f') if alt => Some(Action::Edit(Edit::WordRight)),
        KeyCode::Backspace if alt || ctrl => Some(Action::Edit(Edit::DeleteWordBack)),

        KeyCode::Backspace => Some(Action::Edit(Edit::Backspace)),
        KeyCode::Delete => Some(Action::Edit(Edit::Delete)),
        KeyCode::Left if ctrl || alt => Some(Action::Edit(Edit::WordLeft)),
        KeyCode::Right if ctrl || alt => Some(Action::Edit(Edit::WordRight)),
        KeyCode::Left => Some(Action::Edit(Edit::Left)),
        KeyCode::Right => Some(Action::Edit(Edit::Right)),
        KeyCode::Up => Some(Action::Edit(Edit::Up)),
        KeyCode::Down => Some(Action::Edit(Edit::Down)),
        KeyCode::Home => Some(Action::Edit(Edit::Home)),
        KeyCode::End => Some(Action::Edit(Edit::End)),

        // Printable input. Control/alt combinations we didn't bind are dropped
        // rather than typed, so an unrecognized chord never injects a literal.
        KeyCode::Char(ch) if !ctrl && !alt => Some(Action::Edit(Edit::Insert(ch))),
        _ => None,
    }
}

fn map_sidebar(ctrl: bool, shift: bool, key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('?') => Some(Action::ToggleHelp),
        KeyCode::Char('r') if !ctrl => Some(Action::Reconnect),
        KeyCode::Char('n') => Some(Action::NewSession),
        KeyCode::Char('e') => Some(Action::ToggleArchive),
        KeyCode::Char('A') => Some(Action::ToggleShowArchived),
        KeyCode::Char('m') => Some(Action::ContextMenu),
        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => Some(Action::Open),
        KeyCode::Char('i') => Some(Action::Focus(Focus::Composer)),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::ListDown),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::ListUp),
        KeyCode::Char('g') if !shift => Some(Action::ListTop),
        KeyCode::Char('G') => Some(Action::ListBottom),
        KeyCode::Home => Some(Action::ListTop),
        KeyCode::End => Some(Action::ListBottom),
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => Some(Action::Focus(Focus::Transcript)),
        _ => None,
    }
}

fn map_transcript(ctrl: bool, shift: bool, key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('?') => Some(Action::ToggleHelp),
        KeyCode::Char('r') if !ctrl => Some(Action::Reconnect),
        KeyCode::Char('n') => Some(Action::NewSession),
        KeyCode::Char('A') => Some(Action::ToggleShowArchived),
        KeyCode::Enter | KeyCode::Char('i') => Some(Action::Focus(Focus::Composer)),
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => Some(Action::Focus(Focus::Sidebar)),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::ScrollDown(1)),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::ScrollUp(1)),
        KeyCode::Char('d') if ctrl => Some(Action::PageDown),
        KeyCode::Char('u') if ctrl => Some(Action::PageUp),
        KeyCode::Char('g') if !shift => Some(Action::ScrollTop),
        KeyCode::Char('G') => Some(Action::ScrollBottom),
        KeyCode::Home => Some(Action::ScrollTop),
        KeyCode::End => Some(Action::ScrollBottom),
        _ => None,
    }
}

/// The help overlay's contents — one place, so the overlay can never drift from
/// [`map`].
pub const HELP: &[(&str, &str)] = &[
    ("Tab / Shift-Tab", "cycle panes"),
    ("Ctrl-B", "show/hide the sidebar"),
    ("j / k, ↓ / ↑", "move (list) or scroll (transcript)"),
    ("g / G", "jump to top / bottom"),
    ("Ctrl-U / Ctrl-D", "half page up / down"),
    ("PageUp / PageDown", "page the transcript from anywhere"),
    ("Enter", "open a session, or send the prompt"),
    ("i", "jump to the prompt"),
    ("Esc", "leave the prompt / step left"),
    ("Alt-Enter, Ctrl-J", "newline in the prompt"),
    (
        "Ctrl-W / Ctrl-U / Ctrl-K",
        "delete word / to line start / to line end",
    ),
    ("right-click, m", "context menu (rename, archive, delete)"),
    ("Alt-M", "switch model"),
    ("Alt-R", "branch for a new session"),
    ("Alt-W", "where a new session runs"),
    ("Alt-E", "reasoning effort"),
    ("Alt-← / Alt-→", "previous / next session tab"),
    ("Alt-1 … Alt-9", "jump to a session tab"),
    ("n", "new session in the selected space"),
    ("e", "archive or unarchive the selected session"),
    ("A", "show or hide archived sessions"),
    ("Ctrl-X", "interrupt the running agent"),
    ("r", "reconnect now"),
    ("?", "this help"),
    ("q, Ctrl-C", "detach — the engine keeps running"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    #[test]
    fn key_releases_are_ignored() {
        // Kitty-protocol terminals report Press and Release; acting on both
        // would type every character twice.
        let mut release = press(KeyCode::Char('a'));
        release.kind = KeyEventKind::Release;
        assert_eq!(map(Focus::Composer, false, true, release), None);
        let mut repeat = press(KeyCode::Char('a'));
        repeat.kind = KeyEventKind::Repeat;
        assert_eq!(
            map(Focus::Composer, false, true, repeat),
            Some(Action::Edit(Edit::Insert('a')))
        );
    }

    #[test]
    fn enter_sends_but_modified_enter_inserts_a_newline() {
        assert_eq!(
            map(Focus::Composer, false, false, press(KeyCode::Enter)),
            Some(Action::Send)
        );
        assert_eq!(
            map(Focus::Composer, false, false, alt(KeyCode::Enter)),
            Some(Action::Edit(Edit::Newline))
        );
        assert_eq!(
            map(Focus::Composer, false, false, ctrl('j')),
            Some(Action::Edit(Edit::Newline))
        );
    }

    #[test]
    fn letters_are_text_in_the_composer_and_commands_elsewhere() {
        // 'q' must never quit while typing a prompt.
        assert_eq!(
            map(Focus::Composer, false, false, press(KeyCode::Char('q'))),
            Some(Action::Edit(Edit::Insert('q')))
        );
        assert_eq!(
            map(Focus::Transcript, false, true, press(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
        assert_eq!(
            map(Focus::Sidebar, false, true, press(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
        // 'j' likewise.
        assert_eq!(
            map(Focus::Composer, false, false, press(KeyCode::Char('j'))),
            Some(Action::Edit(Edit::Insert('j')))
        );
        assert_eq!(
            map(Focus::Transcript, false, true, press(KeyCode::Char('j'))),
            Some(Action::ScrollDown(1))
        );
    }

    #[test]
    fn unbound_chords_never_type_a_literal() {
        // Ctrl-P isn't bound; it must be dropped, not inserted as 'p'.
        assert_eq!(map(Focus::Composer, false, false, ctrl('p')), None);
        assert_eq!(
            map(Focus::Composer, false, false, alt(KeyCode::Char('z'))),
            None
        );
    }

    #[test]
    fn ctrl_d_quits_only_on_an_empty_prompt() {
        assert_eq!(
            map(Focus::Composer, false, true, ctrl('d')),
            Some(Action::Quit)
        );
        assert_eq!(
            map(Focus::Composer, false, false, ctrl('d')),
            None,
            "Ctrl-D must not discard a half-written prompt"
        );
        // In the transcript it's a half page down instead.
        assert_eq!(
            map(Focus::Transcript, false, true, ctrl('d')),
            Some(Action::PageDown)
        );
    }

    #[test]
    fn global_bindings_work_from_every_pane() {
        for focus in [Focus::Sidebar, Focus::Transcript, Focus::Composer] {
            assert_eq!(map(focus, false, false, ctrl('c')), Some(Action::Quit));
            assert_eq!(map(focus, false, false, ctrl('x')), Some(Action::Interrupt));
            assert_eq!(
                map(focus, false, false, ctrl('b')),
                Some(Action::ToggleSidebar)
            );
            assert_eq!(
                map(focus, false, false, press(KeyCode::Tab)),
                Some(Action::FocusNext)
            );
            assert_eq!(
                map(focus, false, false, press(KeyCode::PageUp)),
                Some(Action::PageUp)
            );
        }
    }

    #[test]
    fn a_floating_panel_owns_the_keyboard() {
        // Esc dismisses, Enter activates, arrows move inside it.
        assert_eq!(
            map(Focus::Composer, true, false, press(KeyCode::Esc)),
            Some(Action::CloseOverlay)
        );
        assert_eq!(
            map(Focus::Composer, true, false, press(KeyCode::Enter)),
            Some(Action::OverlayConfirm)
        );
        assert_eq!(
            map(Focus::Composer, true, false, press(KeyCode::Down)),
            Some(Action::OverlayStep(1))
        );
        // Printable characters go to the panel's input, never to the shell
        // behind it — `q` must not quit while a rename field is open.
        assert_eq!(
            map(Focus::Transcript, true, true, press(KeyCode::Char('q'))),
            Some(Action::OverlayEdit(Edit::Insert('q')))
        );
        // Ctrl-C still gets you out of the app, not just the panel.
        assert_eq!(
            map(Focus::Composer, true, false, ctrl('c')),
            Some(Action::Quit)
        );
    }

    #[test]
    fn the_context_menu_and_model_picker_are_reachable() {
        // `m` outside the composer; Alt-M everywhere, including mid-prompt.
        assert_eq!(
            map(Focus::Sidebar, false, true, press(KeyCode::Char('m'))),
            Some(Action::ContextMenu)
        );
        assert_eq!(
            map(Focus::Composer, false, false, press(KeyCode::Char('m'))),
            Some(Action::Edit(Edit::Insert('m'))),
            "in the prompt it is just a letter"
        );
        for focus in [Focus::Sidebar, Focus::Transcript, Focus::Composer] {
            assert_eq!(
                map(focus, false, false, alt(KeyCode::Char('m'))),
                Some(Action::PickModel)
            );
        }
    }

    #[test]
    fn shifted_g_jumps_to_the_bottom() {
        // Terminals deliver Shift-g as an uppercase char, sometimes with the
        // SHIFT modifier set and sometimes without — both must work.
        let bare = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE);
        let shifted = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(
            map(Focus::Transcript, false, true, bare),
            Some(Action::ScrollBottom)
        );
        assert_eq!(
            map(Focus::Transcript, false, true, shifted),
            Some(Action::ScrollBottom)
        );
        assert_eq!(
            map(Focus::Transcript, false, true, press(KeyCode::Char('g'))),
            Some(Action::ScrollTop)
        );
    }

    #[test]
    fn focus_cycles_both_ways() {
        assert_eq!(Focus::Sidebar.next().next().next(), Focus::Sidebar);
        assert_eq!(Focus::Sidebar.previous(), Focus::Composer);
        for focus in [Focus::Sidebar, Focus::Transcript, Focus::Composer] {
            assert_eq!(focus.next().previous(), focus);
        }
    }

    #[test]
    fn escape_walks_left_out_of_the_prompt() {
        assert_eq!(
            map(Focus::Composer, false, false, press(KeyCode::Esc)),
            Some(Action::Focus(Focus::Transcript))
        );
        assert_eq!(
            map(Focus::Transcript, false, true, press(KeyCode::Esc)),
            Some(Action::Focus(Focus::Sidebar))
        );
    }
}
