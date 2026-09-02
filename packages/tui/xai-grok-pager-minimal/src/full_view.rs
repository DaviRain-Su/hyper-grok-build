//! Minimal-mode "full view": the whole conversation rendered with every block fully expanded, as one ANSI string opened in `$PAGER` (`less -R`).
//! Reasoning shows in full (not the collapsed `Thought for Xs` marker) and tool output is uncapped.
//!
//! Minimal commits blocks into the terminal's *native* scrollback as static text (collapsed reasoning, truncated tool output).
//! That text cannot be re-rendered in place when the user toggles verbose.
//! So "expand everything" is served by re-rendering the whole transcript off-screen at full fidelity and handing it to a pager (transcript mode).
//! The build reuses [`EntryRenderer`] (per-block layout, syntax highlighting, diff colors) with the display mode forced to `Expanded`.
//! It then serializes the resulting cell buffer to ANSI so colors survive in the pager.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

use xai_grok_pager::app::app_view::AppView;
use xai_grok_pager::minimal_api;
use xai_grok_pager::render::Renderable;
use xai_grok_pager::scrollback::entry::ScrollbackEntry;
use xai_grok_pager::scrollback::types::DisplayMode;
use xai_grok_pager::scrollback::wrappers::EntryRenderer;
use xai_grok_pager::theme::Theme;

/// Fixed render width for the transcript, independent of the current terminal size.
/// The pager wraps to the real terminal, and the committed and full-view wrapping need not match.
const FULL_VIEW_WIDTH: u16 = 100;

/// Per-frame budget for the incremental transcript build.
/// Small enough that a slice never blocks input or streaming noticeably, large enough to drain a big session in a couple of seconds.
/// Frames tick at ~16ms while a build is active (see `AppView::tick_interval_ceiling`).
const PUMP_BUDGET: Duration = Duration::from_millis(8);

/// Advance the in-progress `/transcript` build by one time-budgeted slice.
/// Called once per frame from [`crate::draw`]; a no-op when no build is pending (`minimal_api::request_minimal_transcript`).
///
/// Why sliced: the full-fidelity transcript is a layout, syntax-highlight, and ANSI-serialization pass over every block.
/// Doing it in one shot froze the event loop for seconds on long sessions.
/// It cannot move off-thread: the block model is `!Send` (syntect's resumable highlighter lives inside streaming-markdown blocks).
/// On completion the file is written and `pending_pager_path` set; the event loop then suspends into `$PAGER`.
pub fn pump_transcript(app: &mut AppView) {
    let Some(mut build) = minimal_api::take_minimal_transcript(app) else {
        return;
    };
    // Resolve against the build's OWNING agent, never the active view
    // `EntryId`s are per-`ScrollbackState` counters, so a session switch mid-build must not re-target the snapshot at another agent's scrollback
    // Id collisions would stitch the transcript from the wrong session
    // The owner keeps existing across view switches, so the build survives the user tabbing away; only a truly-removed agent drops it
    let id = build.agent;
    let appearance = super::commit::committed_appearance(&app.appearance);
    {
        let Some(agent) = app.agents.get(&id) else {
            tracing::warn!("minimal: transcript build's agent removed; dropping the build");
            return;
        };
        let theme = Theme::current();
        let sb = &agent.scrollback;

        // Show every thinking entry THAT EXISTS in the session: this view is the advertised full-fidelity "expand everything" transcript
        // Thinking entries render zero rows while the `[ui]` show_thinking_blocks toggle is off, so they were silently omitted here
        // The toggle is thread-local; restore it before returning to live rendering on this same thread
        //
        // Scope caveat: with the setting off, the tracker drops reasoning at INGESTION (`handle_thought_chunk` returns before pushing)
        // That is a deliberate, test-encoded memory tradeoff that predates minimal
        // Sessions run entirely with the toggle off have no thinking entries for any view to show
        // This override covers the sessions that do: toggle on at ingestion (the default), or toggled off mid-session
        let prev_thinking = xai_grok_pager::appearance::cache::load_show_thinking_blocks();
        xai_grok_pager::appearance::cache::set_show_thinking_blocks(true);
        let start = Instant::now();
        while build.next < build.ids.len() {
            let eid = build.ids[build.next];
            build.next += 1;
            // Re-resolve by id: entries removed mid-build (rewind / clear) are skipped rather than skewing positions
            if let Some(entry) = sb.index_of_id(eid).and_then(|idx| sb.entry(idx)) {
                render_entry_to_ansi(
                    entry,
                    &theme,
                    &appearance,
                    &agent.session.cwd,
                    &mut build.out,
                );
            }
            if start.elapsed() >= PUMP_BUDGET {
                break;
            }
        }
        xai_grok_pager::appearance::cache::set_show_thinking_blocks(prev_thinking);
    }

    if build.next < build.ids.len() {
        // More to do: resume next frame (ticks keep flowing via `needs_animation`; progress shows in the status row)
        minimal_api::set_minimal_transcript(app, Some(build));
        return;
    }

    // Done: hand the file to the event loop's suspend-into-$PAGER path.
    finish_transcript(app, id, build.out);
}

/// Write the finished transcript and set `pending_pager_path`; the ANSI flag makes the event loop add `-R` for `less`.
/// Errors land as a system block on the build's owning agent, which may differ from the active view (the user can tab away while the build runs).
fn finish_transcript(app: &mut AppView, id: xai_grok_pager::app::agent::AgentId, out: String) {
    if out.is_empty() {
        if let Some(agent) = app.agents.get_mut(&id) {
            agent
                .scrollback
                .push_block(xai_grok_pager::scrollback::block::RenderBlock::system(
                    "No conversation transcript to view yet",
                ));
        }
        return;
    }
    let path = std::env::temp_dir().join(format!("grok-transcript-{}.ansi", uuid::Uuid::new_v4()));
    match std::fs::write(&path, out) {
        Ok(()) => {
            app.pending_pager_path = Some(path);
            app.pending_pager_ansi = true;
        }
        Err(e) => {
            if let Some(agent) = app.agents.get_mut(&id) {
                agent.scrollback.push_block(
                    xai_grok_pager::scrollback::block::RenderBlock::system(format!(
                        "Failed to write transcript: {e}"
                    )),
                );
            }
        }
    }
}

/// Render one entry (fully expanded, at [`FULL_VIEW_WIDTH`]) and append its ANSI serialization to `out`.
/// Cloning keeps the live entry's display mode, which drives the on-screen committed look, untouched.
fn render_entry_to_ansi(
    entry: &ScrollbackEntry,
    theme: &Theme,
    appearance: &xai_grok_pager::appearance::AppearanceConfig,
    cwd: &std::path::Path,
    out: &mut String,
) {
    let mut expanded = entry.clone();
    expanded.set_display_mode(DisplayMode::Expanded);

    let renderer = EntryRenderer::new(&expanded, theme)
        .with_appearance(appearance.clone())
        .with_cwd(Some(cwd))
        .with_flat_background(true);
    let height = renderer.desired_height(FULL_VIEW_WIDTH);
    if height == 0 {
        return;
    }
    let area = Rect::new(0, 0, FULL_VIEW_WIDTH, height);
    let mut buf = Buffer::empty(area);
    renderer.render(area, &mut buf);
    buffer_to_ansi(&buf, out);
    // Blank line between blocks so the transcript breathes in the pager.
    out.push('\n');
}

/// Serialize a rendered cell [`Buffer`] to ANSI text (one `\n`-terminated line per row).
/// An SGR sequence is emitted whenever the style changes, with a reset at each row end.
/// Trailing blank cells are trimmed so lines stay short.
fn buffer_to_ansi(buf: &Buffer, out: &mut String) {
    let area = buf.area;
    for y in area.y..area.y.saturating_add(area.height) {
        // Last column carrying a visible glyph (trim trailing spaces).
        let mut last: Option<u16> = None;
        for x in (area.x..area.x.saturating_add(area.width)).rev() {
            if let Some(cell) = buf.cell((x, y)) {
                let s = cell.symbol();
                if !s.is_empty() && s != " " {
                    last = Some(x);
                    break;
                }
            }
        }
        if let Some(last_x) = last {
            // Track the current style as the raw (fg, bg, modifier) tuple and only build the escape string on a run boundary
            // Comparing three Copy fields per cell is far cheaper than building and comparing an SGR string per cell
            // The per-cell string build was the hot spot on long transcripts
            let mut cur: Option<(Color, Color, Modifier)> = None;
            let mut sgr = String::with_capacity(32);
            for x in area.x..=last_x {
                let Some(cell) = buf.cell((x, y)) else {
                    continue;
                };
                let sym = cell.symbol();
                if sym.is_empty() {
                    // An empty symbol is the continuation cell of a wide glyph, which was already emitted
                    continue;
                }
                let style = (cell.fg, cell.bg, cell.modifier);
                if cur != Some(style) {
                    cell_sgr(style.0, style.1, style.2, &mut sgr);
                    out.push_str(&sgr);
                    cur = Some(style);
                }
                out.push_str(sym);
            }
            out.push_str("\x1b[0m");
        }
        out.push('\n');
    }
}

/// Build a full SGR sequence (leading reset, then modifiers, fg, bg) for a cell's style.
/// The caller emits it only when the style changes, so the reset can't leak attributes across cells.
///
/// Writes into `sgr` (cleared first) instead of allocating.
/// This runs once per style *run*, which in syntax-highlighted code is nearly once per token.
/// The `Vec<String>` and `join` version dominated the serializer's profile on long transcripts.
fn cell_sgr(fg: Color, bg: Color, modifier: Modifier, sgr: &mut String) {
    use std::fmt::Write as _;

    sgr.clear();
    sgr.push_str("\x1b[0");
    if modifier.contains(Modifier::BOLD) {
        sgr.push_str(";1");
    }
    if modifier.contains(Modifier::DIM) {
        sgr.push_str(";2");
    }
    if modifier.contains(Modifier::ITALIC) {
        sgr.push_str(";3");
    }
    if modifier.contains(Modifier::UNDERLINED) {
        sgr.push_str(";4");
    }
    if modifier.contains(Modifier::REVERSED) {
        sgr.push_str(";7");
    }
    if modifier.contains(Modifier::CROSSED_OUT) {
        sgr.push_str(";9");
    }
    sgr.push(';');
    let _ = write!(sgr, "{}", color_code(fg, false));
    sgr.push(';');
    let _ = write!(sgr, "{}", color_code(bg, true));
    sgr.push('m');
}

/// Map a ratatui [`Color`] to its SGR parameter (foreground, or background when `bg`).
/// Named colors use the 16-color codes; `Indexed`/`Rgb` use the 256 / truecolor forms.
/// `Reset` is the terminal default (39 fg / 49 bg).
fn color_code(color: Color, bg: bool) -> String {
    // Named-color base code (30-series fg); +10 shifts to the 40-series bg.
    let named = |n: u16| -> String { (if bg { n + 10 } else { n }).to_string() };
    match color {
        Color::Reset => named(39),
        Color::Black => named(30),
        Color::Red => named(31),
        Color::Green => named(32),
        Color::Yellow => named(33),
        Color::Blue => named(34),
        Color::Magenta => named(35),
        Color::Cyan => named(36),
        Color::Gray => named(37),
        Color::DarkGray => named(90),
        Color::LightRed => named(91),
        Color::LightGreen => named(92),
        Color::LightYellow => named(93),
        Color::LightBlue => named(94),
        Color::LightMagenta => named(95),
        Color::LightCyan => named(96),
        Color::White => named(97),
        Color::Indexed(i) => format!("{};5;{}", if bg { 48 } else { 38 }, i),
        Color::Rgb(r, g, b) => format!("{};2;{};{};{}", if bg { 48 } else { 38 }, r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_pager::scrollback::block::RenderBlock;

    fn test_cwd() -> &'static std::path::Path {
        std::path::Path::new("/test/session")
    }

    /// With the `[ui]` show_thinking_blocks toggle off, a thinking entry renders zero rows and vanished from the "full-fidelity" transcript.
    /// The pump enables the (thread-local) toggle for the build.
    /// This locks the mechanism: off omits the entry, on (what `pump_transcript` sets) includes it.
    #[test]
    fn transcript_includes_thinking_when_pump_enables_toggle() {
        let theme = Theme::current();
        let appearance = super::super::commit::committed_appearance(
            &xai_grok_pager::appearance::AppearanceConfig::default(),
        );
        let entry = ScrollbackEntry::new(RenderBlock::thinking(
            "deep reasoning about haikus and syllables",
        ));

        xai_grok_pager::appearance::cache::set_show_thinking_blocks(false);
        let mut out = String::new();
        render_entry_to_ansi(&entry, &theme, &appearance, test_cwd(), &mut out);
        assert!(
            out.is_empty(),
            "thinking hidden while the toggle is off: {out:?}"
        );

        // What `pump_transcript` sets for the duration of a slice.
        xai_grok_pager::appearance::cache::set_show_thinking_blocks(true);
        let mut out = String::new();
        render_entry_to_ansi(&entry, &theme, &appearance, test_cwd(), &mut out);
        xai_grok_pager::appearance::cache::set_show_thinking_blocks(false);
        assert!(
            out.contains("reasoning"),
            "thinking content included in the transcript: {out:?}"
        );
    }

    /// A thinking entry built the way live streaming builds it (streaming block, per-chunk pushes, finish) must render its BODY in the transcript.
    /// The collapsed "Thought for Xs" header alone is not enough.
    #[test]
    fn transcript_expands_streamed_thinking_body() {
        use xai_grok_pager::scrollback::state::ScrollbackState;

        let theme = Theme::current();
        let appearance = super::super::commit::committed_appearance(
            &xai_grok_pager::appearance::AppearanceConfig::default(),
        );

        xai_grok_pager::appearance::cache::set_show_thinking_blocks(true);
        let mut sb = ScrollbackState::new();
        let id = sb.push_block(RenderBlock::thinking_streaming());
        assert!(sb.push_chunk_to_thinking(id, "REASONINGBODY pondering "));
        sb.push_chunk_to_thinking(id, "quietly about wraps");
        sb.finish_running_with_time(id, Some(1200));

        let entry = sb.get_by_id(id).expect("thinking entry");
        let mut out = String::new();
        render_entry_to_ansi(entry, &theme, &appearance, test_cwd(), &mut out);
        xai_grok_pager::appearance::cache::set_show_thinking_blocks(false);

        assert!(
            out.contains("REASONINGBODY"),
            "transcript must include the streamed thinking body: {out:?}"
        );
    }

    /// `minimal_collapse_thinking` points users at `/transcript` for the full text, so the transcript must ignore the committed display mode.
    #[test]
    fn transcript_expands_thinking_committed_collapsed() {
        let theme = Theme::current();
        let appearance = super::super::commit::committed_appearance(
            &xai_grok_pager::appearance::AppearanceConfig {
                minimal_collapse_thinking: true,
                ..Default::default()
            },
        );
        let mut entry = ScrollbackEntry::new(RenderBlock::thinking(
            "REASONINGBODY folded away at commit time",
        ));
        entry.set_display_mode(super::super::commit::minimal_commit_display_mode(
            &entry.block,
            &appearance,
        ));
        assert_eq!(entry.display_mode(), DisplayMode::Collapsed);

        xai_grok_pager::appearance::cache::set_show_thinking_blocks(true);
        let mut out = String::new();
        render_entry_to_ansi(&entry, &theme, &appearance, test_cwd(), &mut out);
        xai_grok_pager::appearance::cache::set_show_thinking_blocks(false);

        assert!(
            out.contains("REASONINGBODY"),
            "a collapsed commit must still expand in /transcript: {out:?}"
        );
        assert!(
            !out.contains("ctrl+e to expand"),
            "no expand hint in the fully-expanded transcript: {out:?}"
        );
    }

    #[test]
    fn transcript_uses_owning_session_cwd_for_tool_paths() {
        let theme = Theme::current();
        let appearance = super::super::commit::committed_appearance(
            &xai_grok_pager::appearance::AppearanceConfig::default(),
        );
        let entry =
            ScrollbackEntry::new(RenderBlock::edit("/alternate/worktree/src/main.rs", None));
        let mut out = String::new();

        render_entry_to_ansi(
            &entry,
            &theme,
            &appearance,
            std::path::Path::new("/alternate/worktree"),
            &mut out,
        );

        assert!(out.contains("src/main.rs"), "transcript: {out:?}");
        assert!(
            !out.contains("/alternate/worktree"),
            "session prefix should be elided: {out:?}"
        );
    }

    #[test]
    fn color_code_maps_reset_named_indexed_rgb() {
        assert_eq!(color_code(Color::Reset, false), "39");
        assert_eq!(color_code(Color::Reset, true), "49");
        assert_eq!(color_code(Color::Red, false), "31");
        assert_eq!(color_code(Color::Red, true), "41");
        assert_eq!(color_code(Color::DarkGray, false), "90");
        assert_eq!(color_code(Color::DarkGray, true), "100");
        assert_eq!(color_code(Color::Indexed(200), false), "38;5;200");
        assert_eq!(color_code(Color::Rgb(1, 2, 3), true), "48;2;1;2;3");
    }

    #[test]
    fn cell_sgr_includes_modifiers_and_colors() {
        let mut sgr = String::new();
        cell_sgr(
            Color::Rgb(10, 20, 30),
            Color::Reset,
            Modifier::BOLD | Modifier::ITALIC,
            &mut sgr,
        );
        // Leading reset, bold, italic, truecolor fg, default bg.
        assert_eq!(sgr, "\x1b[0;1;3;38;2;10;20;30;49m");
        // Reused buffer is cleared, not appended.
        cell_sgr(Color::Red, Color::Reset, Modifier::empty(), &mut sgr);
        assert_eq!(sgr, "\x1b[0;31;49m");
    }

    #[test]
    fn buffer_to_ansi_trims_trailing_and_terminates_rows() {
        // A 6-wide, 2-row buffer: "hi" on row 0, blank row 1.
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 2));
        buf.cell_mut((0, 0)).unwrap().set_symbol("h");
        buf.cell_mut((1, 0)).unwrap().set_symbol("i");
        let mut out = String::new();
        buffer_to_ansi(&buf, &mut out);
        let lines: Vec<&str> = out.split('\n').collect();
        // Row 0 has content ending in a reset; row 1 is blank; trailing newline.
        assert!(lines[0].contains('h') && lines[0].contains('i'));
        assert!(
            lines[0].ends_with("\x1b[0m"),
            "row must reset: {:?}",
            lines[0]
        );
        assert_eq!(lines[1], "", "blank row emits nothing but the newline");
        assert!(
            !lines[0].contains("  "),
            "trailing spaces not trimmed: {:?}",
            lines[0]
        );
    }
}
