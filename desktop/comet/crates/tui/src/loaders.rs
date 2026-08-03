//! comet's loaders, rendered into cells.
//!
//! These are the desktop app's own indicators — the 5-cell comet wave, the 2×3
//! mini gradient spinner, and the animated comet mark — driven by the same pure
//! curves in [`comet_proto::motion`]. A loading indicator is a brand surface, so
//! the terminal shows comet's, not a generic `|/-\` cycle.
//!
//! Three translations make that possible in a cell grid:
//!
//! 1. **Opacity becomes a blend.** A terminal has no alpha, but a cell's colour
//!    mixed toward the pane background is visually the same thing, and
//!    [`comet_proto::motion::pulse_opacity`] hands us exactly the coefficient.
//! 2. **The 2×3 mini spinner becomes one braille glyph.** Braille's dot matrix
//!    *is* 2×3, so the ring chase maps cell-for-cell into a single character —
//!    which is all the room a status dot has. Per-dot colour isn't possible, so
//!    the glyph takes the tint of whichever row is brightest.
//! 3. **Scale becomes glyph weight.** `pulse_scale` picks between a small and a
//!    full block, so a cell visibly breathes as well as brightens.
//!
//! All of it is a pure function of elapsed time, so the render loop stays
//! stateless and the animation is frame-rate independent — a dropped frame
//! changes nothing about where the wave is.

use std::time::Duration;

use comet_proto::motion;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// The pane background these loaders blend toward. Matching comet's main
/// surface (`#060606`) rather than pure black keeps the dim end of a pulse from
/// punching a hole in a terminal whose own background is lighter.
const BACKDROP: (u8, u8, u8) = (0x06, 0x06, 0x06);

/// Mix `color` toward the backdrop by `opacity` (1.0 = untouched).
///
/// This is the whole trick that lets a cell grid animate an opacity curve.
/// `Color::Reset` and the indexed palette have no components to mix, so they
/// pass through — a `NO_COLOR` terminal simply sees the glyph animation.
pub fn fade(color: Color, opacity: f32) -> Color {
    let opacity = opacity.clamp(0.0, 1.0);
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(
            motion::lerp(BACKDROP.0 as f32, r as f32, opacity).round() as u8,
            motion::lerp(BACKDROP.1 as f32, g as f32, opacity).round() as u8,
            motion::lerp(BACKDROP.2 as f32, b as f32, opacity).round() as u8,
        ),
        other => other,
    }
}

/// Loader phase (0..1) for a period, from elapsed time. Frame-rate independent:
/// the wave is wherever the clock says, not wherever the last frame left it.
pub fn phase(elapsed: Duration, period_ms: u64) -> f32 {
    if period_ms == 0 {
        return 0.0;
    }
    (elapsed.as_millis() as f64 % period_ms as f64) as f32 / period_ms as f32
}

/// The comet wave loader: five cells pulsing 0.08 → 1 with a per-cell stagger,
/// over a 2.4s period. comet's signature loader; used for the working strip,
/// where there is room for the full thing.
pub fn comet_wave(elapsed: Duration, color: Color) -> Vec<Span<'static>> {
    let raw = phase(elapsed, motion::COMET_PULSE_MS);
    (0..motion::COMET_CELLS)
        .map(|index| {
            let cell = motion::staggered_phase(raw, index, motion::PULSE_STAGGER);
            let opacity = motion::pulse_opacity(cell);
            // Scale reads as weight: the cell thickens as it brightens.
            let glyph = if motion::pulse_scale(cell) > 0.97 {
                "█"
            } else if motion::pulse_scale(cell) > 0.94 {
                "▓"
            } else {
                "▒"
            };
            Span::styled(glyph.to_string(), Style::default().fg(fade(color, opacity)))
        })
        .collect()
}

/// The gradient matrix spinner, flattened to one row: three cells carrying the
/// sunrise tints and the matrix's per-row phases.
///
/// comet's `WorkingIndicator` is a 3×3 block, which a one-line status strip
/// cannot hold. Laying its *rows* out left-to-right keeps everything that
/// carries meaning — three elements, the sunrise gradient, and the phase
/// relationship that makes the pulse travel — and drops only the second
/// dimension, which the strip has no room for either way.
pub fn gradient_spinner(elapsed: Duration) -> Vec<Span<'static>> {
    let raw = phase(elapsed, motion::GRADIENT_SPIN_MS);
    (0..motion::MATRIX_SIDE)
        .map(|row| {
            // The centre column: the matrix is symmetric about it, so it is the
            // representative sample of the row's phase.
            let cell = raw + motion::gspin_cell_phase(row, 1);
            let opacity = motion::gspin_opacity(cell, motion::GSPIN_DIM);
            let tint = motion::GSPIN_ROW_TINTS[row];
            let color = Color::Rgb(
                ((tint >> 16) & 0xff) as u8,
                ((tint >> 8) & 0xff) as u8,
                (tint & 0xff) as u8,
            );
            Span::styled("●".to_string(), Style::default().fg(fade(color, opacity)))
        })
        .collect()
}

/// Braille dot bits, indexed `[row][col]` for a 2×3 matrix. Braille numbers its
/// dots down the left column then down the right, which is not raster order —
/// hence the table rather than arithmetic.
const BRAILLE_BITS: [[u8; 2]; 3] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20]];

/// Opacity above which a braille dot is lit — tuned so roughly half the ring
/// shows at any instant.
const LIT: f32 = 0.2;

/// The 2×3 mini gradient spinner as one braille glyph, with the tint of the
/// brightest row — comet's session-row working indicator, at the one-cell size
/// a status dot actually has.
///
/// Returns the glyph and its colour. The lit threshold sits well below the
/// midpoint on purpose: `gspin_opacity` rests at 0.1 and spikes to 1.0, so a
/// midpoint cut leaves barely one dot lit and the glyph reads as a speck
/// instead of an arc chasing the ring.
pub fn mini_spinner(elapsed: Duration) -> (String, Color) {
    let raw = phase(elapsed, motion::GRADIENT_SPIN_MS);
    let mut bits = 0u8;
    let mut brightest = (0.0f32, 0usize);
    for (row, ring_row) in motion::MINI_RING.iter().enumerate() {
        for (col, ring) in ring_row.iter().enumerate() {
            let cell = raw + *ring as f32 / motion::MINI_RING_LEN;
            let opacity = motion::gspin_opacity(cell, motion::GSPIN_DIM);
            if opacity > LIT {
                bits |= BRAILLE_BITS[row][col];
            }
            if opacity > brightest.0 {
                brightest = (opacity, row);
            }
        }
    }
    let tint = motion::GSPIN_ROW_TINTS[brightest.1];
    let color = Color::Rgb(
        ((tint >> 16) & 0xff) as u8,
        ((tint >> 8) & 0xff) as u8,
        (tint & 0xff) as u8,
    );
    // U+2800 is the braille pattern block; the low 8 bits select the dots.
    let glyph = char::from_u32(0x2800 + bits as u32).unwrap_or('⠿');
    (glyph.to_string(), color)
}

/// Columns and rows of the comet mark, at its 120-unit cell pitch.
const MARK_COLS: usize = 7;
const MARK_ROWS: usize = 8;
/// Each logo pixel is two characters wide, so the mark keeps roughly its real
/// aspect ratio in a cell grid (terminal cells are about twice as tall as wide).
const MARK_CELL: &str = "██";
const MARK_GAP: &str = "  ";

/// The animated comet mark: the logo's pixel grid with a light wave sweeping
/// tail → head, for the boot gate. 14 columns × 8 rows.
pub fn comet_mark(elapsed: Duration, theme: &Theme) -> Vec<Line<'static>> {
    let raw = phase(elapsed, motion::COMET_PULSE_MS);
    // Rasterize the sparse cell list into a grid so rows can be emitted in order.
    let mut grid = [[None::<f32>; MARK_COLS]; MARK_ROWS];
    for (x, y) in motion::MARK_CELLS {
        let col = (x / 120.0).round() as usize;
        let row = (y / 120.0).round() as usize;
        if row < MARK_ROWS && col < MARK_COLS {
            grid[row][col] = Some(motion::pulse_opacity(motion::mark_phase(raw, x, y)));
        }
    }
    grid.iter()
        .map(|row| {
            Line::from(
                row.iter()
                    .map(|cell| match cell {
                        Some(opacity) => Span::styled(
                            MARK_CELL.to_string(),
                            Style::default().fg(fade(theme.text, *opacity)),
                        ),
                        None => Span::raw(MARK_GAP.to_string()),
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fading_walks_between_the_backdrop_and_the_colour() {
        let white = Color::Rgb(255, 255, 255);
        assert_eq!(fade(white, 1.0), white, "opaque is untouched");
        assert_eq!(
            fade(white, 0.0),
            Color::Rgb(BACKDROP.0, BACKDROP.1, BACKDROP.2),
            "transparent reaches the backdrop"
        );
        // Halfway is between the two, on every channel.
        let Color::Rgb(r, _, _) = fade(white, 0.5) else {
            panic!("expected rgb");
        };
        assert!(r > BACKDROP.0 && r < 255, "midpoint {r}");
        // Out-of-range coefficients clamp rather than wrapping to nonsense.
        assert_eq!(fade(white, 2.0), white);
        assert_eq!(fade(white, -1.0), fade(white, 0.0));
        // Non-RGB colours have nothing to mix and pass through, so a NO_COLOR
        // terminal still gets the glyph animation.
        assert_eq!(fade(Color::Reset, 0.3), Color::Reset);
    }

    #[test]
    fn phase_is_periodic_and_frame_rate_independent() {
        let period = motion::COMET_PULSE_MS;
        assert_eq!(phase(Duration::ZERO, period), 0.0);
        assert!((phase(Duration::from_millis(1200), period) - 0.5).abs() < 1e-3);
        // A whole extra period lands in the same place — no accumulated drift.
        assert!(
            (phase(Duration::from_millis(1200), period)
                - phase(Duration::from_millis(1200 + period), period))
            .abs()
                < 1e-3
        );
        // Degenerate period doesn't divide by zero.
        assert_eq!(phase(Duration::from_secs(1), 0), 0.0);
    }

    #[test]
    fn the_wave_has_one_cell_per_comet_cell_and_they_differ() {
        let spans = comet_wave(Duration::from_millis(300), Color::Rgb(255, 255, 255));
        assert_eq!(spans.len(), motion::COMET_CELLS);
        // Staggered, so at a given instant the cells are not all identical —
        // that difference is the wave.
        let colors: std::collections::HashSet<_> = spans.iter().map(|s| s.style.fg).collect();
        assert!(colors.len() > 1, "the wave must not be uniform");
    }

    #[test]
    fn the_gradient_spinner_carries_the_sunrise_tints() {
        let spans = gradient_spinner(Duration::from_millis(200));
        assert_eq!(spans.len(), motion::MATRIX_SIDE);
        // Each cell is its row's tint, faded by that row's phase — so the three
        // differ, and the gradient is visible rather than a uniform blink.
        let colors: std::collections::HashSet<_> = spans.iter().map(|s| s.style.fg).collect();
        assert_eq!(colors.len(), motion::MATRIX_SIDE, "rows must differ");
    }

    #[test]
    fn the_mini_spinner_is_a_single_braille_cell_that_moves() {
        let (glyph, _) = mini_spinner(Duration::ZERO);
        assert_eq!(glyph.chars().count(), 1, "one cell: {glyph:?}");
        let code = glyph.chars().next().unwrap() as u32;
        assert!((0x2800..=0x28ff).contains(&code), "braille: {code:#x}");
        assert_eq!(
            crate::wrap::width_of(&glyph),
            1,
            "must occupy exactly one column"
        );

        // The lit dots travel: sampling across the period yields more than one
        // pattern, and every sample stays inside the braille block.
        let mut seen = std::collections::HashSet::new();
        for step in 0..24 {
            let (glyph, tint) = mini_spinner(Duration::from_millis(step * 31));
            assert_eq!(crate::wrap::width_of(&glyph), 1);
            assert!(matches!(tint, Color::Rgb(..)));
            seen.insert(glyph);
        }
        assert!(seen.len() > 2, "the chase must animate, saw {}", seen.len());

        // The arc is substantial, not a speck: across the period the glyph
        // lights more than one dot at a time.
        let most = (0..24)
            .map(|step| {
                let (glyph, _) = mini_spinner(Duration::from_millis(step * 31));
                (glyph.chars().next().unwrap() as u32 - 0x2800).count_ones()
            })
            .max()
            .unwrap_or(0);
        assert!(most >= 2, "expected an arc, the brightest frame lit {most}");
    }

    #[test]
    fn the_mark_rasterizes_to_a_fixed_grid() {
        let theme = Theme::dark();
        let lines = comet_mark(Duration::from_millis(500), &theme);
        assert_eq!(lines.len(), MARK_ROWS);
        for line in &lines {
            let width: usize = line
                .spans
                .iter()
                .map(|span| crate::wrap::width_of(&span.content))
                .sum();
            assert_eq!(width, MARK_COLS * 2, "every row is the full mark width");
        }
        // All 34 logo pixels landed somewhere in the grid.
        let painted: usize = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.contains('█'))
            .count();
        assert_eq!(painted, motion::MARK_CELLS.len());
    }

    #[test]
    fn the_mark_sweeps_rather_than_blinking_in_unison() {
        // Neighbouring cells on the flight axis must differ, or the "sweep" is
        // just the whole logo flashing.
        let theme = Theme::dark();
        let lines = comet_mark(Duration::from_millis(700), &theme);
        let colors: std::collections::HashSet<_> = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.contains('█'))
            .map(|span| span.style.fg)
            .collect();
        assert!(
            colors.len() > 3,
            "expected a gradient, saw {}",
            colors.len()
        );
    }
}
