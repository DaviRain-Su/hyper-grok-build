//! Consent gate, modelled on folder trust. Which accounts see a notice is a targeting decision.
//!
//! The answer is recorded locally, so it does not survive a second machine or a wiped config.
//!
//! Every failure path fails open: a client that cannot reach settings must stay usable.
//! Validation is the trust boundary, so a notice that survives it is safe to paint.

use std::collections::BTreeMap;

use crossterm::event::{Event, KeyEventKind, MouseButton, MouseEventKind};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use xai_grok_shell::util::config::{ConsentAnswer, ConsentGate};

use crate::app::actions::Action;
use crate::app::app_view::InputOutcome;
use crate::key;

/// Backstop only. Bytes say nothing about rows, so [`MAX_CONSENT_BODY_ROWS`] is the real bound.
const MAX_CONSENT_BODY_BYTES: usize = 2_000;

/// No scrolling and an unreadable notice cannot be accepted, so the body must fit the smallest
/// terminal we support. `the_largest_allowed_body_paints_on_a_standard_terminal` pins it to 80x24.
pub(crate) const MAX_CONSENT_BODY_ROWS: usize = 12;

/// Body width on an 80-column terminal: the screen's own wrap width at the default margin.
const REFERENCE_BODY_COLS: u16 = 76;

/// Display columns, so a wide title cannot push the action row off screen.
const MAX_CONSENT_TITLE_COLS: usize = 78;
const MAX_CONSENT_LABEL_COLS: usize = 24;

/// A fat-fingered version must not arm a gate nobody can clear.
const MAX_CONSENT_VERSION: i32 = 10_000;

/// The id becomes a TOML table key in the user's config, so it stays boring.
const MAX_CONSENT_ID_BYTES: usize = 64;

/// Named so the reason a payload did not arm the gate reaches the log instead of a bare `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsentArmRefusal {
    EmptyBody,
    /// Too tall for the smallest supported terminal, so the gate could never be read or accepted.
    BodyTooTall(usize),
    /// Nothing to record an acceptance against.
    MissingVersion,
    ImplausibleVersion(i32),
    /// The id keys both the stored answer and the upstream record.
    UnusableId,
}

/// Only [`ConsentNotice::try_from_remote`] builds one, so every field here is already validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentNotice {
    pub id: String,
    pub version: i32,
    pub title: String,
    pub body: String,
    pub accept_label: String,
}

/// Accept is withheld while `Illegible`, so text that never painted cannot be accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsentLegibility {
    #[default]
    Illegible,
    Painted,
}

impl ConsentLegibility {
    pub fn can_accept(self) -> bool {
        matches!(self, Self::Painted)
    }
}

#[derive(Debug)]
pub enum ConsentState {
    Done,
    Pending {
        notice: ConsentNotice,
        legibility: ConsentLegibility,
        /// Keys that arrived before the body painted were aimed at whatever it replaced.
        painted_at: Option<std::time::Instant>,
    },
}

/// Newlines survive; an embedded escape would corrupt the frame, and a bidi or zero-width character
/// would let the painted order differ from the text the acceptance is recorded against.
fn sanitize_display_text(raw: &str) -> String {
    raw.chars()
        .filter(|c| {
            *c == '\n'
                || !(crate::render::line_utils::is_unsafe_display_char(*c) || is_format_char(*c))
        })
        .collect()
}

/// The invisible characters the shared set misses: soft hyphen, line and
/// paragraph separators, annotation marks.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}' | '\u{2028}' | '\u{2029}' | '\u{fff9}'..='\u{fffb}'
    )
}

/// Every string the server supplies goes through here, so none can skip the cap or the sanitize.
fn text_or(raw: Option<&str>, fallback: &str, max_cols: usize) -> String {
    let cleaned = raw
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(fallback);
    crate::render::line_utils::truncate_str(&sanitize_display_text(cleaned), max_cols)
}

/// The id must be safe as a TOML key in the user's config and legible in the upstream record.
fn is_usable_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CONSENT_ID_BYTES
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl ConsentNotice {
    /// Validate a served payload.
    ///
    /// # Errors
    ///
    /// [`ConsentArmRefusal`] when the payload cannot safely arm the gate; every variant fails open.
    pub fn try_from_remote(gate: &ConsentGate) -> Result<Self, ConsentArmRefusal> {
        let body_source = gate.body.as_deref().unwrap_or_default();
        // Sanitized before the emptiness check: a body of nothing but control characters would
        // otherwise arm a gate with no text to read.
        let body = sanitize_display_text(xai_grok_tools::util::truncate_str(
            body_source,
            MAX_CONSENT_BODY_BYTES,
        ));
        let body = body.trim();
        if body.is_empty() {
            return Err(ConsentArmRefusal::EmptyBody);
        }

        let version = gate.version.ok_or(ConsentArmRefusal::MissingVersion)?;
        if version <= 0 || version > MAX_CONSENT_VERSION {
            return Err(ConsentArmRefusal::ImplausibleVersion(version));
        }

        if !is_usable_id(&gate.id) {
            return Err(ConsentArmRefusal::UnusableId);
        }

        let rows = wrap(body, REFERENCE_BODY_COLS).len();
        if rows > MAX_CONSENT_BODY_ROWS {
            return Err(ConsentArmRefusal::BodyTooTall(rows));
        }

        Ok(Self {
            id: gate.id.clone(),
            version,
            title: text_or(
                gate.title.as_deref(),
                "Updates to our terms",
                MAX_CONSENT_TITLE_COLS,
            ),
            body: body.to_string(),
            accept_label: text_or(
                gate.accept_label.as_deref(),
                "Got it",
                MAX_CONSENT_LABEL_COLS,
            ),
        })
    }
}

/// One grapheme of the body with the columns it occupies. The terminal pays in columns, so
/// counting characters would clip a wide-character notice in half while still reporting it painted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyCell {
    pub text: String,
    pub cols: u16,
}

pub type BodyRow = Vec<BodyCell>;

pub fn row_cols(row: &[BodyCell]) -> u16 {
    row.iter().map(|cell| cell.cols).sum()
}

/// Preserves the source's exact spacing, so no whitespace is invented where two segments meet.
pub fn wrap(body: &str, width: u16) -> Vec<BodyRow> {
    if width == 0 {
        return Vec::new();
    }

    let mut rows: Vec<BodyRow> = Vec::new();
    let mut row: BodyRow = Vec::new();
    let mut word: BodyRow = Vec::new();

    let flush_word = |row: &mut BodyRow, rows: &mut Vec<BodyRow>, word: &mut BodyRow| {
        if word.is_empty() {
            return;
        }
        if !row.is_empty() && row_cols(row) + row_cols(word) > width {
            rows.push(std::mem::take(row));
        }
        cut_overlong_word(word, rows, width);
        row.append(word);
    };

    let cells = body.graphemes(true).map(|g| BodyCell {
        text: g.to_string(),
        cols: UnicodeWidthStr::width(g) as u16,
    });
    for cell in cells {
        match cell.text.as_str() {
            "\n" => {
                flush_word(&mut row, &mut rows, &mut word);
                rows.push(std::mem::take(&mut row));
            }
            " " => {
                flush_word(&mut row, &mut rows, &mut word);
                if !row.is_empty() && row_cols(&row) < width {
                    row.push(cell);
                }
            }
            _ => word.push(cell),
        }
    }

    flush_word(&mut row, &mut rows, &mut word);
    if !row.is_empty() {
        rows.push(row);
    }

    // Trailing blank rows are padding, not a paragraph break the copy asked for.
    while rows.last().is_some_and(Vec::is_empty) {
        rows.pop();
    }
    rows
}

/// A word wider than the body (a bare url) is cut rather than overflow the block.
fn cut_overlong_word(word: &mut BodyRow, rows: &mut Vec<BodyRow>, width: u16) {
    while row_cols(word) > width {
        let mut taken = 0;
        let cut = word
            .iter()
            .take_while(|cell| {
                taken += cell.cols;
                taken <= width
            })
            .count()
            .max(1);
        rows.push(word.drain(..cut).collect());
    }
}

/// Everything the verdict needs, so the decision stays pure and the caller owns the IO.
pub struct ConsentInputs<'a> {
    pub gate: Option<&'a ConsentGate>,
    /// Answered during this run; the map is only as fresh as the last completed write.
    pub answered_this_run: Option<(&'a str, i32)>,
    /// Every answer this machine holds, keyed by notice id.
    pub answers: &'a BTreeMap<String, ConsentAnswer>,
    /// Whose answers count, so a second account on this machine is asked again.
    pub account: Option<&'a str>,
    /// Minimal mode has no consent renderer, so it stays ungated.
    pub minimal: bool,
}

/// Deliberately does not require the server ack: the local answer stops this machine re-asking.
fn already_answered(inputs: &ConsentInputs<'_>, notice: &ConsentNotice) -> bool {
    let this_run = inputs
        .answered_this_run
        .is_some_and(|(id, version)| id == notice.id && version >= notice.version);
    let on_disk = inputs.answers.get(&notice.id).is_some_and(|answer| {
        answer.account.as_deref() == inputs.account && answer.version >= notice.version
    });
    this_run || on_disk
}

pub fn consent_verdict(inputs: &ConsentInputs<'_>) -> ConsentState {
    if inputs.minimal {
        return ConsentState::Done;
    }

    let Some(gate) = inputs.gate else {
        return ConsentState::Done;
    };

    let notice = match ConsentNotice::try_from_remote(gate) {
        Ok(notice) => notice,
        Err(refusal) => {
            tracing::warn!(?refusal, "consent gate not armed; failing open");
            return ConsentState::Done;
        }
    };

    if already_answered(inputs, &notice) {
        return ConsentState::Done;
    }

    ConsentState::Pending {
        notice,
        legibility: ConsentLegibility::Illegible,
        painted_at: None,
    }
}

#[cfg(test)]
#[path = "consent_tests.rs"]
mod tests;

/// What the welcome input arm needs, so the handler does not reach into the whole welcome context.
pub struct ConsentInputCtx<'a> {
    pub state: &'a ConsentState,
    pub arrived_at: std::time::Instant,
    pub menu_rects: &'a [ratatui::layout::Rect],
    pub menu_index: &'a mut Option<usize>,
}

/// Accept is `a`: `y` accepts on the trust screen one step later, and Enter may be buffered before
/// the notice paints. No decline, so `q` quits and the rest is swallowed.
pub fn handle_answer(ev: &Event, ctx: &mut ConsentInputCtx<'_>) -> InputOutcome {
    let painted_at = match ctx.state {
        ConsentState::Pending { painted_at, .. } => *painted_at,
        ConsentState::Done => None,
    };
    // Both the keys and the menu rects belong to whatever painted last, so an event that reached
    // the process before the notice was aimed at the screen it replaced.
    let after_paint = painted_at.is_some_and(|painted| ctx.arrived_at >= painted);

    if let Event::Key(key) = ev {
        if key.kind == KeyEventKind::Release {
            return InputOutcome::Unchanged;
        }
        if key!('c', CONTROL).matches(key) || key!('d', CONTROL).matches(key) {
            return InputOutcome::Action(Action::Quit);
        }
        if key!('q').matches(key) || key!('Q').matches(key) {
            return InputOutcome::Action(Action::QuitConfirmed);
        }
        if key!('a').matches(key) || key!('A').matches(key) {
            // `a` starts plenty of prompts, so a key from before the paint was aimed at the
            // composer.
            if after_paint {
                return InputOutcome::Action(Action::AcceptConsent);
            }
            return InputOutcome::Unchanged;
        }
        return InputOutcome::Unchanged;
    }

    if let Event::Mouse(mouse) = ev {
        let at = ratatui::layout::Position::new(mouse.column, mouse.row);
        let over_menu_row = ctx.menu_rects.iter().position(|r| r.contains(at));

        if matches!(mouse.kind, MouseEventKind::Moved) {
            if *ctx.menu_index == over_menu_row {
                return InputOutcome::Unchanged;
            }
            *ctx.menu_index = over_menu_row;
            return InputOutcome::Changed;
        }

        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return InputOutcome::Unchanged;
        }

        // The accept row is painted only once the body is readable, so row 0 is accept if present.
        if let Some(row) = over_menu_row {
            let accept_offered = ctx.menu_rects.len() > 1 && after_paint;
            return InputOutcome::Action(if accept_offered && row == 0 {
                Action::AcceptConsent
            } else {
                Action::QuitConfirmed
            });
        }
        return InputOutcome::Unchanged;
    }

    if matches!(ev, Event::Resize(_, _)) {
        return InputOutcome::Changed;
    }
    InputOutcome::Unchanged
}
