//! Minimal (scrollback-native) render mode: `grok --minimal`.
//!
//! In this mode finalized conversation blocks are printed once into the terminal's *native* scrollback.
//! The printing goes through `xai_ratatui_inline::Terminal::insert_before` and reuses `EntryRenderer`.
//! A small pinned live region holds the running-turn status, the prompt, and a minimal status line.
//! The interactive `ScrollbackPane` (scroll, fold, selection, mouse) is not used; the terminal owns history.
//!
//! - [`commit`]: committed-frontier logic, display policy, and the per-frame commit-to-scrollback pass.
//! - [`live`]: the pinned live region (tail, todos, `/btw`, status, prompt).
//! - [`todo`]: the persistent todo panel shown above the prompt.
//! - [`auth`]: the in-region sign-in flow shown before a session exists.
//! - [`overlay`]: the inline-overlay host (prompt-anchored dropdowns; grows / shrinks the live viewport).
//!
//! # Wiring
//!
//! `xai-grok-pager` (the lib) does **not** depend on this crate: this crate reads deeply into the pager's [`AppView`] / view model.
//! A reverse dependency would be a cargo cycle.
//! Instead the pager exposes function-pointer hooks ([`xai_grok_pager::minimal_hook`]).
//! The composition-root binary (`xai-grok-pager-bin`) calls [`install`] once at startup to register this crate's [`draw`] entry point.
//! When the hooks are not installed the pager's minimal-mode branches are inert.

pub mod auth;
pub mod commit;
pub mod full_view;
pub mod live;
pub mod overlay;
pub mod panel;
pub mod plan;
pub mod todo;
pub mod welcome;

#[cfg(test)]
mod guard;

use crossterm::QueueableCommand;
use crossterm::terminal::BeginSynchronizedUpdate;

use xai_grok_pager::app::PagerTerminal;
use xai_grok_pager::app::app_view::AppView;

/// Per-frame entry point for minimal mode, called from [`AppView::draw`].
///
/// Order matters:
/// 0. Open a synchronized update and adopt the current terminal size (see below).
///    Every write this frame (commits and the live region) then presents atomically at the right dimensions.
/// 1. Commit the pending welcome card (fresh session / `/new`) so it lands above the first conversation block.
///    Push any ready plan into scrollback (`plan::maybe_commit_plan`) so it commits like a normal block this frame.
///    The live region then holds only the plan's decision controls.
/// 2. Size the viewport to its **post-commit** height (see [`overlay::sync_viewport`] / [`live::tail_height`]).
///    This runs before the commit so step 3's `insert_before` can reposition the correctly-sized viewport directly after each printed block.
///    The prompt follows the content, and once the screen is full that position is the bottom.
///    Sizing after the commit left the viewport at its tall streaming height, and the shrink stranded the prompt at the top of the screen.
/// 3. Commit finalized blocks into native scrollback; each `insert_before` scrolls committed rows up above the pinned viewport.
///    Then print any `Ctrl+E` / `/expand` re-prints fully expanded below.
/// 4. Redraw the live region (tail · status · overlay · prompt) into the viewport's final position.
///
/// ## Why step 0 exists (resize and flicker)
///
/// **Resize:** `draw_frame` runs `terminal.autoresize()` last, while the commit passes read `viewport_area().width` first.
/// On the frame that processes a terminal resize, a block finalizing in that same frame would be laid out and printed at the stale width.
/// A shrink then hard-wraps every over-wide row on the real terminal, permanently garbling the print-once committed copy.
/// Adopting the new size up front closes that window (a no-op on non-resize frames).
///
/// **Flicker:** the commit `insert_before`s scroll, repaint, and flush per chunk.
/// Without a synchronized update around them, a multi-block commit presents as several visible scroll/paint bursts before the live region repaints.
/// Opening the synchronized update before the commits batches the whole frame (commits, viewport reposition, live redraw) into one atomic present.
/// The matching `EndSynchronizedUpdate` is emitted by `draw_frame` (step 4), which every path through this function reaches.
/// Its own inner `BeginSynchronizedUpdate` is redundant but harmless: DEC 2026 is a mode, not a counter, so the first End closes it.
pub fn draw(app: &mut AppView, terminal: &mut PagerTerminal) {
    let _ = terminal.backend_mut().queue(BeginSynchronizedUpdate);
    let _ = terminal.autoresize();
    // Pending permission/question marks are synced ONCE, up front (see `commit::sync_pending_marks`)
    // The viewport sizing (`sync_viewport` / `tail_height` / `will_commit`) and the commit pass then judge committability against the same state
    commit::sync_pending_marks(app);
    // Advance any in-progress /transcript build by one time-budgeted slice (sets `pending_pager_path` when done; see `full_view::pump_transcript`)
    full_view::pump_transcript(app);
    welcome::maybe_commit_welcome(app, terminal);
    plan::maybe_commit_plan(app);
    overlay::sync_viewport(app, terminal);
    commit::commit_active(app, terminal);
    commit::expand_pending(app, terminal);
    live::draw_live(app, terminal);
}

/// Register the minimal-mode render hooks with `xai-grok-pager`.
///
/// Call this exactly once, early in the binary's `main`, before any frame is drawn.
/// It installs the function-pointer hooks so the pager's `ScreenMode::Minimal` branches dispatch into this crate.
/// Idempotent: subsequent calls are ignored (see [`xai_grok_pager::minimal_hook`]).
pub fn install() {
    xai_grok_pager::minimal_hook::install(xai_grok_pager::minimal_hook::MinimalHooks { draw });
}
