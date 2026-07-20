//! Slash command system -- prompt-centric inline completion and execution.
//!
//! Pager's synchronous dispatch model. Key components:
//!
//! - [`SlashController`] -- derives completion state from prompt text + cursor.
//! - [`SlashState`] / [`SlashSnapshot`] -- snapshot holder for rendering.
//! - [`parse_invocation()`] -- extracts command token + args from input.
//! - [`is_command_complete()`] -- two-bit completeness model.
//! - [`CommandRegistry`] -- maps names/aliases to command implementations.

pub mod acp_command;
pub mod command;
pub mod commands;
pub mod matcher;
pub mod mru;
pub mod registry;

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ops::Range,
};

use crate::acp::model_state::ModelState;

use matcher::FuzzyMatcher;
use registry::{CommandRegistry, CommandSource, CommandTrigger};

pub use command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

/// Maximum number of visible rows in the dropdown (scroll beyond this).
pub const MAX_VISIBLE_SUGGESTIONS: usize = 6;

// ---------------------------------------------------------------------------
// SuggestionRow
// ---------------------------------------------------------------------------

/// A single row in the slash suggestion dropdown.
#[derive(Debug, Clone)]
pub struct SuggestionRow {
    /// Display text (e.g., "/model" or "Grok 4 Fast").
    pub display: String,
    /// Description text (e.g., "Switch the active model").
    pub description: String,
    /// Text to insert into the prompt on acceptance.
    pub insert_text: String,
    /// Character positions for fuzzy match highlighting.
    pub indices: Vec<u32>,
}

impl SuggestionRow {
    fn from_command(trigger: &CommandTrigger) -> Self {
        let mut insert_text = trigger.display.clone();
        if trigger.takes_args {
            insert_text.push(' ');
        }
        Self {
            display: trigger.display.clone(),
            description: trigger.description.clone(),
            insert_text,
            indices: Vec::new(),
        }
    }

    fn from_arg(item: &ArgItem) -> Self {
        Self {
            display: item.display.clone(),
            description: item.description.clone(),
            insert_text: item.insert_text.clone(),
            indices: Vec::new(),
        }
    }

    /// Bare command name from a command-row `display` (strips leading `/`).
    pub(crate) fn command_name(&self) -> &str {
        self.display.strip_prefix('/').unwrap_or(&self.display)
    }
}

/// Prefix match aligned with nucleo `CaseMatching::Smart`: all-lowercase query
/// is case-insensitive; any uppercase in the query requires exact prefix.
fn command_prefix_matches_smart(full_name: &str, query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    let case_sensitive = query.chars().any(|c| c.is_uppercase());
    if case_sensitive {
        return full_name.starts_with(query);
    }
    let mut full_chars = full_name.chars();
    for qc in query.chars() {
        match full_chars.next() {
            Some(fc) if chars_eq_ignore_case(fc, qc) => {}
            _ => return false,
        }
    }
    true
}

fn chars_eq_ignore_case(a: char, b: char) -> bool {
    a == b || a.eq_ignore_ascii_case(&b)
}

/// Ghost suffix from the selected dropdown row (same ranker as Tab).
fn inline_ghost_from_selected_command(
    query: &str,
    token_range: Range<usize>,
    row: &SuggestionRow,
) -> Option<InlineGhost> {
    let full_name = row.command_name();
    if query.is_empty() || !command_prefix_matches_smart(full_name, query) {
        return None;
    }
    let mut rest = full_name.chars();
    for _ in query.chars() {
        rest.next()?;
    }
    let text: String = rest.collect();
    if text.is_empty() {
        return None;
    }
    Some(InlineGhost {
        text,
        token_range,
        full_name: full_name.to_string(),
    })
}

fn sync_inline_ghost_to_selection(inner: &mut SlashSnapshot) {
    if !inner.cursor_in_command || inner.command_recognized {
        return;
    }
    let Some(range) = inner.command_range.clone() else {
        return;
    };
    inner.inline_ghost = inner
        .selection()
        .and_then(|row| inline_ghost_from_selected_command(&inner.query, range, row));
}

// ---------------------------------------------------------------------------
// SlashSnapshot / SlashState
// ---------------------------------------------------------------------------

/// Immutable snapshot of the slash completion state.
///
/// Produced by `SlashController::refresh()`, consumed by the dropdown
/// renderer. Cloned on read (cheap -- small vecs).
#[derive(Debug, Clone, Default)]
pub struct SlashSnapshot {
    /// Whether the input looks like a slash command (starts with `/`).
    pub active: bool,
    /// Whether the suggestion dropdown should be visible.
    pub open: bool,
    /// Current query text (command name part, without `/`).
    pub query: String,
    /// Matching suggestion rows.
    pub matches: Vec<SuggestionRow>,
    /// Currently selected index in `matches`.
    pub selected: usize,
    /// Byte range of the command part in the input (e.g., `0..6` for `/model`).
    pub command_range: Option<Range<usize>>,
    /// Byte range of the arguments part in the input.
    pub args_range: Option<Range<usize>>,
    /// Whether the cursor is in the command part (vs args part).
    pub cursor_in_command: bool,
    /// Placeholder text for args (e.g., "[context]").
    pub args_placeholder: Option<String>,
    /// Whether the args query is empty (for placeholder display).
    pub args_query_is_empty: bool,
    /// Whether the resolved command is a skill (for accent color theming).
    pub is_skill: bool,
    /// Whether the command token resolves to a known command in the registry.
    pub command_recognized: bool,
    /// Mid-text inline completion ghost text (for `/` tokens not at position 0).
    pub inline_ghost: Option<InlineGhost>,
    /// Byte ranges of recognized mid-text `/command` tokens (for teal highlighting).
    pub recognized_tokens: Vec<Range<usize>>,
}

/// Ghost text state for a mid-text `/command` token under the cursor.
#[derive(Debug, Clone)]
pub struct InlineGhost {
    /// The ghost suffix to render (e.g., "it" when the user typed "/comm" and best match is "commit").
    pub text: String,
    /// Byte range of the partial token being completed (including `/`).
    pub token_range: Range<usize>,
    /// The full command name to insert on Tab accept (without `/`).
    pub full_name: String,
}

impl SlashSnapshot {
    /// The currently selected suggestion row, if any.
    pub fn selection(&self) -> Option<&SuggestionRow> {
        if self.matches.is_empty() {
            None
        } else {
            let idx = self.selected.min(self.matches.len() - 1);
            self.matches.get(idx)
        }
    }
}

/// Mutable holder for [`SlashSnapshot`].
///
/// Uses `RefCell` for interior mutability -- the controller writes it,
/// the renderer reads it. Not a trait, just a state container.
#[derive(Debug, Default)]
pub struct SlashState {
    inner: RefCell<SlashSnapshot>,
}

impl SlashState {
    /// Clone the current snapshot.
    pub fn snapshot(&self) -> SlashSnapshot {
        self.inner.borrow().clone()
    }

    /// Replace the entire snapshot.
    pub fn replace(&self, snapshot: SlashSnapshot) {
        *self.inner.borrow_mut() = snapshot;
    }

    /// Mutate the snapshot in place.
    pub fn update(&self, f: impl FnOnce(&mut SlashSnapshot)) {
        let mut guard = self.inner.borrow_mut();
        f(&mut guard);
    }

    /// Close the dropdown.
    pub fn close(&self) {
        self.update(|inner| {
            inner.open = false;
            inner.matches.clear();
            inner.args_range = None;
            inner.args_placeholder = None;
            inner.args_query_is_empty = false;
        });
    }
}

// ---------------------------------------------------------------------------
// SlashController
// ---------------------------------------------------------------------------

/// Derives slash completion state from prompt text + cursor.
///
/// Owns a `CommandRegistry` (mutable for ACP sync) and a `FuzzyMatcher`.
/// The prompt widget calls `refresh()` on every text change.
pub struct SlashController {
    registry: CommandRegistry,
    matcher: FuzzyMatcher,
    cwd: std::path::PathBuf,
    /// When `true`, commands whose [`SlashCommand::session_scoped`] is
    /// `true` are suppressed from completion. Set on session-less
    /// surfaces — the agent dashboard's dispatch input — so the dropdown
    /// only offers pager-global commands. Defaults to `false`.
    hide_session_scoped: bool,
    /// Offer `/announcements` when session announcements (critical or promo) exist.
    has_session_announcements: bool,
    /// Effective render mode of this process (immutable after startup — it only
    /// changes via a full `/minimal`-`/fullscreen` re-exec). Injected via
    /// [`Self::set_screen_mode`] wherever prompts are created; gates the
    /// screen-mode-switcher commands' visibility through [`AppCtx`]. Defaults
    /// to `Fullscreen` (the process default) for tests and unwired surfaces.
    screen_mode: crate::app::ScreenMode,
    /// MRU/recency store. Owned by `AppView` in production and injected via
    /// [`Self::set_mru`] so agent prompts and the dashboard share one store;
    /// defaults to an isolated in-memory store (no disk I/O) for tests and any
    /// surface that has not been wired up.
    mru: std::rc::Rc<std::cell::RefCell<mru::SlashMru>>,
}

impl SlashController {
    /// Create a new controller with the given registry and working directory.
    ///
    /// The MRU store defaults to an isolated, in-memory (non-persisting) store.
    /// Production injects the shared store via [`Self::set_mru`].
    pub fn new(registry: CommandRegistry, cwd: std::path::PathBuf) -> Self {
        let mru = std::rc::Rc::new(std::cell::RefCell::new(mru::SlashMru::new_in_memory()));
        Self::with_mru(registry, cwd, mru)
    }

    /// Controller with an explicit MRU store (tests/production injection).
    pub fn with_mru(
        registry: CommandRegistry,
        cwd: std::path::PathBuf,
        mru: std::rc::Rc<std::cell::RefCell<mru::SlashMru>>,
    ) -> Self {
        Self {
            registry,
            matcher: FuzzyMatcher::new(),
            cwd,
            hide_session_scoped: false,
            has_session_announcements: false,
            screen_mode: crate::app::ScreenMode::Fullscreen,
            mru,
        }
    }

    /// Replace the MRU store with a shared one. Used by `AppView` to inject the
    /// process-wide store into agent prompts and the dashboard dispatch input.
    pub fn set_mru(&mut self, mru: std::rc::Rc<std::cell::RefCell<mru::SlashMru>>) {
        self.mru = mru;
    }

    /// Gate `/announcements` on presence of session announcements (critical or promo).
    pub fn set_has_session_announcements(&mut self, has: bool) {
        self.has_session_announcements = has;
    }

    pub fn has_session_announcements(&self) -> bool {
        self.has_session_announcements
    }

    /// Record the process's effective screen mode (see the field doc).
    pub(crate) fn set_screen_mode(&mut self, mode: crate::app::ScreenMode) {
        self.screen_mode = mode;
    }

    pub(crate) fn screen_mode(&self) -> crate::app::ScreenMode {
        self.screen_mode
    }

    pub(crate) fn app_ctx<'a>(&'a self, models: &'a ModelState) -> AppCtx<'a> {
        AppCtx {
            models,
            cwd: &self.cwd,
            has_session_announcements: self.has_session_announcements,
            screen_mode: self.screen_mode,
        }
    }

    /// Last-used timestamp for a command (0 = never). Test diagnostics.
    #[cfg(test)]
    fn mru_last_used(&mut self, prefix: &str, command_name: &str) -> u64 {
        self.mru.borrow_mut().last_used(prefix, command_name)
    }

    /// Record accept/submit for MRU (canonical command only; typed prefix
    /// ignored). Runs on the UI thread; the resulting snapshot (if any) is
    /// handed to the off-thread serialized writer so the UI never blocks on
    /// disk and writes can't reorder.
    pub fn record_command_use(&mut self, prefix: &str, command_name: &str) {
        let key = command_name.trim().trim_start_matches('/');
        if key.is_empty() {
            return;
        }
        // Dispatch-tier lookup (menu-only hide; see
        // `CommandRegistry::get_for_dispatch`) so menu-hidden submissions
        // canonicalize (alias → name) for MRU like any other.
        let canonical = self
            .registry
            .get_for_dispatch(key)
            .map(|cmd| cmd.name().to_string())
            .unwrap_or_else(|| key.to_string());
        let snapshot = {
            let mut m = self.mru.borrow_mut();
            m.touch(prefix, &canonical);
            m.take_persist_snapshot()
        };
        if let Some(snapshot) = snapshot
            && !mru::persist_async(snapshot)
        {
            // No write could be attempted (writer unavailable and the sync
            // fallback failed): keep the changes dirty so the next record
            // retries instead of silently dropping them.
            self.mru.borrow_mut().mark_dirty();
        }
    }

    /// Create a controller pre-loaded with pager builtin commands.
    pub fn with_builtins(cwd: std::path::PathBuf) -> Self {
        Self::new(CommandRegistry::new(commands::builtin_commands()), cwd)
    }

    /// Mutable access to the registry (for ACP sync).
    pub fn registry_mut(&mut self) -> &mut CommandRegistry {
        &mut self.registry
    }

    /// Immutable access to the registry (for dispatch lookup).
    pub fn registry(&self) -> &CommandRegistry {
        &self.registry
    }

    /// Gate `/auto` on the auto permission-mode feature. When unavailable,
    /// `/auto` is hard-hidden. `/always-approve` is always offered; both
    /// commands are true toggles (re-running the active mode turns it off).
    pub fn set_auto_mode_available(&mut self, available: bool) {
        self.registry.set_auto_mode_available(available);
    }

    /// Suppress (or restore) session-scoped commands in completion.
    ///
    /// Called once on session-less surfaces (the agent dashboard's
    /// dispatch input) so commands that act on a single session never
    /// surface in the dropdown or inline ghost. See
    /// [`SlashCommand::session_scoped`].
    pub fn set_hide_session_scoped(&mut self, hide: bool) {
        self.hide_session_scoped = hide;
    }

    /// Whether session-scoped commands are suppressed on this surface
    /// (see [`Self::set_hide_session_scoped`]).
    pub fn hide_session_scoped(&self) -> bool {
        self.hide_session_scoped
    }

    /// Whether `command` should be offered for completion or execution
    /// given this controller's session-scope policy and the command's
    /// own visibility gates. See [`command_offered`].
    pub fn is_command_offered(&self, command: &dyn SlashCommand, models: &ModelState) -> bool {
        let ctx = self.app_ctx(models);
        command_offered(command, &ctx, self.hide_session_scoped)
    }

    /// Recompute the snapshot from prompt text + cursor position.
    pub fn refresh(&mut self, slash: &SlashState, text: &str, cursor: usize, models: &ModelState) {
        let previous = slash.snapshot();
        let inline_tokens = scan_inline_slash_tokens(text, cursor);
        let leading = analyze_input(text, cursor);

        // Mid-text `/token` under the cursor, or args after that token (before the
        // next `/token`). Uses token-local ranges instead of buffer-start spans.
        if let Some(phase) = mid_text_slash_context(text, cursor, &inline_tokens)
            && should_use_mid_text_refresh(&phase, leading.as_ref(), cursor)
        {
            let token = match &phase {
                MidTextSlashPhase::Command(t) | MidTextSlashPhase::Args(t) => t,
            };
            self.refresh_mid_text_slash(MidTextSlashRefresh {
                slash,
                text,
                cursor,
                token,
                phase,
                all_tokens: &inline_tokens,
                models,
                previous: &previous,
            });
            return;
        }

        let Some(input) = leading else {
            // Text doesn't start with `/` -- check for mid-text slash tokens.
            let inline = self.compute_inline_slash(text, models);
            slash.replace(inline);
            return;
        };

        let args_text_empty = input
            .args_range
            .as_ref()
            .is_some_and(|r| text[r.start..r.end].trim().is_empty());
        let mut snapshot = SlashSnapshot {
            active: true,
            open: false,
            query: input.query.clone(),
            matches: Vec::new(),
            selected: 0,
            command_range: Some(input.command_range.clone()),
            args_range: input.args_range.clone(),
            cursor_in_command: input.cursor_in_command,
            args_placeholder: None,
            args_query_is_empty: args_text_empty,
            is_skill: false,
            command_recognized: false,
            inline_ghost: None,
            recognized_tokens: Vec::new(),
        };

        // Cursor inside the command token opens the command menu even when
        // args follow (e.g. `/` typed at the start of existing text via
        // ctrl-a) — same as mid-text tokens. The query is cursor-clamped, so
        // `/` before existing text shows the full list like an empty composer.
        // The two branches partition: analyze_input sets args_range exactly
        // when the cursor is past the command token.
        if input.cursor_in_command {
            let matches = self.command_suggestions(&input.query, models);
            snapshot.selected = Self::carry_selection(&previous, &matches, true, &input);
            snapshot.open = !matches.is_empty();
            snapshot.matches = matches;
        } else if input.args_range.is_some() {
            let matches = self.arg_suggestions_for_input(text, &input, models);
            snapshot.selected = Self::carry_selection(&previous, &matches, false, &input);
            snapshot.open = !matches.is_empty();
            snapshot.matches = matches;
        }

        // Resolve the command for args placeholder and skill detection.
        // Dispatch-tier lookup (menu-only hide; see
        // `CommandRegistry::get_for_dispatch`): menu-hidden commands still
        // execute on Enter, so the composer must render them as recognized.
        if let Some(invocation) = parse_invocation(text)
            && let Some(command) = self.registry.get_for_dispatch(invocation.token)
        {
            let ctx = self.app_ctx(models);
            if command_offered(command.as_ref(), &ctx, self.hide_session_scoped) {
                snapshot.command_recognized = true;
                snapshot.is_skill = command.is_skill();
                if args_text_empty {
                    snapshot.args_placeholder = command.arg_placeholder().map(|s| s.to_string());
                }
            }
        }

        // Also scan for mid-text slash tokens (after the first one) so that
        // prompts like "/model foo /comm" get ghost text and teal highlighting
        // on the second and subsequent `/` tokens.
        // compute_inline_slash only supplies recognized-token highlights now;
        // the inline ghost is derived solely from the dropdown selection (one
        // ranker, shared with Tab) via sync_inline_ghost_to_selection below.
        let inline = self.compute_inline_slash(text, models);
        snapshot.recognized_tokens = inline.recognized_tokens;
        sync_inline_ghost_to_selection(&mut snapshot);

        slash.replace(snapshot);
    }

    /// Slash completion for a mid-text `/token` (command or args phase).
    fn refresh_mid_text_slash(&mut self, p: MidTextSlashRefresh<'_>) {
        let MidTextSlashRefresh {
            slash,
            text,
            cursor,
            token,
            phase,
            all_tokens,
            models,
            previous,
        } = p;
        // Drop app_ctx before any &mut self call (it borrows self.cwd).
        // Same gate as the leading-`/` path and recognized_token_ranges, so
        // the under-cursor teal (command_recognized) can't disagree with the
        // token-range highlight on scope-restricted surfaces.
        let is_recognized = {
            let ctx = self.app_ctx(models);
            self.registry
                .get(&token.name)
                .is_some_and(|cmd| command_offered(cmd.as_ref(), &ctx, self.hide_session_scoped))
        };

        let mut snapshot = match phase {
            MidTextSlashPhase::Args(_) => {
                self.refresh_mid_text_slash_args(MidTextSlashArgsRefresh {
                    text,
                    cursor,
                    token,
                    all_tokens,
                    models,
                    is_recognized,
                    previous,
                })
            }
            MidTextSlashPhase::Command(_) => {
                let matches = self.command_suggestions(&token.name, models);
                let input = SlashInput {
                    command_range: token.range.clone(),
                    query: token.name.clone(),
                    cursor_in_command: true,
                    args_range: None,
                    args_query: String::new(),
                };
                let selected = Self::carry_selection(previous, &matches, true, &input);
                SlashSnapshot {
                    active: true,
                    open: !matches.is_empty(),
                    query: token.name.clone(),
                    matches,
                    selected,
                    command_range: Some(token.range.clone()),
                    args_range: None,
                    cursor_in_command: true,
                    args_placeholder: None,
                    args_query_is_empty: true,
                    is_skill: false,
                    command_recognized: is_recognized,
                    inline_ghost: None,
                    recognized_tokens: Vec::new(),
                }
            }
        };

        if is_recognized && let Some(command) = self.registry.get(&token.name) {
            snapshot.is_skill = command.is_skill();
        }

        // Same membership rule as every other composer state (and the
        // submit-time capture), so the highlight can't flicker with cursor
        // position or diverge from the echo's ranges.
        snapshot.recognized_tokens = self.recognized_token_ranges(text, models);

        // Same invariant as leading `/` and arrow nav: ghost completes selected row only.
        sync_inline_ghost_to_selection(&mut snapshot);

        slash.replace(snapshot);
    }

    fn refresh_mid_text_slash_args(&mut self, p: MidTextSlashArgsRefresh<'_>) -> SlashSnapshot {
        let MidTextSlashArgsRefresh {
            text,
            cursor,
            token,
            all_tokens,
            models,
            is_recognized,
            previous,
        } = p;
        let mut snapshot = SlashSnapshot {
            active: true,
            open: false,
            query: token.name.clone(),
            matches: Vec::new(),
            selected: 0,
            command_range: Some(token.range.clone()),
            args_range: None,
            cursor_in_command: false,
            args_placeholder: None,
            args_query_is_empty: true,
            is_skill: false,
            command_recognized: is_recognized,
            inline_ghost: None,
            recognized_tokens: Vec::new(),
        };

        let Some(command) = is_recognized
            .then(|| self.registry.get(&token.name).cloned())
            .flatten()
        else {
            return snapshot;
        };
        let visible = {
            let ctx = self.app_ctx(models);
            command.visible(&ctx)
        };
        if !visible || !command.takes_args() {
            return snapshot;
        }

        let token_with_slash = &text[token.range.start..token.range.end];
        if parse_invocation(token_with_slash).is_none() {
            return snapshot;
        }

        let args_start = token.range.end;
        if args_start >= text.len()
            || !text[args_start..]
                .chars()
                .next()
                .is_some_and(|ch| ch.is_whitespace())
        {
            return snapshot;
        }

        let mut start = args_start;
        while start < text.len() {
            let ch = match text[start..].chars().next() {
                Some(ch) => ch,
                None => break,
            };
            if ch.is_whitespace() {
                start += ch.len_utf8();
            } else {
                break;
            }
        }
        let args_end = next_slash_token_start(all_tokens, token).unwrap_or(text.len());
        let args_empty = start >= args_end || text[start..args_end].trim().is_empty();
        let args_query = if cursor > start {
            text[start..cursor.min(args_end)].to_string()
        } else {
            String::new()
        };

        let arg_matches = self.arg_suggestions(command.as_ref(), models, &args_query);
        let args_range = Some(start..args_end);
        let input = SlashInput {
            command_range: token.range.clone(),
            query: token.name.clone(),
            cursor_in_command: false,
            args_range: args_range.clone(),
            args_query: args_query.clone(),
        };

        snapshot.args_query_is_empty = args_empty;
        snapshot.args_range = args_range;
        snapshot.open = !arg_matches.is_empty();
        snapshot.matches = arg_matches;
        snapshot.selected = Self::carry_selection(previous, &snapshot.matches, false, &input);
        if args_empty {
            snapshot.args_placeholder = command.arg_placeholder().map(|s| s.to_string());
        }

        snapshot
    }

    /// Move the dropdown selection by `delta` (positive = down, negative = up),
    /// wrapping around at the ends. Used for keyboard arrow / Ctrl-P/N nav.
    pub fn move_selection(&self, slash: &SlashState, delta: isize) {
        slash.update(|inner| {
            let len = inner.matches.len();
            if len == 0 {
                return;
            }
            let current = inner.selected.min(len - 1) as isize;
            let next = (current + delta).rem_euclid(len as isize) as usize;
            inner.selected = next;
            sync_inline_ghost_to_selection(inner);
        });
    }

    /// Move the dropdown selection by `delta`, clamping at the first/last item
    /// (no wrap-around). Used for mouse-wheel scrolling.
    pub fn scroll_selection(&self, slash: &SlashState, delta: isize) {
        slash.update(|inner| {
            let len = inner.matches.len();
            if len == 0 {
                return;
            }
            let current = inner.selected.min(len - 1) as isize;
            let next = (current + delta).clamp(0, len as isize - 1) as usize;
            inner.selected = next;
            sync_inline_ghost_to_selection(inner);
        });
    }

    /// Try to carry the previous selection across a refresh.
    fn carry_selection(
        previous: &SlashSnapshot,
        matches: &[SuggestionRow],
        cursor_in_command: bool,
        input: &SlashInput,
    ) -> usize {
        if matches.is_empty() {
            return 0;
        }

        let same_context = if cursor_in_command {
            previous.cursor_in_command && previous.query == input.query
        } else {
            !previous.cursor_in_command && previous.args_range == input.args_range
        };
        if !same_context || previous.matches.is_empty() {
            return 0;
        }

        let prev_idx = previous
            .selected
            .min(previous.matches.len().saturating_sub(1));
        if let Some(prev_row) = previous.matches.get(prev_idx)
            && let Some(pos) = matches
                .iter()
                .position(|row| row.insert_text == prev_row.insert_text)
        {
            return pos;
        }

        previous.selected.min(matches.len().saturating_sub(1))
    }

    /// Byte ranges of recognized `/command` tokens anywhere in `text`.
    ///
    /// Single source of truth for the composer's teal token highlighting AND
    /// the scrollback echo of submitted prompts: whitespace-preceded `/{word}`
    /// tokens (see [`scan_inline_slash_tokens`]) whose name resolves to a
    /// command that is offered on this surface ([`command_offered`]).
    /// Cursor-independent. Empty when nothing is recognized.
    pub fn recognized_token_ranges(&self, text: &str, models: &ModelState) -> Vec<Range<usize>> {
        let tokens = scan_inline_slash_tokens(text, 0);
        if tokens.is_empty() {
            return Vec::new();
        }
        let ctx = self.app_ctx(models);
        let hide_session = self.hide_session_scoped;
        tokens
            .into_iter()
            .filter(|token| {
                self.registry
                    .get(&token.name)
                    .is_some_and(|cmd| command_offered(cmd.as_ref(), &ctx, hide_session))
            })
            .map(|token| token.range)
            .collect()
    }

    /// Compute inline slash state for text that doesn't start with `/`.
    ///
    /// Recognized-token highlights only ([`Self::recognized_token_ranges`]).
    /// Ghost for partial commands comes solely from
    /// [`sync_inline_ghost_to_selection`] (dropdown selection).
    fn compute_inline_slash(&self, text: &str, models: &ModelState) -> SlashSnapshot {
        SlashSnapshot {
            recognized_tokens: self.recognized_token_ranges(text, models),
            ..SlashSnapshot::default()
        }
    }

    /// Generate command-level suggestions for a query.
    ///
    /// Filters out any command whose `visible(&AppCtx)` returns `false`.
    fn command_suggestions(&mut self, query: &str, models: &ModelState) -> Vec<SuggestionRow> {
        let ctx = self.app_ctx(models);
        let hide_session = self.hide_session_scoped;
        let visible_indices: HashSet<usize> = (0..self.registry.triggers().len())
            .filter(|i| {
                let trigger = &self.registry.triggers()[*i];
                self.registry
                    .commands_by_index(trigger.command_index)
                    .is_some_and(|cmd| command_offered(cmd.as_ref(), &ctx, hide_session))
            })
            .collect();
        let triggers = self.registry.triggers();
        let trimmed = query.trim();
        if trimmed.is_empty() {
            // Show all unique commands (deduplicate by command_index).
            // No cap here -- the dropdown renderer handles scrolling.
            let mut seen = HashSet::new();
            let mut rows = Vec::new();
            for (i, trigger) in triggers.iter().enumerate() {
                if !visible_indices.contains(&i) {
                    continue;
                }
                if seen.insert(trigger.command_index) {
                    rows.push(SuggestionRow::from_command(trigger));
                }
            }
            return rows;
        }

        // Reject double-slash sequences.
        if trimmed.contains('/') {
            return Vec::new();
        }

        // Restrict the matcher to the visible subset so hidden commands
        // never show up in fuzzy results.
        let visible_triggers: Vec<&CommandTrigger> = triggers
            .iter()
            .enumerate()
            .filter(|(i, _)| visible_indices.contains(i))
            .map(|(_, t)| t)
            .collect();
        let hits = self.matcher.rank(
            &visible_triggers,
            trimmed,
            visible_triggers.len(),
            |trigger| trigger.match_text.as_str(),
        );

        // Deduplicate: keep the best-scoring trigger per command.
        // At equal fuzzy scores the tiebreaker is:
        //   1. Exact match on match_text wins (e.g. alias "/m" for query "m")
        //   2. Canonical name beats aliases (e.g. "/terminal-setup" over
        //      "/terminal-check" for query "terminal")
        //   3. Lexicographic display order as final fallback
        let mut best_per_command: HashMap<usize, (u32, usize)> = HashMap::new();
        for (visible_idx, score) in hits {
            let trigger = visible_triggers[visible_idx];
            best_per_command
                .entry(trigger.command_index)
                .and_modify(|current| {
                    let dominated = if score != current.0 {
                        score > current.0
                    } else {
                        let new_exact = trigger.match_text == trimmed;
                        let cur_exact = visible_triggers[current.1].match_text == trimmed;
                        if new_exact != cur_exact {
                            new_exact
                        } else {
                            let new_canonical = trigger.alias.is_none();
                            let cur_canonical = visible_triggers[current.1].alias.is_none();
                            if new_canonical != cur_canonical {
                                new_canonical
                            } else {
                                trigger.display < visible_triggers[current.1].display
                            }
                        }
                    };
                    if dominated {
                        *current = (score, visible_idx);
                    }
                })
                .or_insert((score, visible_idx));
        }

        let mut deduped: Vec<(u32, usize)> = best_per_command.into_values().collect();
        let mut rows: Vec<SuggestionRow> = visible_triggers
            .iter()
            .map(|t| SuggestionRow::from_command(t))
            .collect();
        let sort_meta: Vec<(String, CommandSource)> = visible_triggers
            .iter()
            .map(|t| (t.canonical.clone(), t.source))
            .collect();
        // Resolve all recency scores under a single borrow (one keystroke =
        // one borrow, not one per candidate).
        let mru_scores: Vec<u64> = {
            let mut m = self.mru.borrow_mut();
            sort_meta
                .iter()
                .map(|(canonical, _)| m.rank_score(trimmed, canonical))
                .collect()
        };
        deduped.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| mru_scores[b.1].cmp(&mru_scores[a.1]))
                .then_with(|| {
                    let a_builtin = sort_meta[a.1].1 == CommandSource::Builtin;
                    let b_builtin = sort_meta[b.1].1 == CommandSource::Builtin;
                    b_builtin.cmp(&a_builtin)
                })
                .then_with(|| rows[a.1].display.cmp(&rows[b.1].display))
        });
        for row in &mut rows {
            row.indices = self.matcher.indices(row.display.as_str());
        }
        deduped
            .into_iter()
            .map(|(_, idx)| rows[idx].clone())
            .collect()
    }

    /// Generate argument suggestions for the current input.
    fn arg_suggestions_for_input(
        &mut self,
        text: &str,
        input: &SlashInput,
        models: &ModelState,
    ) -> Vec<SuggestionRow> {
        let Some(invocation) = parse_invocation(text) else {
            return Vec::new();
        };
        // Clone the Arc to release the borrow on self.registry before
        // calling arg_suggestions (which needs &mut self for the matcher).
        let Some(command) = self.registry.get(invocation.token).cloned() else {
            return Vec::new();
        };
        // Hidden commands never produce arg suggestions either.
        let offered = {
            let visible_ctx = self.app_ctx(models);
            command_offered(command.as_ref(), &visible_ctx, self.hide_session_scoped)
        };
        if !offered {
            return Vec::new();
        }
        self.arg_suggestions(command.as_ref(), models, &input.args_query)
    }

    /// Generate argument suggestions for a specific command.
    fn arg_suggestions(
        &mut self,
        command: &dyn SlashCommand,
        models: &ModelState,
        query: &str,
    ) -> Vec<SuggestionRow> {
        if !command.takes_args() {
            return Vec::new();
        }
        let ctx = self.app_ctx(models);
        let Some(items) = command.suggest_args(&ctx, query) else {
            return Vec::new();
        };
        if items.is_empty() {
            return Vec::new();
        }
        let trimmed = query.trim();
        if trimmed.is_empty() {
            // Scoped default: locked BYOK rows (credential-less platform
            // models shown for discovery) are excluded from the empty-query
            // list — hundreds of them would flood it. Typing a query ranks
            // across them; the /model picker's Tab (All) view also reveals
            // them.
            return items
                .iter()
                .filter(|item| !item.locked)
                .map(SuggestionRow::from_arg)
                .collect();
        }
        let hits = self
            .matcher
            .rank(items.as_slice(), trimmed, items.len(), |item| {
                item.match_text.as_str()
            });
        hits.into_iter()
            .map(|(idx, _)| {
                let mut row = SuggestionRow::from_arg(&items[idx]);
                row.indices = self.matcher.indices(row.display.as_str());
                row
            })
            .collect()
    }
}

/// Whether `command` should be offered for completion **or execution** on
/// the current surface.
///
/// Combines the command's own [`SlashCommand::visible`] gate with the
/// controller's session-scope policy: when `hide_session_scoped` is set
/// (session-less surfaces such as the agent dashboard's dispatch input),
/// commands that act on a single session — `/compact`, `/fork`,
/// `/rewind`, … — are suppressed because there is no "current session"
/// for them to operate on.
///
/// Commands that opt in via [`SlashCommand::offered_when_session_less`]
/// (`/model`, `/plan`, `/multiline`) are exempt from this suppression —
/// they configure the next spawn or the dashboard input surface itself.
///
/// Conversely, [`SlashCommand::dashboard_only`] commands (`/cd`) are
/// offered ONLY when `hide_session_scoped` is set (the dashboard surface)
/// and suppressed on every session surface.
///
/// Callers that execute slash commands on a session-less surface (e.g.
/// `dispatch_dashboard_dispatch_slash`) must consult this before
/// `command.run` so typed tokens that were filtered from the dropdown
/// fall through as ordinary prompt text rather than running invisibly.
pub(crate) fn command_offered(
    command: &dyn SlashCommand,
    ctx: &AppCtx,
    hide_session_scoped: bool,
) -> bool {
    command.visible(ctx)
        && !(hide_session_scoped
            && command.session_scoped()
            && !command.offered_when_session_less())
        // Dashboard-only commands (`/cd`) are the inverse of session-scoped:
        // they only make sense on the session-less dashboard surface (where
        // `hide_session_scoped` is set), so suppress them everywhere else —
        // offered only when the command isn't dashboard-only or we're on the
        // dashboard.
        && (!command.dashboard_only() || hide_session_scoped)
}

// ---------------------------------------------------------------------------
// Input analysis
// ---------------------------------------------------------------------------

/// Parsed input structure for slash completion.
struct SlashInput {
    command_range: Range<usize>,
    query: String,
    cursor_in_command: bool,
    args_range: Option<Range<usize>>,
    args_query: String,
}

/// Analyze prompt text for slash command structure.
///
/// Returns `None` if the text doesn't start with `/` or is empty.
fn analyze_input(text: &str, cursor: usize) -> Option<SlashInput> {
    if text.is_empty() || !text.starts_with('/') {
        return None;
    }

    let cursor = cursor.min(text.len());
    if text[1..].chars().all(|ch| ch.is_whitespace()) {
        return Some(SlashInput {
            command_range: 0..1,
            query: String::new(),
            cursor_in_command: true,
            args_range: None,
            args_query: String::new(),
        });
    }

    let mut command_end = text.len();
    for (idx, ch) in text.char_indices() {
        if idx == 0 {
            continue;
        }
        if ch.is_whitespace() {
            command_end = idx;
            break;
        }
    }

    let query_end = cursor.clamp(1, command_end);
    let query = if query_end <= 1 {
        String::new()
    } else {
        text[1..query_end].to_string()
    };

    let cursor_in_command = cursor <= command_end;

    let mut args_range = None;
    let mut args_query = String::new();
    if !cursor_in_command {
        let mut start = command_end;
        while start < text.len() {
            let ch = match text[start..].chars().next() {
                Some(ch) => ch,
                None => break,
            };
            if ch.is_whitespace() {
                start += ch.len_utf8();
            } else {
                break;
            }
        }
        let end = text.len();
        let query_end = cursor.clamp(start, end);
        if query_end > start {
            args_query = text[start..query_end].to_string();
        }
        args_range = Some(start..end);
    }

    Some(SlashInput {
        command_range: 0..command_end,
        query,
        cursor_in_command,
        args_range,
        args_query,
    })
}

// ---------------------------------------------------------------------------
// Invocation parsing
// ---------------------------------------------------------------------------

/// Parsed slash command invocation.
pub struct SlashInvocation<'a> {
    /// Command token (e.g., "model" for "/model grok-4").
    pub token: &'a str,
    /// Everything after the command token, trimmed on the left.
    pub args: &'a str,
}

/// Parse a line into a slash command invocation.
///
/// Returns `None` if the line doesn't start with `/` or has no command token.
pub fn parse_invocation(line: &str) -> Option<SlashInvocation<'_>> {
    let remainder = line.strip_prefix('/')?;
    if remainder.is_empty() {
        return None;
    }

    let mut command_end = remainder.len();
    for (idx, ch) in remainder.char_indices() {
        if ch.is_whitespace() {
            command_end = idx;
            break;
        }
    }
    let token = remainder[..command_end].trim();
    if token.is_empty() {
        return None;
    }
    let args = if command_end < remainder.len() {
        remainder[command_end..].trim_start()
    } else {
        ""
    };
    Some(SlashInvocation { token, args })
}

// ---------------------------------------------------------------------------
// Completeness check
// ---------------------------------------------------------------------------

/// Check if a slash command line is complete (ready to execute on Enter).
///
/// Uses the two-bit model: `takes_args()` + `args_required()`.
///
/// | `takes_args` | `args_required` | Enter with no args |
/// |-------------|----------------|-------------------|
/// | `false`     | `false`        | Executes          |
/// | `true`      | `false`        | Executes          |
/// | `true`      | `true`         | Blocks            |
///
/// Unknown commands (not in registry) are treated as complete -- they will
/// pass through to the shell.
pub fn is_command_complete(line: &str, registry: &CommandRegistry) -> bool {
    let Some(invocation) = parse_invocation(line) else {
        return false;
    };
    // Dispatch-tier lookup (menu-only hide; see
    // `CommandRegistry::get_for_dispatch`): menu-hidden commands still run
    // on Enter, so their arg contract gates completeness the same way.
    let Some(command) = registry.get_for_dispatch(invocation.token) else {
        // Unknown command -- treat as complete (will PassThrough).
        return true;
    };
    if !command.takes_args() {
        // No args accepted -- always complete.
        return true;
    }
    if !command.args_required() {
        // Args accepted but optional -- always complete.
        return true;
    }
    // Args required -- complete only if non-empty.
    !invocation.args.trim().is_empty()
}

// ---------------------------------------------------------------------------
// Mid-text inline slash token scanning
// ---------------------------------------------------------------------------

/// A `/token` found anywhere in the input text.
#[derive(Debug, Clone)]
pub struct InlineSlashToken {
    /// Byte range of the entire token including `/`.
    pub range: Range<usize>,
    /// The command name part (without `/`).
    pub name: String,
    /// Whether this token's range contains the cursor position.
    pub has_cursor: bool,
}

/// Byte offset of the next `/token` after `current`, or `None` if none.
fn next_slash_token_start(
    tokens: &[InlineSlashToken],
    current: &InlineSlashToken,
) -> Option<usize> {
    tokens
        .iter()
        .filter(|t| t.range.start > current.range.start)
        .map(|t| t.range.start)
        .min()
}

/// Inputs for [`SlashController::refresh_mid_text_slash`].
struct MidTextSlashRefresh<'a> {
    slash: &'a SlashState,
    text: &'a str,
    cursor: usize,
    token: &'a InlineSlashToken,
    phase: MidTextSlashPhase<'a>,
    all_tokens: &'a [InlineSlashToken],
    models: &'a ModelState,
    previous: &'a SlashSnapshot,
}

/// Inputs for [`SlashController::refresh_mid_text_slash_args`].
struct MidTextSlashArgsRefresh<'a> {
    text: &'a str,
    cursor: usize,
    token: &'a InlineSlashToken,
    all_tokens: &'a [InlineSlashToken],
    models: &'a ModelState,
    is_recognized: bool,
    previous: &'a SlashSnapshot,
}

/// Whether the cursor is on a mid-text `/token` or in its args (before the next token).
enum MidTextSlashPhase<'a> {
    Command(&'a InlineSlashToken),
    Args(&'a InlineSlashToken),
}

fn mid_text_slash_context<'a>(
    text: &str,
    cursor: usize,
    tokens: &'a [InlineSlashToken],
) -> Option<MidTextSlashPhase<'a>> {
    if let Some(token) = tokens.iter().find(|t| t.has_cursor) {
        return Some(MidTextSlashPhase::Command(token));
    }

    for (i, token) in tokens.iter().enumerate() {
        if cursor <= token.range.end {
            continue;
        }
        let region_end = tokens
            .get(i + 1)
            .map(|t| t.range.start)
            .unwrap_or(text.len());
        if cursor > region_end {
            continue;
        }
        let after_cmd = text.get(token.range.end..cursor)?;
        if !after_cmd.starts_with(|c: char| c.is_whitespace()) {
            continue;
        }
        return Some(MidTextSlashPhase::Args(token));
    }
    None
}

/// True when completion should use the mid-text path instead of leading `/` parsing.
fn should_use_mid_text_refresh(
    phase: &MidTextSlashPhase<'_>,
    leading: Option<&SlashInput>,
    cursor: usize,
) -> bool {
    match phase {
        MidTextSlashPhase::Command(token) => match leading {
            None => true,
            Some(input) => {
                token.range.start > 0
                    || cursor > input.command_range.end
                    || token.range != input.command_range
            }
        },
        MidTextSlashPhase::Args(token) => match leading {
            None => true,
            Some(input) => token.range != input.command_range || cursor > input.command_range.end,
        },
    }
}

/// Scan input for all `/word` tokens at any position.
///
/// A slash token is `/` followed by one or more non-whitespace chars, where
/// the `/` is either at position 0 or preceded by whitespace (avoids matching
/// file paths like `foo/bar`).
pub fn scan_inline_slash_tokens(text: &str, cursor: usize) -> Vec<InlineSlashToken> {
    let cursor = cursor.min(text.len());
    let mut tokens = Vec::new();
    let mut iter = text.char_indices().peekable();

    while let Some((idx, ch)) = iter.next() {
        if ch != '/' {
            continue;
        }
        // `/` must be at start or preceded by whitespace.
        if idx > 0 {
            let prev_byte = text.as_bytes()[idx - 1];
            if !prev_byte.is_ascii_whitespace() {
                continue;
            }
        }
        // Collect non-whitespace chars after `/`.
        let name_start = idx + ch.len_utf8();
        let mut name_end = name_start;
        while let Some(&(next_idx, next_ch)) = iter.peek() {
            if next_ch.is_whitespace() {
                break;
            }
            name_end = next_idx + next_ch.len_utf8();
            iter.next();
        }
        if name_end <= name_start {
            continue; // bare `/` with nothing after
        }
        let name = text[name_start..name_end].to_string();
        let range = idx..name_end;
        let has_cursor = cursor >= range.start && cursor <= range.end;
        tokens.push(InlineSlashToken {
            range,
            name,
            has_cursor,
        });
    }
    tokens
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_client_protocol as acp;

    use crate::acp::model_state::ModelState;

    use super::registry::CommandRegistry;
    use super::*;

    #[test]
    fn parses_invocation_with_args() {
        let inv = parse_invocation("/model grok-code-fast-1").expect("parsed");
        assert_eq!(inv.token, "model");
        assert_eq!(inv.args, "grok-code-fast-1");
    }

    #[test]
    fn parses_invocation_no_args() {
        let inv = parse_invocation("/exit").expect("parsed");
        assert_eq!(inv.token, "exit");
        assert_eq!(inv.args, "");
    }

    #[test]
    fn rejects_bare_slash() {
        assert!(parse_invocation("/").is_none());
    }

    #[test]
    fn rejects_non_slash() {
        assert!(parse_invocation("model foo").is_none());
    }

    #[test]
    fn rejects_empty_string() {
        assert!(parse_invocation("").is_none());
    }

    // -- is_command_complete tests --

    fn test_registry() -> CommandRegistry {
        CommandRegistry::new(commands::builtin_commands())
    }

    #[test]
    fn no_arg_command_is_complete() {
        let reg = test_registry();
        assert!(is_command_complete("/exit", &reg));
        assert!(is_command_complete("/quit", &reg));
        assert!(is_command_complete("/new", &reg));
        assert!(is_command_complete("/clear", &reg));
    }

    #[test]
    fn optional_arg_command_is_complete_without_args() {
        let reg = test_registry();
        // /compact has takes_args=true, args_required=false.
        assert!(is_command_complete("/compact", &reg));
        assert!(is_command_complete("/compact some context", &reg));
    }

    #[test]
    fn required_arg_command_blocks_without_args() {
        let reg = test_registry();
        // /model has takes_args=true, args_required=true.
        assert!(!is_command_complete("/model", &reg));
        assert!(!is_command_complete("/model ", &reg));
        assert!(is_command_complete("/model grok-4", &reg));
    }

    #[test]
    fn unknown_command_is_complete() {
        let reg = test_registry();
        // Unknown commands pass through.
        assert!(is_command_complete("/unknown", &reg));
        assert!(is_command_complete("/foo bar", &reg));
    }

    #[test]
    fn bare_slash_is_not_complete() {
        let reg = test_registry();
        assert!(!is_command_complete("/", &reg));
    }

    #[test]
    fn non_slash_is_not_complete() {
        let reg = test_registry();
        assert!(!is_command_complete("hello", &reg));
    }

    // -- Controller tests --

    #[test]
    fn controller_surfaces_commands_without_query() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/", 1, &models);
        let snapshot = state.snapshot();
        assert!(snapshot.open);
        assert!(!snapshot.matches.is_empty());
    }

    #[test]
    fn gboom_never_appears_in_suggestions() {
        // The /gboom easter egg is executable but must stay out of the
        // dropdown: not in the full list, not via prefix, not via exact name.
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        for query in ["/", "/g", "/gbo", "/gboom"] {
            ctrl.refresh(&state, query, query.len(), &models);
            let snapshot = state.snapshot();
            assert!(
                snapshot
                    .matches
                    .iter()
                    .all(|row| !row.display.contains("gboom")),
                "/gboom leaked into suggestions for query {query:?}"
            );
        }
    }

    #[test]
    fn gboom_still_resolves_for_execution() {
        // Dispatch resolves via `registry.get()`, which ignores `visible()`.
        let reg = test_registry();
        let cmd = reg.get("gboom").expect("/gboom resolvable for dispatch");
        assert_eq!(cmd.name(), "gboom");
    }

    /// `/debug` lists via `visible()` = cfg!(debug_assertions); tests
    /// compile with debug_assertions, so it must surface here. Release
    /// builds flip the same constant to false (the /gboom hidden
    /// mechanism), which is untestable from a debug test build — hence
    /// the cfg gate rather than a release-side assertion.
    #[test]
    #[cfg(debug_assertions)]
    fn debug_appears_in_suggestions_on_debug_binaries() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        for query in ["/", "/deb", "/debug"] {
            ctrl.refresh(&state, query, query.len(), &models);
            let snapshot = state.snapshot();
            assert!(
                snapshot
                    .matches
                    .iter()
                    .any(|row| row.display.contains("debug")),
                "/debug missing from suggestions for query {query:?}"
            );
        }
    }

    #[test]
    fn debug_resolves_for_execution() {
        let reg = test_registry();
        let cmd = reg.get("debug").expect("/debug resolvable for dispatch");
        assert_eq!(cmd.name(), "debug");
    }

    #[test]
    fn controller_suggests_partial_command() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        let text = "/mo";
        ctrl.refresh(&state, text, text.len(), &models);
        let snapshot = state.snapshot();
        assert!(snapshot.open);
        assert!(
            snapshot.matches.iter().any(|row| row.display == "/model"),
            "expected /model in matches"
        );
    }

    #[test]
    fn controller_silences_double_slash() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "//", 2, &models);
        let snapshot = state.snapshot();
        assert!(!snapshot.open);
        assert!(snapshot.matches.is_empty());
    }

    /// Ctrl-a then `/` in front of existing text must open the menu: the
    /// query is cursor-clamped to "", so the full command list shows exactly
    /// like `/` on an empty composer.
    #[test]
    fn slash_typed_before_existing_text_opens_full_menu() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/", 1, &models);
        let full_list_len = state.snapshot().matches.len();

        ctrl.refresh(&state, "/hello world", 1, &models);
        let snapshot = state.snapshot();
        assert!(snapshot.active);
        assert!(snapshot.open, "menu must open with the cursor on the `/`");
        assert_eq!(snapshot.query, "");
        assert_eq!(
            snapshot.matches.len(),
            full_list_len,
            "cursor-clamped empty query must show the full list"
        );
    }

    /// Cursor mid-token while args follow: the menu opens filtered by the
    /// cursor-clamped prefix instead of staying closed.
    #[test]
    fn cursor_inside_leading_command_with_args_filters_by_prefix() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        // Cursor 3 in "/mod grok-4" clamps the query to "mo".
        ctrl.refresh(&state, "/mod grok-4", 3, &models);
        let snapshot = state.snapshot();
        assert!(snapshot.open);
        assert_eq!(snapshot.query, "mo");
        assert!(
            snapshot.matches.iter().any(|row| row.display == "/model"),
            "expected /model in matches"
        );
    }

    /// Cursor moved back inside an already-complete recognized command opens
    /// the menu too (same as mid-text tokens today); recognition holds and no
    /// inline ghost is drawn over the existing text.
    #[test]
    fn cursor_inside_recognized_command_with_args_opens_menu() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/model grok-4", 3, &models);
        let snapshot = state.snapshot();
        assert!(snapshot.open);
        assert!(snapshot.command_recognized);
        assert!(
            snapshot.matches.iter().any(|row| row.display == "/model"),
            "expected /model in matches"
        );
        assert!(
            snapshot.inline_ghost.is_none(),
            "recognized command must not draw a ghost over existing text"
        );
    }

    #[test]
    fn controller_close_hides_matches() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/model", 6, &models);
        assert!(state.snapshot().open);

        state.close();
        let snapshot = state.snapshot();
        assert!(!snapshot.open);
        assert!(snapshot.matches.is_empty());
    }

    #[test]
    fn controller_suggests_alias_display_for_alias() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/m", 2, &models);
        let snapshot = state.snapshot();
        assert!(snapshot.open);
        let first = snapshot.matches.first().expect("match");
        assert_eq!(first.display, "/m");
        assert!(first.insert_text.starts_with("/m"));
    }

    /// `/sessions` survives the sessions-modal removal as an alias of
    /// `/dashboard`: typing it must complete with the alias spelling and the
    /// dashboard command's description.
    #[test]
    fn controller_suggests_sessions_alias_for_dashboard() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        // `/dashboard` is feature-flag gated (hidden by default); the alias
        // is only offered once the flag reveals the canonical command.
        ctrl.registry_mut().set_dashboard_visible(true);
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/sessions", 9, &models);
        let snapshot = state.snapshot();
        assert!(snapshot.open);
        let row = snapshot
            .matches
            .iter()
            .find(|r| r.display == "/sessions")
            .expect("/sessions must be offered in completion");
        assert_eq!(
            row.description,
            crate::slash::commands::dashboard::DashboardCommand.description(),
            "alias must carry the dashboard command's description"
        );
    }

    #[test]
    fn controller_suggests_model_args() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let mut models = ModelState::default();
        let model_id = acp::ModelId::new(Arc::from("example"));
        models.available.insert(
            model_id.clone(),
            acp::ModelInfo::new(model_id, "Example".to_string()),
        );

        let text = "/model e";
        ctrl.refresh(&state, text, text.len(), &models);
        let snapshot = state.snapshot();
        assert!(snapshot.open);
        assert!(
            snapshot
                .matches
                .iter()
                .any(|row| row.display.contains("Example"))
        );
        assert!(snapshot.args_range.is_some());
    }

    #[test]
    fn no_placeholder_when_cursor_at_start_of_existing_args() {
        // Simulates the user typing "hello", then prepending "/model ".
        // Cursor ends up right at the start of the args ("hello"), so
        // args_query is empty but the args range is non-empty.
        // The placeholder must NOT appear.
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        let text = "/model hello";
        // Cursor at 7: right after "/model ", before 'h'.
        ctrl.refresh(&state, text, 7, &models);
        let snapshot = state.snapshot();
        assert!(
            !snapshot.args_query_is_empty,
            "args_query_is_empty must be false when args range contains text"
        );
        assert!(
            snapshot.args_placeholder.is_none(),
            "no placeholder when existing args text is present"
        );
    }

    #[test]
    fn controller_selection_wraps_around() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/m", 2, &models);
        let len = state.snapshot().matches.len();
        assert!(len >= 2, "need at least 2 matches");

        // At first item, pressing Up wraps to last.
        state.update(|s| s.selected = 0);
        ctrl.move_selection(&state, -1);
        assert_eq!(state.snapshot().selected, len - 1);

        // At last item, pressing Down wraps to first.
        state.update(|s| s.selected = len - 1);
        ctrl.move_selection(&state, 1);
        assert_eq!(state.snapshot().selected, 0);
    }

    #[test]
    fn controller_scroll_clamps_at_edges() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/m", 2, &models);
        let len = state.snapshot().matches.len();
        assert!(len >= 2, "need at least 2 matches");

        // Scrolling up at the first item stays at the first item (no wrap).
        state.update(|s| s.selected = 0);
        ctrl.scroll_selection(&state, -1);
        assert_eq!(state.snapshot().selected, 0);

        // Scrolling down at the last item stays at the last item (no wrap).
        state.update(|s| s.selected = len - 1);
        ctrl.scroll_selection(&state, 1);
        assert_eq!(state.snapshot().selected, len - 1);
    }

    #[test]
    fn non_slash_text_produces_inactive_snapshot() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "hello world", 5, &models);
        let snapshot = state.snapshot();
        assert!(!snapshot.active);
        assert!(!snapshot.open);
    }

    #[test]
    fn file_path_not_recognized_as_command() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/Users/foo/bar", 14, &models);
        let snapshot = state.snapshot();
        assert!(snapshot.active, "still active because starts with /");
        assert!(!snapshot.open, "dropdown should be closed");
        assert!(
            !snapshot.command_recognized,
            "file path should not be recognized as a command"
        );
    }

    #[test]
    fn recognized_command_sets_command_recognized() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/model", 6, &models);
        let snapshot = state.snapshot();
        assert!(snapshot.active);
        assert!(
            snapshot.command_recognized,
            "known command should be recognized"
        );
    }

    // -- session-scoped surface filtering (agent dashboard) --

    /// On a session-less surface (the agent dashboard's dispatch input),
    /// commands that act on a single session are suppressed from completion
    /// while pager-global commands remain. See `SlashCommand::session_scoped`.
    #[test]
    fn hide_session_scoped_filters_session_commands_from_dropdown() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        ctrl.set_hide_session_scoped(true);
        // `/dashboard` is feature-flag gated (hidden by default in the
        // registry); the session-less surface under test is the dashboard's
        // own dispatch input, so the flag is necessarily on there.
        ctrl.registry_mut().set_dashboard_visible(true);
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/", 1, &models);
        let snapshot = state.snapshot();
        assert!(snapshot.open);
        let names: Vec<&str> = snapshot
            .matches
            .iter()
            .map(|r| r.display.as_str())
            .collect();

        // Pager-global commands stay, plus session-scoped opt-ins
        // (`offered_when_session_less`): `/model`/`/plan` stage the next
        // spawn; `/multiline` toggles compose on the dashboard inputs.
        for keep in [
            "/quit",
            "/new",
            "/theme",
            "/settings",
            "/dashboard",
            "/resume",
            "/model",
            "/plan",
            "/multiline",
        ] {
            assert!(
                names.contains(&keep),
                "{keep} should remain on the dashboard, got {names:?}"
            );
        }
        // Session-scoped commands without a session-less opt-in are gone.
        for hide in [
            "/compact",
            "/fork",
            "/rewind",
            "/share",
            "/context",
            "/copy",
            "/export",
            "/rename",
            "/btw",
            "/session-info",
            "/find",
        ] {
            assert!(
                !names.contains(&hide),
                "{hide} should be hidden on the dashboard, got {names:?}"
            );
        }
    }

    /// The default surface (agent view) keeps showing session-scoped commands.
    #[test]
    fn default_surface_keeps_session_scoped_commands() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/", 1, &models);
        let names: Vec<String> = state
            .snapshot()
            .matches
            .iter()
            .map(|r| r.display.clone())
            .collect();
        assert!(names.iter().any(|d| d == "/compact"));
        assert!(names.iter().any(|d| d == "/fork"));
    }

    /// `/cd` is dashboard-only: it appears in the dropdown on the
    /// session-less dashboard surface but is hidden on the default (agent
    /// view) surface — the inverse of session-scoped commands.
    #[test]
    fn dashboard_only_command_hidden_off_dashboard() {
        let models = ModelState::default();

        // Default surface (agent view): `/cd` is hidden.
        let mut agent = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        agent.refresh(&state, "/cd", 3, &models);
        let agent_names: Vec<String> = state
            .snapshot()
            .matches
            .iter()
            .map(|r| r.display.clone())
            .collect();
        assert!(
            !agent_names.iter().any(|d| d == "/cd"),
            "/cd must be hidden off the dashboard, got {agent_names:?}"
        );

        // Dashboard surface (session-less): `/cd` is offered.
        let mut dash = SlashController::with_builtins(std::path::PathBuf::from("."));
        dash.set_hide_session_scoped(true);
        let state = SlashState::default();
        dash.refresh(&state, "/cd", 3, &models);
        let dash_names: Vec<String> = state
            .snapshot()
            .matches
            .iter()
            .map(|r| r.display.clone())
            .collect();
        assert!(
            dash_names.iter().any(|d| d == "/cd"),
            "/cd must be offered on the dashboard, got {dash_names:?}"
        );
    }

    /// Fuzzy queries also exclude session-scoped commands while keeping
    /// global ones that match the same prefix (`/compact` is hidden,
    /// `/compact-mode` stays).
    #[test]
    fn hide_session_scoped_filters_fuzzy_query() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        ctrl.set_hide_session_scoped(true);
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/co", 3, &models);
        let names: Vec<String> = state
            .snapshot()
            .matches
            .iter()
            .map(|r| r.display.clone())
            .collect();
        assert!(!names.iter().any(|d| d == "/compact"));
        assert!(!names.iter().any(|d| d == "/context"));
        assert!(!names.iter().any(|d| d == "/copy"));
        // The pager-global /compact-mode also fuzzy-matches "co" and stays.
        assert!(
            names.iter().any(|d| d == "/compact-mode"),
            "global commands matching the query must remain, got {names:?}"
        );
    }

    /// A fully-typed session command is neither recognized (no teal /
    /// placeholder) nor offered arg suggestions on the dashboard surface.
    #[test]
    fn hidden_session_command_not_recognized_and_no_args() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        ctrl.set_hide_session_scoped(true);
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/compact", 8, &models);
        assert!(
            !state.snapshot().command_recognized,
            "/compact must not be recognized on the session-less dashboard"
        );

        let text = "/compact ";
        ctrl.refresh(&state, text, text.len(), &models);
        assert!(
            state.snapshot().matches.is_empty(),
            "a hidden command must not produce arg suggestions"
        );
    }

    /// `/model`, `/plan`, and `/multiline` opt in via `offered_when_session_less`,
    /// so they stay recognized on the dashboard even though they're session-scoped.
    #[test]
    fn session_less_opt_in_commands_recognized_on_dashboard() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        ctrl.set_hide_session_scoped(true);
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/plan", 5, &models);
        assert!(
            state.snapshot().command_recognized,
            "/plan must be recognized on the dashboard"
        );

        ctrl.refresh(&state, "/model", 6, &models);
        assert!(
            state.snapshot().command_recognized,
            "/model must be recognized on the dashboard"
        );

        ctrl.refresh(&state, "/multiline", 10, &models);
        assert!(
            state.snapshot().command_recognized,
            "/multiline must be recognized on the dashboard"
        );
    }

    // -- scan_inline_slash_tokens tests --

    #[test]
    fn scan_finds_mid_text_slash_token() {
        let tokens = scan_inline_slash_tokens("do /model now", 6);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].name, "model");
        assert_eq!(tokens[0].range, 3..9);
        assert!(tokens[0].has_cursor);
    }

    #[test]
    fn scan_finds_multiple_tokens() {
        let tokens = scan_inline_slash_tokens("run /commit and /review", 4);
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].name, "commit");
        assert_eq!(tokens[1].name, "review");
        assert!(tokens[0].has_cursor);
        assert!(!tokens[1].has_cursor);
    }

    #[test]
    fn scan_ignores_file_paths() {
        let tokens = scan_inline_slash_tokens("edit foo/bar/baz.rs", 10);
        assert!(tokens.is_empty(), "slashes inside words are not tokens");
    }

    #[test]
    fn scan_handles_start_of_line() {
        let tokens = scan_inline_slash_tokens("/exit now", 3);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].name, "exit");
        assert!(tokens[0].has_cursor);
    }

    #[test]
    fn scan_bare_slash_ignored() {
        let tokens = scan_inline_slash_tokens("do / something", 4);
        assert!(tokens.is_empty());
    }

    #[test]
    fn scan_cursor_at_token_end() {
        let tokens = scan_inline_slash_tokens("run /model", 10);
        assert_eq!(tokens.len(), 1);
        assert!(tokens[0].has_cursor, "cursor at end of token should match");
    }

    // -- Inline ghost text tests --

    #[test]
    fn inline_ghost_for_partial_command() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "do /mod", 7, &models);
        let snapshot = state.snapshot();
        assert!(snapshot.active, "mid-text slash under cursor is active");
        if let Some(ref ghost) = snapshot.inline_ghost {
            assert_eq!(ghost.full_name, "model");
            assert_eq!(ghost.text, "el");
            assert_eq!(ghost.token_range, 3..7);
        } else {
            panic!("expected inline ghost for partial /mod");
        }
    }

    #[test]
    fn leading_slash_ghost_tracks_dropdown_selection() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "/p", 2, &models);
        let snapshot = state.snapshot();
        assert!(snapshot.open, "partial /p should open the dropdown");
        let selected = snapshot
            .selection()
            .expect("dropdown should have a selection");
        let selected_name = selected
            .display
            .strip_prefix('/')
            .unwrap_or(&selected.display);
        let ghost = snapshot
            .inline_ghost
            .as_ref()
            .expect("ghost should mirror the selected dropdown row");
        assert_eq!(
            ghost.full_name, selected_name,
            "ghost must complete the selected row (Tab target), not a separate fuzzy winner"
        );
        assert!(
            selected_name.starts_with('p'),
            "selected row for query 'p' should start with p, got {selected_name}"
        );
        assert_eq!(ghost.text, &selected_name[1..]);
    }

    #[test]
    fn smart_case_prefix_match_aligns_ghost_with_fuzzy_ranker() {
        // Nucleo Smart case: all-lowercase query is case-insensitive.
        assert!(command_prefix_matches_smart("Privacy", "p"));
        assert!(command_prefix_matches_smart("Privacy", "pr"));
        // Any uppercase in query requires exact (case-sensitive) prefix.
        assert!(command_prefix_matches_smart("Privacy", "P")); // "P" is exact prefix of "Privacy"
        assert!(!command_prefix_matches_smart("privacy", "Pr"));
        assert!(!command_prefix_matches_smart("Privacy", "PR"));

        let row = SuggestionRow {
            display: "/Privacy".to_string(),
            description: String::new(),
            insert_text: "/Privacy ".to_string(),
            indices: Vec::new(),
        };
        // Without smart-case, starts_with("p") fails on "Privacy" and ghost disappears
        // while the dropdown still highlights the row via CaseMatching::Smart.
        let ghost = inline_ghost_from_selected_command("p", 1..2, &row).expect(
            "lowercase query must ghost-complete a title-case command (dropdown can select it)",
        );
        assert_eq!(ghost.full_name, "Privacy");
        assert_eq!(ghost.text, "rivacy");

        // Mixed-case query that is not an exact prefix must not ghost.
        assert!(inline_ghost_from_selected_command("PR", 1..3, &row).is_none());
    }

    /// Minimal command used to build a hermetic registry where several names
    /// tie at the same fuzzy score, so MRU recency (not the live builtin set)
    /// decides ordering.
    struct TieCmd(&'static str);
    impl SlashCommand for TieCmd {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            ""
        }
        fn usage(&self) -> &str {
            ""
        }
        fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
            CommandResult::Handled
        }
    }

    fn tie_controller(names: &[&'static str], seeds: &[(&str, u64)]) -> SlashController {
        let commands: Vec<Arc<dyn SlashCommand>> = names
            .iter()
            .map(|n| Arc::new(TieCmd(n)) as Arc<dyn SlashCommand>)
            .collect();
        let mut store = mru::SlashMru::new_in_memory();
        for (cmd, ts) in seeds {
            store.seed_for_test("", cmd, *ts);
        }
        let mut ctrl = SlashController::new(
            CommandRegistry::new(commands),
            std::path::PathBuf::from("."),
        );
        ctrl.set_mru(std::rc::Rc::new(std::cell::RefCell::new(store)));
        ctrl
    }

    #[test]
    fn mru_beats_tiebreak_on_equal_fuzzy_score() {
        // Hermetic: three names tie on fuzzy score for `/p`; MRU recency must
        // pick the winner regardless of the live builtin registry.
        let mut ctrl = tie_controller(
            &["privacy", "personas", "plan"],
            &[
                ("privacy", 1_700_000_900),
                ("personas", 1_700_000_010),
                ("plan", 1_700_000_050),
            ],
        );

        let state = SlashState::default();
        let models = ModelState::default();
        ctrl.refresh(&state, "/p", 2, &models);
        let snapshot = state.snapshot();
        let top = snapshot.selection().expect("dropdown open").display.clone();
        assert_eq!(
            top, "/privacy",
            "MRU should rank the most-recently-used tie winner first, got {top}"
        );
        let ghost = snapshot
            .inline_ghost
            .as_ref()
            .expect("ghost tracks selected row");
        assert_eq!(ghost.full_name, "privacy");
    }

    /// Tier-restricted commands stay in the dropdown (discoverability) even
    /// though `get()` blocks execution — invoking one shows the SuperGrok
    /// upsell (covered by the dispatch-level tests).
    #[test]
    fn restricted_commands_stay_visible_in_dropdown() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        ctrl.registry_mut()
            .set_restricted_commands(&["usage".to_string()]);
        assert!(
            ctrl.registry().get("usage").is_none(),
            "execution stays blocked"
        );

        let state = SlashState::default();
        let models = ModelState::default();
        ctrl.refresh(&state, "/usa", 4, &models);
        let snapshot = state.snapshot();
        let top = snapshot.selection().expect("dropdown open").display.clone();
        assert_eq!(
            top, "/usage",
            "restricted command stays discoverable in the dropdown"
        );
    }

    /// Gate open → both `/always-approve` and `/auto` offered + dispatchable.
    /// Gate closed → `/auto` hard-hidden; `/always-approve` still offered.
    #[test]
    fn set_auto_mode_available_gates_only_auto() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let visible = |ctrl: &SlashController, name: &str| ctrl.registry().get(name).is_some();
        let dispatchable =
            |ctrl: &SlashController, name: &str| ctrl.registry().get_for_dispatch(name).is_some();

        ctrl.set_auto_mode_available(true);
        assert!(visible(&ctrl, "always-approve"));
        assert!(visible(&ctrl, "auto"));
        assert!(dispatchable(&ctrl, "always-approve"));
        assert!(dispatchable(&ctrl, "auto"));

        ctrl.set_auto_mode_available(false);
        assert!(visible(&ctrl, "always-approve"));
        assert!(!visible(&ctrl, "auto"));
        assert!(dispatchable(&ctrl, "always-approve"));
        assert!(!dispatchable(&ctrl, "auto"));
    }

    /// With the gate open, both permission-mode toggles appear in completion
    /// for full-list, prefix, and exact-name queries.
    #[test]
    fn permission_mode_toggles_appear_in_completion_when_available() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();
        ctrl.set_auto_mode_available(true);

        for (query, display) in [
            ("/", "/always-approve"),
            ("/alw", "/always-approve"),
            ("/always-approve", "/always-approve"),
            ("/", "/auto"),
            ("/au", "/auto"),
            ("/auto", "/auto"),
        ] {
            ctrl.refresh(&state, query, query.len(), &models);
            let snapshot = state.snapshot();
            assert!(
                snapshot.matches.iter().any(|row| row.display == display),
                "{display} missing from completion for query {query:?}"
            );
        }
    }

    /// No-arg toggles are complete so Enter submits immediately.
    #[test]
    fn permission_mode_toggles_are_complete_for_enter() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        ctrl.set_auto_mode_available(true);
        assert!(is_command_complete("/always-approve", ctrl.registry()));
        assert!(is_command_complete("/auto", ctrl.registry()));
    }

    #[test]
    fn move_selection_updates_inline_ghost() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();
        ctrl.refresh(&state, "/p", 2, &models);
        let before = state.snapshot();
        assert!(before.matches.len() >= 2, "need multiple /p hits");
        let first_name = before
            .selection()
            .expect("selection")
            .command_name()
            .to_string();

        ctrl.move_selection(&state, 1);
        let after = state.snapshot();
        let second_name = after
            .selection()
            .expect("selection after move")
            .command_name()
            .to_string();
        assert_ne!(first_name, second_name, "selection should change");
        let ghost = after
            .inline_ghost
            .as_ref()
            .expect("ghost must refresh with selection");
        assert_eq!(
            ghost.full_name, second_name,
            "arrow nav must keep ghost on the highlighted row"
        );
    }

    #[test]
    fn ghost_tracks_selection_when_skill_wins_mru_tie() {
        // Repro of the reported "/p shows ghost `pager-headless` but Tab inserts
        // `personas`" divergence: a builtin (`personas`) and an ACP skill
        // (`pager-headless`) tie on fuzzy score for `/p`, MRU favors the skill.
        // The ghost must equal the selected (Tab-accepted) row in every case.
        let mut ctrl = SlashController::new(
            CommandRegistry::new(vec![Arc::new(TieCmd("personas"))]),
            std::path::PathBuf::from("."),
        );
        ctrl.registry_mut()
            .set_acp_commands(&[agent_client_protocol::AvailableCommand::new(
                "pager-headless".to_string(),
                String::new(),
            )]);
        let mut store = mru::SlashMru::new_in_memory();
        store.seed_for_test("", "pager-headless", 1_700_000_900);
        ctrl.set_mru(std::rc::Rc::new(std::cell::RefCell::new(store)));

        let state = SlashState::default();
        let models = ModelState::default();
        ctrl.refresh(&state, "/p", 2, &models);
        let snap = state.snapshot();

        let selected = snap
            .selection()
            .expect("dropdown open")
            .command_name()
            .to_string();
        let ghost = snap.inline_ghost.as_ref().expect("ghost present for /p");
        assert_eq!(
            ghost.full_name, selected,
            "ghost must complete the selected row, not a separate ranker's pick"
        );
        assert_eq!(
            selected, "pager-headless",
            "MRU should make the recently-used skill win the /p tie"
        );
    }

    #[test]
    fn record_command_use_stores_canonical_for_alias() {
        // Default controller store is already isolated + in-memory.
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        ctrl.record_command_use("e", "exit");
        ctrl.record_command_use("q", "/quit");
        assert!(ctrl.mru_last_used("", "quit") > 0);
        assert_eq!(ctrl.mru_last_used("", "exit"), 0);
    }

    #[test]
    fn flat_mru_boosts_recent_command_regardless_of_typed_prefix() {
        // Flat schema (hermetic): using `plan` recently boosts it even when
        // typing `/p`, independent of the live builtin registry.
        let mut ctrl = tie_controller(
            &["plan", "personas"],
            &[("plan", 1_700_000_999), ("personas", 1_700_000_010)],
        );

        let state = SlashState::default();
        let models = ModelState::default();
        ctrl.refresh(&state, "/p", 2, &models);
        let snap = state.snapshot();
        let top = snap.selection().expect("dropdown").display.clone();
        assert_eq!(
            top.as_str(),
            "/plan",
            "flat MRU should surface recent /plan on /p ties, got {top}"
        );
    }

    #[test]
    fn submit_records_canonical_for_ranking() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        ctrl.record_command_use("pager-headless", "pager-headless");
        assert!(ctrl.mru_last_used("", "pager-headless") > 0);
    }

    #[test]
    fn no_ghost_for_fully_recognized_command() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "do /model now", 9, &models);
        let snapshot = state.snapshot();
        assert!(
            snapshot.inline_ghost.is_none(),
            "fully recognized command should not show ghost"
        );
        assert_eq!(snapshot.recognized_tokens.len(), 1);
        assert_eq!(snapshot.recognized_tokens[0], 3..9);
    }

    #[test]
    fn no_ghost_for_unmatched_prefix() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "do /zzzzz", 9, &models);
        let snapshot = state.snapshot();
        assert!(
            snapshot.inline_ghost.is_none(),
            "no match should produce no ghost"
        );
    }

    #[test]
    fn inline_ghost_multiline() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        // "/mod" is on line 2; ghost should still be produced with correct byte range.
        ctrl.refresh(&state, "hello\n/mod", 10, &models);
        let snapshot = state.snapshot();
        let ghost = snapshot
            .inline_ghost
            .as_ref()
            .expect("expected ghost for /mod on line 2");
        assert_eq!(ghost.full_name, "model");
        assert_eq!(ghost.text, "el");
        assert_eq!(ghost.token_range, 6..10);
    }

    #[test]
    fn recognized_tokens_highlighted() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        ctrl.refresh(&state, "run /exit and /model please", 0, &models);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.recognized_tokens.len(), 2);
    }

    #[test]
    fn recognized_token_ranges_matches_composer_refresh() {
        // Parity pin: the submit-time helper must produce exactly the ranges
        // the composer highlighted while typing (same registry + gates).
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        let text = "run /exit and /model please but not /zzzzz nor foo/bar";
        ctrl.refresh(&state, text, text.len(), &models);
        let composer = state.snapshot().recognized_tokens;

        let helper = ctrl.recognized_token_ranges(text, &models);
        assert_eq!(helper, composer);
        assert_eq!(helper, vec![4..9, 14..20]);
    }

    #[test]
    fn recognized_token_ranges_parity_in_mid_text_state_with_session_scope_hidden() {
        // Dashboard-style surface (session-scoped commands suppressed), cursor
        // in a mid-text token's args: the mid-text refresh path must apply the
        // same membership rule as the helper — /compact (session-scoped) is
        // excluded on this surface, /theme (pager-global) is highlighted.
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        ctrl.set_hide_session_scoped(true);
        let state = SlashState::default();
        let models = ModelState::default();

        let text = "do /compact then /theme now";
        ctrl.refresh(&state, text, text.len(), &models);
        let composer = state.snapshot().recognized_tokens;

        let helper = ctrl.recognized_token_ranges(text, &models);
        assert_eq!(helper, composer);
        assert_eq!(helper, vec![17..23]);

        // Cursor inside the suppressed /compact token: the under-cursor teal
        // source (command_recognized) must agree with the ranges — no teal
        // flicker while the cursor sits in a not-offered command.
        ctrl.refresh(&state, text, 11, &models);
        let snap = state.snapshot();
        assert!(
            !snap.command_recognized,
            "/compact must not be recognized mid-text on the session-less surface"
        );
        assert_eq!(snap.recognized_tokens, helper);
    }

    #[test]
    fn recognized_token_ranges_excludes_unknown_and_paths() {
        let ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let models = ModelState::default();

        assert!(
            ctrl.recognized_token_ranges("do /frobnicate now", &models)
                .is_empty(),
            "unknown /word must not be recognized"
        );
        assert!(
            ctrl.recognized_token_ranges("edit foo/bar/baz.rs", &models)
                .is_empty(),
            "path-like tokens must not be recognized"
        );
        assert!(
            ctrl.recognized_token_ranges("no tokens here", &models)
                .is_empty()
        );
    }

    #[test]
    fn mid_text_slash_on_second_line_uses_token_range() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        let text = "asdasd /implement    agine\n  /im";
        let cursor = text.len();
        ctrl.refresh(&state, text, cursor, &models);
        let snapshot = state.snapshot();
        assert!(
            snapshot.active,
            "cursor on /im should activate slash completion"
        );
        assert_eq!(
            snapshot.command_range,
            Some(29..32),
            "command_range must be the /im token on line 2, not the leading span"
        );
        assert!(
            snapshot.open || snapshot.inline_ghost.is_some(),
            "partial /im should show dropdown or inline ghost"
        );
    }

    #[test]
    fn leading_slash_on_second_line_after_first_command() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        let text = "/model\n  /im";
        let cursor = text.len();
        ctrl.refresh(&state, text, cursor, &models);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.command_range, Some(9..12));
        assert!(snapshot.cursor_in_command);
    }

    #[test]
    fn mid_text_slash_args_after_token() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        let text = "hi /model ";
        let cursor = text.len();
        ctrl.refresh(&state, text, cursor, &models);
        let snapshot = state.snapshot();
        assert!(
            !snapshot.cursor_in_command,
            "cursor in args after /model should leave command mode"
        );
        assert!(snapshot.args_range.is_some());
        assert!(
            snapshot.open || snapshot.args_placeholder.is_some(),
            "args phase should show suggestions or placeholder for /model"
        );
    }

    #[test]
    fn mid_text_cursor_in_first_slash_stays_in_command_mode() {
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        let text = "hi /imagine\n\n  /execute-plan";
        let cursor = text.find("/imagine").unwrap() + "/imagine".len();
        ctrl.refresh(&state, text, cursor, &models);
        let snapshot = state.snapshot();
        assert!(
            snapshot.cursor_in_command,
            "cursor at end of /imagine must stay in command mode for Tab"
        );
        assert_eq!(snapshot.args_range, None);
        assert_eq!(
            snapshot.command_range,
            Some(3..11),
            "Tab must target /imagine, not the later /execute-plan token"
        );
    }

    /// Fake command: empty query yields a chained "first" row (trailing
    /// space) and a terminal "second" row; "first " yields terminal rows.
    struct ChainCmd;

    impl SlashCommand for ChainCmd {
        fn name(&self) -> &str {
            "chain"
        }
        fn description(&self) -> &str {
            "test chain"
        }
        fn usage(&self) -> &str {
            "/chain <a> <b>"
        }
        fn takes_args(&self) -> bool {
            true
        }
        fn suggest_args(&self, _ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
            let item = |display: &str, match_text: &str, insert: &str| ArgItem {
                display: display.into(),
                match_text: match_text.into(),
                insert_text: insert.into(),
                description: String::new(),
                locked: false, action_id: None, hidden: false,
            };
            if let Some(rest) = args_query.strip_prefix("first")
                && rest.starts_with(char::is_whitespace)
            {
                return Some(vec![
                    item("alpha", "first alpha", "first alpha"),
                    item("beta", "first beta", "first beta"),
                ]);
            }
            Some(vec![
                item("first", "first", "first "),
                item("second", "second", "second"),
            ])
        }
        fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
            CommandResult::Handled
        }
    }

    #[test]
    fn suggest_args_chains_or_terminates_based_on_typed_query() {
        let mut ctrl = SlashController::new(
            CommandRegistry::new(vec![Arc::new(ChainCmd)]),
            std::path::PathBuf::from("."),
        );
        let state = SlashState::default();
        let models = ModelState::default();

        // Empty query: "first" chains (trailing space), "second" is terminal.
        ctrl.refresh(&state, "/chain ", 7, &models);
        let snap = state.snapshot();
        let rows: Vec<(&str, bool)> = snap
            .matches
            .iter()
            .map(|r| (r.display.as_str(), r.insert_text.ends_with(' ')))
            .collect();
        assert_eq!(rows, vec![("first", true), ("second", false)]);

        // Typing "first " triggers the phase-2 sub-menu of terminal rows.
        ctrl.refresh(&state, "/chain first ", 13, &models);
        let snap = state.snapshot();
        let rows: Vec<(&str, bool)> = snap
            .matches
            .iter()
            .map(|r| (r.display.as_str(), r.insert_text.ends_with(' ')))
            .collect();
        assert_eq!(rows, vec![("alpha", false), ("beta", false)]);
    }

    #[test]
    fn dedup_prefers_canonical_over_alias_at_equal_score() {
        // When the query matches both the canonical name and an alias
        // equally well, the dropdown should show the canonical name.
        // Regression: "terminal" used to show "/terminal-check" (alias)
        // instead of "/terminal-setup" (canonical) because the alias
        // sorts lexicographically first.
        let mut ctrl = SlashController::with_builtins(std::path::PathBuf::from("."));
        let state = SlashState::default();
        let models = ModelState::default();

        let text = "/terminal";
        ctrl.refresh(&state, text, text.len(), &models);
        let snap = state.snapshot();
        assert!(snap.open);
        let displays: Vec<&str> = snap.matches.iter().map(|r| r.display.as_str()).collect();
        assert!(
            displays.contains(&"/terminal-setup"),
            "expected /terminal-setup (canonical) in matches, got: {displays:?}"
        );
        assert!(
            !displays.contains(&"/terminal-check"),
            "/terminal-check (alias) should be deduplicated in favor of canonical"
        );
    }

    /// Command whose rows include a locked BYOK model, to exercise the
    /// scoped suggestion filter.
    struct LockedRowCmd;

    impl SlashCommand for LockedRowCmd {
        fn name(&self) -> &str {
            "lockchain"
        }
        fn description(&self) -> &str {
            "test locked rows"
        }
        fn usage(&self) -> &str {
            "/lockchain <model>"
        }
        fn takes_args(&self) -> bool {
            true
        }
        fn suggest_args(&self, _ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
            let item = |display: &str, locked: bool| ArgItem {
                display: display.into(),
                match_text: display.into(),
                insert_text: display.into(),
                description: String::new(),
                locked,
                action_id: None,
                hidden: false,
            };
            Some(vec![
                item("grok-4.5", false),
                item("🔒 DeepSeek V4 Flash — DeepSeek", true),
            ])
        }
        fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
            CommandResult::Handled
        }
    }

    #[test]
    fn arg_suggestions_empty_query_hides_locked_rows_typed_query_reveals() {
        let mut ctrl = SlashController::new(
            CommandRegistry::new(vec![Arc::new(LockedRowCmd)]),
            std::path::PathBuf::from("."),
        );
        let state = SlashState::default();
        let models = ModelState::default();

        // Empty query (scoped default): the locked BYOK row is hidden.
        ctrl.refresh(&state, "/lockchain ", 11, &models);
        let snap = state.snapshot();
        let displays: Vec<&str> = snap.matches.iter().map(|r| r.display.as_str()).collect();
        assert_eq!(displays, ["grok-4.5"]);

        // Typing a query searches across locked rows (BYOK discovery).
        ctrl.refresh(&state, "/lockchain deep", 15, &models);
        let snap = state.snapshot();
        let displays: Vec<&str> = snap.matches.iter().map(|r| r.display.as_str()).collect();
        assert!(
            displays.iter().any(|d| d.contains("DeepSeek")),
            "typed query must surface locked rows, got {displays:?}"
        );
    }
}
