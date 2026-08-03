//! The transcript pane: a per-message layout cache over a virtualized viewport.
//!
//! This is where a chat TUI either is or isn't fast. The naive shape — hand the
//! whole transcript to a `Paragraph` with `Wrap` every frame — re-wraps every
//! byte of every message on every keystroke and every streaming token, so cost
//! grows with conversation length and a long session degrades until typing lags.
//!
//! Two structures avoid that:
//!
//! 1. **A per-message line cache** ([`Block`]). Each message is wrapped once
//!    and keyed by a content [`fingerprint`]. A streaming assistant message
//!    changes its fingerprint on every commit, so it (and only it) is re-wrapped;
//!    the two hundred messages above it are reused untouched. Cost per frame is
//!    proportional to what *changed*, not to what exists.
//!
//! 2. **Prefix sums over block heights** ([`Layout::starts`]). The visible
//!    window is found by binary search and rendered by reference — no `Line` is
//!    cloned, no `Text` is materialized, and messages outside the viewport are
//!    never visited. Cost per frame is proportional to the *pane height*, so
//!    scrolling a 10k-line transcript costs exactly what scrolling a 10-line one
//!    does.
//!
//! A width change is the one event that invalidates everything, because wrap
//! points move; that's a resize, where a full re-layout is both expected and
//! affordable.

use std::hash::{Hash, Hasher};

use comet_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use comet_proto::ToolCall;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render;
use crate::theme::Theme;
use crate::wrap;

/// Columns the content column is inset from the pane edge — must match
/// [`INSET`], which is what actually paints it, and [`render::PAD`], which is
/// where every other line in the main pane starts.
const CONTENT_INSET: usize = render::PAD;

/// One message's laid-out lines plus the fingerprint they were laid out from.
struct Block {
    fingerprint: u64,
    lines: Vec<Line<'static>>,
}

/// The transcript viewport.
///
/// Note what is *not* stored here: the messages. Once a message is laid out,
/// its [`Block`] is the only representation the pane needs, so the doc snapshot
/// is borrowed for the duration of [`Transcript::set_entries`] and never copied.
/// A transcript is the largest thing in the app; holding a second copy of it to
/// render from would double its footprint for no gain.
pub struct Transcript {
    /// Chat these entries belong to. Frames for any other chat are dropped.
    pub chat_id: Option<String>,
    blocks: Vec<Block>,
    /// `starts[i]` is the absolute line index block `i` begins at;
    /// `starts[len]` is the total line count. Always `blocks.len() + 1` long.
    starts: Vec<usize>,
    /// Width the cache was laid out at. A change invalidates every block.
    width: u16,
    /// Absolute index of the first visible line. Meaningful only when not
    /// following.
    top: usize,
    /// Pinned to the bottom: new content scrolls in and stays visible. Any
    /// upward scroll unpins; returning to the bottom re-pins.
    follow: bool,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            chat_id: None,
            blocks: Vec::new(),
            starts: vec![0],
            width: 0,
            top: 0,
            follow: true,
        }
    }

    /// Point the pane at another chat, clearing everything. Cheaper than
    /// rebuilding the struct because the allocations are reused.
    pub fn retarget(&mut self, chat_id: Option<String>) {
        if self.chat_id == chat_id {
            return;
        }
        self.chat_id = chat_id;
        self.blocks.clear();
        self.starts.truncate(1);
        self.top = 0;
        self.follow = true;
    }

    pub fn total_lines(&self) -> usize {
        *self.starts.last().unwrap_or(&0)
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn following(&self) -> bool {
        self.follow
    }

    /// Re-lay out only what changed.
    ///
    /// `doc` is the engine's snapshot and `echoes` the optimistic user messages
    /// not yet confirmed by it; they are rendered as one sequence. The watch
    /// stream hands us a full snapshot per commit, so this runs on every
    /// streaming tick and must stay O(changed), not O(transcript) — which is
    /// exactly what the fingerprint comparison below buys.
    pub fn set_entries(
        &mut self,
        doc: &[SessionMessageEntry],
        echoes: &[SessionMessageEntry],
        width: u16,
        theme: &Theme,
    ) {
        let width_changed = self.width != width;
        self.width = width;
        let text_width = (width as usize).saturating_sub(CONTENT_INSET);

        // Shrink first so `blocks` never indexes past the new entry count.
        self.blocks.truncate(doc.len() + echoes.len());
        let total = doc.len() + echoes.len();
        for (index, entry) in doc.iter().chain(echoes.iter()).enumerate() {
            // The newest message renders its last tool group expanded, so
            // "newest" is part of what the cache is keyed on.
            let is_last = index + 1 == total;
            let fingerprint = fingerprint(entry, is_last);
            match self.blocks.get(index) {
                // Same content at the same width: keep the laid-out lines.
                Some(block) if !width_changed && block.fingerprint == fingerprint => continue,
                Some(_) => {
                    self.blocks[index] = Block {
                        fingerprint,
                        lines: render_entry(entry, text_width, is_last, theme),
                    };
                }
                None => self.blocks.push(Block {
                    fingerprint,
                    lines: render_entry(entry, text_width, is_last, theme),
                }),
            }
        }

        self.reindex();
    }

    /// Rebuild the prefix sums. O(messages), run only when a block's height can
    /// have changed — never per frame.
    fn reindex(&mut self) {
        self.starts.clear();
        self.starts.push(0);
        let mut total = 0usize;
        for block in &self.blocks {
            total += block.lines.len();
            self.starts.push(total);
        }
        // A shrinking transcript (compaction, chat switch) can leave `top` past
        // the end; clamp so the pane never renders blank above real content.
        self.top = self.top.min(total);
    }

    /// Absolute index of the first line to draw for a pane `height` rows tall.
    pub fn top_line(&self, height: usize) -> usize {
        let max_top = self.total_lines().saturating_sub(height);
        if self.follow {
            max_top
        } else {
            self.top.min(max_top)
        }
    }

    pub fn scroll_up(&mut self, lines: usize, height: usize) {
        let from = self.top_line(height);
        self.top = from.saturating_sub(lines);
        // Scrolling up always unpins, even at the very top, so new content
        // doesn't yank the view back down while the user is reading.
        self.follow = false;
    }

    pub fn scroll_down(&mut self, lines: usize, height: usize) {
        let max_top = self.total_lines().saturating_sub(height);
        let next = self.top_line(height).saturating_add(lines);
        if next >= max_top {
            self.to_bottom();
        } else {
            self.top = next;
            self.follow = false;
        }
    }

    pub fn to_bottom(&mut self) {
        self.follow = true;
        self.top = self.total_lines();
    }

    pub fn to_top(&mut self) {
        self.follow = false;
        self.top = 0;
    }

    /// Visit the visible lines in order, paired with their row offset within the
    /// pane. Lines are handed out by reference: nothing is cloned to draw a
    /// frame.
    ///
    /// Only the blocks overlapping the viewport are touched — the binary search
    /// is what keeps a long transcript as cheap to scroll as a short one.
    ///
    /// Content shorter than the pane is pushed to the BOTTOM, matching the
    /// desktop transcript's `ListAlignment::Bottom`: a new conversation starts
    /// just above the composer rather than floating at the top of an empty pane.
    pub fn for_each_visible(&self, height: usize, mut visit: impl FnMut(u16, &Line<'static>)) {
        if height == 0 || self.blocks.is_empty() {
            return;
        }
        let pad = height.saturating_sub(self.total_lines()) as u16;
        let top = self.top_line(height);
        // `partition_point` gives the first block starting after `top`; the one
        // before it is the block containing `top`.
        let first = self.starts.partition_point(|&start| start <= top);
        let mut block_index = first.saturating_sub(1);
        let mut line_index = top.saturating_sub(self.starts[block_index]);
        let mut row = 0u16;

        while row < height as u16 && block_index < self.blocks.len() {
            let lines = &self.blocks[block_index].lines;
            while line_index < lines.len() && row < height as u16 {
                visit(row + pad, &lines[line_index]);
                line_index += 1;
                row += 1;
            }
            block_index += 1;
            line_index = 0;
        }
    }
}

/// A content hash that changes whenever the rendering would.
///
/// Deliberately O(parts), not O(bytes): hashing every byte of a megabyte
/// transcript on each of the ~8 commits per second a stream produces would cost
/// more than the wrapping we're trying to skip. Instead each part contributes
/// its length plus its head and tail bytes, which distinguishes an append (the
/// only mutation the doc schema allows on a text body — ARCHITECTURE §2.1 keeps
/// them as `LoroText`, never LWW value rewrites) and also catches a same-length
/// rewrite, which the schema forbids but a future one might not.
fn fingerprint(entry: &SessionMessageEntry, is_last: bool) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    is_last.hash(&mut hasher);
    entry.id.hash(&mut hasher);
    role_tag(entry.role).hash(&mut hasher);
    entry.status.map(status_tag).hash(&mut hasher);
    entry.continuation_of.hash(&mut hasher);
    entry.parts.len().hash(&mut hasher);
    for part in &entry.parts {
        part.id().hash(&mut hasher);
        match part {
            MessagePart::Text { text, .. } => {
                0u8.hash(&mut hasher);
                hash_text(text, &mut hasher);
            }
            MessagePart::Tool {
                call,
                is_error,
                resolved,
                ..
            } => {
                1u8.hash(&mut hasher);
                // A tool part's rendering turns on its resolution state, which
                // flips without changing any length.
                is_error.hash(&mut hasher);
                resolved.hash(&mut hasher);
                let (label, detail) = comet_proto::view::tool_chip_content(call);
                label.hash(&mut hasher);
                hash_text(&detail, &mut hasher);
            }
            MessagePart::Input {
                questions,
                resolved,
                ..
            } => {
                2u8.hash(&mut hasher);
                resolved.hash(&mut hasher);
                questions.len().hash(&mut hasher);
                for question in questions {
                    hash_text(&question.question, &mut hasher);
                    question.options.len().hash(&mut hasher);
                }
            }
            MessagePart::Error { message, .. } => {
                3u8.hash(&mut hasher);
                hash_text(message, &mut hasher);
            }
        }
    }
    hasher.finish()
}

/// Length plus the first and last 16 bytes — enough to notice an append, a
/// truncation, or a rewrite, without walking the whole string.
fn hash_text(text: &str, hasher: &mut impl Hasher) {
    const EDGE: usize = 16;
    let bytes = text.as_bytes();
    bytes.len().hash(hasher);
    if bytes.len() <= EDGE * 2 {
        bytes.hash(hasher);
    } else {
        bytes[..EDGE].hash(hasher);
        bytes[bytes.len() - EDGE..].hash(hasher);
    }
}

fn role_tag(role: MessageRole) -> u8 {
    match role {
        MessageRole::User => 0,
        MessageRole::Assistant => 1,
        MessageRole::System => 2,
    }
}

fn status_tag(status: MessageStatus) -> u8 {
    match status {
        MessageStatus::Streaming => 0,
        MessageStatus::Complete => 1,
        MessageStatus::Aborted => 2,
    }
}
// ---------------------------------------------------------------------------
// Message → lines
// ---------------------------------------------------------------------------

/// Lay one message out into wrapped, styled lines.
///
/// Shapes follow the desktop viewport (`docs/reference/original-comet*.png`):
///
/// - **The user's own message is a right-aligned bubble** on a raised wash, with
///   the timestamp beneath it. Not a gutter marker — the asymmetry is what makes
///   a transcript scannable at a glance.
/// - **The assistant's text is plain**, at the content column, with no marker
///   and no tint. Paragraph spacing does the structural work.
/// - **Runs of tool calls collapse into one group** ("Ran 3 commands · edited 2
///   files") rather than one row each, because a wall of tool rows buries the
///   answer that follows them.
///
/// `is_last` marks the final message, whose last tool group stays expanded —
/// what you want to see is what it is doing *now*. It is part of the cache
/// fingerprint, so a message stops being last without going stale.
/// Is this row visually empty — nothing but spaces, whatever its styling?
///
/// A block's own padding row is "blank" in this sense but carries the block's
/// fill, which is why [`collapse_blanks`] cannot just compare against
/// `Line::default()`.
fn is_blank(line: &Line<'static>) -> bool {
    line.spans
        .iter()
        .all(|span| span.content.chars().all(|c| c == ' '))
}

/// Does this row paint a fill?
fn is_filled(line: &Line<'static>) -> bool {
    line.spans.iter().any(|span| span.style.bg.is_some())
}

/// Collapse runs of blank rows of the same kind down to one.
///
/// Markdown brings its own blank lines, the separator between groups adds
/// another and every block pads itself, so without this a paragraph ending
/// `\n\n` in front of a tool group opens a three-row hole.
fn collapse_blanks(lines: &mut Vec<Line<'static>>) {
    let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    for line in lines.drain(..) {
        let duplicate = out.last().is_some_and(|previous| {
            // Two blanks of the *same kind* in a row are one blank too many. A
            // plain blank followed by a block's own padding is not a duplicate,
            // though: the plain row is the gap between the block and the text
            // above it, and the padded row is the block's top edge. Collapsing
            // those into one is what made the bands abut the prose.
            is_blank(previous) && is_blank(&line) && is_filled(previous) == is_filled(&line)
        });
        if !duplicate {
            out.push(line);
        }
    }
    *lines = out;
}

/// Push a blank row, unless the last row already is one (or there is no row yet).
///
/// Markdown brings its own blank lines and the block separator adds another, so
/// a paragraph that ends `\n\n` in front of a tool group would otherwise open a
/// three-row hole. One blank is a separator; three is a hole.
fn push_blank(out: &mut Vec<Line<'static>>) {
    if out.last().is_none_or(|line| line.spans.is_empty()) {
        return;
    }
    out.push(Line::default());
}

fn render_entry(
    entry: &SessionMessageEntry,
    width: usize,
    is_last: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Group consecutive tool parts so a run renders as one summary.
    let groups = group_parts(&entry.parts);
    let last_group = groups
        .iter()
        .rposition(|g| matches!(g, PartGroup::Tools(_)));

    // Air between blocks, but only where one is needed: a run of one-liners
    // stays a list, while anything taller than a row gets a blank on both sides.
    // Reply-tools-reply otherwise arrives as one undifferentiated wall, which is
    // the single biggest reason a transcript reads as cluttered.
    let mut previous_height = 0usize;
    for (index, group) in groups.iter().enumerate() {
        if index > 0 && previous_height > 1 {
            push_blank(&mut lines);
        }
        let start = lines.len();
        match group {
            PartGroup::Single(part) => match part {
                MessagePart::Text { text, .. } => match entry.role {
                    MessageRole::User => {
                        render_bubble(text, entry.created_at, width, theme, &mut lines)
                    }
                    _ => render_markdown(text, width, theme, &mut lines),
                },
                MessagePart::Input {
                    questions,
                    resolved,
                    ..
                } => render_questions(questions, *resolved, width, theme, &mut lines),
                MessagePart::Error { message, .. } => {
                    render_error(message, width, theme, &mut lines)
                }
                // Handled by PartGroup::Tools.
                MessagePart::Tool { .. } => {}
            },
            PartGroup::Tools(tools) => {
                // Expanded while anything is still running, and for the newest
                // group in the newest message — matching the reference, where
                // the latest group shows its chips and older ones are summaries.
                let running = tools.iter().any(|(_, _, resolved)| !resolved);
                let newest = is_last && last_group == Some(index);
                render_tool_group(tools, running || newest, width, theme, &mut lines);
            }
        }
        previous_height = lines.len() - start;
        // This block is tall but the one above it was not, so the blank that
        // would have preceded it was never pushed. Slot it in now.
        if previous_height > 1
            && start > 0
            && lines
                .get(start - 1)
                .is_some_and(|line| !line.spans.is_empty())
        {
            lines.insert(start, Line::default());
        }
    }

    if entry.status == Some(MessageStatus::Aborted) {
        lines.push(indented(vec![Span::styled(
            "Interrupted".to_string(),
            theme.hint(),
        )]));
    }

    collapse_blanks(&mut lines);
    // One blank row between messages. Part of the block, so it scrolls with it —
    // and only one, since a reply that ended on a trailing newline has already
    // left its own.
    push_blank(&mut lines);
    lines
}

/// A message's parts, with consecutive tool calls folded together.
enum PartGroup<'a> {
    Single(&'a MessagePart),
    /// `(call, is_error, resolved)` in order.
    Tools(Vec<(&'a ToolCall, bool, bool)>),
}

fn group_parts(parts: &[MessagePart]) -> Vec<PartGroup<'_>> {
    let mut groups: Vec<PartGroup<'_>> = Vec::new();
    for part in parts {
        match part {
            MessagePart::Tool {
                call,
                is_error,
                resolved,
                ..
            } => {
                let entry = (call, *is_error, *resolved);
                match groups.last_mut() {
                    Some(PartGroup::Tools(tools)) => tools.push(entry),
                    _ => groups.push(PartGroup::Tools(vec![entry])),
                }
            }
            other => groups.push(PartGroup::Single(other)),
        }
    }
    groups
}

/// Left inset of the content column. The original keeps its prose well off the
/// pane edge; two columns is the terminal equivalent, and lines the transcript
/// up with the composer's text and the working strip below it.
const INSET: &str = "  ";

fn indented(mut spans: Vec<Span<'static>>) -> Line<'static> {
    spans.insert(0, Span::raw(INSET.to_string()));
    Line::from(spans)
}

/// The bar down the left of a block that is *yours*. opencode marks the user's
/// message and the prompt with the same stroke, and it is the only colour in the
/// whole transcript.
const BAR: &str = "┃";

/// One row of a full-width block.
///
/// This is the shape opencode's transcript is made of: a filled band running the
/// content column's full width, optionally with an accent bar down its left
/// edge. Padding out to the full width is the whole point — a fill that stops
/// where the glyphs stop reads as a highlighter stain, while one that runs the
/// column reads as a region. It is also why these rows do not use [`indented`]:
/// the padding has to be *inside* the fill, not before it.
fn block_row(
    body: Vec<Span<'static>>,
    width: usize,
    fill: Style,
    bar: Option<Style>,
    offset: usize,
) -> Line<'static> {
    let used: usize = body.iter().map(|span| wrap::width_of(&span.content)).sum();
    let lead = if bar.is_some() { 2 } else { INSET.len() };
    let mut spans = Vec::with_capacity(body.len() + 4);
    // Unstyled, so a right-aligned block sits on the app surface rather than
    // dragging its own fill across the column it was pushed away from.
    if offset > 0 {
        spans.push(Span::raw(" ".repeat(offset)));
    }
    match bar {
        Some(style) => {
            spans.push(Span::styled(BAR.to_string(), style));
            spans.push(Span::styled(" ".to_string(), fill));
        }
        None => spans.push(Span::styled(" ".repeat(lead), fill)),
    }
    spans.extend(body);
    // Trailing padding closes the band; without it the fill ends ragged at the
    // longest line and the block loses its right edge.
    let pad = width.saturating_sub(lead + used);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), fill));
    }
    Line::from(spans)
}

/// A blank row of a block, so the fill has vertical padding of its own.
fn block_pad(width: usize, fill: Style, bar: Option<Style>, offset: usize) -> Line<'static> {
    block_row(Vec::new(), width, fill, bar, offset)
}

/// The user's message: a full-width block with the accent bar down its left.
///
/// This is opencode's shape rather than the desktop's right-aligned bubble. A
/// narrow bubble floating at the right margin is what made this transcript read
/// as ragged: at terminal widths it leaves a long tail of dead space on every
/// user turn, and the eye has to travel to find where the message starts. A band
/// with a coloured edge says "you said this" without moving the reading column,
/// and it is the same mark the prompt below carries — the two things that are
/// yours look alike.
fn render_bubble(
    text: &str,
    created_at: i64,
    width: usize,
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
) {
    let fill = theme.bubble();
    let bar = Some(theme.accent_bar());
    // Capped, then shrunk to the longest line it actually used. A block sized to
    // the whole column makes a three-word question look like a paragraph; sized
    // to its text, the bar and the fill say "you said this" and the block's
    // right edge says how much of it there was.
    let cap = ((width * 4) / 5).saturating_sub(4).max(8);
    let mut rows: Vec<String> = Vec::new();
    for paragraph in wrap::sanitize(text).split('\n') {
        rows.extend(wrap::wrap(paragraph, cap, ""));
    }
    let clock = format_clock(created_at);
    // Wide enough for the text, and never so narrow that the timestamp on the
    // closing row has to overrun it.
    let widest = rows
        .iter()
        .map(|row| wrap::width_of(row))
        .max()
        .unwrap_or(0)
        .max(wrap::width_of(&clock));
    let block = (widest + 4).min(width);
    // Pushed to the right margin, as the desktop bubble is: a message that is
    // yours reads as *sent* when it sits opposite the replies rather than in the
    // same column as them, and the block's own width is what makes that legible
    // at a glance.
    let offset = width.saturating_sub(block);

    out.push(block_pad(block, fill, bar, offset));
    for row in rows {
        out.push(block_row(
            vec![Span::styled(row, fill)],
            block,
            fill,
            bar,
            offset,
        ));
    }
    // The timestamp closes the block from inside it, right-aligned, so the block
    // ends on a padded row either way.
    let pad = block.saturating_sub(2 + wrap::width_of(&clock) + 2);
    out.push(block_row(
        vec![
            Span::styled(" ".repeat(pad), fill),
            Span::styled(clock, fill.patch(Style::default().fg(theme.faint))),
        ],
        block,
        fill,
        bar,
        offset,
    ));
}

/// A tool group: a summary row, and the chips when expanded.
fn render_tool_group(
    tools: &[(&ToolCall, bool, bool)],
    expanded: bool,
    width: usize,
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
) {
    let pairs: Vec<(ToolCall, bool)> = tools
        .iter()
        .map(|(call, is_error, _)| ((*call).clone(), *is_error))
        .collect();
    let summary = comet_proto::view::tool_group_summary(&pairs);
    let chevron = if expanded { "⌄" } else { "›" };
    // Not a block. A tool run is the agent's own bookkeeping, not something it
    // said to you, and giving it a fill of its own made every reply look like it
    // was interrupted by a panel. The chevron and the indent are enough.
    out.push(indented(vec![
        Span::styled(format!("{chevron} "), theme.hint()),
        Span::styled(
            wrap::truncate(&summary, width.saturating_sub(4)),
            theme.subtle(),
        ),
    ]));
    if !expanded {
        return;
    }
    // Indented under the chevron rather than railed beside it: the summary above
    // already brackets the run, so a vertical stroke is a second bracket.
    for (call, is_error, resolved) in tools {
        let (label, detail) = comet_proto::view::tool_chip_content(call);
        let label_style = Style::default().fg(if *is_error {
            theme.danger
        } else if *resolved {
            theme.muted
        } else {
            theme.faint
        });
        let used = 4 + wrap::width_of(label) + 2;
        out.push(indented(vec![
            Span::raw("  ".to_string()),
            Span::styled(label.to_string(), label_style),
            Span::styled(
                format!(
                    "  {}",
                    wrap::truncate(&wrap::sanitize(&detail), width.saturating_sub(used))
                ),
                theme.hint(),
            ),
        ]));
    }
}

fn render_error(message: &str, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    for row in wrap::wrap(&wrap::sanitize(message), width.saturating_sub(1), "") {
        out.push(indented(vec![Span::styled(
            row,
            Style::default().fg(theme.danger),
        )]));
    }
}

fn render_questions(
    questions: &[comet_proto::UserInputQuestion],
    resolved: bool,
    width: usize,
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
) {
    for question in questions {
        // Awaiting input is the accent, matching the sidebar dot for the same
        // state — one color, one meaning.
        let marker_style = if resolved {
            theme.hint()
        } else {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        };
        for (index, row) in wrap::wrap(
            &wrap::sanitize(&question.question),
            width.saturating_sub(3),
            "  ",
        )
        .into_iter()
        .enumerate()
        {
            out.push(indented(vec![
                Span::styled(
                    if index == 0 { "? " } else { "  " }.to_string(),
                    marker_style,
                ),
                Span::styled(row, theme.body()),
            ]));
        }
        for option in &question.options {
            out.push(indented(vec![
                Span::styled("  · ".to_string(), theme.hint()),
                Span::styled(
                    wrap::truncate(&wrap::sanitize(option), width.saturating_sub(5)),
                    theme.subtle(),
                ),
            ]));
        }
    }
}

/// Markdown-lite: headings, fenced code, bullets, and inline `code`.
///
/// Deliberately not a full parser — the goal is that agent output reads well in
/// a terminal, and every rule here is one pass over already-sanitized lines.
/// Monochrome throughout: the original tints nothing here, using weight and a
/// wash for emphasis instead.
fn render_markdown(text: &str, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    // Fenced code is buffered rather than emitted line by line, because the card
    // it becomes is only as wide as its widest line and that isn't known until
    // the fence closes.
    let mut fence: Option<Vec<String>> = None;
    for raw in text.split('\n') {
        let line = wrap::sanitize(raw);
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") {
            match fence.take() {
                Some(rows) => render_code_block(rows, width, theme, out),
                None => fence = Some(Vec::new()),
            }
            continue;
        }

        if let Some(rows) = fence.as_mut() {
            rows.push(line);
            continue;
        }

        if line.trim().is_empty() {
            push_blank(out);
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("### ").or_else(|| {
            trimmed
                .strip_prefix("## ")
                .or_else(|| trimmed.strip_prefix("# "))
        }) {
            for (index, row) in wrap::wrap(rest, width.saturating_sub(1), "")
                .into_iter()
                .enumerate()
            {
                let style = theme.body().add_modifier(if index == 0 {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                });
                out.push(indented(vec![Span::styled(row, style)]));
            }
            continue;
        }

        // Bullet or numbered item. Unordered markers become `·`, as the original
        // renders them; the continuation hangs under the text, not the marker.
        let (body, marker, hang) = match bullet_marker(trimmed) {
            Some(len) => {
                let lead = line.len() - trimmed.len();
                let raw_marker = &trimmed[..len];
                let marker = if raw_marker.starts_with(['-', '*', '+']) {
                    format!("{}· ", " ".repeat(lead))
                } else {
                    format!("{}{}", " ".repeat(lead), raw_marker)
                };
                let hang = " ".repeat(wrap::width_of(&marker));
                (trimmed[len..].to_string(), marker, hang)
            }
            None => (line.clone(), String::new(), String::new()),
        };

        let available = width
            .saturating_sub(1)
            .saturating_sub(wrap::width_of(&marker));
        let mut inline = Inline::default();
        for (index, chunk) in wrap::wrap(&body, available, &hang).into_iter().enumerate() {
            let mut spans = Vec::new();
            if !marker.is_empty() {
                spans.push(Span::styled(
                    if index == 0 {
                        marker.clone()
                    } else {
                        " ".repeat(wrap::width_of(&marker))
                    },
                    theme.hint(),
                ));
            }
            spans.extend(inline_spans(&chunk, theme, &mut inline));
            out.push(indented(spans));
        }
    }

    // A fence still open at the end of the text is a code block mid-stream, not
    // a mistake: draw what has arrived instead of swallowing it until the model
    // types the closing backticks.
    if let Some(rows) = fence {
        render_code_block(rows, width, theme, out);
    }
}

/// A fenced code block: a wash card, sized to its own widest line.
///
/// The same decision inline `code` makes, at block scale — the original washes
/// code rather than tinting or framing it, and a card needs no rail to say where
/// it starts. Code is never re-wrapped at word boundaries (indentation is
/// meaning); overlong lines are truncated instead.
fn render_code_block(rows: Vec<String>, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    // Trailing blank lines before the closing fence are the model's formatting,
    // not content, and washing them paints an empty band.
    let end = rows.iter().rposition(|row| !row.trim().is_empty());
    let Some(end) = end else { return };
    let start = rows
        .iter()
        .position(|row| !row.trim().is_empty())
        .unwrap_or(0);

    // Full width, not sized to the widest line. A card that shrinks to its
    // content makes every code block a different width, and a column of
    // different-width cards is the ragged look this was trying to avoid.
    let fill = theme.code();
    let room = width.saturating_sub(4).max(4);
    out.push(block_pad(width, fill, None, 0));
    for row in &rows[start..=end] {
        out.push(block_row(
            vec![Span::styled(wrap::truncate(row, room), fill)],
            width,
            fill,
            None,
            0,
        ));
    }
    out.push(block_pad(width, fill, None, 0));
}

/// Length of a leading list marker (`- `, `* `, `1. `), if the line has one.
fn bullet_marker(trimmed: &str) -> Option<usize> {
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        return Some(2);
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && trimmed[digits..].starts_with(". ") {
        return Some(digits + 2);
    }
    None
}

/// Inline emphasis carried across the wrapped lines of one paragraph, so a
/// `**bold**` run that straddles a wrap point still renders as bold on both
/// rows instead of leaking its markers.
#[derive(Debug, Default, Clone, Copy)]
struct Inline {
    code: bool,
    bold: bool,
    italic: bool,
}

impl Inline {
    fn style(self, theme: &Theme) -> Style {
        // Inline code is a wash, not a hue — the original's `inline-code washes`.
        let mut style = if self.code {
            theme.code()
        } else {
            theme.body()
        };
        if self.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        style
    }
}

/// Turn one laid-out line into styled spans, consuming `` ` ``, `**` and `*`
/// markers. Not a markdown parser — a reader for the three inline forms agent
/// output actually uses, in one pass with no allocation beyond the spans.
///
/// An unclosed marker styles to end of paragraph, which reads better than
/// showing the marker.
fn inline_spans(text: &str, theme: &Theme, state: &mut Inline) -> Vec<Span<'static>> {
    // Fast path: most lines have no markers at all.
    if !text.contains('`') && !text.contains('*') {
        return vec![Span::styled(text.to_string(), state.style(theme))];
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buffer = String::new();
    let bytes = text.as_bytes();
    let mut index = 0usize;

    macro_rules! flush {
        () => {
            if !buffer.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut buffer),
                    state.style(theme),
                ));
            }
        };
    }

    while index < bytes.len() {
        match bytes[index] {
            b'`' => {
                flush!();
                state.code = !state.code;
                index += 1;
            }
            // Markers are inert inside code, where a literal asterisk is common.
            b'*' if !state.code && bytes.get(index + 1) == Some(&b'*') => {
                flush!();
                state.bold = !state.bold;
                index += 2;
            }
            // A single `*` opens emphasis only when something follows it; a
            // trailing or space-separated asterisk is just an asterisk.
            b'*' if !state.code
                && (state.italic
                    || bytes
                        .get(index + 1)
                        .is_some_and(|next| !next.is_ascii_whitespace())) =>
            {
                flush!();
                state.italic = !state.italic;
                index += 1;
            }
            _ => {
                let ch = text[index..].chars().next().expect("char boundary");
                buffer.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    flush!();
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), state.style(theme)));
    }
    spans
}

/// `HH:MM` in the local zone, from epoch millis.
fn format_clock(created_at: i64) -> String {
    use chrono::TimeZone as _;
    match chrono::Local.timestamp_millis_opt(created_at) {
        chrono::offset::LocalResult::Single(time) => time.format("%H:%M").to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_entry(id: &str, role: MessageRole, body: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: body.into(),
            }],
            created_at: 0,
            device_id: "d".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn theme() -> Theme {
        Theme::dark()
    }

    #[test]
    fn appending_to_the_tail_relays_out_only_the_tail() {
        let mut view = Transcript::new();
        let entries: Vec<_> = (0..20)
            .map(|i| text_entry(&format!("m{i}"), MessageRole::Assistant, "hello there"))
            .collect();
        view.set_entries(&entries, &[], 40, &theme());

        // Fingerprints of the untouched prefix must be identical across a
        // streaming update — that identity is what the cache reuses.
        let before: Vec<u64> = view.blocks.iter().map(|b| b.fingerprint).collect();
        let mut streamed = entries;
        let last = streamed.last_mut().unwrap();
        last.parts = vec![MessagePart::Text {
            id: "t0".into(),
            text: "hello there, and more arriving".into(),
        }];
        view.set_entries(&streamed, &[], 40, &theme());
        let after: Vec<u64> = view.blocks.iter().map(|b| b.fingerprint).collect();

        assert_eq!(before[..19], after[..19], "prefix must be cache-stable");
        assert_ne!(
            before[19], after[19],
            "the streaming message must be re-laid out"
        );
    }

    #[test]
    fn fingerprint_notices_every_rendered_change() {
        let base = text_entry("m", MessageRole::Assistant, "abc");
        // Same-length rewrite (the schema forbids it, but the cache must not
        // depend on that promise).
        let rewritten = text_entry("m", MessageRole::Assistant, "xyz");
        assert_ne!(fingerprint(&base, false), fingerprint(&rewritten, false));
        // Role, status, and part count all change the rendering.
        let mut other_role = base.clone();
        other_role.role = MessageRole::User;
        assert_ne!(fingerprint(&base, false), fingerprint(&other_role, false));
        let mut aborted = base.clone();
        aborted.status = Some(MessageStatus::Aborted);
        assert_ne!(fingerprint(&base, false), fingerprint(&aborted, false));
        // A tool resolving flips no length, only flags.
        let pending = SessionMessageEntry {
            parts: vec![MessagePart::Tool {
                id: "p".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
                is_error: false,
                resolved: false,
            }],
            ..base.clone()
        };
        let resolved = SessionMessageEntry {
            parts: vec![MessagePart::Tool {
                id: "p".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
                is_error: false,
                resolved: true,
            }],
            ..base.clone()
        };
        assert_ne!(fingerprint(&pending, false), fingerprint(&resolved, false));
        // An append past the hashed edges is still caught (length differs).
        let long = text_entry("m", MessageRole::Assistant, &"a".repeat(200));
        let longer = text_entry("m", MessageRole::Assistant, &"a".repeat(201));
        assert_ne!(fingerprint(&long, false), fingerprint(&longer, false));
        // Identical content hashes identically — the whole point.
        assert_eq!(
            fingerprint(&base, false),
            fingerprint(&text_entry("m", MessageRole::Assistant, "abc"), false)
        );
    }

    #[test]
    fn width_change_invalidates_the_whole_cache() {
        let mut view = Transcript::new();
        let entries = vec![text_entry(
            "m",
            MessageRole::Assistant,
            "a fairly long sentence that will wrap differently at other widths",
        )];
        view.set_entries(&entries, &[], 30, &theme());
        let narrow = view.total_lines();
        view.set_entries(&entries, &[], 70, &theme());
        assert!(
            view.total_lines() < narrow,
            "wider pane must produce fewer lines ({} then {})",
            narrow,
            view.total_lines()
        );
    }

    #[test]
    fn visible_window_is_bounded_by_pane_height_not_transcript_length() {
        let mut view = Transcript::new();
        let entries: Vec<_> = (0..500)
            .map(|i| text_entry(&format!("m{i}"), MessageRole::Assistant, "line"))
            .collect();
        view.set_entries(&entries, &[], 40, &theme());
        assert!(view.total_lines() > 500);

        let mut rows = 0usize;
        view.for_each_visible(10, |_, _| rows += 1);
        assert_eq!(rows, 10, "only the visible rows are visited");

        // Row offsets are contiguous from 0 — the renderer trusts them as y.
        let mut seen = Vec::new();
        view.for_each_visible(5, |row, _| seen.push(row));
        assert_eq!(seen, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn follow_pins_to_the_bottom_and_scrolling_up_releases_it() {
        let mut view = Transcript::new();
        let entries: Vec<_> = (0..30)
            .map(|i| text_entry(&format!("m{i}"), MessageRole::Assistant, "x"))
            .collect();
        view.set_entries(&entries, &[], 40, &theme());
        let height = 10;
        assert!(view.following());
        assert_eq!(view.top_line(height), view.total_lines() - height);

        view.scroll_up(4, height);
        assert!(!view.following());
        let parked = view.top_line(height);

        // New content arrives while parked: the view must not jump.
        let mut grown = entries;
        grown.push(text_entry("new", MessageRole::Assistant, "arriving"));
        view.set_entries(&grown, &[], 40, &theme());
        assert_eq!(
            view.top_line(height),
            parked,
            "a parked reader must not be dragged by new output"
        );

        // Scrolling back to the bottom re-pins.
        view.scroll_down(1000, height);
        assert!(view.following());
    }

    #[test]
    fn scroll_math_clamps_at_both_ends() {
        let mut view = Transcript::new();
        let entries: Vec<_> = (0..4)
            .map(|i| text_entry(&format!("m{i}"), MessageRole::Assistant, "x"))
            .collect();
        view.set_entries(&entries, &[], 40, &theme());
        // Pane taller than the content: top is pinned at 0, never negative.
        assert_eq!(view.top_line(100), 0);
        view.scroll_up(50, 100);
        assert_eq!(view.top_line(100), 0);
        view.to_top();
        assert_eq!(view.top_line(4), 0);
        view.scroll_up(1, 4);
        assert_eq!(view.top_line(4), 0);
        // Empty transcript renders nothing rather than panicking.
        let empty = Transcript::new();
        assert_eq!(empty.total_lines(), 0);
        empty.for_each_visible(10, |_, _| panic!("nothing to draw"));
    }

    #[test]
    fn retarget_clears_and_repins() {
        let mut view = Transcript::new();
        view.retarget(Some("chat-a".into()));
        let entries: Vec<_> = (0..20)
            .map(|i| text_entry(&format!("m{i}"), MessageRole::Assistant, "x"))
            .collect();
        view.set_entries(&entries, &[], 40, &theme());
        view.to_top();
        view.retarget(Some("chat-b".into()));
        assert!(view.is_empty());
        assert_eq!(view.total_lines(), 0);
        assert!(
            view.following(),
            "a freshly opened chat starts at the bottom"
        );
        // Retargeting to the same chat is a no-op, so a redundant selection
        // doesn't wipe a transcript mid-read.
        view.set_entries(
            &[text_entry("m", MessageRole::Assistant, "x")],
            &[],
            40,
            &theme(),
        );
        view.retarget(Some("chat-b".into()));
        assert!(!view.is_empty());
    }

    #[test]
    fn shrinking_transcript_clamps_the_cache_and_index() {
        let mut view = Transcript::new();
        let entries: Vec<_> = (0..10)
            .map(|i| text_entry(&format!("m{i}"), MessageRole::Assistant, "x"))
            .collect();
        view.set_entries(&entries, &[], 40, &theme());
        view.to_top();
        // Compaction can hand us fewer messages than we hold.
        view.set_entries(
            &[text_entry("m0", MessageRole::Assistant, "x")],
            &[],
            40,
            &theme(),
        );
        assert_eq!(view.blocks.len(), 1);
        assert_eq!(view.starts.len(), 2);
        assert_eq!(view.total_lines(), *view.starts.last().unwrap());
        let mut rows = 0;
        view.for_each_visible(20, |_, _| rows += 1);
        assert_eq!(rows, view.total_lines());
    }

    /// Flatten a rendered line to its text, for assertions.
    fn flatten(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn inline_markers_are_consumed_not_printed() {
        let mut lines = Vec::new();
        render_markdown(
            "a **bold** word, an *emphasised* one, and `code`, plus 2 * 3 and a lone *",
            200,
            &theme(),
            &mut lines,
        );
        let text = flatten(&lines[0]);
        assert!(!text.contains("**"), "bold markers leaked: {text:?}");
        assert!(text.contains("bold"), "{text:?}");
        assert!(text.contains("emphasised"), "{text:?}");
        assert!(text.contains("code"), "{text:?}");
        // A space-separated asterisk is arithmetic, not emphasis, and must
        // survive verbatim.
        assert!(text.contains("2 * 3"), "{text:?}");

        // The styles actually landed on the right spans.
        let bold = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("bold"))
            .expect("a bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("code"))
            .expect("a code span");
        // Inline code is a wash, as in the original — not a colored foreground.
        assert_eq!(code.style.bg, Some(theme().code_wash));
    }

    #[test]
    fn emphasis_spanning_a_wrap_point_stays_styled() {
        let mut lines = Vec::new();
        // Narrow enough that the bold run must break across two rows.
        render_markdown(
            "prefix **this whole run is bold across the break** suffix",
            20,
            &theme(),
            &mut lines,
        );
        assert!(lines.len() > 2, "expected a wrapped paragraph");
        for line in &lines {
            assert!(
                !flatten(line).contains("**"),
                "markers leaked onto a continuation row: {:?}",
                flatten(line)
            );
        }
        // The second row is inside the bold run, so its text span carries BOLD.
        let second = lines[1]
            .spans
            .iter()
            .find(|s| !s.content.trim().is_empty() && s.content.as_ref() != "  ")
            .expect("content on the second row");
        assert!(
            second.style.add_modifier.contains(Modifier::BOLD),
            "continuation row lost the emphasis: {second:?}"
        );
    }

    #[test]
    fn asterisks_inside_code_are_literal() {
        let mut lines = Vec::new();
        render_markdown("call `a ** b` then done", 200, &theme(), &mut lines);
        let text = flatten(&lines[0]);
        assert!(text.contains("a ** b"), "{text:?}");
    }

    #[test]
    fn code_fences_are_not_word_wrapped() {
        let mut lines = Vec::new();
        render_markdown(
            "before\n```\nlet x = 1;   // aligned\n```\nafter",
            40,
            &theme(),
            &mut lines,
        );
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        // Interior spacing survives, because code is truncated not re-wrapped.
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("let x = 1;   // aligned")),
            "{rendered:?}"
        );
    }

    #[test]
    fn escape_sequences_in_tool_output_never_reach_a_span() {
        let entry = SessionMessageEntry {
            parts: vec![MessagePart::Tool {
                id: "p".into(),
                call: ToolCall::Exec {
                    command: "printf '\x1b[31m'".into(),
                },
                is_error: false,
                resolved: true,
            }],
            ..text_entry("m", MessageRole::Assistant, "")
        };
        let lines = render_entry(&entry, 60, true, &theme());
        for line in &lines {
            for span in &line.spans {
                assert!(
                    !span.content.contains('\x1b'),
                    "raw ESC would desync the terminal: {:?}",
                    span.content
                );
            }
        }
    }
}
