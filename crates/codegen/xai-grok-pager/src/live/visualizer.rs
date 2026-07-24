//! Live visualizer — a fixed-height 5-row full-width visualizer that replaces
//! the editor while Live is active.
//!
//! Layout (5 rows):
//! 1. Top border
//! 2. Waveform row 1 (user channel)
//! 3. Waveform row 2 (assistant channel)
//! 4. User transcript (accumulated finalized user segments)
//! 5. Phase footer (connecting / listening / working / speaking / muted / error)
//!
//! Narrow fallback: when the terminal width is too small for the full
//! visualizer, a compact 3-row layout is used (phase + waveform + transcript).
//!
//! Keyboard:
//! - Space toggles mute
//! - Esc / Ctrl+C ends the session
//! - Mouse hit areas: click on the waveform toggles mute, click on the footer
//!   stops.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

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

/// Render the Live visualizer into the given area (replaces the editor).
pub fn render(f: &mut Frame, area: Rect, state: &LiveVisualizerState) {
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
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme.accent_user))
        .title(Span::styled(
            " Live ",
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ));

    if narrow {
        render_narrow(f, area, state, block);
    } else {
        render_full(f, area, state, block);
    }
}

fn render_full(f: &mut Frame, area: Rect, state: &LiveVisualizerState, block: Block) {
    let chunks = ratatui::layout::Layout::vertical([
        // Row 1: top border (the block provides it, so this is 0-height filler).
        ratatui::layout::Constraint::Length(0),
        // Row 2: waveform row 1 (user).
        ratatui::layout::Constraint::Length(1),
        // Row 3: waveform row 2 (assistant).
        ratatui::layout::Constraint::Length(1),
        // Row 4: user transcript.
        ratatui::layout::Constraint::Length(1),
        // Row 5: phase footer.
        ratatui::layout::Constraint::Length(1),
    ])
    .split(block.inner(area));

    // The block itself renders the top border.
    f.render_widget(block, area);

    let theme = crate::theme::Theme::current();

    // Waveform row 1 (user).
    let user_bar = render_waveform(state.levels.user_peak, state.peak_decay, theme.accent_user);
    f.render_widget(Paragraph::new(user_bar), chunks[1]);

    // Waveform row 2 (assistant).
    let assistant_bar = render_waveform(
        state.levels.assistant_peak,
        state.peak_decay,
        theme.accent_assistant,
    );
    f.render_widget(Paragraph::new(assistant_bar), chunks[2]);

    // User transcript.
    let transcript = if state.user_transcript.is_empty() {
        Line::from(Span::styled("…", Style::default().fg(theme.text_secondary)))
    } else {
        Line::from(Span::styled(
            truncate(&state.user_transcript, area.width as usize),
            Style::default().fg(theme.text_primary),
        ))
    };
    f.render_widget(Paragraph::new(transcript), chunks[3]);

    // Phase footer.
    let phase_line = render_phase_footer(state.phase, &state.error_message, theme.accent_user);
    f.render_widget(Paragraph::new(phase_line), chunks[4]);
}

fn render_narrow(f: &mut Frame, area: Rect, state: &LiveVisualizerState, block: Block) {
    let chunks = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Length(0),
        // Phase + waveform combined.
        ratatui::layout::Constraint::Length(1),
        // Waveform.
        ratatui::layout::Constraint::Length(1),
        // Transcript.
        ratatui::layout::Constraint::Length(1),
    ])
    .split(block.inner(area));

    f.render_widget(block, area);

    let theme = crate::theme::Theme::current();

    // Phase footer.
    let phase_line = render_phase_footer(state.phase, &state.error_message, theme.accent_user);
    f.render_widget(Paragraph::new(phase_line), chunks[1]);

    // Combined waveform.
    let peak = state.levels.user_peak.max(state.levels.assistant_peak);
    let bar = render_waveform(peak, state.peak_decay, theme.accent_user);
    f.render_widget(Paragraph::new(bar), chunks[2]);

    // Transcript.
    let transcript = if state.user_transcript.is_empty() {
        Line::from(Span::styled("…", Style::default().fg(theme.text_secondary)))
    } else {
        Line::from(Span::styled(
            truncate(&state.user_transcript, area.width as usize),
            Style::default().fg(theme.text_primary),
        ))
    };
    f.render_widget(Paragraph::new(transcript), chunks[3]);
}

/// Render a single waveform row as a bar of `█` characters proportional to
/// the peak level.
fn render_waveform(peak: f32, decay: f32, color: Color) -> Line<'static> {
    let effective = peak.max(decay);
    let width = ((effective * 40.0).round() as usize).min(40);
    let bar: String = "█".repeat(width);
    let padding: String = " ".repeat(40usize.saturating_sub(width));
    Line::from(vec![
        Span::styled(bar, Style::default().fg(color)),
        Span::raw(padding),
    ])
}

/// Render the phase footer line.
fn render_phase_footer(phase: LivePhase, error: &Option<String>, color: Color) -> Line<'static> {
    let (label, style) = match phase {
        LivePhase::Connecting => ("Connecting", Style::default().fg(color)),
        LivePhase::Listening => ("Listening", Style::default().fg(Color::Green)),
        LivePhase::Working => ("Working", Style::default().fg(Color::Yellow)),
        LivePhase::Speaking => ("Speaking", Style::default().fg(Color::Cyan)),
        LivePhase::Muted => ("Muted", Style::default().fg(Color::DarkGray)),
        LivePhase::Error => ("Error", Style::default().fg(Color::Red)),
    };
    let mut spans = vec![
        Span::styled(format!(" {label} "), style.add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled("Space: mute  Esc: stop", Style::default().fg(color)),
    ];
    if let Some(msg) = error
        && phase == LivePhase::Error
    {
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
