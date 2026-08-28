use super::*;
use crate::scrollback::ToolCallBlock;
use crate::scrollback::block::RenderBlock;
use crate::scrollback::render::ScratchBuffer;
use crate::scrollback::scrollback_pane::ScrollbackPane;
use crate::scrollback::state::ScrollbackState;
use crate::scrollback::text_selection::{
    ActiveTextDrag, ResolvedSelectionBoundaries, ResolvedSelectionModel,
    reconstruct_full_selection_text_with_boundaries, reconstruct_selection_text_with_boundaries,
};
use crate::scrollback::types::{BlockContext, RenderedBlockOutput, line_plain_text};

fn context(width: u16, mode: DisplayMode) -> BlockContext {
    BlockContext {
        width,
        mode,
        is_running: false,
        raw: false,
        max_lines: None,
        appearance: Default::default(),
        is_selected: false,
        cwd: None,
    }
}

fn rendered(block: &SentMessageToolCallBlock, width: u16, mode: DisplayMode) -> String {
    block
        .output(&context(width, mode))
        .lines
        .iter()
        .map(|line| line_plain_text(&line.content))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_selection_state(
    block: SentMessageToolCallBlock,
    width: u16,
) -> (
    ResolvedSelectionModel,
    ResolvedSelectionBoundaries,
    RenderedBlockOutput,
) {
    let mut state = ScrollbackState::new();
    let id = state.push_block(RenderBlock::ToolCall(ToolCallBlock::SentMessage(block)));
    state
        .get_by_id_mut(id)
        .expect("message entry")
        .set_display_mode(DisplayMode::Expanded);
    let area = ratatui::layout::Rect::new(0, 0, width, 40);
    state.prepare_layout(area.width, area.height);
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    let mut scratch = ScratchBuffer::default();
    let rendered = ScrollbackPane::new()
        .active(true)
        .render_with_scratch_and_selection_boundaries(area, &mut buffer, &state, &mut scratch);
    let content_width = rendered
        .output
        .selection_model
        .visible_block_content_width(0)
        .expect("message content width");
    let entry = state.get(0).expect("message entry");
    entry.ensure_cached(content_width, state.appearance(), false, state.cwd());
    let cached = entry.cached_rendered_output_ref().clone();
    (
        rendered.output.selection_model,
        rendered.selection_boundaries,
        cached,
    )
}

fn reachable_full_range_drag(model: &ResolvedSelectionModel, range_id: u16) -> ActiveTextDrag {
    let range = model.range(0, range_id).expect("selection range");
    let first = range.lines.first().expect("selection range first line");
    let last = range.lines.last().expect("selection range last line");
    let anchor = model
        .hit_test_text_exact(
            first.screen_x.saturating_add(first.selectable_cols.start),
            first.screen_y,
        )
        .expect("first painted cell must be hit-testable");
    assert_eq!(anchor.range_id, range_id);
    let last_col = last
        .screen_x
        .saturating_add(last.selectable_cols.end.saturating_sub(1));
    let head = model
        .hit_test_nearest_in_range(anchor, last_col, last.screen_y)
        .expect("last painted cell must be reachable in range");
    assert_eq!(head.range_id, range_id);
    ActiveTextDrag {
        anchor,
        head,
        kind: Default::default(),
        anchor_content_width: None,
    }
}

fn reconstruct_visible_and_cached(
    model: &ResolvedSelectionModel,
    boundaries: &ResolvedSelectionBoundaries,
    cached: &RenderedBlockOutput,
    drag: &ActiveTextDrag,
) -> (String, String) {
    let visible = reconstruct_selection_text_with_boundaries(model, boundaries, drag)
        .expect("visible message range reconstructs");
    let full = reconstruct_full_selection_text_with_boundaries(
        &cached.output.lines,
        &cached.boundaries,
        drag,
    )
    .expect("production full message range reconstructs");
    (visible, full)
}

#[test]
fn sent_block_uses_exact_success_title_and_renders_arguments_as_inert_text() {
    let text =
        "Questions asked:\n- \"keep /tmp/existing.mp4 and ![x](/tmp/x.png)\"\n  Answer: yes\n";
    let block = SentMessageToolCallBlock::new(
        SentMessagePresentation::Sent,
        Some("sub-123".into()),
        Some(text.into()),
    );

    assert_eq!(
        rendered(&block, 120, DisplayMode::Collapsed),
        "Sent message to subagent"
    );
    assert_eq!(
        rendered(&block, 120, DisplayMode::Expanded),
        format!("Sent message to subagent\n\nSubagent ID: sub-123\n\nMessage:\n{text}")
    );
    assert!(block.image_references().is_empty());
    assert!(block.video_references().is_empty());
    assert!(block.inline_open_button().is_none());
}

#[test]
fn wrapped_message_joiners_reconstruct_word_midword_and_trailing_newline_exactly() {
    let text = "alpha beta supercalifragilisticexpialidocious\n";
    let block = SentMessageToolCallBlock::new(
        SentMessagePresentation::Sent,
        Some("sub-123".into()),
        Some(text.into()),
    );
    let output = block.output(&context(22, DisplayMode::Expanded));
    let message_start = output
        .lines
        .iter()
        .position(|line| line_plain_text(&line.content) == "Message:")
        .expect("message heading")
        + 1;
    let message_lines = &output.lines[message_start..];
    assert!(
        message_lines
            .iter()
            .any(|line| line.joiner.as_deref() == Some(" "))
    );
    assert!(
        message_lines
            .iter()
            .any(|line| line.joiner.as_deref() == Some(""))
    );

    let (model, boundaries, cached) = render_selection_state(block, 22);
    assert!(
        model.range(0, SENT_MESSAGE_TEXT_RANGE).is_some(),
        "message selection range"
    );
    let drag = reachable_full_range_drag(&model, SENT_MESSAGE_TEXT_RANGE);
    let visible = reconstruct_selection_text_with_boundaries(&model, &boundaries, &drag)
        .expect("visible message range reconstructs");
    let full = reconstruct_full_selection_text_with_boundaries(
        &cached.output.lines,
        &cached.boundaries,
        &drag,
    )
    .expect("production full message range reconstructs");
    assert_eq!(visible, text);
    assert_eq!(full, text);

    let last_line = model
        .range(0, SENT_MESSAGE_TEXT_RANGE)
        .and_then(|range| range.lines.last())
        .expect("last reachable message row");
    let partial_hit = model
        .hit_test_text_exact(
            last_line
                .screen_x
                .saturating_add(last_line.selectable_cols.start),
            last_line.screen_y,
        )
        .expect("partial selection endpoint must be hit-testable");
    let mut partial = drag;
    partial.anchor = partial_hit;
    partial.head = partial_hit;
    let visible_partial = reconstruct_selection_text_with_boundaries(&model, &boundaries, &partial)
        .expect("visible partial message selection");
    let full_partial = reconstruct_full_selection_text_with_boundaries(
        &cached.output.lines,
        &cached.boundaries,
        &partial,
    )
    .expect("production partial message selection");
    assert!(!visible_partial.ends_with('\n'));
    assert!(!full_partial.ends_with('\n'));
    assert_eq!(
        model
            .range(0, SENT_MESSAGE_ID_RANGE)
            .expect("destination selection range")
            .range_id,
        SENT_MESSAGE_ID_RANGE
    );
}

#[test]
fn newline_only_messages_have_real_selection_anchors_and_copy_exactly() {
    for text in ["\n", "\n\n"] {
        let block = SentMessageToolCallBlock::new(
            SentMessagePresentation::Sent,
            Some("sub-123".into()),
            Some(text.into()),
        );
        let (model, boundaries, cached) = render_selection_state(block, 40);
        let range = model
            .range(0, SENT_MESSAGE_TEXT_RANGE)
            .expect("blank message selection range");
        let blank_rows = text.chars().filter(|&ch| ch == '\n').count();
        assert_eq!(range.lines.len(), blank_rows);
        assert!(range.lines.iter().all(|line| {
            line.text.is_empty()
                && line.painted_region.as_deref() == Some("")
                && line
                    .selectable_cols
                    .end
                    .saturating_sub(line.selectable_cols.start)
                    == 1
                && model
                    .hit_test_text_exact(
                        line.screen_x.saturating_add(line.selectable_cols.start),
                        line.screen_y,
                    )
                    .is_some()
        }));

        let drag = reachable_full_range_drag(&model, SENT_MESSAGE_TEXT_RANGE);
        let (visible, full) = reconstruct_visible_and_cached(&model, &boundaries, &cached, &drag);
        assert_eq!(visible, text);
        assert_eq!(full, text);
        let output_text = cached
            .output
            .lines
            .iter()
            .map(|line| line_plain_text(&line.content))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!output_text.contains('\u{200B}'));
        assert!(!output_text.contains('\u{00A0}'));

        if range.lines.len() > 1 {
            let first = range.lines.first().expect("first blank row");
            let first_hit = model
                .hit_test_text_exact(
                    first.screen_x.saturating_add(first.selectable_cols.start),
                    first.screen_y,
                )
                .expect("first blank row anchor");
            let partial = ActiveTextDrag {
                anchor: first_hit,
                head: first_hit,
                kind: Default::default(),
                anchor_content_width: None,
            };
            let (visible_partial, full_partial) =
                reconstruct_visible_and_cached(&model, &boundaries, &cached, &partial);
            assert_eq!(visible_partial, "");
            assert_eq!(full_partial, "");
        }
    }
}

#[test]
fn destination_id_wraps_literally_and_reconstructs_through_joiners() {
    let subagent_id = "019d0000-0000-7000-8000-000000000001";
    let block = SentMessageToolCallBlock::new(
        SentMessagePresentation::Sent,
        Some(subagent_id.into()),
        Some("hello".into()),
    );
    let output = block.output(&context(22, DisplayMode::Expanded));
    let id_start = output
        .lines
        .iter()
        .position(|line| line_plain_text(&line.content).starts_with("Subagent ID:"))
        .expect("subagent id heading");
    let id_end = output.lines[id_start..]
        .iter()
        .position(|line| line_plain_text(&line.content).is_empty())
        .map_or(output.lines.len(), |offset| id_start + offset);
    let id_lines = &output.lines[id_start..id_end];
    assert!(
        id_lines
            .iter()
            .skip(1)
            .all(|line| line.joiner.as_deref() == Some("")),
        "UUID-only continuations are mid-word/hyphen joins"
    );

    let (model, boundaries, cached) = render_selection_state(block, 22);
    assert!(
        model.range(0, SENT_MESSAGE_ID_RANGE).is_some(),
        "destination selection range"
    );
    let drag = reachable_full_range_drag(&model, SENT_MESSAGE_ID_RANGE);
    let visible = reconstruct_selection_text_with_boundaries(&model, &boundaries, &drag)
        .expect("visible destination range reconstructs");
    let full = reconstruct_full_selection_text_with_boundaries(
        &cached.output.lines,
        &cached.boundaries,
        &drag,
    )
    .expect("production destination range reconstructs");
    assert_eq!(visible, subagent_id);
    assert_eq!(full, subagent_id);
    assert!(
        model.range(0, SENT_MESSAGE_TEXT_RANGE).is_some(),
        "destination and message must have independent ranges"
    );
}

#[test]
fn dedicated_block_participates_in_search_selection_and_export() {
    let block = SentMessageToolCallBlock::new(
        SentMessagePresentation::Unconfirmed {
            reason: "Delivery could not be confirmed".into(),
        },
        Some("sub-123".into()),
        Some("literal follow up".into()),
    );
    let render_block = RenderBlock::ToolCall(ToolCallBlock::SentMessage(block));

    let searchable = render_block.searchable_text().expect("searchable message");
    assert!(searchable.contains("sub-123"));
    assert!(searchable.contains("literal follow up"));
    let selected = render_block
        .copy_visible_text_in_state(&context(40, DisplayMode::Expanded))
        .expect("copyable visible message");
    assert!(selected.contains("literal follow up"));
    assert_eq!(
        crate::scrollback::export::render_blocks_to_markdown([&render_block]),
        "## Tools\n\n- Message delivery unconfirmed"
    );
}

#[test]
fn rejected_and_unconfirmed_have_distinct_failure_semantics() {
    let rejected = SentMessageToolCallBlock::new(
        SentMessagePresentation::Rejected {
            reason: "Subagent is not active or is finalizing.".into(),
        },
        Some("null".into()),
        Some(String::new()),
    );
    let unconfirmed = SentMessageToolCallBlock::new(
        SentMessagePresentation::Unconfirmed {
            reason: "The message may already have been accepted.".into(),
        },
        Some("sub-123".into()),
        Some("retry?".into()),
    );

    assert!(rejected.is_foldable());
    assert!(unconfirmed.is_foldable());
    assert!(!rejected.is_success());
    assert!(rejected.is_failure());
    assert!(!rejected.is_unconfirmed());
    assert!(!unconfirmed.is_success());
    assert!(!unconfirmed.is_failure());
    assert!(unconfirmed.is_unconfirmed());

    let sending = SentMessageToolCallBlock::new(SentMessagePresentation::Sending, None, None);
    assert!(!sending.is_success());
    assert!(!sending.is_failure());
    assert!(!sending.is_unconfirmed());
    assert!(
        rendered(&rejected, 120, DisplayMode::Expanded)
            .starts_with("Failed to send message to subagent")
    );
    assert!(
        rendered(&unconfirmed, 120, DisplayMode::Expanded)
            .starts_with("Message delivery unconfirmed")
    );
}
