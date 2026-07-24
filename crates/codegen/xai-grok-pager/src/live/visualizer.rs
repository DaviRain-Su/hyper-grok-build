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

/// Clickable controls painted by the visualizer in the current frame.
///
/// Rectangles are clipped to the rendered area, so callers never retain a hit
/// target for text that was truncated off-screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VisualizerHitAreas {
    pub mute: Option<Rect>,
    pub stop: Option<Rect>,
}

/// Render the Live visualizer into the given buffer area (replaces the editor)
/// and return its clickable mute/stop controls.
/// Called from the agent view's `draw` method when Live is active.
pub fn render(buf: &mut Buffer, area: Rect, state: &LiveVisualizerState) -> VisualizerHitAreas {
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
        render_narrow(buf, area, state, block)
    } else {
        render_full(buf, area, state, block)
    }
}

fn render_full(
    buf: &mut Buffer,
    area: Rect,
    state: &LiveVisualizerState,
    block: Block,
) -> VisualizerHitAreas {
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
    let level_bar = render_waveform(state.level, theme.accent_user, 40);
    Paragraph::new(level_bar).render(chunks[1], buf);

    // Waveform row 2 (peak decay).
    let decay_bar = render_waveform(state.peak_decay, theme.accent_assistant, 40);
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
    let footer = render_phase_footer(
        state.phase,
        &state.error_message,
        state.muted,
        state.level,
        state.delegation_active,
        theme.accent_user,
        false,
    );
    let hit_areas = footer.hit_areas(chunks[4]);
    Paragraph::new(footer.line).render(chunks[4], buf);
    hit_areas
}

fn render_narrow(
    buf: &mut Buffer,
    area: Rect,
    state: &LiveVisualizerState,
    block: Block,
) -> VisualizerHitAreas {
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

    // Compact status + clickable controls + a short waveform on one line.
    // The full keyboard labels are shortened after the colon (for example,
    // `Space: mute` → `[mute]`) so both mouse targets survive narrow layouts.
    let footer = render_phase_footer(
        state.phase,
        &state.error_message,
        state.muted,
        state.level,
        state.delegation_active,
        theme.accent_user,
        true,
    );
    let hit_areas = footer.hit_areas(chunks[0]);
    let peak = state.level.max(state.peak_decay);
    let bar = render_waveform(peak, theme.accent_user, 8);
    let mut spans = footer.line.spans;
    spans.push(Span::raw(" "));
    spans.extend(bar.spans);
    Paragraph::new(Line::from(spans)).render(chunks[0], buf);

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
    hit_areas
}

/// Render a waveform as a bar of `█` characters proportional to the level.
fn render_waveform(level: f64, color: Color, max_width: usize) -> Line<'static> {
    let level = level.clamp(0.0, 1.0);
    let width = ((level * max_width as f64).round() as usize).min(max_width);
    let bar: String = "█".repeat(width);
    let padding: String = " ".repeat(max_width.saturating_sub(width));
    Line::from(vec![
        Span::styled(bar, Style::default().fg(color)),
        Span::raw(padding),
    ])
}

struct PhaseFooter {
    line: Line<'static>,
    mute_offset: u16,
    mute_width: u16,
    stop_offset: u16,
    stop_width: u16,
}

impl PhaseFooter {
    fn hit_areas(&self, area: Rect) -> VisualizerHitAreas {
        VisualizerHitAreas {
            mute: clipped_hit_rect(area, self.mute_offset, self.mute_width),
            stop: clipped_hit_rect(area, self.stop_offset, self.stop_width),
        }
    }
}

fn clipped_hit_rect(area: Rect, offset: u16, width: u16) -> Option<Rect> {
    if area.height == 0 || area.width == 0 || width == 0 {
        return None;
    }
    let right = area.x.saturating_add(area.width);
    let x = area.x.saturating_add(offset);
    (x < right).then(|| Rect::new(x, area.y, width.min(right - x), 1))
}

fn display_width(text: &str) -> u16 {
    unicode_width::UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16
}

fn compact_hint(hint: &str) -> String {
    let label = hint
        .split_once(':')
        .map(|(_, label)| label.trim())
        .filter(|label| !label.is_empty())
        .unwrap_or(hint);
    format!("[{label}]")
}

/// Render the phase footer line and retain the exact offsets of its clickable
/// controls. Derives a display status from the core transport phase + mute +
/// output level + delegation active.
fn render_phase_footer(
    phase: LivePhase,
    error: &Option<String>,
    muted: bool,
    level: f64,
    delegation_active: bool,
    color: Color,
    compact: bool,
) -> PhaseFooter {
    // OMP output-active threshold: the voice core treats output audio above
    // 0.015 as "actively speaking". Using the same threshold here keeps the
    // visualizer's speaking/listening flip aligned with the transport's own
    // activity detection (the prior 0.05 threshold lagged behind real speech).
    const SPEAKING_LEVEL_THRESHOLD: f64 = 0.015;
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
                } else if level > SPEAKING_LEVEL_THRESHOLD {
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
    // Show the unmute hint while muted (Space will unmute), the mute hint
    // otherwise — so the chord always matches the current state.
    let mute_hint_key = if muted {
        "live.hint.unmute"
    } else {
        "live.hint.mute"
    };
    let mute_hint = rust_i18n::t!(mute_hint_key);
    let stop_hint = rust_i18n::t!("live.hint.stop");

    let status_text = if compact {
        status_label.to_string()
    } else {
        format!(" {status_label} ")
    };
    let gap = if compact { " " } else { "  " };
    let mute_text = if compact {
        compact_hint(&mute_hint)
    } else {
        mute_hint.to_string()
    };
    let stop_text = if compact {
        compact_hint(&stop_hint)
    } else {
        stop_hint.to_string()
    };

    let mute_offset = display_width(&status_text).saturating_add(display_width(gap));
    let mute_width = display_width(&mute_text);
    let stop_offset = mute_offset
        .saturating_add(mute_width)
        .saturating_add(display_width(gap));
    let stop_width = display_width(&stop_text);

    let mut spans = vec![
        Span::styled(status_text, status_style.add_modifier(Modifier::BOLD)),
        Span::raw(gap),
        Span::styled(mute_text, Style::default().fg(color)),
        Span::raw(gap),
        Span::styled(stop_text, Style::default().fg(color)),
    ];

    // Narrow mode already has to share one row with a mini waveform; the full
    // error text remains available in wide mode and in the textual status.
    if !compact && let Some(msg) = error {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            truncate(msg, 40),
            Style::default().fg(Color::Red),
        ));
    }

    PhaseFooter {
        line: Line::from(spans),
        mute_offset,
        mute_width,
        stop_offset,
        stop_width,
    }
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
