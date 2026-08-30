//! Dedicated presentation for `send_subagent_message` tool calls.
//!
//! Destination and message arguments are inert literal text: they never enter generic media discovery, command interpretation, or Q&A parsing.

use std::sync::Arc;

use ratatui::text::{Line, Span};

use crate::appearance::AppearanceConfig;
use crate::render::wrapping::{RtOptions, word_wrap_lines, word_wrap_lines_with_joiners};
use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode,
    RenderedBlockOutput, Selectable, SelectionBoundaries, SelectionBoundary,
    SelectionBoundaryEntry, derive_selection_text, line_plain_text, selectable_cols,
};
use crate::theme::Theme;

const SENT_MESSAGE_ID_RANGE: u16 = 0;
const SENT_MESSAGE_TEXT_RANGE: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentMessagePresentation {
    Sending,
    Sent,
    Rejected { reason: String },
    Unconfirmed { reason: String },
}

impl SentMessagePresentation {
    pub(crate) fn title(&self) -> &'static str {
        match self {
            Self::Sending => "Sending message to subagent",
            Self::Sent => "Sent message to subagent",
            Self::Rejected { .. } => "Failed to send message to subagent",
            Self::Unconfirmed { .. } => "Message delivery unconfirmed",
        }
    }

    fn detail(&self) -> Option<(&str, MessageDetailStyle)> {
        match self {
            Self::Sending | Self::Sent => None,
            Self::Rejected { reason } => Some((reason, MessageDetailStyle::Error)),
            Self::Unconfirmed { reason } => Some((reason, MessageDetailStyle::Warning)),
        }
    }

    fn accent(&self, theme: &Theme, is_running: bool) -> ratatui::style::Color {
        match self {
            Self::Sending if is_running => theme.accent_running,
            Self::Sending | Self::Sent => theme.accent_tool,
            Self::Rejected { .. } => theme.accent_error,
            Self::Unconfirmed { .. } => theme.warning,
        }
    }

    pub(crate) fn is_failure(&self) -> bool {
        matches!(self, Self::Rejected { .. })
    }

    pub(crate) fn is_unconfirmed(&self) -> bool {
        matches!(self, Self::Unconfirmed { .. })
    }
}

#[derive(Debug, Clone, Copy)]
enum MessageDetailStyle {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct SentMessageToolCallBlock {
    pub presentation: SentMessagePresentation,
    pub subagent_id: Option<String>,
    pub text: Option<String>,
    pub started_at: Option<std::time::Instant>,
    pub elapsed_ms: Option<i64>,
}

impl SentMessageToolCallBlock {
    pub fn new(
        presentation: SentMessagePresentation,
        subagent_id: Option<String>,
        text: Option<String>,
    ) -> Self {
        Self {
            presentation,
            subagent_id,
            text,
            started_at: None,
            elapsed_ms: None,
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self.presentation, SentMessagePresentation::Sent)
    }

    pub fn is_failure(&self) -> bool {
        self.presentation.is_failure()
    }

    pub fn is_unconfirmed(&self) -> bool {
        self.presentation.is_unconfirmed()
    }

    pub fn finish(&mut self) {
        if self.elapsed_ms.is_none()
            && let Some(start) = self.started_at
        {
            self.elapsed_ms = Some(start.elapsed().as_millis() as i64);
        }
    }

    pub fn elapsed_ms(&self) -> Option<i64> {
        self.elapsed_ms.or_else(|| {
            self.started_at
                .map(|start| start.elapsed().as_millis() as i64)
        })
    }

    pub(crate) fn searchable_text(&self) -> Option<String> {
        crate::scrollback::block::join_searchable([
            Some(self.presentation.title().to_owned()),
            self.subagent_id.clone(),
            self.text.clone(),
            self.presentation
                .detail()
                .map(|(detail, _)| detail.to_owned()),
        ])
    }

    fn header(&self, theme: &Theme, is_muted: bool) -> Line<'static> {
        let style = if is_muted {
            theme.muted()
        } else {
            theme.primary()
        }
        .add_modifier(ratatui::style::Modifier::BOLD);
        Line::from(Span::styled(self.presentation.title(), style))
    }

    pub(crate) fn rendered_output(&self, ctx: &BlockContext) -> RenderedBlockOutput {
        let output = self.output(ctx);
        let boundaries = self.text.as_deref().map_or_else(Vec::new, |text| {
            let last_message_line = output
                .lines
                .iter()
                .rposition(|line| line.selection_range == Some(SENT_MESSAGE_TEXT_RANGE));
            let is_newline_only = !text.is_empty() && text.chars().all(|ch| ch == '\n');
            output
                .lines
                .iter()
                .enumerate()
                .filter_map(|(line_index, line)| {
                    if line.selection_range != Some(SENT_MESSAGE_TEXT_RANGE) {
                        return None;
                    }
                    let is_last = Some(line_index) == last_message_line;
                    let suffix = (is_last && text.ends_with('\n')).then(|| "\n".to_owned());
                    if is_newline_only
                        && selectable_cols(&line.content, &line.selectable)
                            .is_some_and(|cols| cols.is_empty())
                    {
                        Some(SelectionBoundaryEntry {
                            line_index,
                            boundary: Arc::new(SelectionBoundary::empty_row_anchor(
                                String::new(),
                                suffix.unwrap_or_default(),
                            )),
                        })
                    } else {
                        suffix.map(|suffix| SelectionBoundaryEntry {
                            line_index,
                            boundary: Arc::new(SelectionBoundary::new(String::new(), suffix)),
                        })
                    }
                })
                .collect()
        });
        RenderedBlockOutput {
            output,
            boundaries: SelectionBoundaries::from_entries(boundaries),
        }
    }
}

impl BlockContent for SentMessageToolCallBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let theme = Theme::current();
        let is_muted =
            ctx.mute_when_collapsed(ctx.appearance.scrollback.blocks.tool.muted_collapsed);
        if ctx.mode == DisplayMode::Collapsed {
            return BlockOutput {
                lines: vec![self.header(&theme, is_muted).into()],
            };
        }

        let width = (ctx.width as usize).saturating_sub(2).max(20);
        let mut lines: Vec<BlockLine> = vec![self.header(&theme, false).into()];
        if let Some((detail, style)) = self.presentation.detail() {
            let color = match style {
                MessageDetailStyle::Error => theme.accent_error,
                MessageDetailStyle::Warning => theme.warning,
            };
            lines.push(BlockLine::separator(Line::from("")));
            for line in word_wrap_lines(
                detail
                    .split('\n')
                    .map(|line| Line::from(Span::styled(line.to_owned(), theme.fg(color)))),
                width,
            ) {
                let mut line = BlockLine::styled(line);
                line.selectable = Selectable::None;
                lines.push(line);
            }
        }

        lines.push(Line::from("").into());
        let id_wrap = RtOptions::new(width)
            .initial_indent(Line::from(Span::styled("Subagent ID: ", theme.muted())));
        let id_value = Line::from(Span::styled(
            self.subagent_id
                .as_deref()
                .unwrap_or("unavailable")
                .to_owned(),
            theme.primary(),
        ));
        let (wrapped_id, id_joiners) =
            word_wrap_lines_with_joiners(std::iter::once(id_value), id_wrap);
        for (index, (line, joiner)) in wrapped_id.into_iter().zip(id_joiners).enumerate() {
            let mut line = BlockLine::styled(line)
                .with_selection_range(Some(SENT_MESSAGE_ID_RANGE))
                .with_joiner(joiner);
            if index == 0 {
                line.selectable = Selectable::Spans(1..line.content.spans.len());
            }
            line.selection_text = Some(derive_selection_text(&line));
            lines.push(line);
        }
        lines.push(Line::from("").into());
        lines.push(BlockLine::separator(Line::from(Span::styled(
            "Message:",
            theme.muted(),
        ))));
        match &self.text {
            Some(text) => {
                let source_lines = text
                    .split('\n')
                    .map(|line| Line::from(Span::styled(line.to_owned(), theme.muted())));
                let (wrapped, joiners) = word_wrap_lines_with_joiners(source_lines, width);
                let trailing_empty_row = text.ends_with('\n')
                    && wrapped
                        .last()
                        .is_some_and(|line| line_plain_text(line).is_empty());
                let visible_len = wrapped
                    .len()
                    .saturating_sub(usize::from(trailing_empty_row));
                for (index, (line, joiner)) in wrapped.into_iter().zip(joiners).enumerate() {
                    if trailing_empty_row && index == visible_len {
                        break;
                    }
                    let selection_text = line_plain_text(&line);
                    lines.push(
                        BlockLine::styled(line)
                            .with_selection_range(Some(SENT_MESSAGE_TEXT_RANGE))
                            .with_selection_text(Some(selection_text))
                            .with_joiner(joiner),
                    );
                }
                if trailing_empty_row {
                    lines.push(BlockLine::separator(Line::from("")));
                }
            }
            None => {
                let mut line =
                    BlockLine::styled(Line::from(Span::styled("unavailable", theme.muted())));
                line.selectable = Selectable::None;
                lines.push(line);
            }
        }

        BlockOutput { lines }
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        if ctx.mode == DisplayMode::Collapsed {
            return None;
        }
        Some(AccentStyle::static_color(
            self.presentation.accent(&Theme::current(), ctx.is_running),
        ))
    }

    fn bullet(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        match &self.presentation {
            SentMessagePresentation::Rejected { .. }
            | SentMessagePresentation::Unconfirmed { .. } => Some(AccentStyle::static_color(
                self.presentation.accent(&Theme::current(), ctx.is_running),
            )),
            SentMessagePresentation::Sending | SentMessagePresentation::Sent
                if ctx.mode == DisplayMode::Collapsed =>
            {
                None
            }
            SentMessagePresentation::Sending | SentMessagePresentation::Sent => self.accent(ctx),
        }
    }

    fn has_vpad_for(&self, _appearance: &AppearanceConfig) -> bool {
        false
    }

    fn background(&self, _ctx: &BlockContext) -> BlockBackground {
        BlockBackground::None
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_groupable(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        self.subagent_id.is_some() || self.text.is_some() || self.presentation.detail().is_some()
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Collapsed
    }

    fn next_fold_mode(&self, current: DisplayMode, is_running: bool) -> DisplayMode {
        if is_running {
            match current {
                DisplayMode::Truncated => DisplayMode::Expanded,
                DisplayMode::Collapsed | DisplayMode::Expanded => DisplayMode::Truncated,
            }
        } else {
            match current {
                DisplayMode::Collapsed => DisplayMode::Expanded,
                DisplayMode::Truncated | DisplayMode::Expanded => DisplayMode::Collapsed,
            }
        }
    }

    fn collapse_mode(&self, is_running: bool) -> DisplayMode {
        if is_running {
            DisplayMode::Truncated
        } else {
            DisplayMode::Collapsed
        }
    }
}

#[cfg(test)]
#[path = "sent_message_tests.rs"]
mod tests;
