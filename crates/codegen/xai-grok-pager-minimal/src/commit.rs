//! Minimal-mode commit pipeline: which finalized blocks get printed into the
//! terminal's native scrollback, and in what display mode.
//!
//! This module is deliberately terminal-agnostic and unit-testable: the actual
//! `insert_before` call is injected as a closure by the caller (the per-frame
//! draw loop, added in a later PR). The "committed frontier" is the leading
//! contiguous run of finalized, non-pending entries; everything past it stays
//! in the live region until it finalizes.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

use xai_grok_pager::app::PagerTerminal;
use xai_grok_pager::app::app_view::{ActiveView, AppView};
use xai_grok_pager::appearance::AppearanceConfig;
use xai_grok_pager::minimal_api;
use xai_grok_pager::render::Renderable;
use xai_grok_pager::scrollback::block::RenderBlock;
use xai_grok_pager::scrollback::blocks::ToolCallBlock;
use xai_grok_pager::scrollback::entry::{EntryId, ScrollbackEntry};
use xai_grok_pager::scrollback::state::ScrollbackState;
use xai_grok_pager::scrollback::types::DisplayMode;
use xai_grok_pager::scrollback::wrappers::EntryRenderer;
use xai_grok_pager::theme::Theme;

/// Blank rows emitted after each committed block — and held after each live-tail
/// entry (`super::live`) — in minimal mode.
///
/// Now `0`: adjacent blocks abut, relying on each block's own chrome (the accent
/// column + the `◆`/bullet that marks a block start) to read the boundary. A
/// full separator row per block made the transcript too airy for short
/// collapsed thinking / tool blocks (dogfood feedback), so the gap is dropped.
/// The constant is kept (rather than deleting the plumbing) so the spacing stays
/// tunable in one place.
///
/// Whatever the value, it must be applied identically on both sides of the
/// commit frontier (the committed footprint here and the live-tail footprint in
/// [`super::live::draw_tail`] / [`super::live::tail_height`]) so a block's total
/// height is unchanged when it moves from the live region into native
/// scrollback — otherwise the prompt would shift on every commit.
pub(crate) const MINIMAL_BLOCK_GAP: u16 = 0;

/// Whether the entry at the commit frontier may be committed to native
/// scrollback yet. `is_last` is whether it is the final entry in the scrollback
/// (the only one that may still be actively streaming).
///
/// While a turn is **running**, a block is committable once it has finished
/// running AND is not awaiting user input. The pending-input gate is what makes
/// the mid-stream permission hold fall out for free: a tool blocked on a
/// permission / `ask_user_question` prompt is flagged `is_pending_user_input`,
/// so the leading-run scan stops *before* it and it stays in the live region
/// until the user answers.
///
/// **Lingering-`is_running` agent messages (the "accumulate, don't cap" fix):**
/// the tracker leaves an agent message's `is_running` flag set until turn end —
/// `handle_tool_call` resets `current_agent_msg` to `None` *without* finishing
/// the entry when a tool follows. That stale flag would wedge the frontier at
/// the message, piling the rest of the turn into the fixed-height live tail
/// (which then scrolls/caps). But an agent message that has a *later* block is
/// provably complete (the tracker moved past it and will never append to it
/// again), so we commit it anyway. The commit pass finalizes it before
/// rendering, so it prints in finished form. Only agent messages get this
/// relaxation: a running **tool** may still update its result, so it keeps the
/// strict `is_running` gate, and the **last** entry always stays live.
///
/// **Background-task lifecycle blocks commit even while "running".** A fresh
/// background task is pushed as a `BgTask` "started" block with the entry's
/// `is_running` flag set (`handle_task_backgrounded` → `set_last_running(true)`),
/// but that flag only drives bullet animation — the block's *content* never
/// changes (task completion pushes a **separate** `bg_task_completed`/`failed`
/// block, and live output goes to the task store, not this entry). An async task
/// can outlive its turn, so gating it on `is_running` would wedge the commit
/// frontier for the rest of the turn: the "started" block — and everything after
/// it — would stay stuck in the live tail (scrolled out of view), so the task is
/// invisible until it finishes. Committing it immediately matches design §6.11
/// ("status blocks committed") and keeps the frontier moving.
///
/// Once the turn is **idle** (`turn_running == false`) everything except a
/// pending-user-input block is stable and committable: the tracker can also
/// leave a thinking block's `is_running` flag set after the turn ends (finalize
/// missed at a transition), and that stale flag must not permanently wedge the
/// commit frontier. The caller finalizes such entries before rendering so they
/// print in their finished form. The pending-input gate deliberately applies in
/// every turn state (see the first check below).
///
/// ⚠️ **Print-once caveat for idle-pushed running blocks:** a spinner-style
/// entry pushed as `running` while the turn is idle is committed *immediately*
/// by this rule — any later in-place fill of that entry never reaches the
/// terminal. Handlers that fill placeholders (e.g. `SessionRecap`) must check
/// `ScrollbackState::is_committed` and append a fresh block instead.
pub fn is_committable(entry: &ScrollbackEntry, turn_running: bool, is_last: bool) -> bool {
    // A block awaiting user input (permission / ask_user_question) holds the
    // frontier in EVERY turn state, not just mid-turn: its rendered form is
    // still going to change when the prompt resolves, so committing it
    // (print-once) would freeze the "waiting" form on the terminal. The idle
    // case is defensive — permissions normally resolve within the turn, but a
    // pending mark must never be committed out from under its modal.
    if entry.is_pending_user_input {
        return false;
    }
    if !turn_running {
        return true;
    }
    if !entry.is_running {
        return true;
    }
    // Running, mid-turn. Two block kinds are safe to commit despite a set
    // `is_running` flag: a BgTask lifecycle block (finalized event; flag is
    // animation-only — see above), and a non-last agent message (the tracker has
    // provably moved past it). A running **tool** may still update its result,
    // and the **last** non-BgTask entry may still be streaming, so both stay live.
    matches!(entry.block, RenderBlock::BgTask(_))
        || (!is_last && matches!(entry.block, RenderBlock::AgentMessage(_)))
}

/// The display mode a block should be committed in (minimal mode, print-once).
pub fn minimal_commit_display_mode(block: &RenderBlock) -> DisplayMode {
    match block {
        RenderBlock::ToolCall(ToolCallBlock::Edit(_)) => DisplayMode::Expanded,
        RenderBlock::ToolCall(
            tc @ (ToolCallBlock::Search(_)
            | ToolCallBlock::Read(_)
            | ToolCallBlock::ListDir(_)
            | ToolCallBlock::MemorySearch(_)
            | ToolCallBlock::IntegrationSearch(_)),
        ) if tc.is_success() => DisplayMode::Collapsed,
        RenderBlock::ToolCall(_) => DisplayMode::Truncated,
        RenderBlock::Thinking(_) => DisplayMode::Expanded,
        _ => DisplayMode::Expanded,
    }
}

/// One step of the frontier walk — the single classification shared by every
/// consumer ([`commit_leading_run`], [`scan_frontier`]). Keeping this in one
/// place is load-bearing: the commit pass, the `will_commit` resize gate, the
/// viewport sizing (`tail_height`), and the tail renderer must all agree on
/// where the frontier stops, or a block's height flips between the live region
/// and native scrollback and the prompt jumps on commit.
enum Step {
    /// Uncommitted and committable: a commit pass consumes it.
    Commit,
    /// Already committed: skip over it (the id-set is authoritative; the scan
    /// cursor is only a lower-bound hint).
    Skip,
    /// End of entries, or the first uncommitted non-committable entry — the
    /// live tail starts here.
    Stop,
}

/// Classify the entry at `i` relative to the commit frontier.
fn classify(state: &ScrollbackState, i: usize, turn_running: bool) -> Step {
    let is_last = i + 1 >= state.len();
    match state.get(i) {
        None => Step::Stop,
        Some(e) if minimal_api::is_committed(state, e) => Step::Skip,
        Some(e) if !is_committable(e, turn_running, is_last) => Step::Stop,
        Some(_) => Step::Commit,
    }
}

/// Read-only projection of what a commit pass would do, for the consumers that
/// must agree with it without running it.
pub struct FrontierScan {
    /// Index of the first entry a commit pass would NOT consume — where the
    /// live tail starts after this frame's commit. Everything from here on
    /// stays in the pinned live region.
    pub tail_start: usize,
    /// Whether a commit pass would emit at least one block into native
    /// scrollback this frame.
    pub will_commit: bool,
}

/// Walk the frontier read-only (no cursor mutation, nothing marked committed).
///
/// Used by the overlay host's viewport sizing ([`super::overlay::sync_viewport`]
/// via [`super::live::tail_height`]) and its commit gate — both run *before*
/// [`commit_active`] in the frame and must mirror its stop condition exactly.
pub fn scan_frontier(state: &ScrollbackState, turn_running: bool) -> FrontierScan {
    let mut i = minimal_api::commit_scan_cursor(state);
    let mut will_commit = false;
    loop {
        match classify(state, i, turn_running) {
            Step::Stop => break,
            Step::Skip => i += 1,
            Step::Commit => {
                will_commit = true;
                i += 1;
            }
        }
    }
    FrontierScan {
        tail_start: i,
        will_commit,
    }
}

/// Commit the leading contiguous run of newly-committable entries (those past
/// the scan cursor that are finalized and not pending), in insertion order.
///
/// For each such entry, `on_commit(state, index)` runs first — the caller
/// finalizes/stamps the entry and renders it into native scrollback — and
/// **only if it returns `true`** is the entry marked committed. A `false`
/// return (the terminal write failed) stops the walk with the entry still
/// uncommitted and the cursor before it, so the next frame retries instead of
/// marking a block committed that never reached the terminal (a print-once
/// mode can never re-emit it — the block would silently vanish; bugbot).
///
/// The scan stops at the first still-running / pending entry (so a turn
/// streams smoothly and a sibling tool awaiting permission holds the
/// frontier). Returns the number of entries committed.
///
/// This is the ONE mutating frontier walk; [`commit_active`] drives it in
/// production and the unit tests below drive it directly, so the tested loop
/// and the production loop cannot drift.
pub fn commit_leading_run(
    state: &mut ScrollbackState,
    turn_running: bool,
    mut on_commit: impl FnMut(&mut ScrollbackState, usize) -> bool,
) -> usize {
    let mut i = minimal_api::commit_scan_cursor(state);
    let mut count = 0usize;
    loop {
        match classify(state, i, turn_running) {
            Step::Stop => break,
            Step::Skip => i += 1,
            Step::Commit => {
                if !on_commit(state, i) {
                    break; // emit failed — leave uncommitted, retry next frame
                }
                minimal_api::mark_committed(state, i);
                count += 1;
                i += 1;
            }
        }
    }
    minimal_api::set_commit_scan_cursor(state, i);
    count
}

/// Appearance used when committing blocks to native scrollback.
///
/// Timestamps are forced off: `EntryRenderer::desired_height` subtracts the
/// timestamp column but `render` treats it as an overlay, so leaving timestamps
/// on would make the reserved `insert_before` height disagree with the painted
/// rows (design decision K5). Block horizontal padding is zeroed so committed
/// content is flush-left with the welcome card (which paints edge-to-edge);
/// paired with [`EntryRenderer::with_hide_accent`] reclaiming the accent
/// column, glyphs start at column 0. The live region's prompt / status /
/// info rows mirror that via [`super::live::live_left_inset`].
pub(crate) fn committed_appearance(base: &AppearanceConfig) -> AppearanceConfig {
    let mut a = base.clone();
    a.show_timestamps = false;
    // Flush-left minimal look: no block horizontal padding (align with the
    // welcome card, which paints edge-to-edge with no outer h-pad).
    a.scrollback.layout.block_pad_left = 0;
    a.scrollback.layout.block_pad_right = 0;
    a
}

/// Build the renderer used for a committed (print-once) block: no selection
/// highlight, a static tick (no running-wave animation), timestamps off.
fn committed_renderer<'a>(
    entry: &'a ScrollbackEntry,
    theme: &'a Theme,
    appearance: AppearanceConfig,
    cwd: &'a std::path::Path,
) -> EntryRenderer<'a> {
    EntryRenderer::new(entry, theme)
        .with_appearance(appearance)
        .with_cwd(Some(cwd))
        .with_tick(0)
        // Blend committed blocks with the real terminal background (no
        // user-message `bg_light` band etc.).
        .with_flat_background(true)
        // Drop the left accent bar for a cleaner, un-gutter'd minimal look; the
        // per-block `◆`/bullet marker still reads the block boundary.
        .with_hide_accent(true)
}

/// Emit one committed block into native scrollback via `insert_before`, capping
/// its height at `max_rows` (0 = unbounded).
///
/// "Diffs always full" (K9) means a multi-thousand-line `Edit` would otherwise
/// allocate one `Buffer` of `desired_height` rows and emit a huge writer-thread
/// send burst (§6.15). When the block is taller than `max_rows`, only the top
/// `max_rows - 1` content rows are committed and the final row becomes a
/// `… N more lines — /transcript to view` footer. The block is laid out at its
/// **full** `desired_height` so wrapping is byte-identical to an uncapped commit
/// (K5); the `insert_before` buffer is only `commit_h` rows tall, so any content
/// past it is clipped — bounding the allocation to the cap.
fn insert_committed(
    terminal: &mut PagerTerminal,
    renderer: EntryRenderer<'_>,
    width: u16,
    max_rows: u16,
    footer_style: Style,
) -> std::io::Result<()> {
    let full_h = renderer.desired_height(width);
    if full_h == 0 {
        return Ok(());
    }
    let commit_h = if max_rows > 0 && full_h > max_rows {
        max_rows
    } else {
        full_h
    };
    // Propagated (not swallowed): the caller must NOT mark the entry committed
    // when the terminal write failed — print-once means a marked-but-unprinted
    // block can never be emitted again (bugbot).
    terminal.insert_before(commit_h, move |buf| {
        paint_committed(buf, renderer, width, full_h, footer_style);
    })?;
    insert_gap(terminal);
    Ok(())
}

/// Emit [`MINIMAL_BLOCK_GAP`] blank rows into native scrollback as the trailing
/// gap after a committed block. The rows are left unpainted, so they inherit the
/// terminal's own background (matching the flat, transparent committed look).
pub(super) fn insert_gap(terminal: &mut PagerTerminal) {
    if MINIMAL_BLOCK_GAP == 0 {
        return;
    }
    let _ = terminal.insert_before(MINIMAL_BLOCK_GAP, |_buf| {});
}

/// Paint a committed block into `buf` (a `commit_h`-row buffer), laying it out at
/// its full `full_h` so wrapping matches an uncapped commit exactly (K5). When
/// `buf` is shorter than `full_h` the block is capped: rows past it are clipped
/// and the final row becomes a `… N more lines — /transcript to view` footer
/// (§6.15). Extracted from [`insert_committed`] so the cap is unit-testable
/// without a live terminal.
fn paint_committed(
    buf: &mut ratatui::buffer::Buffer,
    renderer: EntryRenderer<'_>,
    width: u16,
    full_h: u16,
    footer_style: Style,
) {
    let commit_h = buf.area.height;
    let area = Rect {
        x: buf.area.x,
        y: buf.area.y,
        width,
        height: full_h,
    };
    renderer.render(area, buf);
    if commit_h > 0 && commit_h < full_h {
        // Top `commit_h - 1` rows are content; the last row is the footer.
        let hidden = full_h.saturating_sub(commit_h.saturating_sub(1));
        let y = buf.area.y + commit_h - 1;
        let row = Rect {
            x: buf.area.x,
            y,
            width,
            height: 1,
        };
        // Dim default-fg chrome (not hard-coded DarkGray) under terminal-native.
        let style = footer_style.bg(Color::Reset);
        // Clear any clipped content that landed on the footer row first.
        buf.set_style(row, style);
        let text = format!("\u{2026} {hidden} more lines \u{2014} /transcript to view");
        buf.set_span(buf.area.x, y, &Span::styled(text, style), width);
    }
}

/// Commit the active agent's newly-finalized blocks into native scrollback.
///
/// For each entry in the leading committable run: stamp its print-once display
/// mode (K9), then `insert_before` it using the shared `EntryRenderer` at
/// exactly `desired_height(width)` rows (K5).
///
/// On resume/attach (`loading_replay`) the replayed transcript is printed into
/// native scrollback like any other finalized block — minimal has no separate
/// history pane, so the terminal's scrollback *is* the history; without this a
/// resumed session looks empty (nothing redrawn). The commit frontier
/// (`committed` flags + `commit_scan_cursor`) still guarantees each block prints
/// exactly once.
pub fn commit_active(app: &mut AppView, terminal: &mut PagerTerminal) {
    let id = match &app.active_view {
        ActiveView::Agent(id) => *id,
        _ => return, // welcome / dashboard: nothing to commit
    };
    // Snapshot the commit appearance before borrowing `agents` mutably.
    let appearance = committed_appearance(&app.appearance);
    let Some(agent) = app.agents.get_mut(&id) else {
        return;
    };
    // Hold commits while a centered fullscreen app-modal (settings) is open: it
    // takes the whole live region, so an `insert_before` underneath it would
    // scroll the popup. Deferred commits flush on the next frame after it closes.
    if super::overlay::app_modal_active(agent) {
        return;
    }
    // NB: `sync_pending_user_input_marks` already ran at the top of the frame
    // ([`sync_pending_marks`], called from `crate::draw` BEFORE the viewport
    // sizing) so the sizing pass and this commit pass judge committability
    // against the same marks — syncing here made a tool look committable to
    // `sync_viewport`/`tail_height` on the very frame its permission arrived.
    //
    // Whether a turn is actively running. When idle, every remaining entry is
    // stable and committable (see `is_committable`); a stale `is_running` flag
    // left by the tracker must not wedge the frontier.
    let turn_running = agent.session.state.is_turn_running();
    let cwd = agent.session.cwd.as_path();
    let sb = &mut agent.scrollback;

    // NB: resume/attach replay (`agent.session.loading_replay`) intentionally
    // falls through to the normal commit pass below, so the loaded transcript is
    // printed into native scrollback (a resumed session must be visible).

    let theme = Theme::current();
    let footer_style = theme.dim();
    let max_rows = appearance.minimal_max_commit_rows;
    let width = terminal.viewport_area().width;
    if width == 0 {
        return;
    }

    // Drive the ONE frontier walk (`commit_leading_run` — also what the unit
    // tests exercise) with the production per-entry work: finalize, stamp the
    // print-once display mode, print, then remember folded blocks for Ctrl+E.
    commit_leading_run(sb, turn_running, |sb, i| {
        // If the turn is idle but this entry still carries a stale
        // `is_running` flag, finalize it first so it renders in its
        // finished form (e.g. "Thought for Xs", not an animated "Thinking…").
        if let Some(id) = sb.get(i).filter(|e| e.is_running).map(|e| e.id) {
            sb.finish_running(id);
        }
        // Stamp the print-once display mode before measuring/rendering.
        if let Some(e) = sb.get_mut(i) {
            let mode = minimal_commit_display_mode(&e.block);
            e.set_display_mode(mode);
        }
        if let Some(e) = sb.get(i) {
            // `insert_committed` pushes these rows above the pinned viewport,
            // into the terminal's own scrollback (capped — §6.15). A failed
            // write returns `false` so the walk leaves the entry uncommitted
            // (retried next frame) instead of marking a never-printed block.
            //
            // NOTE (print-once contract): from a successful insert on, the
            // entry's content is frozen on the user's terminal. Mutating it in
            // place later (`get_by_id_mut` + edit, the `/recap` fill pattern)
            // will NOT reach the screen — append a fresh block instead (see
            // the `SessionRecap` handler in `acp_handler.rs`).
            let renderer = committed_renderer(e, &theme, appearance.clone(), cwd);
            if insert_committed(terminal, renderer, width, max_rows, footer_style).is_err() {
                return false;
            }
        }
        // Remember folded blocks (collapsed reasoning / truncated output) so
        // `Ctrl+E` / `/expand` can re-print them in full later (K10) — only
        // after the print actually succeeded.
        if let Some((id, mode)) = sb.get(i).map(|e| (e.id, e.display_mode()))
            && matches!(mode, DisplayMode::Collapsed | DisplayMode::Truncated)
        {
            minimal_api::record_committed_for_expand(sb, id);
        }
        true
    });

    // Stamp the still-uncommitted "live tail" entries with the same print-once
    // display policy they will commit with (collapsed reasoning, truncated tool
    // output, full diffs/messages). The tail renders each entry at its current
    // `display_mode`, but blocks stream Expanded and commit folded — so without
    // this the live region is tall while a block streams and snaps short the
    // instant it finalizes, jerking the prompt upward (dogfood nit). Matching
    // the tail height to the committed height keeps the prompt put across a
    // commit. Idempotent: `set_display_mode` no-ops when unchanged.
    let mut j = minimal_api::commit_scan_cursor(sb);
    while let Some(e) = sb.get_mut(j) {
        let mode = minimal_commit_display_mode(&e.block);
        e.set_display_mode(mode);
        j += 1;
    }
}

/// Re-print the entries queued by `Ctrl+E` / `/expand` into native scrollback,
/// fully expanded, below the committed conversation (design decision K10).
///
/// Committed terminal text cannot be mutated in place, so "expanding" a folded
/// block is an honest re-print of the same entry in `Expanded` mode. The entry
/// itself is already committed and past the scan cursor, so flipping its display
/// mode has no effect on the live tail.
///
/// The re-print is **uncapped** (`max_rows = 0`): the initial commit truncated
/// the block under `minimal_max_commit_rows`, and this is the explicit "show me
/// the whole thing" action — capping it again just reprinted the same footer
/// (bugbot). A one-shot user-initiated tall insert is an acceptable burst.
pub fn expand_pending(app: &mut AppView, terminal: &mut PagerTerminal) {
    if minimal_api::minimal_pending_expand(app).is_empty() {
        return;
    }
    let id = match &app.active_view {
        ActiveView::Agent(id) => *id,
        _ => return,
    };
    let width = terminal.viewport_area().width;
    if width == 0 {
        return;
    }
    let appearance = committed_appearance(&app.appearance);
    // Guards: a missing active agent must leave the IDs queued, so confirm it
    // exists before consuming the queue below (the queue take needs `&mut app`,
    // which can't overlap the agent borrow — hence the check-then-reborrow).
    // Likewise hold the whole queue while a centered app-modal owns the live
    // region — an `insert_before` would scroll the popup and the user wouldn't
    // see the re-print (same hold as `commit_active`; bugbot).
    match app.agents.get(&id) {
        Some(agent) if !super::overlay::app_modal_active(agent) => {}
        _ => return,
    }
    let theme = Theme::current();
    let footer_style = theme.dim();
    // Consume the expand queue only after every guard above has passed: a
    // non-agent active view, a 0-width (probe) frame, or an open app-modal must
    // leave the IDs queued for a later frame, not silently drop a Ctrl+E /
    // /expand request.
    let ids = minimal_api::take_minimal_pending_expand(app);
    let mut requeue: Vec<EntryId> = Vec::new();
    {
        let Some(agent) = app.agents.get_mut(&id) else {
            // Can't happen (existence checked just above, nothing in between
            // can remove the agent) — but if it ever does, the drained queue
            // must go back rather than silently vanish.
            minimal_api::requeue_minimal_pending_expand(app, ids);
            return;
        };
        let cwd = agent.session.cwd.as_path();
        let sb = &mut agent.scrollback;
        let mut iter = ids.into_iter();
        while let Some(eid) = iter.next() {
            let Some(idx) = sb.index_of_id(eid) else {
                continue; // entry removed (rewind / clear) since the keypress
            };
            if let Some(e) = sb.get_mut(idx) {
                e.set_display_mode(DisplayMode::Expanded);
            }
            if let Some(e) = sb.get(idx) {
                let renderer = committed_renderer(e, &theme, appearance.clone(), cwd);
                if insert_committed(terminal, renderer, width, 0, footer_style).is_err() {
                    // Terminal write failed: keep this id and the rest queued
                    // so the request retries next frame instead of vanishing.
                    requeue.push(eid);
                    requeue.extend(iter);
                    break;
                }
            }
        }
    }
    if !requeue.is_empty() {
        minimal_api::requeue_minimal_pending_expand(app, requeue);
    }
}

/// Re-mark tool entries that are blocked on a pending permission/question so
/// the frontier holds them in the live region (the full TUI does this each
/// frame in `AgentView::draw`, which minimal bypasses); `is_committable` reads
/// `is_pending_user_input`.
///
/// Called at the TOP of the frame (from [`crate::draw`]), before
/// [`super::overlay::sync_viewport`]: the viewport sizing, the `will_commit`
/// gate, and the commit pass must all judge committability against the same
/// marks — syncing inside the commit pass let a just-arrived permission's tool
/// look committable to the sizing walk for one frame (bugbot).
pub fn sync_pending_marks(app: &mut AppView) {
    if let ActiveView::Agent(id) = &app.active_view
        && let Some(agent) = app.agents.get_mut(id)
    {
        minimal_api::sync_pending_user_input_marks(agent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    use xai_grok_pager::scrollback::block::RenderBlock;
    use xai_grok_pager::scrollback::entry::ScrollbackEntry;
    use xai_grok_pager::scrollback::state::ScrollbackState;

    fn test_cwd() -> &'static std::path::Path {
        std::path::Path::new("/test/session")
    }

    fn finalized(text: &str) -> ScrollbackEntry {
        ScrollbackEntry::new(RenderBlock::stub(text, Color::Blue))
    }

    fn running(text: &str) -> ScrollbackEntry {
        ScrollbackEntry::running(RenderBlock::stub(text, Color::Blue))
    }

    /// Run a commit pass for a RUNNING turn, returning the emitted indices.
    fn commit_collect(state: &mut ScrollbackState) -> Vec<usize> {
        let mut seen = Vec::new();
        commit_leading_run(state, true, |_, i| {
            seen.push(i);
            true
        });
        seen
    }

    #[test]
    fn commits_leading_finalized_run_and_stops_at_running() {
        let mut s = ScrollbackState::new();
        s.push(finalized("a"));
        s.push(finalized("b"));
        s.push(running("c"));
        s.push(finalized("d")); // after the running block — must NOT commit yet

        assert_eq!(commit_collect(&mut s), vec![0, 1]);
        assert_eq!(minimal_api::commit_scan_cursor(&s), 2);
        assert!(minimal_api::is_committed(&s, s.get(0).unwrap()));
        assert!(minimal_api::is_committed(&s, s.get(1).unwrap()));
        assert!(!minimal_api::is_committed(&s, s.get(2).unwrap()));
        assert!(!minimal_api::is_committed(&s, s.get(3).unwrap()));

        // Finalize "c"; the next pass commits "c" then "d".
        s.get_mut(2).unwrap().mark_completed();
        assert_eq!(commit_collect(&mut s), vec![2, 3]);
        assert_eq!(minimal_api::commit_scan_cursor(&s), 4);
    }

    #[test]
    fn pending_user_input_holds_the_frontier() {
        let mut s = ScrollbackState::new();
        s.push(finalized("a"));
        let tool = s.push(finalized("tool")); // finalized but awaiting permission
        s.push(finalized("after"));
        assert!(s.set_pending_user_input(tool, true));

        // Stops before the pending tool, even though it (and "after") are finalized.
        assert_eq!(commit_collect(&mut s), vec![0]);

        // Resolving the prompt releases the rest of the run.
        assert!(s.set_pending_user_input(tool, false));
        assert_eq!(commit_collect(&mut s), vec![1, 2]);
    }

    #[test]
    fn running_agent_message_commits_once_a_later_block_exists() {
        // The tracker leaves an agent message's `is_running` flag set until turn
        // end (handle_tool_call resets current_agent_msg without finishing the
        // entry when a tool follows). Minimal must still commit that message
        // mid-turn once a later block proves it's complete — otherwise the rest
        // of the turn piles up in the fixed-height live tail and scrolls instead
        // of accumulating into native scrollback.
        let mut s = ScrollbackState::new();
        s.push(ScrollbackEntry::running(RenderBlock::agent_message(
            "answer text",
        )));

        // While it's the last entry it may still be streaming → stays live.
        assert_eq!(commit_collect(&mut s), Vec::<usize>::new());
        assert_eq!(minimal_api::commit_scan_cursor(&s), 0);

        // A later block (the tracker moved on) proves the message is done → it
        // commits even though its is_running flag still lingers. The new
        // last/running entry stays in the live tail.
        s.push(running("tool"));
        assert_eq!(commit_collect(&mut s), vec![0]);
        assert!(minimal_api::is_committed(&s, s.get(0).unwrap()));
        assert!(!minimal_api::is_committed(&s, s.get(1).unwrap()));
    }

    #[test]
    fn running_tool_still_holds_the_frontier_even_with_a_later_block() {
        // The agent-message relaxation must NOT extend to tools: a running tool
        // can still update its result, so committing it (print-once) would lose
        // the update. It holds the frontier regardless of later blocks.
        let mut s = ScrollbackState::new();
        s.push(finalized("a"));
        s.push(running("running tool")); // stub == not an AgentMessage
        s.push(finalized("after"));
        assert_eq!(commit_collect(&mut s), vec![0]);
        assert_eq!(minimal_api::commit_scan_cursor(&s), 1);
    }

    #[test]
    fn bg_task_started_commits_while_running_and_does_not_wedge_frontier() {
        // A fresh background task is pushed as a running "started" block
        // (`set_last_running(true)`). Its `is_running` flag is animation-only —
        // the block is a finalized lifecycle event whose content never changes —
        // so it must commit immediately even mid-turn. Otherwise it wedges the
        // frontier and the task (plus everything after it) stays hidden in the
        // live tail until the task finishes (the reported dogfood bug).
        let mut s = ScrollbackState::new();
        s.push(finalized("a"));
        s.push(ScrollbackEntry::running(RenderBlock::bg_task(
            "sleep 60", "task-1",
        )));
        s.push(running("later tool")); // more turn output after the bg task

        // "a" + the running bg task commit; only the trailing running tool stays.
        assert_eq!(commit_collect(&mut s), vec![0, 1]);
        assert!(minimal_api::is_committed(&s, s.get(1).unwrap()));
        assert!(!minimal_api::is_committed(&s, s.get(2).unwrap()));
    }

    #[test]
    fn bg_task_started_commits_as_last_running_entry() {
        // Even as the last entry of a still-running turn the bg "started" block
        // commits — a lifecycle block never streams more content (completion is
        // a separate block).
        let mut s = ScrollbackState::new();
        s.push(finalized("a"));
        s.push(ScrollbackEntry::running(RenderBlock::bg_task(
            "sleep 60", "task-1",
        )));
        assert_eq!(commit_collect(&mut s), vec![0, 1]);
    }

    #[test]
    fn no_double_commit_after_mid_list_shift_remove() {
        let mut s = ScrollbackState::new();
        let a = s.push(finalized("a"));
        s.push(finalized("b"));
        s.push(finalized("c"));
        assert_eq!(commit_collect(&mut s), vec![0, 1, 2]);
        assert_eq!(minimal_api::commit_scan_cursor(&s), 3);

        // Remove an already-committed entry below the cursor (shift_remove shifts
        // the remaining indices down). The cursor is clamped; the per-entry
        // `committed` flags travel with "b"/"c", so neither is re-emitted.
        assert!(s.remove_entry(a));
        s.push(finalized("d")); // now at index 2

        assert_eq!(commit_collect(&mut s), vec![2]);
        assert!(minimal_api::is_committed(&s, s.get(0).unwrap())); // b
        assert!(minimal_api::is_committed(&s, s.get(1).unwrap())); // c
        assert!(minimal_api::is_committed(&s, s.get(2).unwrap())); // d
    }

    #[test]
    fn mid_list_removal_below_cursor_does_not_strand_uncommitted_entries() {
        // Regression (review bug 2): a committed placeholder ("Loading
        // session...") is removed AFTER new uncommitted entries were appended
        // past the cursor — the `/resume` / reconnect `SessionLoaded` ordering.
        // Removing below the cursor shifts the uncommitted entries down one;
        // without the cursor decrement in `remove_entry` the first of them
        // slid below the cursor and was never committed NOR drawn in the live
        // tail (silently missing from minimal mode).
        let mut s = ScrollbackState::new();
        s.push(finalized("old-1"));
        let placeholder = s.push(finalized("Loading session..."));

        // A draw commits both; cursor = 2.
        let mut seen = Vec::new();
        commit_leading_run(&mut s, false, |_, i| {
            seen.push(i);
            true
        });
        assert_eq!(seen, vec![0, 1]);
        assert_eq!(minimal_api::commit_scan_cursor(&s), 2);

        // Replay appends entries, then the placeholder is removed in the same
        // event cycle (before the next commit pass).
        s.push(finalized("replayed-A"));
        s.push(finalized("replayed-B"));
        assert!(s.remove_entry(placeholder));
        // The cursor moved down with the shifted entries.
        assert_eq!(minimal_api::commit_scan_cursor(&s), 1);

        // The next pass commits BOTH replayed entries — none stranded.
        let mut seen = Vec::new();
        commit_leading_run(&mut s, false, |_, i| {
            seen.push(i);
            true
        });
        assert_eq!(seen, vec![1, 2]);
        assert!(minimal_api::is_committed(&s, s.get(1).unwrap())); // replayed-A
        assert!(minimal_api::is_committed(&s, s.get(2).unwrap())); // replayed-B
    }

    #[test]
    fn pending_user_input_holds_the_frontier_even_when_idle() {
        // A block awaiting a permission / question answer must never commit,
        // even if the turn state reads idle (e.g. a prompt outliving its turn):
        // its rendered form still changes when the prompt resolves, and a
        // committed copy is frozen. The idle relaxation only applies to
        // *stale-running* flags, not pending-input marks.
        let mut s = ScrollbackState::new();
        s.push(finalized("a"));
        let tool = s.push(finalized("tool"));
        assert!(s.set_pending_user_input(tool, true));

        let mut seen = Vec::new();
        commit_leading_run(&mut s, false, |_, i| {
            seen.push(i);
            true
        });
        assert_eq!(seen, vec![0], "pending entry must hold the frontier");
        assert!(!minimal_api::is_committed(&s, s.get(1).unwrap()));

        // Resolving the prompt releases it.
        assert!(s.set_pending_user_input(tool, false));
        let mut seen = Vec::new();
        commit_leading_run(&mut s, false, |_, i| {
            seen.push(i);
            true
        });
        assert_eq!(seen, vec![1]);
    }

    #[test]
    fn failed_emit_leaves_entry_uncommitted_for_retry() {
        // Regression (bugbot "Committed flag set on IO failure"): a terminal
        // write failure must NOT mark the entry committed — print-once means a
        // marked-but-unprinted block can never be emitted again. The walk stops
        // with the cursor before the failed entry and retries next frame.
        let mut s = ScrollbackState::new();
        s.push(finalized("a"));
        s.push(finalized("b"));

        // First pass: the emit fails on the first entry.
        let mut calls = 0usize;
        let n = commit_leading_run(&mut s, false, |_, _| {
            calls += 1;
            false
        });
        assert_eq!(n, 0, "nothing committed on failure");
        assert_eq!(calls, 1, "walk stops at the first failure");
        assert!(!minimal_api::is_committed(&s, s.get(0).unwrap()));
        assert_eq!(minimal_api::commit_scan_cursor(&s), 0, "cursor holds");

        // Retry pass succeeds and commits both.
        let n = commit_leading_run(&mut s, false, |_, _| true);
        assert_eq!(n, 2);
        assert!(minimal_api::is_committed(&s, s.get(0).unwrap()));
        assert!(minimal_api::is_committed(&s, s.get(1).unwrap()));
    }

    #[test]
    fn scan_frontier_mirrors_commit_leading_run() {
        // `scan_frontier` (read-only: viewport sizing + the will-commit gate)
        // must agree exactly with the mutating walk, in every phase.
        let mut s = ScrollbackState::new();
        s.push(finalized("a"));
        s.push(finalized("b"));
        s.push(running("c"));
        s.push(finalized("d"));

        // Pre-commit: the pass would commit a+b and stop at the running entry.
        let scan = scan_frontier(&s, true);
        assert!(scan.will_commit);
        assert_eq!(scan.tail_start, 2);

        let n = commit_leading_run(&mut s, true, |_, _| true);
        assert_eq!(n, 2);
        assert_eq!(minimal_api::commit_scan_cursor(&s), scan.tail_start);

        // Post-commit: nothing left to commit; the tail starts at the cursor.
        let scan = scan_frontier(&s, true);
        assert!(!scan.will_commit);
        assert_eq!(scan.tail_start, 2);

        // Idle with no entries pending: everything committable.
        let scan = scan_frontier(&s, false);
        assert!(scan.will_commit);
        assert_eq!(scan.tail_start, 4);
    }

    #[test]
    fn remove_from_below_frontier_then_push_still_commits() {
        let mut s = ScrollbackState::new();
        s.push(finalized("a"));
        s.push(finalized("b"));
        s.push(finalized("c"));
        assert_eq!(commit_collect(&mut s), vec![0, 1, 2]);

        // Rewind: drop everything from index 1 (keep only "a"). Without the
        // cursor clamp this would strand the cursor at 3 and silently skip the
        // next pushes.
        let removed = s.remove_from(1);
        assert_eq!(removed.len(), 2);
        assert_eq!(minimal_api::commit_scan_cursor(&s), 1);

        s.push(finalized("d")); // index 1
        assert_eq!(commit_collect(&mut s), vec![1]);
    }

    #[test]
    fn btw_block_emits_once_across_repeated_frontier_passes() {
        let mut s = ScrollbackState::new();
        s.push(ScrollbackEntry::new(RenderBlock::Btw(
            xai_grok_pager::scrollback::blocks::BtwBlock::new(
                "original question",
                "original answer",
            ),
        )));

        let mut emitted = Vec::new();
        assert_eq!(
            commit_leading_run(&mut s, false, |state, i| {
                let RenderBlock::Btw(block) = &state.get(i).unwrap().block else {
                    panic!("expected Btw block")
                };
                assert_eq!(block.question, "original question");
                assert_eq!(block.content().text(), "original answer");
                emitted.push(i);
                true
            }),
            1
        );
        assert!(minimal_api::is_committed(&s, s.get(0).unwrap()));

        assert_eq!(
            commit_leading_run(&mut s, false, |_, i| {
                emitted.push(i);
                true
            }),
            0
        );
        assert_eq!(emitted, vec![0]);
        assert!(!scan_frontier(&s, false).will_commit);
    }

    #[test]
    fn commit_leading_run_advances_frontier_and_marks_committed_once() {
        let mut s = ScrollbackState::new();
        s.push(finalized("h1"));
        s.push(finalized("h2"));
        s.push(finalized("h3"));

        // Advances the frontier, marking the leading finalized run committed.
        let mut emitted = Vec::new();
        let n = commit_leading_run(&mut s, false, |_, i| {
            emitted.push(i);
            true
        });
        assert_eq!(n, 3);
        assert_eq!(emitted, vec![0, 1, 2]);
        assert_eq!(minimal_api::commit_scan_cursor(&s), 3);
        assert!((0..3).all(|i| minimal_api::is_committed(&s, s.get(i).unwrap())));

        // A second pass commits nothing (already-committed entries are skipped).
        let mut again = Vec::new();
        commit_leading_run(&mut s, false, |_, i| {
            again.push(i);
            true
        });
        assert!(again.is_empty());
    }

    #[test]
    fn idle_turn_commits_past_stale_running_entry() {
        // Regression for the missing-edit/stuck-spinner bug: the agent tracker
        // can leave an entry's `is_running` flag set after the turn ends (e.g. a
        // thinking block whose finalize was missed at the thinking→tool
        // transition). While the turn runs, that entry correctly holds the
        // frontier; once the turn is idle the frontier must advance past it.
        let mut s = ScrollbackState::new();
        s.push(finalized("a"));
        s.push(running("stale")); // stale is_running flag
        s.push(finalized("c"));

        // Running turn: blocked at the running entry.
        let mut seen = Vec::new();
        commit_leading_run(&mut s, true, |_, i| {
            seen.push(i);
            true
        });
        assert_eq!(seen, vec![0]);
        assert_eq!(minimal_api::commit_scan_cursor(&s), 1);

        // Idle turn: commit everything past the stale flag.
        let mut seen = Vec::new();
        commit_leading_run(&mut s, false, |_, i| {
            seen.push(i);
            true
        });
        assert_eq!(seen, vec![1, 2]);
        assert_eq!(minimal_api::commit_scan_cursor(&s), 3);
    }

    #[test]
    fn clear_resets_the_frontier() {
        let mut s = ScrollbackState::new();
        s.push(finalized("a"));
        commit_collect(&mut s);
        assert_eq!(minimal_api::commit_scan_cursor(&s), 1);

        s.clear();
        assert_eq!(minimal_api::commit_scan_cursor(&s), 0);
    }

    /// Height-exactness guard (design K5 / risk #1). `commit_active` reserves
    /// exactly `desired_height(width)` rows via `insert_before`; if `render`
    /// paints real content beyond that, those rows are silently clipped (lost)
    /// from native scrollback. Render each block type into an over-tall buffer
    /// and assert no non-space glyph lands past `desired_height`. (Background
    /// fill of blank spaces past `h` is fine — only real content matters.)
    fn assert_committed_fits(label: &str, block: RenderBlock, width: u16) {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let mut entry = ScrollbackEntry::new(block);
        entry.set_display_mode(minimal_commit_display_mode(&entry.block));
        let theme = Theme::current();
        let appearance = committed_appearance(&AppearanceConfig::default());

        let renderer = committed_renderer(&entry, &theme, appearance, test_cwd());
        let h = renderer.desired_height(width);
        assert!(h > 0, "{label}@{width}: desired_height was 0");
        // The accent bar and background fill intentionally stretch to the given
        // area height (chrome, not content). Only the content columns
        // (x >= chrome_width) carry real text that `insert_before` would clip.
        let chrome = renderer.chrome_width();

        let extra = 8u16;
        let area = Rect::new(0, 0, width, h + extra);
        let mut buf = Buffer::empty(area);
        renderer.render(area, &mut buf);

        for y in h..(h + extra) {
            for x in chrome..width {
                let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                assert!(
                    sym.trim().is_empty(),
                    "{label}@{width}: content {sym:?} painted at row {y} col {x}, past \
                     desired_height {h} — insert_before would clip it from scrollback"
                );
            }
        }
    }

    #[test]
    fn committed_renderer_uses_owning_session_cwd_for_tool_paths() {
        use ratatui::buffer::Buffer;

        let cwd = std::path::Path::new("/alternate/worktree");
        let mut entry =
            ScrollbackEntry::new(RenderBlock::edit("/alternate/worktree/src/main.rs", None));
        entry.set_display_mode(DisplayMode::Expanded);
        let theme = Theme::current();
        let appearance = committed_appearance(&AppearanceConfig::default());
        let renderer = committed_renderer(&entry, &theme, appearance, cwd);
        let width = 80;
        let height = renderer.desired_height(width);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        renderer.render(area, &mut buf);

        let mut text = String::new();
        for y in 0..height {
            for x in 0..width {
                text.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(text.contains("src/main.rs"), "rendered text: {text:?}");
        assert!(
            !text.contains("/alternate/worktree"),
            "session prefix should be elided: {text:?}"
        );
    }

    #[test]
    fn committed_blocks_fit_desired_height() {
        // Thinking blocks render zero rows unless `show_thinking_blocks` is on.
        // The toggle is a thread-local, so pin it on here so the thinking
        // block's committed height is actually exercised.
        minimal_api::set_show_thinking_blocks(true);

        let long = "Hello there — this is a longer message that should wrap across \
                    several lines at narrow widths to exercise the wrapping math, with \
                    enough words to overflow eighty columns comfortably.";
        for width in [40u16, 80, 120] {
            assert_committed_fits("user_prompt", RenderBlock::user_prompt(long), width);
            assert_committed_fits(
                "agent_message",
                RenderBlock::agent_message(format!(
                    "{long}\n\n- bullet one\n- bullet two\n\n```rust\nfn main() {{}}\n```"
                )),
                width,
            );
            assert_committed_fits("thinking", RenderBlock::thinking(long), width);
            assert_committed_fits(
                "execute",
                RenderBlock::execute_with_output(
                    "cargo build --release",
                    "line 1\nline 2\nline 3\nline 4\nline 5\nline 6",
                    None::<String>,
                ),
                width,
            );
            assert_committed_fits("edit", RenderBlock::edit("src/main.rs", None), width);
            assert_committed_fits("read", RenderBlock::read("src/main.rs", None), width);
            assert_committed_fits(
                "list_dir",
                RenderBlock::list_dir_with_output("src", "a.rs\nb.rs\nc.rs"),
                width,
            );
            assert_committed_fits("search", RenderBlock::search("TODO", 0, vec![]), width);
            assert_committed_fits("system", RenderBlock::system("Session restored"), width);
            assert_committed_fits("bg_task", RenderBlock::bg_task("sleep 30", "task-1"), width);
        }
    }

    /// Regression pinned: `md_style::to_anstyle` used to map `Color::Reset`
    /// to a concrete ANSI-7 silver, washing out assistant/thinking markdown
    /// body text on light terminals; `highlight_bash_command` leaked raw
    /// syntect RGB.
    #[test]
    fn terminal_native_lock_paints_only_native_colors() {
        use ratatui::buffer::Buffer;
        use xai_grok_pager::theme::cache as theme_cache;

        let _guard = theme_cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        struct LockReset;
        impl Drop for LockReset {
            fn drop(&mut self) {
                xai_grok_pager::theme::cache::set_terminal_native_lock(false);
            }
        }
        let _reset = LockReset;
        theme_cache::set_terminal_native_lock(true);
        minimal_api::set_show_thinking_blocks(true);

        let md = "Intro paragraph with **bold**, _italic_, `inline code`, and a \
                  [link](https://example.com).\n\n# Heading one\n\n## Heading two\n\n\
                  - item one\n- item two\n\n> a quote\n\n```rust\nfn main() { println!(\"hi\"); }\n```";
        use similar::ChangeTag;
        let hunk = vec![
            xai_grok_pager::diff::DiffLine {
                text: "let x = 1;\n".into(),
                lo: 1,
                ln: 1,
                tag: ChangeTag::Equal,
            },
            xai_grok_pager::diff::DiffLine {
                text: "let y = 2;\n".into(),
                lo: 2,
                ln: 0,
                tag: ChangeTag::Delete,
            },
            xai_grok_pager::diff::DiffLine {
                text: "let y = 3;\n".into(),
                lo: 0,
                ln: 2,
                tag: ChangeTag::Insert,
            },
        ];
        let blocks = vec![
            ("agent_message", RenderBlock::agent_message(md)),
            (
                "thinking",
                RenderBlock::thinking("Weighing the *options* with `care`."),
            ),
            ("user_prompt", RenderBlock::user_prompt("run the tests")),
            (
                "edit",
                RenderBlock::edit_with_hunks("src/main.rs", vec![hunk]),
            ),
            (
                "execute",
                RenderBlock::execute_with_output("cargo build", "ok\n", None::<String>),
            ),
            ("system", RenderBlock::system("Session restored")),
        ];

        for (label, block) in blocks {
            let mut entry = ScrollbackEntry::new(block);
            entry.set_display_mode(minimal_commit_display_mode(&entry.block));
            let theme = Theme::current();
            let appearance = committed_appearance(&AppearanceConfig::default());
            let renderer = committed_renderer(&entry, &theme, appearance, test_cwd());

            let width = 100u16;
            let h = renderer.desired_height(width).max(1);
            let area = Rect::new(0, 0, width, h);
            let mut buf = Buffer::empty(area);
            renderer.render(area, &mut buf);

            for y in 0..h {
                for x in 0..width {
                    let Some(cell) = buf.cell((x, y)) else {
                        continue;
                    };
                    for (which, c) in [("fg", cell.fg), ("bg", cell.bg)] {
                        assert!(
                            !matches!(c, Color::Rgb(..) | Color::Indexed(_)),
                            "{label}: non-native {which} {c:?} at ({x},{y}) under \
                             symbol {:?} — minimal must only use Reset / named \
                             ANSI-16 so the terminal palette controls rendering",
                            cell.symbol()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn large_commit_is_capped_with_footer() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let theme = Theme::current();
        let appearance = committed_appearance(&AppearanceConfig::default());
        // A tall block: a fenced code block keeps each line on its own row
        // (markdown would otherwise join soft-wrapped prose into one paragraph),
        // so the block is comfortably taller than the cap.
        let lines: Vec<String> = (0..60).map(|i| format!("line {i}")).collect();
        let body = format!("```\n{}\n```", lines.join("\n"));
        let mut entry = ScrollbackEntry::new(RenderBlock::agent_message(body));
        entry.set_display_mode(minimal_commit_display_mode(&entry.block));

        let width = 80u16;
        let renderer = committed_renderer(&entry, &theme, appearance, test_cwd());
        let full_h = renderer.desired_height(width);
        assert!(full_h > 12, "expected a tall block, got {full_h}");

        // Paint into a cap-height buffer (what `insert_committed` allocates).
        let cap = 12u16;
        let area = Rect::new(0, 0, width, cap);
        let mut buf = Buffer::empty(area);
        paint_committed(&mut buf, renderer, width, full_h, theme.dim());

        // The final row is the overflow footer naming the hidden line count and
        // pointing at /transcript; the buffer is exactly `cap` rows (bounded).
        let last: String = (0..width)
            .filter_map(|x| buf.cell((x, cap - 1)).map(|c| c.symbol().to_string()))
            .collect();
        assert!(last.contains("more lines"), "footer row: {last:?}");
        assert!(last.contains("/transcript"), "footer row: {last:?}");
        // A hidden-line count is present (full_h minus the kept content rows).
        let hidden = full_h - (cap - 1);
        assert!(
            last.contains(&hidden.to_string()),
            "footer should name {hidden} hidden lines: {last:?}"
        );
    }

    #[test]
    fn small_commit_is_not_capped() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let theme = Theme::current();
        let appearance = committed_appearance(&AppearanceConfig::default());
        let mut entry = ScrollbackEntry::new(RenderBlock::agent_message("one short line"));
        entry.set_display_mode(minimal_commit_display_mode(&entry.block));

        let width = 80u16;
        let renderer = committed_renderer(&entry, &theme, appearance, test_cwd());
        let full_h = renderer.desired_height(width);

        // Buffer is exactly the block's height → no footer (uncapped path).
        let area = Rect::new(0, 0, width, full_h);
        let mut buf = Buffer::empty(area);
        paint_committed(&mut buf, renderer, width, full_h, theme.dim());

        let mut all = String::new();
        for y in 0..full_h {
            for x in 0..width {
                all.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
        }
        assert!(
            !all.contains("more lines"),
            "no footer when uncapped: {all:?}"
        );
    }

    #[test]
    fn committed_edit_keeps_diff_line_backgrounds() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use similar::ChangeTag;
        use xai_grok_pager::diff::DiffLine;

        let hunk = vec![
            DiffLine {
                text: "let x = 1;\n".into(),
                lo: 10,
                ln: 10,
                tag: ChangeTag::Equal,
            },
            DiffLine {
                text: "let y = 2;\n".into(),
                lo: 11,
                ln: 0,
                tag: ChangeTag::Delete,
            },
            DiffLine {
                text: "let y = 3;\n".into(),
                lo: 0,
                ln: 11,
                tag: ChangeTag::Insert,
            },
            DiffLine {
                text: "let z = 4;\n".into(),
                lo: 12,
                ln: 12,
                tag: ChangeTag::Equal,
            },
        ];
        let block = RenderBlock::edit_with_hunks("src/main.rs", vec![hunk]);
        let mut entry = ScrollbackEntry::new(block);
        entry.set_display_mode(minimal_commit_display_mode(&entry.block));
        let theme = Theme::current();
        let appearance = committed_appearance(&AppearanceConfig::default());
        let renderer = committed_renderer(&entry, &theme, appearance, test_cwd());

        let width = 80u16;
        let h = renderer.desired_height(width);
        let area = Rect::new(0, 0, width, h);
        let mut buf = Buffer::empty(area);
        renderer.render(area, &mut buf);

        // The committed edit uses a flat background (terminal transparency), but
        // must still paint the per-line diff backgrounds — otherwise an added /
        // removed line is indistinguishable from context.
        let mut saw_insert = false;
        let mut saw_delete = false;
        for y in 0..h {
            for x in 0..width {
                if let Some(cell) = buf.cell((x, y)) {
                    saw_insert |= cell.bg == theme.diff_insert_bg;
                    saw_delete |= cell.bg == theme.diff_delete_bg;
                }
            }
        }
        assert!(
            saw_insert,
            "committed edit lost the insert (green) diff background"
        );
        assert!(
            saw_delete,
            "committed edit lost the delete (red) diff background"
        );
    }

    #[test]
    fn commit_display_mode_policy() {
        assert_eq!(
            minimal_commit_display_mode(&RenderBlock::thinking("reasoning")),
            DisplayMode::Expanded
        );
        assert_eq!(
            minimal_commit_display_mode(&RenderBlock::edit("file.rs", None)),
            DisplayMode::Expanded
        );
        assert_eq!(
            minimal_commit_display_mode(&RenderBlock::execute("ls")),
            DisplayMode::Truncated
        );
        assert_eq!(
            minimal_commit_display_mode(&RenderBlock::agent_message("hi")),
            DisplayMode::Expanded
        );
    }

    #[test]
    fn commit_display_mode_lookups_collapse_on_success_only() {
        use xai_grok_pager::scrollback::blocks::{
            ListDirToolCallBlock, ReadToolCallBlock, SearchToolCallBlock,
        };

        assert_eq!(
            minimal_commit_display_mode(&RenderBlock::search("pat", 3, vec![])),
            DisplayMode::Collapsed
        );
        assert_eq!(
            minimal_commit_display_mode(&RenderBlock::read("src/lib.rs", None)),
            DisplayMode::Collapsed
        );
        assert_eq!(
            minimal_commit_display_mode(&RenderBlock::list_dir_with_output("src", "a.rs\nb.rs")),
            DisplayMode::Collapsed
        );

        for failed in [
            RenderBlock::ToolCall(ToolCallBlock::Search(
                SearchToolCallBlock::new("pat").with_error("regex parse error"),
            )),
            RenderBlock::ToolCall(ToolCallBlock::Read(
                ReadToolCallBlock::new("gone.rs").with_error("file not found"),
            )),
            RenderBlock::ToolCall(ToolCallBlock::ListDir(
                ListDirToolCallBlock::new("gone/").with_error("no such directory"),
            )),
        ] {
            assert_eq!(
                minimal_commit_display_mode(&failed),
                DisplayMode::Truncated,
                "failed lookup must stay truncated: {failed:?}"
            );
        }
    }
}
