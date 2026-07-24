//! Live visualizer — a fixed-height 5-row full-width visualizer that replaces
//! the editor while Live is active.
//!
//! Layout (5 rows):
//! 1. Top border
//! 2. Waveform row 1 (output level)
//! 3. Waveform row 2 (peak decay)
//! 4. User transcript (accumulated finalized input segments)
//! 5. Phase footer (connecting / connected / speaking / working / muted / error)
//!
//! Narrow fallback: when the terminal width is too small for the full
//! visualizer, a compact 3-row layout is used (phase+waveform + transcript).
//!
//! Keyboard:
//! - Space toggles mute
//! - Esc / Ctrl+C ends the session

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use super::LivePhase;
use super::state::LiveVisualizerState;

/// The fixed height of the full visualizer (5 rows).
pub const VISUALIZER_HEIGHT: u16 = 5;
/// The height of the narrow fallback (3 rows).
pub const VISUALIZER_NARROW_HEIGHT: u16 = 3;
/// The width threshold below which the narrow fallback is used.
pub const NARROW_WIDTH_THRESHOLD: u16 = 40;

/// Whether the given area is too narrow for the full visualizer.
pub fn is_narrow(area: Rect) -> bool {
    area.width < NARROW_WIDTH_THRESHOLD
}

/// Render the Live visualizer into the given buffer area (replaces the editor).
/// Called from the agent view's `draw` method when Live is active.
pub fn render(buf: &mut Buffer, area: Rect, state: &LiveVisualizerState) {
    let narrow = is_narrow(area);
    let height = if narrow {
        VISUALIZER_NARROW_HEIGHT
    } else {
        VISUALIZER_HEIGHT
    };
    let area = Rect {
        height: area.height.min(height),
        ..area
    };

    let theme = crate::theme::Theme::current();
    let title = rust_i18n::t!("live.visualizer.title");
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme.accent_user))
        .title(Span::styled(
            title.to_string(),
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ));

    if narrow {
        render_narrow(buf, area, state, block);
    } else {
        render_full(buf, area, state, block);
    }
}

fn render_full(buf: &mut Buffer, area: Rect, state: &LiveVisualizerState, block: Block) {
    let chunks = ratatui::layout::Layout::vertical([
        // Row 1: top border (the block provides it, so this is 0-height filler).
        ratatui::layout::Constraint::Length(0),
        // Row 2: waveform row 1 (output level).
        ratatui::layout::Constraint::Length(1),
        // Row 3: waveform row 2 (peak decay).
        ratatui::layout::Constraint::Length(1),
        // Row 4: user transcript.
        ratatui::layout::Constraint::Length(1),
        // Row 5: phase footer.
        ratatui::layout::Constraint::Length(1),
    ])
    .split(block.inner(area));

    block.render(area, buf);

    let theme = crate::theme::Theme::current();

    // Waveform row 1 (output level).
    let level_bar = render_waveform(state.level, theme.accent_user);
    Paragraph::new(level_bar).render(chunks[1], buf);

    // Waveform row 2 (peak decay).
    let decay_bar = render_waveform(state.peak_decay, theme.accent_assistant);
    Paragraph::new(decay_bar).render(chunks[2], buf);

    // User transcript.
    let transcript = if state.user_transcript.is_empty() {
        Line::from(Span::styled("…", Style::default().fg(theme.text_secondary)))
    } else {
        Line::from(Span::styled(
            truncate(&state.user_transcript, area.width as usize),
            Style::default().fg(theme.text_primary),
        ))
    };
    Paragraph::new(transcript).render(chunks[3], buf);

    // Phase footer.
    let phase_line = render_phase_footer(
        state.phase,
        &state.error_message,
        state.muted,
        state.level,
        state.delegation_active,
        theme.accent_user,
    );
    Paragraph::new(phase_line).render(chunks[4], buf);
}

fn render_narrow(buf: &mut Buffer, area: Rect, state: &LiveVisualizerState, block: Block) {
    // The block has a top border (1 row). With 3 total rows, `block.inner(area)`
    // gives 2 rows. We use a 2-row layout: phase+waveform combined, transcript.
    let inner = block.inner(area);
    block.render(area, buf);

    let chunks = ratatui::layout::Layout::vertical([
        // Phase + waveform combined row.
        ratatui::layout::Constraint::Length(1),
        // Transcript.
        ratatui::layout::Constraint::Length(1),
    ])
    .split(inner);

    let theme = crate::theme::Theme::current();

    // Phase footer + waveform on one line.
    let phase_line = render_phase_footer(
        state.phase,
        &state.error_message,
        state.muted,
        state.level,
        state.delegation_active,
        theme.accent_user,
    );
    let peak = state.level.max(state.peak_decay);
    let bar = render_waveform(peak, theme.accent_user);
    let combined = Line::from(vec![
        phase_line.spans[0].clone(),
        Span::raw(" "),
        bar.spans[0].clone(),
    ]);
    Paragraph::new(combined).render(chunks[0], buf);

    // Transcript.
    let transcript = if state.user_transcript.is_empty() {
        Line::from(Span::styled("…", Style::default().fg(theme.text_secondary)))
    } else {
        Line::from(Span::styled(
            truncate(&state.user_transcript, area.width as usize),
            Style::default().fg(theme.text_primary),
        ))
    };
    Paragraph::new(transcript).render(chunks[1], buf);
}

/// Render a single waveform row as a bar of `█` characters proportional to
/// the level.
fn render_waveform(level: f64, color: Color) -> Line<'static> {
    let level = level.clamp(0.0, 1.0);
    let width = ((level * 40.0).round() as usize).min(40);
    let bar: String = "█".repeat(width);
    let padding: String = " ".repeat(40usize.saturating_sub(width));
    Line::from(vec![
        Span::styled(bar, Style::default().fg(color)),
        Span::raw(padding),
    ])
}

/// Render the phase footer line.
/// Render the phase footer line. Derives a display status from the core
/// transport phase + mute + output level + delegation active.
fn render_phase_footer(
    phase: LivePhase,
    error: &Option<String>,
    muted: bool,
    level: f64,
    delegation_active: bool,
    color: Color,
) -> Line<'static> {
    // Derive the display status: error > connecting > muted > speaking >
    // working > connected.
    let (status_key, status_style) = if error.is_some() {
        ("live.status.error", Style::default().fg(Color::Red))
    } else {
        match phase {
            LivePhase::Connecting => ("live.status.connecting", Style::default().fg(color)),
            LivePhase::Closing | LivePhase::Closed => {
                ("live.status.closed", Style::default().fg(Color::DarkGray))
            }
            LivePhase::Connected => {
                if muted {
                    ("live.status.muted", Style::default().fg(Color::Yellow))
                } else if level > 0.05 {
                    ("live.status.speaking", Style::default().fg(Color::Green))
                } else if delegation_active {
                    ("live.status.working", Style::default().fg(Color::Cyan))
                } else {
                    ("live.status.listening", Style::default().fg(Color::Green))
                }
            }
        }
    };

    let status_label = rust_i18n::t!(status_key);
    let mute_hint = rust_i18n::t!("live.hint.mute");
    let stop_hint = rust_i18n::t!("live.hint.stop");

    let mut spans = vec![
        Span::styled(
            format!(" {status_label} "),
            status_style.add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{mute_hint}  {stop_hint}"),
            Style::default().fg(color),
        ),
    ];

    if let Some(msg) = error {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            truncate(msg, 40),
            Style::default().fg(Color::Red),
        ));
    }

    Line::from(spans)
}

/// Truncate a string to `max` chars, appending `…` if truncated.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}
