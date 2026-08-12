//! Scroll-aware rendering for scrollback entries.

use std::sync::Arc;

use derive_more::{Deref, DerefMut};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::block::{BlockContent, RenderBlock};
use super::entry::ScrollbackEntry;
use super::layout::HorizontalLayout;
use super::state::EntryLayoutInfo;
use super::state::groups::{GroupKind, GroupSpan, span_containing};
use super::state::verb_group::{
    GroupHeaderLabel, truncation_header_label, verb_group_header_label,
};
use super::text_selection::{
    ResolvedSelectableLine, ResolvedSelectionBoundaries, ResolvedSelectionModel,
    VisibleBlockGeometry,
};
use super::types::{
    DisplayMode, derive_selection_text, line_plain_text_into, selectable_cols,
    selectable_cols_usize,
};
use super::wrappers::{EntryRenderer, group_header_chrome_prefix_width};
use crate::appearance::AppearanceConfig;
use crate::render::Renderable;
use crate::render::osc8::{LinkOverlay, OverlayLink};
use crate::theme::Theme;

/// `range_id` of a labeled group header's synthetic selectable row — verb-run
/// headers and truncation headers carrying an aggregated label. Reserved at
/// the top of the id space: block `selection_range` ids count up from 0, and
/// in the EXPANDED verb slot member 0's own line 0 (range 0, block line 0) is
/// mapped alongside the header — a shared id would merge both rows into one
/// selectable range, so a drag on either row selected and copied both.
pub(crate) const GROUP_HEADER_RANGE_ID: u16 = u16::MAX;

/// Label for the inline-media native-open text button (terminals without
/// inline graphics). Graphics terminals use a shorter overlay `[Open]` instead.
pub fn media_open_button_label(is_video: bool) -> &'static str {
    if is_video {
        "[Open Video]"
    } else {
        "[Open Image]"
    }
}

/// Centered left column for the `[Open]` text button. Shared by the renderer
/// and the hit-area computation so the label and click target stay aligned.
pub fn media_open_button_col(content_width: u16, is_video: bool) -> u16 {
    let label_w = media_open_button_label(is_video).len() as u16;
    content_width.saturating_sub(label_w) / 2
}

/// Width reserved for the timestamp on message blocks.
///
/// Matches the constant in `EntryRenderer::timestamp_reserved()`.
fn timestamp_reserved_for_block(block: &RenderBlock, appearance: &AppearanceConfig) -> u16 {
    if appearance.show_timestamps
        && matches!(
            block,
            RenderBlock::UserPrompt(_) | RenderBlock::AgentMessage(_) | RenderBlock::Btw(_)
        )
    {
        10
    } else {
        0
    }
}

/// A reusable scratch buffer for rendering clipped entries.
///
/// This wraps ratatui's `Buffer` with `Deref`/`DerefMut` so all standard
/// Buffer methods work directly. The wrapper exists to:
/// - Make scratch buffer usage greppable in the codebase
/// - Provide a place for future optimization methods (bulk copy, fill, etc.)
///
/// # Usage
///
/// Create once and reuse across frames:
/// ```ignore
/// let mut scratch = ScratchBuffer::new();
/// // In render loop:
/// scratch.prepare(width, height);
/// // ... use scratch for rendering ...
/// ```
///
/// # Future Enhancements
///
/// ratatui's Buffer has limitations we could address:
/// - `resize()` always reallocates when growing (no capacity tracking)
/// - No efficient `fill()` - could use `vec.fill()` instead of cell-by-cell
/// - No bulk copy operations
#[derive(Default, Deref, DerefMut)]
pub struct ScratchBuffer(Buffer);

impl ScratchBuffer {
    /// Create a new empty scratch buffer.
    pub fn new() -> Self {
        Self(Buffer::default())
    }

    /// Resize and reset buffer for reuse.
    ///
    /// We must reset because `set_style()` only changes style, not content.
    /// If previous content was longer than new content, old chars would remain.
    /// TODO: When we fork Buffer, use `vec.fill(Cell::default())` for efficiency.
    pub fn prepare(&mut self, width: u16, height: u16) {
        self.resize(Rect::new(0, 0, width, height));
        self.reset();
    }

    /// Prepare and return mutable reference (convenience for chaining).
    pub fn prepared(&mut self, width: u16, height: u16) -> &mut Self {
        self.prepare(width, height);
        self
    }
}

/// A visible inline media entry with its screen position and crop info.
#[derive(Debug, Clone)]
pub struct InlineMediaPlacement {
    /// Media metadata (path, dimensions, type).
    pub info: crate::prompt_images::InlineMediaInfo,
    /// Screen rect where the visible portion of the image is rendered.
    pub screen_rect: ratatui::layout::Rect,
    /// Total image rows when fully visible (for crop calculation).
    pub full_rows: u16,
    /// Number of rows cropped from the top (0 = no crop).
    pub top_crop_rows: u16,
    /// Screen rect of the filepath line (second line of the media block header),
    /// if visible. Used for click-to-copy hit testing.
    pub filepath_screen_rect: Option<ratatui::layout::Rect>,
    /// Screen rect of the text `[Open]` button line, if visible. Present only
    /// for text-button placements (media on terminals without inline-graphics
    /// support; `full_rows` is 0). Used for click-to-open-natively hit testing.
    pub open_button_screen_rect: Option<ratatui::layout::Rect>,
    /// Whether this placement reserves a trailing `[Open]/[Copy]` (or play)
    /// button row beneath the image. True for an overlay/image tool-media
    /// placement; false for the text-`[Open]` placement (terminals without
    /// inline graphics), whose button is the `[Open]` text line itself.
    pub has_button_row: bool,
}

/// A visible Mermaid diagram affordance row with its screen position and the
/// diagram source its buttons act on. The draw loop paints
/// `◇ mermaid [Open Image] [Copy Image Path] [Copy Source]` onto `screen_rect` and registers
/// the click hit-rects; the reserved (blank) row already scrolls with the
/// surrounding content. Rendering is lazy (driven from the source on click), so
/// no rendered path/state is carried here.
#[derive(Debug, Clone)]
pub struct DiagramAffordancePlacement {
    /// Screen rect of the affordance row (one row tall, content-area width).
    pub screen_rect: ratatui::layout::Rect,
    /// Diagram source (the fence body); the data every button acts on.
    pub source: String,
}

/// Result of rendering entries.
#[derive(Debug, Clone, Default)]
pub struct ScrollRenderResult {
    /// Virtual-y end of the passed slice: `content_y0` + heights + gaps of the
    /// entries given to the renderer. Equals the full content height only when
    /// the caller passes the full list from content top; the production
    /// windowed caller ignores it and uses `prepare_layout()`'s total.
    /// `usize` so tall sessions (> `u16::MAX` rows) are not truncated.
    pub total_height: usize,
    /// Area occupied by the selected entry (if visible).
    /// This is used for drawing selection borders.
    /// None if selected entry is not visible or partially clipped.
    pub selected_area: Option<SelectedEntryArea>,
    /// Per-frame resolved selection metadata for visible content.
    pub selection_model: ResolvedSelectionModel,
    /// Accumulated link overlay for OSC 8 post-flush emission.
    pub link_overlay: LinkOverlay,
    /// Inline media to render via post-flush escape sequences.
    pub inline_media: Vec<InlineMediaPlacement>,
    /// Diagram affordance rows to paint + register click hit-rects for.
    pub diagram_affordances: Vec<DiagramAffordancePlacement>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ScrollRenderResultWithBoundaries {
    pub(crate) result: ScrollRenderResult,
    pub(crate) selection_boundaries: ResolvedSelectionBoundaries,
}

/// Information about the selected entry's visible area.
#[derive(Debug, Clone)]
pub struct SelectedEntryArea {
    /// The area where the entry was rendered.
    pub area: Rect,
    /// Whether the top of the entry is clipped (don't draw top border).
    pub top_clipped: bool,
    /// Whether the bottom of the entry is clipped (don't draw bottom border).
    pub bottom_clipped: bool,
}

/// Render entries with scroll support.
///
/// # Parameters
/// - `entry_layouts_cache`: Pre-computed layout info (height + gap_after) for each entry.
///   Must be same length as `entries`. Comes from the LayoutCache populated by `prepare_layout()`.
/// - `tick`: Animation tick counter for animated elements (e.g., running block accents).
/// - `content_y0`: Virtual-Y of `entries[0]` in scroll-offset space (0 when the
///   slice starts at content top). Callers that pass a viewport-only window set
///   this from the layout cache so off-screen history is not re-walked here.
/// - `entry_index_base`: Added to each slice index for selection-model indices and
///   `selected_idx` / `dim_from_entry` matching (relative to the caller's full
///   visible range, not the paint window).
/// - `group_spans`: The fold model from the layout cache plus the absolute
///   entry index of `entries[0]`, used to bound verb-group header labels to
///   exactly the folded run and to build the aggregated labels on truncation
///   headers. `None` (harnesses without a layout pass) falls back to the
///   verb label walk's own run classification and to the plain "N more" /
///   "N tool calls & thoughts" truncation text.
/// - `cwd`: Session/worktree cwd used for path-aware measurement and paint.
///
/// # Panics
/// Debug-asserts if `entry_layouts_cache.len() != entries.len()`.
#[allow(clippy::too_many_arguments)]
pub fn render_scrolled_entries_with_scratch(
    buf: &mut Buffer,
    viewport: Rect,
    entries: &[&ScrollbackEntry],
    scroll_offset: usize,
    selected_idx: Option<usize>,
    theme: &Theme,
    appearance: &AppearanceConfig,
    entry_layouts_cache: &[EntryLayoutInfo],
    tick: u64,
    mouse_pos: Option<(u16, u16)>,
    dim_from_entry: Option<usize>,
    search_highlight: Option<&regex::Regex>,
    content_y0: usize,
    entry_index_base: usize,
    // Absolute paths of media generated in this transcript, used to resolve the
    // short relative paths the model prints (`images/1.jpg`) to clickable links.
    media_paths: &[std::path::PathBuf],
    group_spans: Option<(&[GroupSpan], usize)>,
    cwd: Option<&std::path::Path>,
) -> ScrollRenderResult {
    render_scrolled_entries_with_selection_boundaries(
        buf,
        viewport,
        entries,
        scroll_offset,
        selected_idx,
        theme,
        appearance,
        entry_layouts_cache,
        tick,
        mouse_pos,
        dim_from_entry,
        search_highlight,
        content_y0,
        entry_index_base,
        media_paths,
        group_spans,
        cwd,
    )
    .result
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_scrolled_entries_with_selection_boundaries(
    buf: &mut Buffer,
    viewport: Rect,
    entries: &[&ScrollbackEntry],
    scroll_offset: usize,
    selected_idx: Option<usize>,
    theme: &Theme,
    appearance: &AppearanceConfig,
    entry_layouts_cache: &[EntryLayoutInfo],
    tick: u64,
    mouse_pos: Option<(u16, u16)>,
    dim_from_entry: Option<usize>,
    search_highlight: Option<&regex::Regex>,
    content_y0: usize,
    entry_index_base: usize,
    media_paths: &[std::path::PathBuf],
    group_spans: Option<(&[GroupSpan], usize)>,
    cwd: Option<&std::path::Path>,
) -> ScrollRenderResultWithBoundaries {
    if entries.is_empty() || viewport.width == 0 || viewport.height == 0 {
        return ScrollRenderResultWithBoundaries::default();
    }

    let mut result = ScrollRenderResult::default();
    let mut selection_boundaries = ResolvedSelectionBoundaries::default();

    debug_assert_eq!(
        entry_layouts_cache.len(),
        entries.len(),
        "entry_layouts_cache must match entries length - was prepare_layout() called?"
    );

    // Create horizontal layout for this viewport using config
    let layout = HorizontalLayout::new(viewport, &appearance.scrollback.layout);

    // Total height = y of first passed entry + span of this slice (incl. gaps).
    // Use usize so tall sessions are never truncated. When the caller
    // passes a viewport window, this is only the window's end — production uses
    // prepare_layout()'s total; full-slice callers (tests) get the true total.
    result.total_height = content_y0
        + entry_layouts_cache
            .iter()
            .map(|l| l.height as usize + l.gap_after as usize)
            .sum::<usize>();

    let viewport_start = scroll_offset;
    let viewport_end = scroll_offset + viewport.height as usize;

    result.selection_model.content_area = layout.content;

    // Reused across all visible rows so the search-highlight pass allocates at
    // most once per frame (not once per row). Empty until search is active.
    let mut highlight_text = String::new();

    // Walk only the passed slice (viewport window in production). No full-list
    // EntryLayout vec — y advances from content_y0 using cached heights/gaps.
    let mut y = content_y0;
    for (i, entry) in entries.iter().enumerate() {
        let height = entry_layouts_cache[i].height;
        let entry_start = y;
        let entry_end = entry_start + height as usize;

        // Skip if completely above viewport
        if entry_end <= viewport_start {
            y = entry_end + entry_layouts_cache[i].gap_after as usize;
            continue;
        }
        // Stop if completely below viewport
        if entry_start >= viewport_end {
            break;
        }

        let logical_idx = i + entry_index_base;

        // Calculate visibility
        let top_clipped = entry_start < viewport_start;
        let bottom_clipped = entry_end > viewport_end;

        // Calculate render position (narrowed to u16 for screen coordinates —
        // these deltas are always within viewport height which fits in u16).
        let render_y: u16 = if top_clipped {
            viewport.y
        } else {
            viewport.y + (entry_start - viewport_start) as u16
        };

        // Calculate visible height
        let skip_rows: u16 = if top_clipped {
            (viewport_start - entry_start) as u16
        } else {
            0
        };
        let visible_height: u16 = if bottom_clipped {
            (viewport_end - entry_start.max(viewport_start)) as u16
        } else {
            height - skip_rows
        };

        // Calculate actual render height (clamped to viewport)
        let render_height = visible_height.min(viewport.height);

        if render_height == 0 {
            y = entry_end + entry_layouts_cache[i].gap_after as usize;
            continue;
        }

        // Create the area for this entry's content
        let entry_row_layout = layout.for_row(render_y, render_height);
        let entry_content_area = entry_row_layout.entry_content_area();

        // Render the entry — skip_rows handles partial visibility directly
        let is_selected = selected_idx == Some(logical_idx);
        let entry_layout_info = &entry_layouts_cache[i];
        // Group headers rebuild their aggregated label each frame so counts/
        // tense/live target track the streaming run in place. Both fold
        // families feed the one label channel; a header row belongs to
        // exactly one fold, so the branches are exclusive by construction.
        //
        // Verb-group headers: the fold's span bounds the walk to exactly the
        // claimed run; without spans the walk stops at its own run-breaker
        // classification.
        let header_label = if entry_layout_info.verb_group_header {
            let show_thinking = crate::appearance::cache::load_show_thinking_blocks();
            let end = group_spans
                .and_then(|(spans, base)| {
                    let span = span_containing(spans, base + i)?;
                    Some(span.range.end.saturating_sub(base))
                })
                .unwrap_or(entries.len());
            Some(GroupHeaderLabel::VerbRun(verb_group_header_label(
                entries,
                i,
                end,
                show_thinking,
                theme,
            )))
        } else if entry_layout_info.is_group_header()
            && crate::appearance::cache::load_group_tool_verbs()
        {
            // Truncation headers get the same aggregated vocabulary over the
            // rows they hide (prefix only while collapsed; the whole run when
            // expanded), gated on the "Group tool calls" setting that owns
            // this vocabulary. Falls back to the renderer's plain "N more"
            // count when the setting is off, spans are absent, or the walk
            // declines (pure thoughts, or hidden rows it cannot name).
            let show_thinking = crate::appearance::cache::load_show_thinking_blocks();
            group_spans
                .and_then(|(spans, base)| {
                    let span = span_containing(spans, base + i)?;
                    let GroupKind::Truncation { hidden, .. } = span.kind else {
                        return None;
                    };
                    let start = span.range.start.saturating_sub(base);
                    let end = span.range.end.saturating_sub(base);
                    truncation_header_label(
                        entries,
                        start..end,
                        (!span.expanded).then_some(hidden),
                        show_thinking,
                        theme,
                    )
                })
                .map(GroupHeaderLabel::Truncation)
        } else {
            None
        };
        let renderer = EntryRenderer::new(entry, theme)
            .with_appearance_ref(appearance)
            .with_tick(tick)
            .with_skip_rows(skip_rows)
            .with_groupable(entry.block.is_groupable())
            .with_selected(is_selected)
            .with_mouse_pos(mouse_pos)
            .with_group_header_count(entry_layout_info.group_header_count)
            .with_group_collapse_header(entry_layout_info.group_collapse_header)
            .with_group_header_label(header_label.as_ref())
            .with_cwd(cwd);
        renderer.render(entry_content_area, buf);

        if dim_from_entry.is_some_and(|d| logical_idx >= d) {
            let dim_fg = theme.gray_dim;
            for cy in entry_content_area.y..entry_content_area.y + entry_content_area.height {
                for cx in entry_content_area.x..entry_content_area.x + entry_content_area.width {
                    if let Some(cell) = buf.cell_mut((cx, cy)) {
                        cell.fg = dim_fg;
                    }
                }
            }
        }

        // Use cached output for selection model building.
        // EntryRenderer::render() above already populated the cache for non-selected
        // entries. For selected entries, ensure_cached() will compute and cache the
        // output once. This avoids a redundant block.output() call (which includes
        // expensive syntax highlighting for edit blocks) that was previously done
        // via effective_output() on every frame.
        //
        // Must use the same effective width as the renderer (reduced by timestamp
        // reservation for message blocks) to avoid cache thrashing.
        let ts_reserved = timestamp_reserved_for_block(&entry.block, appearance);
        let content_width = entry_row_layout.content_width().saturating_sub(ts_reserved);
        entry.ensure_cached(content_width, appearance, is_selected, cwd);
        let cached_rendered = entry.cached_rendered_output_ref();
        let cached_output = &cached_rendered.output;
        let cached_boundaries = &cached_rendered.boundaries;

        result
            .selection_model
            .visible_blocks
            .push(VisibleBlockGeometry {
                entry_idx: logical_idx,
                area: entry_row_layout.entry_area(),
                content_area: entry_row_layout.content,
                selection_area: entry_row_layout.selection_area(),
                content_width,
                top_clipped,
                bottom_clipped,
                drag_startable: entry.block.is_drag_block_selectable(),
            });

        let ctx = entry.context(content_width, appearance, cwd);
        let has_vpad = entry.block.has_vpad(&ctx);
        let vpad_top = if has_vpad { 1u16 } else { 0 };
        let content_skip = skip_rows.saturating_sub(vpad_top) as usize;
        let first_visible_content_y = render_y + if skip_rows < vpad_top { 1 } else { 0 };
        let max_y = render_y + render_height;

        // Group-header entries draw synthetic "N more" text instead of
        // `cached_output.lines` (the truncation fold forces height 1), so
        // every pass mapping content lines or geometry to screen rows skips
        // them. The EXPANDED verb-group slot is the exception: its header line
        // sits above member 0's own content, which stays mapped (selectable
        // like every other member row) one row below.
        let is_group_header = entry_layout_info.is_group_header();
        let verb_expanded_slot = entry_layout_info.is_expanded_verb_header();

        let mapped_lines = if is_group_header && !verb_expanded_slot {
            &[][..]
        } else {
            &cached_output.lines[..]
        };

        // Expanded verb-group slot: the header consumes the slot's first
        // screen row, so member 0's content maps one row below (mirroring
        // EntryRenderer's collapse-header render path: when the header is
        // scrolled off, the first skipped row is the header, not content).
        // Shared by the selection lines, the hyperlink map, and the URL
        // scanner below — they all read these two offsets.
        let (first_visible_content_y, content_skip) = if verb_expanded_slot {
            if skip_rows == 0 {
                (first_visible_content_y + 1, content_skip)
            } else {
                (first_visible_content_y, content_skip.saturating_sub(1))
            }
        } else {
            (first_visible_content_y, content_skip)
        };
        let mut screen_y = first_visible_content_y;

        // Labeled group header (either fold family): one synthetic selectable
        // row so drag/copy on the header yields the aggregated label text.
        // Plain-count headers carry no label and stay non-selectable.
        if let Some(header) = &header_label
            && skip_rows == 0
            && render_y < max_y
        {
            let label = header.label();
            // Every labeled header draws the diamond chrome before the label;
            // shift the hitbox onto the label glyphs so highlight matches
            // the copied text (the chrome is affordance, not content).
            let chrome_offset = group_header_chrome_prefix_width();
            result.selection_model.push_line(ResolvedSelectableLine {
                entry_idx: logical_idx,
                range_id: GROUP_HEADER_RANGE_ID,
                block_line_idx: 0,
                screen_y: render_y,
                screen_x: entry_row_layout.content.x.saturating_add(chrome_offset),
                selectable_cols: 0..(label.line.width() as u16),
                text: label.text.clone(),
                joiner_to_previous: None,
            });
        }
        for (block_line_idx, line) in mapped_lines.iter().enumerate().skip(content_skip) {
            if screen_y >= max_y {
                break;
            }
            // Search highlight: re-run the query regex over this rendered row
            // and invert matching cells (decoupled from the source-text index,
            // mirroring `list_pane`). Each `BlockLine` is one already-wrapped
            // screen row, so the single-row paint path applies.
            //
            // The haystack is the rendered glyphs, not the indexed source text,
            // so the highlighted set can diverge from the index match set: a
            // match split across a soft-wrap boundary highlights on neither row,
            // and markdown markers present in source but absent on screen won't
            // highlight. This is the intended decoupling — navigation/counting
            // use the index; on-screen highlighting follows what's drawn.
            if let Some(re) = search_highlight {
                highlight_text.clear();
                line_plain_text_into(&line.content, &mut highlight_text);
                crate::render::highlight::paint_match_highlights(
                    buf,
                    entry_row_layout.content,
                    screen_y,
                    max_y,
                    0,
                    0,
                    &highlight_text,
                    re,
                    true,
                );
            }
            if let (Some(range_id), Some(cols)) = (
                line.selection_range,
                selectable_cols(&line.content, &line.selectable),
            ) {
                let resolved_line = ResolvedSelectableLine {
                    entry_idx: logical_idx,
                    range_id,
                    block_line_idx,
                    screen_y,
                    screen_x: entry_row_layout.content.x,
                    selectable_cols: cols,
                    text: derive_selection_text(line),
                    joiner_to_previous: line.joiner.clone(),
                };
                if let Some(boundary) = cached_boundaries.get(block_line_idx) {
                    selection_boundaries.push(&resolved_line, Arc::clone(boundary));
                }
                result.selection_model.push_line(resolved_line);
            }
            screen_y += 1;
        }

        // Collect hyperlinks for the link overlay. Group headers render
        // synthetic label text with no links; the expanded verb slot's member
        // row keeps its links (row offsets already shifted past the header).
        if !is_group_header || verb_expanded_slot {
            let content_line_offset = match &entry.block {
                RenderBlock::Btw(_) if ctx.mode != DisplayMode::Collapsed => 2,
                RenderBlock::Thinking(_)
                    if ctx.mode != DisplayMode::Collapsed
                        && appearance.scrollback.blocks.thinking.header =>
                {
                    2
                }
                _ => 0,
            };
            entry.block.with_hyperlinks(|hyperlinks| {
                if !hyperlinks.is_empty() {
                    map_hyperlinks_to_overlay(
                        hyperlinks,
                        cached_output,
                        content_skip,
                        first_visible_content_y,
                        max_y,
                        entry_row_layout.content.x,
                        content_line_offset,
                        media_paths,
                        &mut result.link_overlay,
                    );
                }
            });

            // Basename/relative tool headers need the stored absolute target.
            // Hit box = selectable path span (respects bullet prepend + Selectable shift).
            {
                for (idx, bl) in cached_output.lines.iter().enumerate().skip(content_skip) {
                    let visible_offset = (idx - content_skip) as u16;
                    let screen_row = first_visible_content_y + visible_offset;
                    if screen_row >= max_y {
                        break;
                    }
                    let Some(target) = bl.link_target.as_ref() else {
                        continue;
                    };
                    let Some(cols) = selectable_cols_usize(&bl.content, &bl.selectable) else {
                        continue;
                    };
                    let visible_width =
                        usize::from(content_width.min(entry_row_layout.content.width));
                    let start = cols.start.min(visible_width);
                    let end = cols.end.min(visible_width);
                    if start >= end {
                        continue;
                    }
                    let (Ok(start), Ok(end)) = (u16::try_from(start), u16::try_from(end)) else {
                        continue;
                    };
                    let (Some(col_start), Some(col_end)) = (
                        entry_row_layout.content.x.checked_add(start),
                        entry_row_layout.content.x.checked_add(end),
                    ) else {
                        continue;
                    };
                    if result.link_overlay.overlaps(screen_row, col_start, col_end) {
                        continue;
                    }
                    let painted = derive_selection_text(bl);
                    let fully_visible = cols.end <= visible_width;
                    result.link_overlay.push(OverlayLink {
                        screen_row,
                        col_start,
                        col_end,
                        target: target.clone(),
                        presentation: if fully_visible {
                            crate::render::osc8::file_link_presentation(&painted, target, cwd)
                        } else {
                            crate::render::osc8::LinkPresentation::Opaque
                        },
                        id: None,
                    });
                }
            }

            // Scan post-wrap lines for plain-text URLs and file paths.
            // For markdown blocks, markdown hyperlinks are already in the
            // overlay (mapped above); explicit tool-link rows are authoritative.
            {
                let visible_lines = cached_output
                    .lines
                    .iter()
                    .enumerate()
                    .skip(content_skip)
                    .filter(|(_, bl)| bl.link_target.is_none())
                    .map(|(idx, bl)| {
                        let visible_offset = (idx - content_skip) as u16;
                        let screen_row = first_visible_content_y + visible_offset;
                        (screen_row, &bl.content, bl.joiner.as_deref())
                    })
                    .take_while(|(screen_row, _, _)| *screen_row < max_y);

                crate::render::osc8::scan_lines_for_url_overlays(
                    visible_lines,
                    entry_row_layout.content.x,
                    media_paths,
                    &mut result.link_overlay,
                );
            }
        }

        // Collect inline media placements for visible media. Each media block
        // (tool media only) yields one trailing placement anchored at its own
        // `row_offset`. Partial visibility crops top/bottom so the image slides
        // into/out of view during scrolling.
        // Member 0's content starts one virtual row below the slot's header
        // line; every virtual anchor below offsets from here.
        let content_y_start = entry_start + usize::from(verb_expanded_slot);
        let media_placements = if is_group_header && !verb_expanded_slot {
            Vec::new()
        } else {
            entry.block.inline_media_placements(&ctx)
        };
        for placement in media_placements {
            let image_offset = placement.row_offset as usize;
            let full_image_h = placement.rows as usize;
            let image_virtual_start = content_y_start + image_offset;
            let image_virtual_end = image_virtual_start + full_image_h;
            let viewport_bottom = viewport_start + viewport.height as usize;
            // Keep the image clear of the right-aligned timestamp overlay
            // (message blocks reserve trailing columns for it); tool blocks
            // reserve 0, so this is a no-op there.
            let media_width = entry_content_area.width.saturating_sub(ts_reserved);

            // Check if any part of the image area is visible (height 1 = hint-only banner).
            if image_virtual_start < viewport_bottom
                && image_virtual_end > viewport_start
                && full_image_h >= 1
                && media_width >= 4
            {
                // Compute visible portion, cropping top and bottom.
                // Results are narrowed to u16 — they are viewport-relative
                // offsets that always fit in screen coordinates.
                let top_crop = viewport_start.saturating_sub(image_virtual_start) as u16;
                let visible_start = image_virtual_start.max(viewport_start);
                let visible_end = image_virtual_end.min(viewport_bottom);
                let visible_h = visible_end.saturating_sub(visible_start) as u16;
                let screen_y = viewport.y + (visible_start - viewport_start) as u16;

                if visible_h >= 1 {
                    // Tool media exposes its second output line as the
                    // click-to-copy filepath and reserves a button row.
                    let filepath_virtual_y = content_y_start + 1;
                    let filepath_screen_rect = if filepath_virtual_y >= viewport_start
                        && filepath_virtual_y < viewport_bottom
                    {
                        Some(ratatui::layout::Rect {
                            x: entry_content_area.x,
                            y: viewport.y + (filepath_virtual_y - viewport_start) as u16,
                            width: entry_content_area.width,
                            height: 1,
                        })
                    } else {
                        None
                    };

                    result.inline_media.push(InlineMediaPlacement {
                        info: placement.info,
                        screen_rect: ratatui::layout::Rect {
                            x: entry_content_area.x,
                            y: screen_y,
                            width: media_width,
                            height: visible_h,
                        },
                        full_rows: full_image_h as u16,
                        top_crop_rows: top_crop,
                        filepath_screen_rect,
                        open_button_screen_rect: None,
                        has_button_row: true,
                    });
                }
            }
        }

        // Diagram affordance rows: map each block-relative reserved row to a
        // screen rect when visible. The blank row already scrolls with the
        // content; the draw loop paints the buttons + registers click hit-rects.
        // Agent messages (the only producer) have no top vpad, so `row_offset`
        // is measured straight from `y_start`, like inline media above.
        // Header gate unreachable today (producers are agent messages — run
        // breakers, so never verb-group members either); structural.
        let diagram_affordances = if is_group_header {
            Vec::new()
        } else {
            entry.block.diagram_affordances(&ctx)
        };
        for aff in diagram_affordances {
            let virtual_y = entry_start + aff.row_offset as usize;
            let viewport_bottom = viewport_start + viewport.height as usize;
            if virtual_y >= viewport_start && virtual_y < viewport_bottom {
                result.diagram_affordances.push(DiagramAffordancePlacement {
                    screen_rect: ratatui::layout::Rect {
                        x: entry_row_layout.content.x,
                        y: viewport.y + (virtual_y - viewport_start) as u16,
                        width: content_width,
                        height: 1,
                    },
                    source: aff.source,
                });
            }
        }

        // Click targets for the text `[Open]` button + filepath of media blocks
        // without an inline overlay (terminals without inline-graphics support).
        if (!is_group_header || verb_expanded_slot)
            && let Some((open_path, is_video)) = entry.block.inline_open_button()
        {
            let content_lines = cached_output.lines.len();
            let viewport_bottom = viewport_start + viewport.height as usize;

            // Virtual-y coordinates are usize (tall scrollback); the resulting
            // screen y is a viewport-relative offset that fits in u16.
            let line_screen_rect =
                |virtual_y: usize, width: u16| -> Option<ratatui::layout::Rect> {
                    if virtual_y >= viewport_start && virtual_y < viewport_bottom {
                        Some(ratatui::layout::Rect {
                            x: entry_content_area.x,
                            y: viewport.y + (virtual_y - viewport_start) as u16,
                            width,
                            height: 1,
                        })
                    } else {
                        None
                    }
                };

            // Filepath line (index 1) → click-to-copy.
            let filepath_screen_rect =
                line_screen_rect(content_y_start + 1, entry_content_area.width);

            // Centered `[Open]` button → click-to-open. It is the second-to-last
            // content line (the last line is a blank spacer).
            let open_button_screen_rect = if content_lines >= 2 {
                let label_w = media_open_button_label(is_video).len() as u16;
                let col = media_open_button_col(content_width, is_video);
                let button_virtual_y = content_y_start + (content_lines - 2);
                line_screen_rect(button_virtual_y, label_w).map(|mut rect| {
                    rect.x = rect.x.saturating_add(col);
                    rect
                })
            } else {
                None
            };

            if filepath_screen_rect.is_some() || open_button_screen_rect.is_some() {
                result.inline_media.push(InlineMediaPlacement {
                    info: crate::prompt_images::InlineMediaInfo {
                        path: open_path,
                        width: 0,
                        height: 0,
                        is_video,
                        alt_text: String::new(),
                    },
                    screen_rect: ratatui::layout::Rect {
                        x: entry_content_area.x,
                        y: viewport.y,
                        width: 0,
                        height: 0,
                    },
                    full_rows: 0,
                    top_crop_rows: 0,
                    filepath_screen_rect,
                    open_button_screen_rect,
                    // Text-button placement: the button is the text [Open] line
                    // (`open_button_screen_rect`), not an image-overlay row.
                    has_button_row: false,
                });
            }
        }

        // Track selected entry
        if selected_idx == Some(logical_idx) {
            let selection_area = entry_row_layout.selection_area();
            result.selected_area = Some(SelectedEntryArea {
                area: selection_area,
                top_clipped,
                bottom_clipped,
            });
        }

        y = entry_end + entry_layouts_cache[i].gap_after as usize;
    }

    ScrollRenderResultWithBoundaries {
        result,
        selection_boundaries,
    }
}

use super::types::BlockOutput;

/// Map pre-wrap `HyperlinkTarget`s to screen-space `OverlayLink`s.
///
/// Walks the post-wrap `BlockOutput` lines, using joiner metadata to
/// reconstruct the pre-wrap → post-wrap line mapping.  For each
/// hyperlink, finds the wrapped line(s) it overlaps and emits an
/// `OverlayLink` with the correct screen row and column offsets.
///
/// `content_line_offset` accounts for non-markdown header lines prepended
/// by block types like `BtwBlock` (header + separator) and `ThinkingBlock`
/// (header + blank when the header config is enabled).
///
/// Also used by the `/btw` inline panel (no header offset — pure markdown body).
#[allow(clippy::too_many_arguments)]
pub(crate) fn map_hyperlinks_to_overlay(
    hyperlinks: &[xai_grok_markdown::HyperlinkTarget],
    block_output: &BlockOutput,
    content_skip: usize,
    first_visible_screen_y: u16,
    max_screen_y: u16,
    content_x: u16,
    content_line_offset: usize,
    media_paths: &[std::path::PathBuf],
    overlay: &mut LinkOverlay,
) {
    // Build mapping: pre-wrap line index → vec of (wrapped_idx, col_start_in_prewrap, col_end_in_prewrap).
    // A joiner of None means a new pre-wrap line starts.
    let mut pre_wrap_segments: Vec<Vec<(usize, usize, usize)>> = Vec::new();
    let mut current_segments: Vec<(usize, usize, usize)> = Vec::new();
    let mut cumulative_col: usize = 0;

    for (wrapped_idx, line) in block_output.lines.iter().enumerate() {
        if line.joiner.is_none() && !current_segments.is_empty() {
            pre_wrap_segments.push(std::mem::take(&mut current_segments));
            cumulative_col = 0;
        }
        // Joiner represents the whitespace consumed at the wrap point —
        // it occupies display columns in the pre-wrap line but doesn't
        // appear in either wrapped line. Add BEFORE this segment so
        // the column mapping stays aligned.
        if let Some(ref joiner) = line.joiner {
            cumulative_col += unicode_width::UnicodeWidthStr::width(joiner.as_str());
        }
        let line_width = line.content.width();
        current_segments.push((wrapped_idx, cumulative_col, cumulative_col + line_width));
        cumulative_col += line_width;
    }
    if !current_segments.is_empty() {
        pre_wrap_segments.push(current_segments);
    }

    // Map each hyperlink to screen-space OverlayLinks. Unsafe schemes
    // (javascript:, data:, …) are dropped since OSC 8 URLs reach the terminal
    // without the link_opener scheme filter. Local-file destinations such as
    // `[videos/1.mp4](videos/1.mp4)` resolve against generated media.
    let scheme_filter = crate::terminal::hyperlinks::SchemeFilter::Standard;
    for h in hyperlinks {
        let target = if crate::app::link_opener::is_safe_to_open(&h.url, scheme_filter) {
            crate::render::osc8::LinkTarget::Url(Arc::from(h.url.as_str()))
        } else if let Some(file_target) =
            crate::render::osc8::local_link_to_file_target(&h.url, media_paths)
        {
            file_target
        } else {
            continue;
        };
        let adjusted_line = h.line_index + content_line_offset;
        if adjusted_line >= pre_wrap_segments.len() {
            continue;
        }
        let segments = &pre_wrap_segments[adjusted_line];
        for &(wrapped_idx, seg_col_start, seg_col_end) in segments {
            // Check if hyperlink's column range overlaps this wrapped segment.
            let overlap_start = h.column_range.start.max(seg_col_start);
            let overlap_end = h.column_range.end.min(seg_col_end);
            if overlap_start >= overlap_end {
                continue;
            }

            // Check visibility (accounting for content_skip).
            if wrapped_idx < content_skip {
                continue;
            }
            let visible_offset = (wrapped_idx - content_skip) as u16;
            let screen_row = first_visible_screen_y + visible_offset;
            if screen_row >= max_screen_y {
                continue;
            }

            let local_col_start = (overlap_start - seg_col_start) as u16;
            let local_col_end = (overlap_end - seg_col_start) as u16;

            overlay.push(OverlayLink {
                screen_row,
                col_start: content_x + local_col_start,
                col_end: content_x + local_col_end,
                target: target.clone(),
                presentation: crate::render::osc8::LinkPresentation::Opaque,
                id: Some(h.id),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appearance::AppearanceConfig;
    use crate::render::osc8::{
        LinkPresentation, resolve_link_target, resolve_link_target_for_context,
    };
    use crate::scrollback::RenderBlock;
    use crate::scrollback::block::BlockContent;
    use crate::scrollback::types::DisplayMode;
    use crate::scrollback::wrappers::EntryRenderer;
    use pretty_assertions::assert_eq;
    use ratatui::style::Color;

    fn make_entries(count: usize) -> Vec<ScrollbackEntry> {
        (0..count)
            .map(|i| ScrollbackEntry::new(RenderBlock::stub(format!("Entry {i}"), Color::Blue)))
            .collect()
    }

    fn make_markdown_entry(text: &str) -> ScrollbackEntry {
        ScrollbackEntry::new(RenderBlock::agent_message(text))
    }

    /// Compute EntryLayoutInfo for a set of entries (heights + gap_after).
    /// Uses default appearance. Gap rule: all stubs are groupable+expanded → gap=1.
    fn compute_layouts(
        entries: &[ScrollbackEntry],
        viewport_width: u16,
        appearance: &AppearanceConfig,
    ) -> Vec<EntryLayoutInfo> {
        let theme = Theme::current();
        let layout = HorizontalLayout::new(
            Rect::new(0, 0, viewport_width, 1),
            &appearance.scrollback.layout,
        );
        let content_width = layout.entry_content_area().width;

        let n = entries.len();
        let mut layouts: Vec<EntryLayoutInfo> = entries
            .iter()
            .map(|e| {
                let renderer = EntryRenderer::new(e, &theme).with_appearance_ref(appearance);
                let height = renderer.desired_height(content_width);
                EntryLayoutInfo {
                    height,
                    gap_after: 1,
                    group_header_count: 0,
                    group_collapse_header: false,
                    verb_group_header: false,
                } // placeholder
            })
            .collect();

        // Compute gap_after using the pairwise rule
        for i in 0..n.saturating_sub(1) {
            let a_groupable = entries[i].block.is_groupable();
            let b_groupable = entries[i + 1].block.is_groupable();
            let a_collapsed = entries[i].display_mode == DisplayMode::Collapsed;
            let b_collapsed = entries[i + 1].display_mode == DisplayMode::Collapsed;
            layouts[i].gap_after = if a_groupable && b_groupable && a_collapsed && b_collapsed {
                0
            } else {
                1
            };
        }
        // Last entry: trailing gap = 1
        if n > 0 {
            layouts[n - 1].gap_after = 1;
        }
        layouts
    }

    /// Render entries using the production scratch path.
    fn render_with_scratch(
        entries: &[ScrollbackEntry],
        viewport: Rect,
        scroll_offset: usize,
        selected_idx: Option<usize>,
    ) -> ScrollRenderResult {
        render_with_scratch_and_buffer(entries, viewport, scroll_offset, selected_idx).0
    }

    fn render_with_scratch_and_buffer(
        entries: &[ScrollbackEntry],
        viewport: Rect,
        scroll_offset: usize,
        selected_idx: Option<usize>,
    ) -> (ScrollRenderResult, Buffer) {
        render_with_scratch_and_buffer_with_cwd(
            entries,
            viewport,
            scroll_offset,
            selected_idx,
            None,
        )
    }

    fn render_with_scratch_and_buffer_with_cwd(
        entries: &[ScrollbackEntry],
        viewport: Rect,
        scroll_offset: usize,
        selected_idx: Option<usize>,
        cwd: Option<&std::path::Path>,
    ) -> (ScrollRenderResult, Buffer) {
        let theme = Theme::current();
        let appearance = AppearanceConfig::default();
        let layouts = compute_layouts(entries, viewport.width, &appearance);
        let refs: Vec<&ScrollbackEntry> = entries.iter().collect();
        let mut buf = Buffer::empty(viewport);

        let result = render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            scroll_offset,
            selected_idx,
            &theme,
            &appearance,
            &layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            cwd,
        );
        (result, buf)
    }

    fn render_with_selection_boundaries(
        entries: &[ScrollbackEntry],
        viewport: Rect,
        scroll_offset: usize,
    ) -> ScrollRenderResultWithBoundaries {
        let theme = Theme::current();
        let appearance = AppearanceConfig::default();
        let layouts = compute_layouts(entries, viewport.width, &appearance);
        let refs: Vec<&ScrollbackEntry> = entries.iter().collect();
        let mut buf = Buffer::empty(viewport);

        render_scrolled_entries_with_selection_boundaries(
            &mut buf,
            viewport,
            &refs,
            scroll_offset,
            None,
            &theme,
            &appearance,
            &layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            None,
        )
    }

    /// Render entries with a search-highlight regex and return the buffer.
    fn render_with_highlight(
        entries: &[ScrollbackEntry],
        viewport: Rect,
        re: Option<&regex::Regex>,
    ) -> Buffer {
        let theme = Theme::current();
        let appearance = AppearanceConfig::default();
        let layouts = compute_layouts(entries, viewport.width, &appearance);
        let refs: Vec<&ScrollbackEntry> = entries.iter().collect();
        let mut buf = Buffer::empty(viewport);
        render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &theme,
            &appearance,
            &layouts,
            0,
            None,
            None,
            re,
            0,
            0,
            &[],
            None,
            None,
        );
        buf
    }

    fn count_reversed(buf: &Buffer) -> usize {
        (buf.area.top()..buf.area.bottom())
            .map(|y| reversed_cols_on_row(buf, y).len())
            .sum()
    }

    /// The plain text of buffer row `y` (viewport x starts at 0, so a byte
    /// index into this string equals the buffer column for ASCII content).
    fn buffer_row_text(buf: &Buffer, y: u16) -> String {
        (buf.area.left()..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    /// Buffer columns on row `y` whose cell carries the REVERSED modifier.
    fn reversed_cols_on_row(buf: &Buffer, y: u16) -> Vec<u16> {
        (buf.area.left()..buf.area.right())
            .filter(|&x| {
                buf[(x, y)]
                    .modifier
                    .contains(ratatui::style::Modifier::REVERSED)
            })
            .collect()
    }

    /// Columns covered by every occurrence of `needle` in `row` (ASCII only).
    fn expected_match_cols(row: &str, needle: &str) -> Vec<u16> {
        let mut cols = Vec::new();
        let mut from = 0;
        while let Some(rel) = row[from..].find(needle) {
            let start = from + rel;
            cols.extend((start as u16)..(start + needle.len()) as u16);
            from = start + needle.len();
        }
        cols
    }

    #[test]
    fn search_highlight_inverts_exactly_the_match_columns() {
        let entries = vec![make_markdown_entry("alpha needle beta needle")];
        let viewport = Rect::new(0, 0, 80, 10);
        let re = regex::Regex::new("needle").unwrap();

        let buf = render_with_highlight(&entries, viewport, Some(&re));

        let y = (0..viewport.height)
            .find(|&y| buffer_row_text(&buf, y).contains("needle"))
            .expect("rendered row containing the text");
        let row = buffer_row_text(&buf, y);
        let expected = expected_match_cols(&row, "needle");
        assert_eq!(expected.len(), 12, "two 6-char matches");
        assert_eq!(
            reversed_cols_on_row(&buf, y),
            expected,
            "exactly the matched glyph columns are inverted"
        );
        // No other row carries inverted cells.
        assert_eq!(count_reversed(&buf), expected.len());
    }

    #[test]
    fn search_highlight_maps_to_wrapped_continuation_row() {
        // A narrow viewport forces the message to wrap; the match lands on a
        // continuation row, exercising the per-row screen_y mapping.
        let entries = vec![make_markdown_entry(
            "alpha bravo charlie delta echo foxtrot golf hotel needle",
        )];
        // Width 40 wraps the sentence (message blocks also reserve 10 cols for
        // the timestamp) while keeping "needle" whole on a continuation row.
        let viewport = Rect::new(0, 0, 40, 12);
        let re = regex::Regex::new("needle").unwrap();

        let buf = render_with_highlight(&entries, viewport, Some(&re));

        let y = (0..viewport.height)
            .find(|&y| buffer_row_text(&buf, y).contains("needle"))
            .expect("rendered row containing the match");
        assert!(y > 0, "match wrapped onto a continuation row, not row 0");
        let row = buffer_row_text(&buf, y);
        assert_eq!(
            reversed_cols_on_row(&buf, y),
            expected_match_cols(&row, "needle"),
        );
        assert_eq!(count_reversed(&buf), 6, "only the single match is inverted");
    }

    #[test]
    fn some_regex_with_no_match_inverts_nothing() {
        let entries = vec![make_markdown_entry("alpha needle beta")];
        let viewport = Rect::new(0, 0, 80, 10);
        let re = regex::Regex::new("zzz").unwrap();

        let buf = render_with_highlight(&entries, viewport, Some(&re));
        assert_eq!(count_reversed(&buf), 0);
    }

    #[test]
    fn no_highlight_regex_inverts_nothing() {
        let entries = vec![make_markdown_entry("alpha needle beta")];
        let viewport = Rect::new(0, 0, 80, 10);

        let buf = render_with_highlight(&entries, viewport, None);
        assert_eq!(count_reversed(&buf), 0);
    }

    #[test]
    fn test_total_height_calculation() {
        let entries = make_entries(3);
        let viewport = Rect::new(0, 0, 80, 20);
        let result = render_with_scratch(&entries, viewport, 0, None);

        // Each stub entry: 3 lines (1 content + 2 vpad). All are groupable+expanded → gap=1.
        // Total: 3*3 + 2 gaps + 1 trailing = 12
        assert_eq!(result.total_height, 12);
    }

    #[test]
    fn test_scroll_offset_skips_content() {
        let entries = make_entries(5);
        let viewport = Rect::new(0, 0, 80, 10);
        let result = render_with_scratch(&entries, viewport, 4, None);

        // 5 entries * 3 lines + 4 gaps + 1 trailing = 20
        assert_eq!(result.total_height, 5 * 3 + 5);
    }

    /// Large scrollback mid-offset: shipped render + viewport paint window must
    /// match a full-list pass (geometry / selection / total_height of full list).
    #[test]
    fn large_scrollback_mid_offset_viewport_window_matches_full_pass() {
        const N: usize = 3000;
        let entries = make_entries(N);
        let viewport = Rect::new(0, 0, 80, 24);
        let theme = Theme::current();
        let appearance = AppearanceConfig::default();
        let layouts = compute_layouts(&entries, viewport.width, &appearance);

        // Prefix virtual_y as LayoutCache does (absolute y of each entry start).
        let mut virtual_y = Vec::with_capacity(N);
        let mut y = 0usize;
        for layout in &layouts {
            virtual_y.push(y);
            y += layout.height as usize + layout.gap_after as usize;
        }
        let full_total = y;
        // Mid-session offset: past thousands of entries, not at top/bottom.
        let scroll_offset = full_total / 2;
        let selected_abs = virtual_y
            .partition_point(|&vy| vy <= scroll_offset)
            .saturating_sub(1)
            .min(N - 1);

        // Full-list baseline (content_y0=0, index_base=0).
        let full_refs: Vec<&ScrollbackEntry> = entries.iter().collect();
        let mut full_buf = Buffer::empty(viewport);
        let full = render_scrolled_entries_with_scratch(
            &mut full_buf,
            viewport,
            &full_refs,
            scroll_offset,
            Some(selected_abs),
            &theme,
            &appearance,
            &layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            None,
        );

        // Production window: the same helper `render_content` paints through.
        let (paint_range, content_y0) = crate::scrollback::state::compute_paint_window(
            &virtual_y,
            &layouts,
            0..N,
            scroll_offset,
            viewport.height as usize,
            |_| unreachable!("plain stub entries form no verb-group runs"),
        );
        let (first, end) = (paint_range.start, paint_range.end);
        assert!(
            end - first < N / 10,
            "paint window should be << total history: first={first} end={end} N={N}"
        );
        let window_refs: Vec<&ScrollbackEntry> = entries[first..end].iter().collect();
        let mut win_buf = Buffer::empty(viewport);
        let windowed = render_scrolled_entries_with_scratch(
            &mut win_buf,
            viewport,
            &window_refs,
            scroll_offset,
            Some(selected_abs),
            &theme,
            &appearance,
            &layouts[first..end],
            0,
            None,
            None,
            None,
            content_y0,
            first,
            &[],
            None,
            None,
        );

        assert_eq!(full.total_height, full_total);

        let full_sel = full
            .selected_area
            .as_ref()
            .expect("selected visible (full)");
        let win_sel = windowed
            .selected_area
            .as_ref()
            .expect("selected visible (window)");
        assert_eq!(full_sel.area, win_sel.area);
        assert_eq!(full_sel.top_clipped, win_sel.top_clipped);
        assert_eq!(full_sel.bottom_clipped, win_sel.bottom_clipped);

        assert_eq!(
            full.selection_model.visible_blocks.len(),
            windowed.selection_model.visible_blocks.len()
        );
        for (a, b) in full
            .selection_model
            .visible_blocks
            .iter()
            .zip(windowed.selection_model.visible_blocks.iter())
        {
            assert_eq!(a.entry_idx, b.entry_idx);
            assert_eq!(a.area, b.area);
            assert_eq!(a.top_clipped, b.top_clipped);
            assert_eq!(a.bottom_clipped, b.bottom_clipped);
        }

        // Cell content must match — proves paint of the window equals full walk.
        assert_eq!(full_buf, win_buf);
    }

    /// A verb-group header on the viewport's last row must paint the label of
    /// the FULL run (count + failure suffix), not just the on-screen members:
    /// `paint_window` extends the slice past the window bottom so the label
    /// walk sees every member.
    #[test]
    fn windowed_paint_renders_full_verb_group_label_for_offscreen_members() {
        use crate::scrollback::ScrollbackState;
        use crate::scrollback::blocks::tool::{ReadToolCallBlock, ToolCallBlock};

        crate::appearance::cache::set_group_tool_verbs(true);
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut state = ScrollbackState::new();
        for i in 0..20 {
            state.push_block(RenderBlock::agent_message(format!("filler {i}")));
        }
        let header = state.len();
        for i in 0..48 {
            state.push_block(RenderBlock::read(format!("f{i}.rs"), None));
        }
        for i in 0..2 {
            state.push_block(RenderBlock::ToolCall(ToolCallBlock::Read(
                ReadToolCallBlock::new(format!("gone{i}.rs")).with_error("no such file"),
            )));
        }
        let viewport = Rect::new(0, 0, 80, 24);
        state.prepare_layout(viewport.width, viewport.height);

        let virtual_y = state.get_cached_virtual_y().expect("layout cache");
        let layouts = state.get_cached_entry_layouts().expect("layout cache");
        // Header row on the viewport's last row: all 50 members are off-screen.
        let scroll = virtual_y[header] + 1 - viewport.height as usize;
        let (paint_range, content_y0) =
            state.paint_window(0..state.len(), scroll, viewport.height as usize);
        assert_eq!(
            paint_range.end,
            header + 50,
            "window must cover the whole run"
        );

        let window = state.entries_in_range(paint_range.clone());
        let mut buf = Buffer::empty(viewport);
        render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &window,
            scroll,
            None,
            &Theme::current(),
            state.appearance(),
            &layouts[paint_range.clone()],
            0,
            None,
            None,
            None,
            content_y0,
            paint_range.start,
            &[],
            None,
            None,
        );

        let header_row = buffer_row_text(&buf, viewport.height - 1);
        assert!(
            header_row.contains("Read 50 files · 2 failed"),
            "header label must aggregate the full off-screen run: {header_row:?}"
        );
    }

    /// A collapsed truncation header on the viewport's last row must label
    /// its FULL hidden prefix: the height-0 hidden rows share the tail's
    /// virtual_y past the window bottom, so `paint_window`'s gate must
    /// extend the slice for truncation headers too — not just verb headers.
    #[test]
    fn windowed_paint_labels_truncation_header_on_last_viewport_row() {
        use crate::scrollback::ScrollbackState;

        crate::appearance::cache::set_group_tool_verbs(true);
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.display.group_max_visible = 3;
        state.set_appearance(appearance);
        for i in 0..20 {
            state.push_block(RenderBlock::agent_message(format!("filler {i}")));
        }
        let header = state.len();
        for i in 0..6 {
            state.push_block(RenderBlock::execute(format!("cmd{i}")));
        }
        let viewport = Rect::new(0, 0, 80, 24);
        state.prepare_layout(viewport.width, viewport.height);

        let virtual_y = state.get_cached_virtual_y().expect("layout cache");
        let layouts = state.get_cached_entry_layouts().expect("layout cache");
        assert!(!layouts[header].verb_group_header, "truncation, not verb");
        assert_eq!(layouts[header].group_header_count, 2);
        // Header row on the viewport's last row: the hidden prefix and tail
        // sit past the window bottom.
        let scroll = virtual_y[header] + 1 - viewport.height as usize;
        let (paint_range, content_y0) =
            state.paint_window(0..state.len(), scroll, viewport.height as usize);
        assert_eq!(
            paint_range.end,
            header + 6,
            "window must cover the whole run"
        );

        let window = state.entries_in_range(paint_range.clone());
        let mut buf = Buffer::empty(viewport);
        render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &window,
            scroll,
            None,
            &Theme::current(),
            state.appearance(),
            &layouts[paint_range.clone()],
            0,
            None,
            None,
            None,
            content_y0,
            paint_range.start,
            &[],
            Some((state.group_spans(), paint_range.start)),
            None,
        );

        let header_row = buffer_row_text(&buf, viewport.height - 1);
        assert!(
            header_row.contains("Ran 3 commands"),
            "label must cover the full off-screen hidden prefix: {header_row:?}"
        );
    }

    #[test]
    fn rendered_verb_group_header_aggregates_hook_outcomes_and_keeps_compact_members() {
        use crate::scrollback::ScrollbackState;
        use crate::scrollback::blocks::tool::{HookPhase, HookRunEntry, HookRunStatus};

        crate::appearance::cache::set_group_tool_verbs(true);
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut state = ScrollbackState::new();
        let first = state.push_block(RenderBlock::read("a.rs", None));
        let second = state.push_block(RenderBlock::list_dir_with_output("src", "a.rs"));
        let third = state.push_block(RenderBlock::search("TODO", 1, Vec::new()));
        let elapsed = std::time::Duration::from_millis(1);
        state.attach_hooks(
            first,
            HookPhase::Post,
            vec![HookRunEntry {
                name: "ok-hook".to_owned(),
                status: HookRunStatus::Success { elapsed },
                output: None,
            }],
        );
        state.attach_hooks(
            second,
            HookPhase::Post,
            vec![HookRunEntry {
                name: "blocked-hook".to_owned(),
                status: HookRunStatus::Blocked {
                    detail: "denied".to_owned(),
                    elapsed,
                },
                output: None,
            }],
        );
        state.attach_hooks(
            third,
            HookPhase::Post,
            vec![HookRunEntry {
                name: "failed-hook".to_owned(),
                status: HookRunStatus::Failed {
                    error: "exit 1".to_owned(),
                    elapsed,
                },
                output: None,
            }],
        );
        let narrow_viewport = Rect::new(0, 0, 80, 24);
        state.prepare_layout(narrow_viewport.width, narrow_viewport.height);
        let (narrow_buf, _) = render_state(&state, narrow_viewport, true);
        let narrow_header = buffer_row_text(&narrow_buf, 0);
        assert!(
            narrow_header.contains("1 failed]"),
            "narrow headers must reserve the complete outcome suffix: {narrow_header:?}"
        );

        let viewport = Rect::new(0, 0, 120, 24);
        state.prepare_layout(viewport.width, viewport.height);
        let (buf, _) = render_state(&state, viewport, true);
        let header_row = buffer_row_text(&buf, 0);
        assert!(
            header_row.contains(
                "Read 1 file, Listed 1 dir, Searched 1 pattern  [hooks: 1 ok, 1 blocked, 1 failed]"
            ),
            "collapsed header must show every hidden hook outcome: {header_row:?}"
        );
        assert_eq!(
            buf[(0, 0)].fg,
            Theme::current().accent_error,
            "failed hook marks the group header as errored"
        );

        state.set_selected(Some(0));
        assert!(state.toggle_group_expansion());
        state.prepare_layout(viewport.width, viewport.height);
        let layouts = state.get_cached_entry_layouts().expect("layout cache");
        let member_width = HorizontalLayout::new(viewport, &state.appearance().scrollback.layout)
            .entry_content_area()
            .width;
        for (idx, layout) in layouts[..3].iter().enumerate() {
            let member_height = EntryRenderer::new(state.entry(idx).unwrap(), &Theme::current())
                .with_appearance_ref(state.appearance())
                .with_cwd(state.cwd())
                .desired_height(member_width);
            assert_eq!(
                layout.height,
                member_height.saturating_add(u16::from(idx == 0)),
                "expanded member {idx} keeps its ordinary collapsed row height"
            );
        }
        let (buf, _) = render_state(&state, viewport, true);
        let rows: Vec<String> = (0..viewport.height)
            .map(|y| buffer_row_text(&buf, y))
            .collect();
        let hook_rows: Vec<_> = rows.iter().filter(|row| row.contains("[hooks:")).collect();
        assert_eq!(
            hook_rows.len(),
            4,
            "the group header and each member carry one compact hook summary: {rows:?}"
        );
        assert_eq!(
            hook_rows
                .iter()
                .filter(|row| row.contains("[hooks: 1]"))
                .count(),
            3,
            "each expanded member keeps one standalone compact suffix: {rows:?}"
        );
        assert!(
            ["ok-hook", "blocked-hook", "failed-hook", "post_tool_use"]
                .iter()
                .all(|detail| rows.iter().all(|row| !row.contains(detail))),
            "expanded groups must not reveal per-hook detail sections: {rows:?}"
        );
    }

    /// A hidden thinking entry inside a folded run stays transparent through
    /// the whole production path: the layout fold spans it AND the rendered
    /// header label counts the members on both sides — pinning the
    /// `show_thinking` handoff from the render loop into the label walk.
    #[test]
    fn rendered_verb_group_label_spans_hidden_thinking_inside_folded_run() {
        use crate::scrollback::ScrollbackState;

        crate::appearance::cache::set_group_tool_verbs(true);
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut state = ScrollbackState::new();
        state.push_block(RenderBlock::read("a.rs", None));
        state.push_block(RenderBlock::thinking("hmm"));
        state.push_block(RenderBlock::read("b.rs", None));
        let viewport = Rect::new(0, 0, 80, 10);
        state.prepare_layout(viewport.width, viewport.height);

        let layouts = state.get_cached_entry_layouts().expect("layout cache");
        assert!(
            layouts[0].verb_group_header,
            "fold must span the hidden thinking entry"
        );

        let refs = state.entries_in_range(0..state.len());
        let mut buf = Buffer::empty(viewport);
        render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &Theme::current(),
            state.appearance(),
            layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            None,
        );

        let header_row = buffer_row_text(&buf, 0);
        assert!(
            header_row.contains("Read 2 files"),
            "label must count members on both sides of hidden thinking: {header_row:?}"
        );
    }

    /// A finished collapsed thought inside a folded run stays out of the
    /// collapsed header entirely: the fold claims it (no standalone
    /// "Thought" row anywhere) while the rendered label counts tools only.
    #[test]
    fn rendered_verb_group_label_stays_tools_only_across_folded_thought() {
        use crate::scrollback::ScrollbackState;

        crate::appearance::cache::set_group_tool_verbs(true);
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut state = ScrollbackState::new();
        state.push_block(RenderBlock::read("a.rs", None));
        let thought = state.push_block(RenderBlock::thinking("weighed the options"));
        state
            .get_by_id_mut(thought)
            .unwrap()
            .set_display_mode(DisplayMode::Collapsed);
        state.push_block(RenderBlock::read("b.rs", None));
        let viewport = Rect::new(0, 0, 80, 10);
        state.prepare_layout(viewport.width, viewport.height);

        let layouts = state.get_cached_entry_layouts().expect("layout cache");
        assert!(layouts[0].verb_group_header, "run folds across the thought");
        assert_eq!(layouts[1].height, 0, "the thought claims into the fold");

        let refs = state.entries_in_range(0..state.len());
        let mut buf = Buffer::empty(viewport);
        render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &Theme::current(),
            state.appearance(),
            layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            None,
        );

        let header_row = buffer_row_text(&buf, 0);
        assert!(
            header_row.contains("Read 2 files"),
            "label must stay tools-only: {header_row:?}"
        );
        for y in 0..viewport.height {
            let row = buffer_row_text(&buf, y);
            assert!(
                !row.contains("Thought") && !row.contains("weighed"),
                "no folded-thought row may render (row {y}): {row:?}"
            );
        }
    }

    /// A single groupable tool call folds on its own: the run renders the
    /// aggregated header label — not the tool's own row — and finished
    /// thoughts fold behind it just like in a multi-member run.
    #[test]
    fn rendered_verb_group_singleton_folds_tool_and_trailing_thoughts() {
        use crate::scrollback::ScrollbackState;

        crate::appearance::cache::set_group_tool_verbs(true);
        crate::appearance::cache::set_show_thinking_blocks(true);
        let mut state = ScrollbackState::new();
        state.push_block(RenderBlock::list_dir("src"));
        for text in ["scanned the tree", "picked a file"] {
            let thought = state.push_block(RenderBlock::thinking(text));
            state
                .get_by_id_mut(thought)
                .unwrap()
                .set_display_mode(DisplayMode::Collapsed);
        }
        let viewport = Rect::new(0, 0, 80, 10);
        state.prepare_layout(viewport.width, viewport.height);

        let layouts = state.get_cached_entry_layouts().expect("layout cache");
        assert!(layouts[0].verb_group_header, "singleton run folds");
        assert_eq!(layouts[1].height, 0, "thoughts claim into the fold");
        assert_eq!(layouts[2].height, 0, "thoughts claim into the fold");

        let refs = state.entries_in_range(0..state.len());
        let mut buf = Buffer::empty(viewport);
        render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &Theme::current(),
            state.appearance(),
            layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            None,
        );

        let header_row = buffer_row_text(&buf, 0);
        assert!(
            header_row.contains("Listed 1 dir"),
            "singleton header must render the aggregated label: {header_row:?}"
        );
        for y in 0..viewport.height {
            let row = buffer_row_text(&buf, y);
            assert!(
                !row.contains("List src") && !row.contains("Thought"),
                "neither the raw tool row nor a thought row may render (row {y}): {row:?}"
            );
        }
    }

    /// A subagent lifecycle row folds into the verb group: the collapsed
    /// header renders the aggregated label, and expanding the group reveals
    /// the subagent's own row with its live ` — activity` suffix intact.
    #[test]
    fn rendered_verb_group_folds_subagent_row_and_expansion_keeps_activity() {
        use crate::scrollback::ScrollbackState;
        use crate::scrollback::blocks::SubagentBlock;

        crate::appearance::cache::set_group_tool_verbs(true);
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut state = ScrollbackState::new();
        state.push_block(RenderBlock::read("a.rs", None));
        let mut block = SubagentBlock::started(
            "task", "child-A", "explore", None, None, None, /*is_background=*/ false,
        );
        block.activity_label = Some("Thinking".to_string());
        state.push_block(RenderBlock::Subagent(block));
        let viewport = Rect::new(0, 0, 80, 10);
        state.prepare_layout(viewport.width, viewport.height);

        let layouts = state.get_cached_entry_layouts().expect("layout cache");
        assert!(layouts[0].verb_group_header, "run folds with the subagent");
        assert_eq!(layouts[1].height, 0, "subagent row claims into the fold");

        let refs = state.entries_in_range(0..state.len());
        let mut buf = Buffer::empty(viewport);
        render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &Theme::current(),
            state.appearance(),
            layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            None,
        );
        let header_row = buffer_row_text(&buf, 0);
        assert!(
            header_row.contains("Read 1 file, Ran 1 subagent"),
            "header must aggregate tool and subagent members: {header_row:?}"
        );

        // Expanded: the subagent member surfaces with its live suffix.
        state.set_selected(Some(0));
        assert!(state.toggle_group_expansion());
        state.prepare_layout(viewport.width, viewport.height);
        let layouts = state.get_cached_entry_layouts().expect("layout cache");
        let refs = state.entries_in_range(0..state.len());
        let mut buf = Buffer::empty(viewport);
        render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &Theme::current(),
            state.appearance(),
            layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            None,
        );
        let member_rows: Vec<String> = (0..viewport.height)
            .map(|y| buffer_row_text(&buf, y))
            .collect();
        assert!(
            member_rows
                .iter()
                .any(|r| r.contains("Subagent") && r.contains("\u{2014} Thinking")),
            "expanded member row must keep the activity suffix: {member_rows:?}"
        );
    }

    /// Render a prepared state — feeding the fold's spans when `with_spans` —
    /// and return the frame buffer plus the full render result (selection
    /// model included). The span-less path is what harnesses exercise and
    /// must keep the legacy plain-count header text.
    fn render_state(
        state: &crate::scrollback::ScrollbackState,
        viewport: Rect,
        with_spans: bool,
    ) -> (Buffer, ScrollRenderResult) {
        let layouts = state.get_cached_entry_layouts().expect("layout cache");
        let refs = state.entries_in_range(0..state.len());
        let mut buf = Buffer::empty(viewport);
        let result = render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &Theme::current(),
            state.appearance(),
            layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            with_spans.then(|| (state.group_spans(), 0usize)),
            None,
        );
        (buf, result)
    }

    /// Render the header row (row 0) of a truncation-grouped state, with or
    /// without the fold's spans.
    fn truncation_header_row(
        state: &crate::scrollback::ScrollbackState,
        viewport: Rect,
        with_spans: bool,
    ) -> String {
        buffer_row_text(&render_state(state, viewport, with_spans).0, 0)
    }

    /// A collapsed truncation ("N more") header fed with the fold's spans
    /// renders the aggregated bucket label for exactly its hidden prefix —
    /// the header row plus the rows folded behind it — while the span-less
    /// path keeps the plain count.
    #[test]
    fn truncation_header_renders_bucket_label_with_spans_and_plain_count_without() {
        use crate::scrollback::ScrollbackState;

        crate::appearance::cache::set_group_tool_verbs(true);
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.display.group_max_visible = 3;
        state.set_appearance(appearance);
        for i in 0..6 {
            state.push_block(RenderBlock::execute(format!("cmd{i}")));
        }
        let viewport = Rect::new(0, 0, 80, 10);
        state.prepare_layout(viewport.width, viewport.height);

        let layouts = state.get_cached_entry_layouts().expect("layout cache");
        assert!(!layouts[0].verb_group_header, "commands never verb-fold");
        // 6 participants, max_visible=3 → hidden=3; the plain count shows
        // hidden-1 while the label describes all 3 hidden participants.
        assert_eq!(layouts[0].group_header_count, 2);

        let labeled = truncation_header_row(&state, viewport, true);
        assert!(
            labeled.contains("Ran 3 commands"),
            "spans feed the hidden-prefix bucket label: {labeled:?}"
        );
        let plain = truncation_header_row(&state, viewport, false);
        assert!(
            plain.contains("2 more"),
            "span-less render keeps the legacy count: {plain:?}"
        );
    }

    /// The aggregated vocabulary is owned by the "Group tool calls" setting:
    /// toggled off, a span-fed truncation header keeps the plain "N more"
    /// count (with the toggle off, previously verb-groupable tools feed
    /// truncation runs — they must not come back verb-labeled anyway).
    #[test]
    fn truncation_header_keeps_plain_count_when_group_tool_verbs_off() {
        use crate::scrollback::ScrollbackState;

        crate::appearance::cache::set_group_tool_verbs(false);
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.display.group_max_visible = 3;
        state.set_appearance(appearance);
        for i in 0..6 {
            state.push_block(RenderBlock::execute(format!("cmd{i}")));
        }
        let viewport = Rect::new(0, 0, 80, 10);
        state.prepare_layout(viewport.width, viewport.height);

        let layouts = state.get_cached_entry_layouts().expect("layout cache");
        assert_eq!(layouts[0].group_header_count, 2, "run still truncates");

        let row = truncation_header_row(&state, viewport, true);
        assert!(
            row.contains("2 more") && !row.contains("Ran"),
            "toggle off keeps the legacy count even with spans: {row:?}"
        );
    }

    /// The expanded truncation collapse header describes the WHOLE run —
    /// every participant, visible tail included — when spans are present,
    /// and keeps the legacy "N tool calls & thoughts" count without them.
    #[test]
    fn expanded_truncation_collapse_header_renders_whole_run_label() {
        use crate::scrollback::ScrollbackState;

        crate::appearance::cache::set_group_tool_verbs(true);
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.display.group_max_visible = 3;
        state.set_appearance(appearance);
        for i in 0..6 {
            state.push_block(RenderBlock::execute(format!("cmd{i}")));
        }
        let viewport = Rect::new(0, 0, 80, 10);
        state.prepare_layout(viewport.width, viewport.height);
        state.set_selected(Some(0));
        assert!(state.toggle_group_expansion());
        state.prepare_layout(viewport.width, viewport.height);

        let layouts = state.get_cached_entry_layouts().expect("layout cache");
        assert!(layouts[0].group_collapse_header);
        assert_eq!(layouts[0].group_header_count, 5);

        let labeled = truncation_header_row(&state, viewport, true);
        assert!(
            labeled.contains("Ran 6 commands"),
            "expanded header must describe the whole run: {labeled:?}"
        );
        let plain = truncation_header_row(&state, viewport, false);
        assert!(
            plain.contains("5 tool calls & thoughts"),
            "span-less render keeps the legacy count: {plain:?}"
        );
    }

    /// A same-row drag spanning a line's full selectable width, for copy
    /// reconstruction assertions.
    fn full_row_drag(
        line: &ResolvedSelectableLine,
    ) -> crate::scrollback::text_selection::ActiveTextDrag {
        use crate::scrollback::text_selection::{ActiveTextDrag, RangeHit};
        let hit = RangeHit {
            entry_idx: line.entry_idx,
            range_id: line.range_id,
            block_line_idx: line.block_line_idx,
            col_within_range: 0,
        };
        ActiveTextDrag {
            anchor: hit,
            head: RangeHit {
                col_within_range: line.selectable_cols.end.saturating_sub(1),
                ..hit
            },
            kind: Default::default(),
            anchor_content_width: None,
        }
    }

    /// A labeled truncation header gets copy parity with verb headers: its
    /// synthetic selectable row carries the aggregated label as the copy
    /// text, hitbox shifted past the diamond chrome, in both fold states.
    #[test]
    fn labeled_truncation_header_synthetic_line_copies_label_text() {
        use crate::scrollback::ScrollbackState;
        use crate::scrollback::text_selection::reconstruct_selection_text;

        crate::appearance::cache::set_group_tool_verbs(true);
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.display.group_max_visible = 3;
        state.set_appearance(appearance);
        for i in 0..6 {
            state.push_block(RenderBlock::execute(format!("cmd{i}")));
        }
        let viewport = Rect::new(0, 0, 80, 10);
        state.prepare_layout(viewport.width, viewport.height);

        // Collapsed: the header row copies its hidden-prefix label.
        let (buf, result) = render_state(&state, viewport, true);
        let model = &result.selection_model;
        let header = model.range(0, GROUP_HEADER_RANGE_ID).expect("header range");
        assert_eq!(header.lines.len(), 1);
        assert_eq!(header.lines[0].text, "Ran 3 commands");
        assert_eq!(header.lines[0].screen_y, 0);
        // Pin the hitbox to the DRAWN glyphs, not just to the chrome helper:
        // the frame's own cells must spell the label starting at screen_x, so
        // a chrome edit that misaligned highlight from paint would fail here.
        let screen_x = header.lines[0].screen_x;
        let drawn: String = (screen_x..screen_x + header.lines[0].selectable_cols.end)
            .map(|x| buf[(x, 0)].symbol())
            .collect();
        assert_eq!(
            drawn, "Ran 3 commands",
            "synthetic hitbox must start exactly where the drawn label starts"
        );
        let expected_x = HorizontalLayout::ACCENT
            + state.appearance().scrollback.layout.block_pad_left
            + group_header_chrome_prefix_width();
        assert_eq!(
            screen_x, expected_x,
            "hitbox sits past accent chrome + diamond prefix"
        );
        let copy = reconstruct_selection_text(model, &full_row_drag(&header.lines[0]))
            .expect("header copy");
        assert_eq!(copy, "Ran 3 commands");

        // Expanded: the collapse header describes the whole run and copies it.
        state.set_selected(Some(0));
        assert!(state.toggle_group_expansion());
        state.prepare_layout(viewport.width, viewport.height);
        let (_, result) = render_state(&state, viewport, true);
        let model = &result.selection_model;
        let header = model
            .range(0, GROUP_HEADER_RANGE_ID)
            .expect("expanded header range");
        assert_eq!(header.lines[0].text, "Ran 6 commands");
        let copy = reconstruct_selection_text(model, &full_row_drag(&header.lines[0]))
            .expect("expanded header copy");
        assert_eq!(copy, "Ran 6 commands");
    }

    /// With the vocabulary gate off, a span-fed truncation header keeps the
    /// plain count AND stays non-copyable: no synthetic selectable line may
    /// land on the header row (the span-less path is pinned by
    /// `group_header_entry_contributes_no_selectable_lines`).
    #[test]
    fn plain_count_truncation_header_contributes_no_selectable_line() {
        use crate::scrollback::ScrollbackState;

        crate::appearance::cache::set_group_tool_verbs(false);
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.display.group_max_visible = 3;
        state.set_appearance(appearance);
        for i in 0..6 {
            state.push_block(RenderBlock::execute(format!("cmd{i}")));
        }
        let viewport = Rect::new(0, 0, 80, 10);
        state.prepare_layout(viewport.width, viewport.height);

        let (buf, result) = render_state(&state, viewport, true);
        assert!(
            buffer_row_text(&buf, 0).contains("2 more"),
            "gate off keeps the plain count"
        );
        assert!(
            result
                .selection_model
                .range(0, GROUP_HEADER_RANGE_ID)
                .is_none(),
            "plain-count header must not gain the synthetic label range"
        );
        let rows: Vec<u16> = result
            .selection_model
            .ranges
            .iter()
            .flat_map(|r| r.lines.iter())
            .map(|l| l.screen_y)
            .collect();
        assert!(
            rows.iter().all(|row| *row != 0),
            "no selectable line may land on the plain '◈ N more' header row, got: {rows:?}"
        );
    }

    /// Both fold families feed the single label channel in one frame: the
    /// verb header and the labeled truncation header each render their
    /// aggregated label and expose it through the shared reserved range.
    #[test]
    fn verb_and_truncation_headers_share_one_label_channel() {
        use crate::scrollback::ScrollbackState;

        crate::appearance::cache::set_group_tool_verbs(true);
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.display.group_max_visible = 3;
        state.set_appearance(appearance);
        state.push_block(RenderBlock::read("a.rs", None));
        state.push_block(RenderBlock::read("b.rs", None));
        for i in 0..6 {
            state.push_block(RenderBlock::execute(format!("cmd{i}")));
        }
        let viewport = Rect::new(0, 0, 80, 12);
        state.prepare_layout(viewport.width, viewport.height);

        let layouts = state.get_cached_entry_layouts().expect("layout cache");
        assert!(layouts[0].verb_group_header, "reads verb-fold");
        assert!(
            !layouts[2].verb_group_header && layouts[2].is_group_header(),
            "commands truncation-fold behind the verb run"
        );

        let (buf, result) = render_state(&state, viewport, true);
        let rows: Vec<String> = (0..viewport.height)
            .map(|y| buffer_row_text(&buf, y))
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("Read 2 files")),
            "verb header must render its label: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("Ran 3 commands")),
            "truncation header must render its label: {rows:?}"
        );

        let model = &result.selection_model;
        let verb = model
            .range(0, GROUP_HEADER_RANGE_ID)
            .expect("verb header range");
        assert_eq!(verb.lines[0].text, "Read 2 files");
        let trunc = model
            .range(2, GROUP_HEADER_RANGE_ID)
            .expect("truncation header range");
        assert_eq!(trunc.lines[0].text, "Ran 3 commands");
    }

    #[test]
    fn test_selected_entry_tracking() {
        let entries = make_entries(3);
        let viewport = Rect::new(0, 0, 80, 20);
        let result = render_with_scratch(&entries, viewport, 0, Some(1));

        assert!(result.selected_area.is_some());
        let selected = result.selected_area.unwrap();
        assert!(!selected.top_clipped);
        assert!(!selected.bottom_clipped);
    }

    #[test]
    fn test_partial_visibility_top_clipped() {
        let entries = make_entries(3);
        let viewport = Rect::new(0, 0, 80, 10);
        let result = render_with_scratch(&entries, viewport, 1, Some(0));

        assert!(result.selected_area.is_some());
        let selected = result.selected_area.unwrap();
        assert!(selected.top_clipped);
        assert!(!selected.bottom_clipped);
    }

    #[test]
    fn test_selection_model_maps_visible_lines_for_markdown_entry() {
        let entries = vec![make_markdown_entry(
            "hello world this should wrap across lines",
        )];
        let viewport = Rect::new(0, 0, 20, 10);
        let result = render_with_scratch(&entries, viewport, 0, None);

        assert!(!result.selection_model.ranges.is_empty());
        let range = &result.selection_model.ranges[0];
        assert_eq!(range.entry_idx, 0);
        assert!(range.lines.len() > 1);
        assert!(
            range
                .lines
                .windows(2)
                .all(|w| w[0].screen_y < w[1].screen_y)
        );
    }

    #[test]
    fn test_selection_model_top_clipped_markdown_entry() {
        let entries = vec![make_markdown_entry(
            "hello world this should wrap across lines",
        )];
        let viewport = Rect::new(0, 0, 20, 3);
        let result = render_with_scratch(&entries, viewport, 1, None);

        let range = &result.selection_model.ranges[0];
        assert_eq!(range.lines[0].screen_y, 0);
    }

    #[test]
    fn test_selection_model_bottom_clipped_markdown_entry() {
        let entries = vec![make_markdown_entry(
            "hello world this should wrap across lines",
        )];
        let viewport = Rect::new(0, 0, 20, 2);
        let result = render_with_scratch(&entries, viewport, 0, None);

        let range = &result.selection_model.ranges[0];
        assert!(range.lines.len() <= 2);
    }

    #[test]
    fn test_vpad_rows_do_not_become_selectable_lines() {
        let entries = vec![ScrollbackEntry::new(RenderBlock::stub(
            "hello",
            Color::Blue,
        ))];
        let viewport = Rect::new(0, 0, 20, 5);
        let result = render_with_scratch(&entries, viewport, 0, None);

        assert!(result.selection_model.ranges.is_empty());
    }

    #[test]
    fn test_selected_entry_output_divergence_uses_selected_branch() {
        let entries = vec![make_markdown_entry("hello")];
        let viewport = Rect::new(0, 0, 20, 5);
        let result = render_with_scratch(&entries, viewport, 0, Some(0));

        assert!(!result.selection_model.visible_blocks.is_empty());
        assert_eq!(result.selection_model.visible_blocks[0].entry_idx, 0);
    }

    /// Message-style blocks (`AgentMessage`, `UserPrompt`, `Btw`)
    /// reserve 10 columns on the right for the timestamp overlay, so their cached
    /// output is wrapped at `content_area.width - 10`, not `content_area.width`.
    ///
    /// `VisibleBlockGeometry.content_width` must report the same reduced width
    /// that was used to populate the cache; otherwise any code that re-derives
    /// the wrapped lines from the model (notably `finish_text_drag`) would call
    /// `effective_output` at the wrong width, get a different wrapping, and
    /// slice the wrong content for the clipboard.
    #[test]
    fn message_block_content_width_subtracts_timestamp_reservation() {
        // Picked so the message wraps to a different line count at
        // `content_width - 10` than at `content_width`. With viewport=30
        // and chrome=4, pane_content_width=26 and per-block content_width=16.
        let entries = vec![make_markdown_entry(
            "hello world foo bar baz qux quux corge grault garply waldo",
        )];
        let viewport = Rect::new(0, 0, 30, 20);
        let result = render_with_scratch(&entries, viewport, 0, None);

        let pane_content_width = result.selection_model.content_area.width;
        let block = &result.selection_model.visible_blocks[0];
        assert_eq!(
            block.content_width,
            pane_content_width.saturating_sub(10),
            "AgentMessage should reserve 10 cols for the timestamp"
        );

        // The lines registered in the resolved model came from the cached
        // output computed at `block.content_width`. Re-deriving them at the
        // same width must produce the same line count so block_line_idx values
        // remain valid; deriving at the wider `pane_content_width` produces a
        // different wrapping (the bug `finish_text_drag` previously triggered).
        let appearance = AppearanceConfig::default();
        let model_lines = result.selection_model.ranges[0].lines.len();
        let entry_lines_narrow = entries[0]
            .effective_output(block.content_width, &appearance, false, None)
            .output()
            .lines
            .len();
        let entry_lines_wide = entries[0]
            .effective_output(pane_content_width, &appearance, false, None)
            .output()
            .lines
            .len();
        assert_eq!(
            entry_lines_narrow, model_lines,
            "model lines must match a re-derivation at the per-block content_width"
        );
        assert_ne!(
            entry_lines_wide, model_lines,
            "wider width must wrap differently — proves passing content_area.width \
             to effective_output would break block_line_idx alignment"
        );
    }

    // ── map_hyperlinks_to_overlay ──

    use crate::scrollback::types::BlockLine;
    use ratatui::text::Line as RatatuiLine;

    fn make_block_output(specs: &[(&str, Option<&str>)]) -> BlockOutput {
        BlockOutput {
            lines: specs
                .iter()
                .map(|(text, joiner)| {
                    let mut bl = BlockLine::styled(RatatuiLine::from(text.to_string()));
                    bl.joiner = joiner.map(|s| s.to_string());
                    bl
                })
                .collect(),
        }
    }

    fn make_hyperlink(
        line: usize,
        cols: std::ops::Range<usize>,
        url: &str,
        id: u32,
    ) -> xai_grok_markdown::HyperlinkTarget {
        xai_grok_markdown::HyperlinkTarget {
            line_index: line,
            column_range: cols,
            url: url.to_string(),
            id,
        }
    }

    #[test]
    fn overlay_single_line_link() {
        let output = make_block_output(&[("hello world", None)]);
        let links = [make_hyperlink(0, 0..5, "https://a.com", 1)];
        let mut overlay = LinkOverlay::new();
        map_hyperlinks_to_overlay(&links, &output, 0, 10, 20, 4, 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        let link = &overlay.links()[0];
        assert_eq!(link.screen_row, 10);
        assert_eq!(link.col_start, 4);
        assert_eq!(link.col_end, 9);
        assert_eq!(
            &*resolve_link_target(&link.target)
                .and_then(|resolved| resolved.osc8_url)
                .expect("url"),
            "https://a.com"
        );
        assert_eq!(link.id, Some(1));
    }

    #[test]
    fn overlay_markdown_relative_link_opens_as_file_url() {
        // End-to-end through the real render path: a markdown link to a short
        // media path that matches this transcript's generated media becomes a
        // `file://` overlay.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("images")).unwrap();
        std::fs::write(dir.path().join("images/1.png"), b"x").unwrap();
        let media = vec![dir.path().join("images/1.png")];

        let mut entries = vec![make_markdown_entry("Saved [images/1.png](images/1.png)\n")];
        if let RenderBlock::AgentMessage(b) = &mut entries[0].block {
            b.finish();
        }
        let viewport = Rect::new(0, 0, 80, 10);
        let theme = Theme::current();
        let appearance = AppearanceConfig::default();
        let layouts = compute_layouts(&entries, viewport.width, &appearance);
        let refs: Vec<&ScrollbackEntry> = entries.iter().collect();
        let mut buf = Buffer::empty(viewport);
        let result = render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &theme,
            &appearance,
            &layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &media,
            None,
            None,
        );

        let links: Vec<Arc<str>> = result
            .link_overlay
            .links()
            .iter()
            .map(|l| {
                resolve_link_target(&l.target)
                    .and_then(|resolved| resolved.osc8_url)
                    .expect("url")
            })
            .collect();
        assert!(
            links
                .iter()
                .any(|u| u.starts_with("file://") && u.ends_with("/images/1.png")),
            "expected a file:// link to images/1.png, got {links:?}",
        );
    }

    #[test]
    fn overlay_word_wrap_splits_link() {
        // Pre-wrap line "hello world" (11 chars) wrapped into two segments:
        // wrapped line 0: "hello" (5 chars, joiner=None  → new pre-wrap line)
        // wrapped line 1: "world" (5 chars, joiner=Some(" ") → continuation)
        // The joiner " " is the space consumed at the wrap point (col 5).
        let output = make_block_output(&[("hello", None), ("world", Some(" "))]);
        // Link spans the full pre-wrap line: cols 0..11
        let links = [make_hyperlink(0, 0..11, "https://b.com", 2)];
        let mut overlay = LinkOverlay::new();
        map_hyperlinks_to_overlay(&links, &output, 0, 0, 10, 0, 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 2);
        // First segment: cols 0..5 on screen row 0
        assert_eq!(overlay.links()[0].screen_row, 0);
        assert_eq!(overlay.links()[0].col_start, 0);
        assert_eq!(overlay.links()[0].col_end, 5);
        // Second segment: cols 0..5 on screen row 1
        assert_eq!(overlay.links()[1].screen_row, 1);
        assert_eq!(overlay.links()[1].col_start, 0);
        assert_eq!(overlay.links()[1].col_end, 5);
        assert_eq!(
            &*resolve_link_target(&overlay.links()[0].target)
                .and_then(|resolved| resolved.osc8_url)
                .expect("url"),
            &*resolve_link_target(&overlay.links()[1].target)
                .and_then(|resolved| resolved.osc8_url)
                .expect("url")
        );
    }

    #[test]
    fn overlay_content_skip_hides_scrolled_lines() {
        let output = make_block_output(&[("line0", None), ("line1", None), ("line2", None)]);
        // Link on line 0 (scrolled off), link on line 2 (visible)
        let links = [
            make_hyperlink(0, 0..5, "https://hidden.com", 1),
            make_hyperlink(2, 0..5, "https://visible.com", 2),
        ];
        let mut overlay = LinkOverlay::new();
        // content_skip=2 means first 2 wrapped lines are above viewport
        map_hyperlinks_to_overlay(&links, &output, 2, 0, 10, 0, 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(
            &*resolve_link_target(&overlay.links()[0].target)
                .and_then(|resolved| resolved.osc8_url)
                .expect("url"),
            "https://visible.com"
        );
        assert_eq!(overlay.links()[0].screen_row, 0);
    }

    #[test]
    fn overlay_max_screen_y_clips_links() {
        let output = make_block_output(&[("line0", None), ("line1", None), ("line2", None)]);
        let links = [
            make_hyperlink(0, 0..5, "https://visible.com", 1),
            make_hyperlink(2, 0..5, "https://clipped.com", 2),
        ];
        let mut overlay = LinkOverlay::new();
        // max_screen_y=2 → only rows 0..2 are visible (screen_y 0 and 1)
        map_hyperlinks_to_overlay(&links, &output, 0, 0, 2, 0, 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(
            &*resolve_link_target(&overlay.links()[0].target)
                .and_then(|resolved| resolved.osc8_url)
                .expect("url"),
            "https://visible.com"
        );
    }

    #[test]
    fn overlay_content_line_offset_skips_header_lines() {
        // Simulates BtwBlock: header + separator + markdown body
        let output = make_block_output(&[
            ("/btw question", None), // header (offset 0)
            ("", None),              // separator (offset 1)
            ("body text", None),     // markdown body line 0
        ]);
        // Hyperlink on markdown body line_index=0, cols 0..4
        let links = [make_hyperlink(0, 0..4, "https://body.com", 1)];
        let mut overlay = LinkOverlay::new();
        // content_line_offset=2 shifts line_index by 2
        map_hyperlinks_to_overlay(&links, &output, 0, 0, 10, 0, 2, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(overlay.links()[0].screen_row, 2);
        assert_eq!(overlay.links()[0].col_start, 0);
        assert_eq!(overlay.links()[0].col_end, 4);
    }

    #[test]
    fn overlay_content_line_offset_out_of_range_is_safe() {
        let output = make_block_output(&[("only line", None)]);
        // line_index=0, but offset=5 pushes adjusted_line=5 past pre_wrap_segments
        let links = [make_hyperlink(0, 0..4, "https://x.com", 1)];
        let mut overlay = LinkOverlay::new();
        map_hyperlinks_to_overlay(&links, &output, 0, 0, 10, 0, 5, &[], &mut overlay);

        assert!(overlay.is_empty());
    }

    #[test]
    fn overlay_partial_column_overlap() {
        // Link spans cols 3..8, but the line is only 6 chars wide (cols 0..6)
        let output = make_block_output(&[("abcdef", None)]);
        let links = [make_hyperlink(0, 3..8, "https://partial.com", 1)];
        let mut overlay = LinkOverlay::new();
        map_hyperlinks_to_overlay(&links, &output, 0, 5, 10, 2, 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        let link = &overlay.links()[0];
        assert_eq!(link.screen_row, 5);
        assert_eq!(link.col_start, 5); // content_x(2) + local_col_start(3)
        assert_eq!(link.col_end, 8); // content_x(2) + local_col_end(6)
    }

    #[test]
    fn overlay_empty_hyperlinks_produces_nothing() {
        let output = make_block_output(&[("text", None)]);
        let links: &[xai_grok_markdown::HyperlinkTarget] = &[];
        let mut overlay = LinkOverlay::new();
        map_hyperlinks_to_overlay(links, &output, 0, 0, 10, 0, 0, &[], &mut overlay);

        assert!(overlay.is_empty());
    }

    // ── URL scanning for non-markdown blocks ──

    #[test]
    fn execute_block_urls_get_overlay_links() {
        let entries = vec![ScrollbackEntry::new(RenderBlock::execute_with_output(
            "curl https://api.example.com/data",
            "HTTP/1.1 200 OK\nSee https://docs.example.com/api for details.",
            None::<String>,
        ))];
        // Force expanded mode so output is visible.
        let mut entries = entries;
        entries[0].display_mode = DisplayMode::Expanded;

        let viewport = Rect::new(0, 0, 80, 20);
        let result = render_with_scratch(&entries, viewport, 0, None);

        // Should find URL(s) in the output text.
        assert!(
            !result.link_overlay.is_empty(),
            "execute block output should have linkified URLs"
        );
        let urls: Vec<Arc<str>> = result
            .link_overlay
            .links()
            .iter()
            .map(|l| {
                resolve_link_target(&l.target)
                    .and_then(|resolved| resolved.osc8_url)
                    .expect("url")
            })
            .collect();
        assert!(
            urls.iter()
                .any(|url| url.as_ref() == "https://docs.example.com/api"),
            "expected URL from stdout in overlay, got: {:?}",
            urls,
        );
    }

    #[test]
    fn markdown_wrapped_session_media_path_fully_linkified() {
        // Regression: imagine-tool prose whose long session path soft-wraps
        // across rows. The whole path must be clickable (one overlay region
        // per row, all pointing at the full file:// URL) — not just the
        // leading path fragment on the first row.
        let path = "/Users/alice/.grok/sessions/%2FUsers%2Falice%2Fcode%2Fxai/\
                    019e0000-0000-7000-8000-000000000001/images/1.jpg";
        let entries = vec![make_markdown_entry(&format!(
            "Image generated and saved to {path}\n"
        ))];
        // Narrow viewport so the path wraps across several rows.
        let viewport = Rect::new(0, 0, 40, 20);
        let result = render_with_scratch(&entries, viewport, 0, None);

        let expected_url = url::Url::from_file_path(path).unwrap();
        let path_links: Vec<_> = result
            .link_overlay
            .links()
            .iter()
            .filter(|l| {
                resolve_link_target(&l.target)
                    .and_then(|resolved| resolved.osc8_url)
                    .is_some_and(|url| url.as_ref() == expected_url.as_str())
            })
            .collect();
        assert!(
            path_links.len() >= 2,
            "wrapped path should yield one overlay region per visual row, got: {:?}",
            result
                .link_overlay
                .links()
                .iter()
                .map(|l| (
                    resolve_link_target(&l.target)
                        .and_then(|resolved| resolved.osc8_url)
                        .expect("url"),
                    l.screen_row,
                    l.col_start,
                    l.col_end
                ))
                .collect::<Vec<_>>()
        );
        // Regions land on consecutive distinct rows.
        let mut rows: Vec<u16> = path_links.iter().map(|l| l.screen_row).collect();
        rows.dedup();
        assert_eq!(
            rows.len(),
            path_links.len(),
            "each visual row gets one region"
        );
        assert!(rows.windows(2).all(|w| w[1] == w[0] + 1));
    }

    #[test]
    fn two_autolink_documents_with_restarted_ids_stay_separate_hits() {
        // Each agent message is its own markdown document, so both autolinks
        // get id=0. Consecutive same-id overlay entries must not merge when
        // the URLs differ (Apple Terminal Cmd+hover/click).
        let entries = vec![
            make_markdown_entry("<https://example.com/aaa>\n"),
            make_markdown_entry("<https://example.com/bbb>\n"),
        ];
        let viewport = Rect::new(0, 0, 80, 20);
        let result = render_with_scratch(&entries, viewport, 0, None);
        let overlay = &result.link_overlay;
        assert!(
            overlay.links().iter().all(|l| l.id == Some(0)),
            "each document restarts at id=0: {:?}",
            overlay.links().iter().map(|l| l.id).collect::<Vec<_>>()
        );

        let mut map = crate::scrollback::VisibleLinkMap::default();
        map.rebuild(1, overlay, vec![]);
        assert_eq!(map.len(), 2, "colliding source ids must stay two hits");

        let second = overlay
            .links()
            .iter()
            .find(|l| {
                resolve_link_target(&l.target)
                    .and_then(|resolved| resolved.osc8_url)
                    .is_some_and(|url| url.as_ref() == "https://example.com/bbb")
            })
            .expect("second autolink in overlay");
        let col = second.col_start + (second.col_end - second.col_start) / 2;
        assert_eq!(
            map.link_at(col, second.screen_row)
                .and_then(|hit| resolve_link_target(&hit.target))
                .and_then(|resolved| resolved.osc8_url)
                .as_deref(),
            Some("https://example.com/bbb")
        );
    }

    #[test]
    fn markdown_block_does_not_double_scan() {
        // Agent message with a URL — should get exactly one hyperlink per URL
        // from the markdown renderer, not doubled by the plain-text scan.
        let entries = vec![make_markdown_entry("Visit https://example.com for info.\n")];
        let viewport = Rect::new(0, 0, 80, 10);
        let result = render_with_scratch(&entries, viewport, 0, None);

        let url_count = result
            .link_overlay
            .links()
            .iter()
            .filter(|l| {
                resolve_link_target(&l.target)
                    .and_then(|resolved| resolved.osc8_url)
                    .is_some_and(|url| url.as_ref() == "https://example.com")
            })
            .count();
        assert_eq!(
            url_count, 1,
            "markdown block should not be scanned again by the plain-text scanner"
        );
    }

    #[test]
    fn collapsed_block_body_urls_not_visible() {
        // URLs in the output body are not rendered when collapsed (only the
        // header line is shown), so no link overlay entries should appear.
        let entries = vec![ScrollbackEntry::new(RenderBlock::execute_with_output(
            "echo test",
            "See https://example.com",
            None::<String>,
        ))];
        let mut entries = entries;
        entries[0].display_mode = DisplayMode::Collapsed;

        let viewport = Rect::new(0, 0, 80, 10);
        let result = render_with_scratch(&entries, viewport, 0, None);

        assert!(
            result.link_overlay.is_empty(),
            "output-body URLs should not appear when block is collapsed"
        );
    }

    #[test]
    fn collapsed_block_header_file_path_is_scanned() {
        // File paths in the command header line should be linkified even
        // when the block is collapsed.
        let mut entries = vec![ScrollbackEntry::new(RenderBlock::execute_with_output(
            "cd /Users/foo/project && ls",
            "file1\nfile2",
            None::<String>,
        ))];
        entries[0].display_mode = DisplayMode::Collapsed;

        let viewport = Rect::new(0, 0, 80, 10);
        let result = render_with_scratch(&entries, viewport, 0, None);

        let urls: Vec<Arc<str>> = result
            .link_overlay
            .links()
            .iter()
            .map(|l| {
                resolve_link_target(&l.target)
                    .and_then(|resolved| resolved.osc8_url)
                    .expect("url")
            })
            .collect();
        assert!(
            urls.iter()
                .any(|u| u.starts_with("file:///Users/foo/project")),
            "file path in collapsed header should be linkified, got: {urls:?}"
        );
    }

    #[test]
    fn group_header_entry_does_not_leak_hidden_line_links() {
        let mut entries = vec![
            ScrollbackEntry::new(RenderBlock::execute_with_output(
                "cd /Users/foo/project && ls",
                "out",
                None::<String>,
            )),
            ScrollbackEntry::new(RenderBlock::execute_with_output(
                "echo hidden",
                "out",
                None::<String>,
            )),
            ScrollbackEntry::new(RenderBlock::execute_with_output(
                "echo visible",
                "out",
                None::<String>,
            )),
        ];
        for e in &mut entries {
            e.display_mode = DisplayMode::Collapsed;
        }

        let layouts = vec![
            EntryLayoutInfo {
                height: 1,
                gap_after: 0,
                group_header_count: 1,
                group_collapse_header: false,
                verb_group_header: false,
            },
            EntryLayoutInfo {
                height: 0,
                gap_after: 0,
                group_header_count: 0,
                group_collapse_header: false,
                verb_group_header: false,
            },
            EntryLayoutInfo {
                height: 1,
                gap_after: 1,
                group_header_count: 0,
                group_collapse_header: false,
                verb_group_header: false,
            },
        ];
        let theme = Theme::current();
        let appearance = AppearanceConfig::default();
        let refs: Vec<&ScrollbackEntry> = entries.iter().collect();
        let viewport = Rect::new(0, 0, 80, 10);
        let mut buf = Buffer::empty(viewport);
        let result = render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &theme,
            &appearance,
            &layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            None,
        );

        let links: Vec<(u16, u16, u16, String)> = result
            .link_overlay
            .links()
            .iter()
            .map(|l| {
                (
                    l.screen_row,
                    l.col_start,
                    l.col_end,
                    resolve_link_target(&l.target)
                        .and_then(|resolved| resolved.osc8_url)
                        .expect("url")
                        .to_string(),
                )
            })
            .collect();
        assert!(
            links
                .iter()
                .all(|(_, _, _, url)| !url.contains("/Users/foo/project")),
            "path from the header-replaced entry's hidden line must not be linkified, got: {links:?}"
        );
        assert!(
            links.iter().all(|(row, ..)| *row != 0),
            "no link may land on the '◈ N more' header row, got: {links:?}"
        );
    }

    #[test]
    fn collapse_header_entry_does_not_leak_links_but_visible_group_entries_do() {
        // Smallest shape the truncation fold can produce for an expanded
        // group: 3 entries, header count = group_len - 1 = 2.
        let mut entries = vec![
            ScrollbackEntry::new(RenderBlock::execute_with_output(
                "cd /Users/foo/hidden && ls",
                "out",
                None::<String>,
            )),
            ScrollbackEntry::new(RenderBlock::execute_with_output(
                "cat /Users/foo/visible/file.txt",
                "out",
                None::<String>,
            )),
            ScrollbackEntry::new(RenderBlock::execute_with_output(
                "echo plain",
                "out",
                None::<String>,
            )),
        ];
        for e in &mut entries {
            e.display_mode = DisplayMode::Collapsed;
        }

        let layouts = vec![
            EntryLayoutInfo {
                height: 1,
                gap_after: 0,
                group_header_count: 2,
                group_collapse_header: true,
                verb_group_header: false,
            },
            EntryLayoutInfo {
                height: 1,
                gap_after: 0,
                group_header_count: 0,
                group_collapse_header: false,
                verb_group_header: false,
            },
            EntryLayoutInfo {
                height: 1,
                gap_after: 1,
                group_header_count: 0,
                group_collapse_header: false,
                verb_group_header: false,
            },
        ];
        let theme = Theme::current();
        let appearance = AppearanceConfig::default();
        let refs: Vec<&ScrollbackEntry> = entries.iter().collect();
        let viewport = Rect::new(0, 0, 80, 10);
        let mut buf = Buffer::empty(viewport);
        let result = render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &theme,
            &appearance,
            &layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            None,
        );

        let links: Vec<(u16, String)> = result
            .link_overlay
            .links()
            .iter()
            .map(|l| {
                (
                    l.screen_row,
                    resolve_link_target(&l.target)
                        .and_then(|resolved| resolved.osc8_url)
                        .expect("url")
                        .to_string(),
                )
            })
            .collect();
        assert!(
            links
                .iter()
                .all(|(_, url)| !url.contains("/Users/foo/hidden")),
            "collapse-header entry's hidden line must not be linkified, got: {links:?}"
        );
        assert!(
            links
                .iter()
                .any(|(row, url)| *row == 1 && url.contains("/Users/foo/visible/file.txt")),
            "visible group entry below the collapse header must still be linkified, got: {links:?}"
        );
    }

    #[test]
    fn group_header_entry_contributes_no_selectable_lines() {
        let mut entries = vec![
            ScrollbackEntry::new(RenderBlock::execute_with_output(
                "echo hidden-secret-command",
                "out",
                None::<String>,
            )),
            ScrollbackEntry::new(RenderBlock::execute_with_output(
                "echo also-hidden",
                "out",
                None::<String>,
            )),
            ScrollbackEntry::new(RenderBlock::execute_with_output(
                "echo visible-command",
                "out",
                None::<String>,
            )),
        ];
        for e in &mut entries {
            e.display_mode = DisplayMode::Collapsed;
        }

        let layouts = vec![
            EntryLayoutInfo {
                height: 1,
                gap_after: 0,
                group_header_count: 1,
                group_collapse_header: false,
                verb_group_header: false,
            },
            EntryLayoutInfo {
                height: 0,
                gap_after: 0,
                group_header_count: 0,
                group_collapse_header: false,
                verb_group_header: false,
            },
            EntryLayoutInfo {
                height: 1,
                gap_after: 1,
                group_header_count: 0,
                group_collapse_header: false,
                verb_group_header: false,
            },
        ];
        let theme = Theme::current();
        let appearance = AppearanceConfig::default();
        let refs: Vec<&ScrollbackEntry> = entries.iter().collect();
        let viewport = Rect::new(0, 0, 80, 10);
        let mut buf = Buffer::empty(viewport);
        let result = render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &theme,
            &appearance,
            &layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            None,
        );

        let lines: Vec<(usize, u16, String)> = result
            .selection_model
            .ranges
            .iter()
            .flat_map(|r| r.lines.iter())
            .map(|l| (l.entry_idx, l.screen_y, l.text.clone()))
            .collect();
        assert!(
            lines
                .iter()
                .all(|(_, _, text)| !text.contains("hidden-secret-command")),
            "the header-replaced entry's hidden line must not be selectable, got: {lines:?}"
        );
        assert!(
            lines.iter().all(|(_, row, _)| *row != 0),
            "no selectable line may land on the '◈ N more' header row, got: {lines:?}"
        );
        assert!(
            lines.iter().any(|(idx, row, text)| *idx == 2
                && *row == 1
                && text.contains("echo visible-command")),
            "visible entry below the header must still be selectable, got: {lines:?}"
        );
    }

    /// The verb-group header's synthetic selectable line tracks the rendered
    /// label geometry: both fold states shift the hitbox past the diamond
    /// chrome onto the label glyphs, so highlight always matches the copied
    /// text.
    #[test]
    fn verb_group_header_selection_geometry_tracks_chrome() {
        crate::appearance::cache::set_show_thinking_blocks(false);
        // Absolute path so the URL/path scanner linkifies member 0's row.
        let mut entries = vec![
            ScrollbackEntry::new(RenderBlock::read("/tmp/verbgeo/a1.rs", None)),
            ScrollbackEntry::new(RenderBlock::read("a2.rs", None)),
        ];
        for e in &mut entries {
            e.display_mode = DisplayMode::Collapsed;
        }
        let refs: Vec<&ScrollbackEntry> = entries.iter().collect();
        let theme = Theme::current();
        let appearance = AppearanceConfig::default();
        let viewport = Rect::new(0, 0, 80, 10);

        let layouts_for = |expanded: bool| {
            vec![
                EntryLayoutInfo {
                    // Expanded slot = header line + entry 0's own row.
                    height: if expanded { 2 } else { 1 },
                    gap_after: 0,
                    // Verb headers carry the run's tool-member count.
                    group_header_count: 2,
                    group_collapse_header: expanded,
                    verb_group_header: true,
                },
                EntryLayoutInfo {
                    height: if expanded { 1 } else { 0 },
                    gap_after: 1,
                    group_header_count: 0,
                    group_collapse_header: false,
                    verb_group_header: false,
                },
            ]
        };
        let header_line = |expanded: bool| {
            let mut buf = Buffer::empty(viewport);
            let result = render_scrolled_entries_with_scratch(
                &mut buf,
                viewport,
                &refs,
                0,
                None,
                &theme,
                &appearance,
                &layouts_for(expanded),
                0,
                None,
                None,
                None,
                0,
                0,
                &[],
                None,
                None,
            );
            result
                .selection_model
                .ranges
                .iter()
                .flat_map(|r| r.lines.iter())
                .find(|l| l.entry_idx == 0 && l.screen_y == 0)
                .cloned()
                .unwrap_or_else(|| panic!("verb header must contribute a selectable line"))
        };

        let collapsed = header_line(false);
        assert_eq!(collapsed.text, "Read 2 files");
        let expanded = header_line(true);
        assert_eq!(expanded.text, "Read 2 files");
        assert_eq!(
            collapsed.selectable_cols, expanded.selectable_cols,
            "hitbox always spans exactly the label glyphs"
        );
        // The expanded slot also exposes member 0's own content line at the
        // row below the header, selectable like any other member row.
        let member_line_at = |expanded: bool| {
            let mut buf = Buffer::empty(viewport);
            let result = render_scrolled_entries_with_scratch(
                &mut buf,
                viewport,
                &refs,
                0,
                None,
                &theme,
                &appearance,
                &layouts_for(expanded),
                0,
                None,
                None,
                None,
                0,
                0,
                &[],
                None,
                None,
            );
            result
                .selection_model
                .ranges
                .iter()
                .flat_map(|r| r.lines.iter())
                .find(|l| l.entry_idx == 0 && l.screen_y == 1)
                .cloned()
        };
        let member = member_line_at(true).expect("expanded slot maps member 0's line at row 1");
        assert!(
            member.text.contains("a1.rs"),
            "member 0's own line must be selectable below the header, got {:?}",
            member.text
        );
        assert!(
            member_line_at(false).is_none(),
            "collapsed slot must not map hidden member content"
        );
        // Collapsed Read headers show basename only (`a1.rs`), so the plain-
        // text path scanner no longer sees the absolute `/tmp/verbgeo/…`
        // string and will not emit a file link for it. Folded members still
        // expose no links; expanded members remain selectable (asserted above).
        let member_link_rows = |expanded: bool| {
            let mut buf = Buffer::empty(viewport);
            let result = render_scrolled_entries_with_scratch(
                &mut buf,
                viewport,
                &refs,
                0,
                None,
                &theme,
                &appearance,
                &layouts_for(expanded),
                0,
                None,
                None,
                None,
                0,
                0,
                &[],
                None,
                None,
            );
            result
                .link_overlay
                .links()
                .iter()
                .filter(|l| {
                    resolve_link_target(&l.target)
                        .and_then(|resolved| resolved.osc8_url)
                        .is_some_and(|url| url.contains("a1.rs"))
                })
                .map(|l| l.screen_row)
                .collect::<Vec<_>>()
        };
        // If a link is still produced (e.g. relative basename scanners), it
        // must sit on the member content row under the verb header.
        for row in member_link_rows(true) {
            assert_eq!(
                row, 1,
                "member 0 path link must map to the row below the header"
            );
        }
        assert!(
            member_link_rows(false).is_empty(),
            "folded member must expose no links"
        );
        // Both states wear the diamond chrome: the hitbox starts past it at
        // the same x either way. The label begins at the content column
        // (accent + left pad — NOT `chrome_width`, which also counts the
        // right pad) plus the diamond prefix.
        let expected_x = HorizontalLayout::ACCENT
            + appearance.scrollback.layout.block_pad_left
            + group_header_chrome_prefix_width();
        assert_eq!(
            collapsed.screen_x, expected_x,
            "collapsed hitbox sits past accent chrome + diamond prefix"
        );
        assert_eq!(
            expanded.screen_x, expected_x,
            "expanded hitbox sits past accent chrome + diamond prefix"
        );
    }

    /// The expanded slot's two rows are independent drag targets: the
    /// synthetic header line keys its own reserved range, so a drag anchored
    /// on either row paints and copies that row alone. Before the reserved
    /// id, both rows shared (entry 0, range 0, block line 0) — `push_line`
    /// merged them into one range and a drag on either selected both.
    #[test]
    fn verb_group_expanded_slot_header_and_member_select_independently() {
        use crate::scrollback::text_selection::reconstruct_selection_text;

        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut entries = vec![
            ScrollbackEntry::new(RenderBlock::read("/tmp/verbind/b1.rs", None)),
            ScrollbackEntry::new(RenderBlock::read("b2.rs", None)),
        ];
        for e in &mut entries {
            e.display_mode = DisplayMode::Collapsed;
        }
        let refs: Vec<&ScrollbackEntry> = entries.iter().collect();
        let theme = Theme::current();
        let appearance = AppearanceConfig::default();
        let viewport = Rect::new(0, 0, 80, 10);
        // Expanded slot: header line + member 0's own row.
        let layouts = vec![
            EntryLayoutInfo {
                height: 2,
                gap_after: 0,
                group_header_count: 2,
                group_collapse_header: true,
                verb_group_header: true,
            },
            EntryLayoutInfo {
                height: 1,
                gap_after: 1,
                group_header_count: 0,
                group_collapse_header: false,
                verb_group_header: false,
            },
        ];
        let mut buf = Buffer::empty(viewport);
        let result = render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &theme,
            &appearance,
            &layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            None,
        );
        let model = &result.selection_model;

        // The two rows resolve to two distinct ranges, one line each on
        // their own screen rows.
        let header = model.range(0, GROUP_HEADER_RANGE_ID).expect("header range");
        assert_eq!(header.lines.len(), 1);
        assert_eq!(header.lines[0].text, "Read 2 files");
        assert_eq!(header.lines[0].screen_y, 0);
        let member = model
            .range(0, crate::scrollback::blocks::tool::TOOL_HEADER_RANGE)
            .expect("member range");
        assert_eq!(member.lines.len(), 1);
        assert!(member.lines[0].text.contains("b1.rs"));
        assert_eq!(member.lines[0].screen_y, 1);

        // A same-row drag over each row reconstructs that row's text only.
        let header_copy = reconstruct_selection_text(model, &full_row_drag(&header.lines[0]))
            .expect("header copy");
        assert_eq!(header_copy, "Read 2 files");
        let member_copy = reconstruct_selection_text(model, &full_row_drag(&member.lines[0]))
            .expect("member copy");
        assert!(
            member_copy.contains("b1.rs") && !member_copy.contains("Read 2 files"),
            "member drag must not drag the header along: {member_copy:?}"
        );
    }

    #[test]
    fn group_header_entry_not_search_highlighted_from_hidden_text() {
        let mut entries = vec![
            ScrollbackEntry::new(RenderBlock::execute_with_output(
                "echo hidden-secret-command",
                "out",
                None::<String>,
            )),
            ScrollbackEntry::new(RenderBlock::execute_with_output(
                "echo step",
                "out",
                None::<String>,
            )),
            ScrollbackEntry::new(RenderBlock::execute_with_output(
                "echo visible-command",
                "out",
                None::<String>,
            )),
        ];
        for e in &mut entries {
            e.display_mode = DisplayMode::Collapsed;
        }

        let layouts = vec![
            EntryLayoutInfo {
                height: 1,
                gap_after: 0,
                group_header_count: 1,
                group_collapse_header: false,
                verb_group_header: false,
            },
            EntryLayoutInfo {
                height: 0,
                gap_after: 0,
                group_header_count: 0,
                group_collapse_header: false,
                verb_group_header: false,
            },
            EntryLayoutInfo {
                height: 1,
                gap_after: 1,
                group_header_count: 0,
                group_collapse_header: false,
                verb_group_header: false,
            },
        ];
        let theme = Theme::current();
        let appearance = AppearanceConfig::default();
        let refs: Vec<&ScrollbackEntry> = entries.iter().collect();
        let viewport = Rect::new(0, 0, 80, 10);
        let mut buf = Buffer::empty(viewport);
        let re = regex::Regex::new("command").unwrap();
        render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &theme,
            &appearance,
            &layouts,
            0,
            None,
            None,
            Some(&re),
            0,
            0,
            &[],
            None,
            None,
        );

        assert!(
            reversed_cols_on_row(&buf, 0).is_empty(),
            "hidden-line match must not highlight cells on the '◈ N more' header row"
        );
        assert!(
            !reversed_cols_on_row(&buf, 1).is_empty(),
            "match in the visible entry below the header must still highlight"
        );
    }

    #[test]
    fn group_header_media_entry_registers_no_media_placements() {
        use crate::scrollback::blocks::tool::{OtherToolCallBlock, ToolCallBlock};
        use crate::terminal::image::{GraphicsProtocol, set_protocol_for_test};

        let _guard = set_protocol_for_test(GraphicsProtocol::Kitty);

        let dir = tempfile::tempdir().unwrap();
        let image_path = dir.path().join("hidden.png");
        std::fs::write(&image_path, make_test_png(120, 120)).unwrap();

        let mut entries = vec![
            ScrollbackEntry::new(RenderBlock::ToolCall(ToolCallBlock::Other(
                OtherToolCallBlock::new("image_gen", "saved image")
                    .with_media_ref(&image_path, false),
            ))),
            ScrollbackEntry::new(RenderBlock::execute_with_output(
                "echo hidden",
                "out",
                None::<String>,
            )),
            ScrollbackEntry::new(RenderBlock::execute_with_output(
                "echo visible",
                "out",
                None::<String>,
            )),
        ];
        for e in &mut entries {
            e.display_mode = DisplayMode::Collapsed;
        }

        let layouts = vec![
            EntryLayoutInfo {
                height: 1,
                gap_after: 0,
                group_header_count: 1,
                group_collapse_header: false,
                verb_group_header: false,
            },
            EntryLayoutInfo {
                height: 0,
                gap_after: 0,
                group_header_count: 0,
                group_collapse_header: false,
                verb_group_header: false,
            },
            EntryLayoutInfo {
                height: 1,
                gap_after: 1,
                group_header_count: 0,
                group_collapse_header: false,
                verb_group_header: false,
            },
        ];
        let theme = Theme::current();
        let appearance = AppearanceConfig::default();
        let refs: Vec<&ScrollbackEntry> = entries.iter().collect();
        let viewport = Rect::new(0, 0, 80, 10);
        let mut buf = Buffer::empty(viewport);
        let result = render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &theme,
            &appearance,
            &layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            None,
        );

        assert!(
            result.inline_media.is_empty(),
            "a header-replaced media entry must not register image rows or click \
             targets over the following entries' rows, got: {:?}",
            result.inline_media
        );
    }

    #[test]
    fn truncated_execute_block_detects_urls_in_head_and_tail() {
        // Default truncation: first_lines=2, last_lines=3, threshold=5.
        // Build output > 5 lines with URLs in both the head and tail sections.
        let output = [
            "https://head.example.com/first",
            "line two",
            "line three",
            "line four",
            "line five",
            "line six",
            "line seven",
            "line eight",
            "https://tail.example.com/last",
        ]
        .join("\n");

        let mut entries = vec![ScrollbackEntry::new(RenderBlock::execute_with_output(
            "curl test",
            &output,
            None::<String>,
        ))];
        entries[0].display_mode = DisplayMode::Truncated;

        let viewport = Rect::new(0, 0, 80, 30);
        let result = render_with_scratch(&entries, viewport, 0, None);

        let urls: Vec<Arc<str>> = result
            .link_overlay
            .links()
            .iter()
            .map(|l| {
                resolve_link_target(&l.target)
                    .and_then(|resolved| resolved.osc8_url)
                    .expect("url")
            })
            .collect();
        assert!(
            urls.iter()
                .any(|url| url.as_ref() == "https://head.example.com/first"),
            "URL in head section should be detected, got: {urls:?}",
        );
        assert!(
            urls.iter()
                .any(|url| url.as_ref() == "https://tail.example.com/last"),
            "URL in tail section should be detected, got: {urls:?}",
        );
        // The ellipsis separator line should not produce spurious links.
        assert_eq!(
            urls.len(),
            2,
            "only the two real URLs should be detected, got: {urls:?}",
        );
    }

    /// Collapsed Edit header: after bullet prepend the path is span 2, and the
    /// OSC8 overlay must cover path cols only (not the verb or bullet).
    #[test]
    fn tool_header_link_target_overlay_covers_path_after_bullet() {
        use crate::appearance::ToolBullet;
        use crate::scrollback::types::{BlockContext, selectable_cols};
        use unicode_width::UnicodeWidthStr;

        let abs = "/Users/me/project/src/foo.rs";
        let cwd = std::path::PathBuf::from("/Users/me/project");
        let mut entry = ScrollbackEntry::new(RenderBlock::edit(abs, None));
        entry.display_mode = DisplayMode::Collapsed;

        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.tool.bullet = ToolBullet::Diamond;

        let ctx = BlockContext {
            mode: DisplayMode::Collapsed,
            is_running: false,
            width: 80,
            raw: false,
            max_lines: None,
            appearance: appearance.clone(),
            is_selected: false,
            cwd: Some(cwd.clone()),
        };
        let painted = entry.block.output(&ctx);
        let header = &painted.lines[0];
        assert!(
            header.content.spans.len() >= 3,
            "expected [bullet, Edit , path], got {} spans",
            header.content.spans.len()
        );
        let path_span = header.content.spans[2].content.as_ref();
        assert_eq!(path_span, "foo.rs");
        let target = header.link_target.as_ref().expect("link target on header");
        assert_eq!(
            target,
            &crate::render::osc8::LinkTarget::File(Arc::from(std::path::Path::new(abs)))
        );

        let cols = selectable_cols(&header.content, &header.selectable)
            .expect("path span should be selectable");
        let bullet_w = header.content.spans[0].content.width() as u16;
        let verb_w = header.content.spans[1].content.width() as u16;
        let path_w = path_span.width() as u16;
        assert_eq!(
            cols,
            (bullet_w + verb_w)..(bullet_w + verb_w + path_w),
            "selectable cols must be path-only after bullet shift"
        );

        let viewport = Rect::new(0, 0, 80, 10);
        let theme = Theme::current();
        let layouts = compute_layouts(std::slice::from_ref(&entry), viewport.width, &appearance);
        let refs: Vec<&ScrollbackEntry> = vec![&entry];
        let mut buf = Buffer::empty(viewport);
        let result = render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &theme,
            &appearance,
            &layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            Some(cwd.as_path()),
        );

        let file_links: Vec<_> = result
            .link_overlay
            .links()
            .iter()
            .filter(|l| {
                resolve_link_target(&l.target)
                    .and_then(|resolved| resolved.osc8_url)
                    .is_some_and(|url| url.contains("foo.rs") && url.starts_with("file://"))
            })
            .collect();
        assert_eq!(
            file_links.len(),
            1,
            "expected one file:// overlay for the tool path, got {:?}",
            result.link_overlay.links()
        );
        let hlayout = HorizontalLayout::new(viewport, &appearance.scrollback.layout);
        assert_eq!(
            file_links[0].col_start,
            hlayout.content.x.saturating_add(cols.start),
            "overlay must start on path, not bullet/verb"
        );
        assert_eq!(
            file_links[0].col_end,
            hlayout.content.x.saturating_add(cols.end),
            "overlay must end at path end"
        );
    }

    fn official_vscode_remote_context() -> crate::terminal::TerminalContext {
        crate::terminal::TerminalContext {
            brand: crate::terminal::TerminalName::VsCode,
            is_ssh: true,
            is_official_vscode_remote: true,
            ..Default::default()
        }
    }

    fn file_link_policy(
        link: &OverlayLink,
        terminal: &crate::terminal::TerminalContext,
    ) -> crate::render::osc8::ResolvedLinkTarget {
        resolve_link_target_for_context(&link.target, link.presentation, terminal)
            .expect("file target policy")
    }

    #[test]
    fn official_vscode_remote_delegates_scanned_absolute_path() {
        let path = "/worktree/src/main.rs";
        let entry = make_markdown_entry(path);
        let viewport = Rect::new(0, 0, 80, 5);
        let (result, buf) =
            render_with_scratch_and_buffer(std::slice::from_ref(&entry), viewport, 0, None);
        let link = result
            .link_overlay
            .links()
            .iter()
            .find(|link| matches!(&link.target, crate::render::osc8::LinkTarget::File(_)))
            .expect("scanned file target");

        assert!((0..viewport.height).any(|row| buffer_row_text(&buf, row).contains(path)));
        assert_eq!(link.presentation, LinkPresentation::SelfResolvingPath);
        assert_eq!(
            file_link_policy(link, &official_vscode_remote_context()),
            crate::render::osc8::ResolvedLinkTarget {
                osc8_url: None,
                open_target: None,
            }
        );
    }

    #[test]
    fn official_vscode_remote_tool_headers_delegate_only_self_resolving_paint() {
        let cwd = std::path::PathBuf::from("/worktree");
        let target = "/worktree/src/nested/main.rs";
        let terminal = official_vscode_remote_context();

        for (name, block) in [
            ("Read", RenderBlock::read(target, None)),
            ("Edit", RenderBlock::edit(target, None)),
        ] {
            for (mode, width, expected_paint, expected_presentation) in [
                (
                    DisplayMode::Collapsed,
                    80,
                    "main.rs",
                    LinkPresentation::Opaque,
                ),
                (
                    DisplayMode::Collapsed,
                    16,
                    "\u{2026}",
                    LinkPresentation::Opaque,
                ),
                (
                    DisplayMode::Expanded,
                    80,
                    "src/nested/main.rs",
                    LinkPresentation::SelfResolvingPath,
                ),
            ] {
                let mut entry = ScrollbackEntry::new(block.clone());
                entry.display_mode = mode;
                let viewport = Rect::new(0, 0, width, 8);
                let (result, buf) = render_with_scratch_and_buffer_with_cwd(
                    std::slice::from_ref(&entry),
                    viewport,
                    0,
                    None,
                    Some(&cwd),
                );
                let path_links: Vec<_> = result
                    .link_overlay
                    .links()
                    .iter()
                    .filter(|link| matches!(&link.target, crate::render::osc8::LinkTarget::File(_)))
                    .collect();
                let painted_rows = (0..viewport.height)
                    .map(|row| buffer_row_text(&buf, row).trim_end().to_owned())
                    .filter(|row| !row.is_empty())
                    .collect::<Vec<_>>();

                assert!(
                    painted_rows.iter().any(|row| row.contains(expected_paint)),
                    "{name} {mode:?} width={width}: {painted_rows:?}"
                );
                assert!(!path_links.is_empty(), "{name} {mode:?} width={width}");
                assert!(
                    path_links
                        .iter()
                        .all(|link| link.presentation == expected_presentation),
                    "{name} {mode:?} width={width}: {path_links:?}"
                );
                let expected_owned = expected_presentation == LinkPresentation::Opaque;
                assert!(path_links.iter().all(|link| {
                    assert_eq!(
                        link.target,
                        crate::render::osc8::LinkTarget::File(Arc::from(std::path::Path::new(
                            target
                        )))
                    );
                    let policy = file_link_policy(link, &terminal);
                    policy.osc8_url.is_some() == expected_owned
                        && policy.open_target.is_some() == expected_owned
                }));
            }

            let mut entry = ScrollbackEntry::new(block);
            entry.display_mode = DisplayMode::Expanded;
            let viewport = Rect::new(0, 0, 16, 8);
            let (result, _) = render_with_scratch_and_buffer_with_cwd(
                std::slice::from_ref(&entry),
                viewport,
                0,
                None,
                Some(&cwd),
            );
            let path_links: Vec<_> = result
                .link_overlay
                .links()
                .iter()
                .filter(|link| matches!(&link.target, crate::render::osc8::LinkTarget::File(_)))
                .collect();
            assert!(!path_links.is_empty(), "{name} narrow expanded header");
            assert!(
                path_links
                    .iter()
                    .all(|link| link.presentation == LinkPresentation::Opaque)
            );
            assert!(path_links.iter().all(|link| {
                let policy = file_link_policy(link, &terminal);
                policy.osc8_url.is_some() && policy.open_target.is_some()
            }));
        }
    }

    #[test]
    fn basename_headers_stay_grok_owned_for_duplicate_and_outside_targets() {
        let cwd = std::path::PathBuf::from("/worktree");
        let terminal = official_vscode_remote_context();
        let cases = [
            ("duplicate-a", "/worktree/src/a/main.rs"),
            ("duplicate-b", "/worktree/src/b/main.rs"),
            ("outside", "/opt/service/main.rs"),
        ];

        for (name, target) in cases {
            for (tool, block) in [
                ("Read", RenderBlock::read(target, None)),
                ("Edit", RenderBlock::edit(target, None)),
            ] {
                let entry = ScrollbackEntry::new(block);
                let viewport = Rect::new(0, 0, 80, 5);
                let (result, buf) = render_with_scratch_and_buffer_with_cwd(
                    std::slice::from_ref(&entry),
                    viewport,
                    0,
                    None,
                    Some(&cwd),
                );
                let link = result
                    .link_overlay
                    .links()
                    .iter()
                    .find(|link| matches!(&link.target, crate::render::osc8::LinkTarget::File(_)))
                    .unwrap_or_else(|| panic!("{tool} {name} file target"));

                let painted = (0..viewport.height)
                    .map(|row| buffer_row_text(&buf, row).trim_end().to_owned())
                    .find(|row| row.contains("main.rs"))
                    .unwrap_or_else(|| panic!("{tool} {name} painted basename"));
                assert!(!painted.contains('/'), "{tool} {name}: {painted}");
                assert_eq!(
                    link.target,
                    crate::render::osc8::LinkTarget::File(Arc::from(std::path::Path::new(target))),
                    "{tool} {name} semantic target"
                );
                assert_eq!(link.presentation, LinkPresentation::Opaque, "{tool} {name}");
                let policy = file_link_policy(link, &terminal);
                assert!(policy.osc8_url.is_some(), "{tool} {name}");
                assert!(policy.open_target.is_some(), "{tool} {name}");
            }
        }
    }

    #[test]
    fn long_read_header_link_is_clipped_to_offset_content_area() {
        let path = "/outside/a/very/long/path/that/is/clipped/main.rs";
        let mut entry = ScrollbackEntry::new(RenderBlock::read(path, None));
        entry.display_mode = DisplayMode::Expanded;
        let viewport = Rect::new(11, 0, 24, 5);

        let result = render_with_scratch(std::slice::from_ref(&entry), viewport, 0, None);
        let link = result
            .link_overlay
            .links()
            .iter()
            .find(|link| {
                resolve_link_target(&link.target)
                    .and_then(|resolved| resolved.osc8_url)
                    .is_some_and(|url| url.contains("main.rs"))
            })
            .expect("read header file link");
        let content =
            HorizontalLayout::new(viewport, &AppearanceConfig::default().scrollback.layout).content;
        assert!(link.col_start >= content.left());
        assert_eq!(link.col_end, content.right());
    }

    #[test]
    fn explicit_tool_link_clips_before_u16_conversion() {
        let path = format!("/outside/{}.rs", "x".repeat(70_000));
        let mut entry = ScrollbackEntry::new(RenderBlock::read(path, None));
        entry.display_mode = DisplayMode::Expanded;
        let viewport = Rect::new(9, 0, 40, 5);

        let result = render_with_scratch(std::slice::from_ref(&entry), viewport, 0, None);
        let link = result
            .link_overlay
            .links()
            .iter()
            .find(|link| {
                resolve_link_target(&link.target)
                    .and_then(|resolved| resolved.osc8_url)
                    .is_some_and(|url| url.ends_with(".rs"))
            })
            .expect("long Read header file link");
        let content =
            HorizontalLayout::new(viewport, &AppearanceConfig::default().scrollback.layout).content;
        assert!(link.col_start >= content.left());
        assert_eq!(link.col_end, content.right());
    }

    #[test]
    fn edit_boundary_sidecar_stays_aligned_when_prefix_row_is_clipped() {
        let mut entry = ScrollbackEntry::new(RenderBlock::edit("   foo.rs", None));
        entry.display_mode = DisplayMode::Expanded;
        let viewport = Rect::new(0, 0, 8, 3);
        let rendered = render_with_selection_boundaries(std::slice::from_ref(&entry), viewport, 1);
        let result = &rendered.result;

        let (line, boundary) = result
            .selection_model
            .ranges
            .iter()
            .flat_map(|range| &range.lines)
            .find_map(|line| {
                let hit = crate::scrollback::text_selection::RangeHit {
                    entry_idx: line.entry_idx,
                    range_id: line.range_id,
                    block_line_idx: line.block_line_idx,
                    col_within_range: 0,
                };
                rendered
                    .selection_boundaries
                    .boundary_for_hit(&hit)
                    .map(|boundary| (line, boundary))
            })
            .expect("visible path fragment keeps its cached boundary");
        let copied = boundary.apply(line.text.clone(), true, false);
        assert!(copied.starts_with("   "), "copied={copied:?}");
    }

    #[test]
    fn ordinary_render_has_no_resolved_selection_boundaries() {
        let entries = vec![make_markdown_entry("ordinary selectable text")];
        let rendered = render_with_selection_boundaries(&entries, Rect::new(0, 0, 40, 5), 0);

        assert!(!rendered.result.selection_model.ranges.is_empty());
        assert!(rendered.selection_boundaries.is_empty());
    }

    #[test]
    fn read_header_link_is_dropped_when_path_has_no_visible_columns() {
        let mut entry = ScrollbackEntry::new(RenderBlock::read("/outside/long-file-name.rs", None));
        entry.display_mode = DisplayMode::Expanded;
        let viewport = Rect::new(7, 0, 5, 3);

        let result = render_with_scratch(std::slice::from_ref(&entry), viewport, 0, None);
        assert!(
            result.link_overlay.links().iter().all(|link| {
                !resolve_link_target(&link.target)
                    .and_then(|resolved| resolved.osc8_url)
                    .is_some_and(|url| url.contains("/outside/long-file-name.rs"))
            }),
            "off-row path cells must not be clickable: {:?}",
            result.link_overlay.links()
        );
    }

    /// Collect all `OverlayLink`s for `url` grouped by `OverlayLink::id`,
    /// returning the largest id-group (the wrapped URL's fragment set).
    ///
    /// In pretty mode `[text](url)` produces two HyperlinkTargets that BOTH
    /// reference `url` but have distinct ids: one for the link text
    /// (lower id, parser-produced) and one for the `(url)` suffix
    /// (higher id, url_scan-produced).  The wrapped URL fragments share
    /// an id; for URL-wrap tests the url_scan group is the one that
    /// spans multiple rows.  Panics with a verbose diagnostic if no
    /// group has at least one entry.
    ///
    /// Tie behaviour: `Iterator::max_by_key` returns the LAST equally-
    /// maximum element, and `BTreeMap::into_iter()` yields entries in
    /// ascending key order, so if both groups have the same number of
    /// fragments the higher-id group wins — which is the
    /// url_scan-produced URL-suffix group, the right pick for these
    /// tests.  The helper is unambiguous for all current callers; a
    /// future caller with a different `[parser, url_scan]` shape may
    /// want to filter `result.link_overlay.links()` directly.
    fn url_overlay_group<'a>(result: &'a ScrollRenderResult, url: &str) -> Vec<&'a OverlayLink> {
        let mut by_id: std::collections::BTreeMap<u32, Vec<&OverlayLink>> =
            std::collections::BTreeMap::new();
        for l in result.link_overlay.links() {
            if resolve_link_target(&l.target)
                .and_then(|resolved| resolved.osc8_url)
                .is_some_and(|target| target.as_ref() == url)
                && let Some(id) = l.id
            {
                by_id.entry(id).or_default().push(l);
            }
        }
        if by_id.is_empty() {
            panic!(
                "no OverlayLinks matched url={url:?}; all overlays: {:?}",
                result
                    .link_overlay
                    .links()
                    .iter()
                    .map(|l| (
                        l.screen_row,
                        l.col_start,
                        l.col_end,
                        resolve_link_target(&l.target)
                            .and_then(|resolved| resolved.osc8_url)
                            .expect("url"),
                        l.id
                    ))
                    .collect::<Vec<_>>(),
            );
        }
        let (_id, group) = by_id
            .into_iter()
            .max_by_key(|(_, v)| v.len())
            .expect("checked non-empty above");
        group
    }

    /// Assert that `fragments` are on strictly-increasing CONSECUTIVE
    /// screen rows (rows must differ by exactly 1).  Catches partial
    /// regressions where the middle of a wrapped URL is silently skipped.
    fn assert_consecutive_rows(fragments: &[&OverlayLink]) {
        for w in fragments.windows(2) {
            assert_eq!(
                w[1].screen_row,
                w[0].screen_row + 1,
                "wrapped URL fragments must be on consecutive screen rows: {:?}",
                fragments
                    .iter()
                    .map(|o| (o.screen_row, o.col_start, o.col_end))
                    .collect::<Vec<_>>(),
            );
        }
    }

    /// Regression test for the bug where a markdown link `[text](url)`
    /// rendered in pretty mode loses the OSC 8 wrapper (and the terminal's
    /// auto-styling) on every wrapped row of the URL portion EXCEPT the
    /// first.  Every wrapped row covering URL bytes must receive its own
    /// `OverlayLink` so the entire URL is clickable and styled.
    #[test]
    fn overlay_pretty_link_url_wraps_across_rows() {
        // Long URL with hyphens that force HyphenSplitter to wrap mid-URL.
        // Synthetic long hyphenated URL (length/shape match wrap regression needs).
        let url = "https://example.com/d/seg-00-wrap-seg-01-wrap-seg-02-wrap-seg-03-wrap-seg-04-wrap-seg-05-wrap-seg-06-wrap-seg-07-wrap-seg-08-wrap-seg-09-wrap-seg-10";
        let markdown = format!("[Example Dashboard -- Long Title For Wrap]({url})\n");
        let entries = vec![make_markdown_entry(&markdown)];

        // Picked so the rendered link `text (url)` wraps multiple times.
        let viewport = Rect::new(0, 0, 50, 20);
        let result = render_with_scratch(&entries, viewport, 0, None);

        // The pre-wrap line is ~185 cells wide and per-block content area
        // is ~35 cells (viewport - timestamp reservation - layout chrome).
        // The link text takes row 0 (33 cells), then the URL portion
        // wraps onto exactly 5 continuation rows.  This is a plain
        // paragraph (no `subsequent_indent`), so the combined fragment
        // widths must equal the URL's display width — a strong
        // invariant that would fail under any row drop or off-by-N
        // column-tracking regression.
        let group = url_overlay_group(&result, url);
        assert_eq!(
            group.len(),
            5,
            "expected the URL to wrap onto exactly 5 rows; got {} fragments: {:?}",
            group.len(),
            group
                .iter()
                .map(|o| (o.screen_row, o.col_start, o.col_end))
                .collect::<Vec<_>>(),
        );
        assert_consecutive_rows(&group);
        let combined_width: u32 = group.iter().map(|o| (o.col_end - o.col_start) as u32).sum();
        assert_eq!(
            combined_width as usize,
            url.len(),
            "combined fragment widths must equal URL display width; got fragments: {:?}",
            group
                .iter()
                .map(|o| (o.screen_row, o.col_start, o.col_end))
                .collect::<Vec<_>>(),
        );
    }

    /// Short-ish URL inside prose, wrapping onto multiple rows (no
    /// blockquote/list indent involvement).  For paragraph wrapping
    /// `map_hyperlinks_to_overlay` produces OverlayLinks whose combined
    /// width exactly equals the URL's display width — the strongest
    /// invariant we can pin for the non-indented case.
    #[test]
    fn overlay_pretty_link_url_wraps_multi_row_paragraph() {
        let url = "https://example.com/some/long/path/that/will/wrap/once-and-twice";
        let markdown = format!("See [docs]({url}) for more.\n");
        let entries = vec![make_markdown_entry(&markdown)];

        let viewport = Rect::new(0, 0, 50, 10);
        let result = render_with_scratch(&entries, viewport, 0, None);

        let group = url_overlay_group(&result, url);
        assert!(
            group.len() >= 2,
            "URL should wrap onto multiple rows; got {:?}",
            group
                .iter()
                .map(|o| (o.screen_row, o.col_start, o.col_end))
                .collect::<Vec<_>>(),
        );
        assert_consecutive_rows(&group);
        // The combined width of all fragments must equal the URL's
        // display width (URLs are pure ASCII → display width == byte len).
        // Any silently-dropped middle row would make this sum unequal.
        let combined_width: u32 = group.iter().map(|o| (o.col_end - o.col_start) as u32).sum();
        assert_eq!(
            combined_width as usize,
            url.len(),
            "combined fragment widths must equal URL display width; got fragments: {:?}",
            group
                .iter()
                .map(|o| (o.screen_row, o.col_start, o.col_end))
                .collect::<Vec<_>>(),
        );
    }

    /// CJK link text: column tracking must be display-width aware.  If
    /// the bug had used byte length for `日` (3 bytes, but 2 display
    /// cells), the column accounting would be off by N cells per CJK
    /// character and the URL fragments wouldn't sum to the URL's
    /// display width.  We assert that combined fragment widths equal
    /// the URL's display width — the strongest cell-width invariant we
    /// can pin without re-deriving the entire wrap layout.
    #[test]
    fn overlay_pretty_link_url_with_cjk_text() {
        use unicode_width::UnicodeWidthStr;
        let url = "https://example.com/very-long-path-with-many-hyphens/foo/bar/baz/qux";
        let markdown = format!("[日本語のリンク テキスト]({url})\n");
        let entries = vec![make_markdown_entry(&markdown)];

        let viewport = Rect::new(0, 0, 40, 10);
        let result = render_with_scratch(&entries, viewport, 0, None);

        let group = url_overlay_group(&result, url);
        assert!(
            group.len() >= 2,
            "CJK link text with wrapping URL should produce at least 2 overlay rows",
        );
        assert_consecutive_rows(&group);

        // Combined fragment widths must equal the URL's display width.
        // For pure-ASCII URLs that's `url.len()`.  A byte-vs-cell
        // regression in CJK column tracking would propagate as a wrong
        // pre-wrap column for the URL HyperlinkTarget, which in turn
        // would clip or shift one of the fragments — making the sum
        // unequal.
        let combined_width: u32 = group.iter().map(|o| (o.col_end - o.col_start) as u32).sum();
        assert_eq!(
            combined_width as usize,
            UnicodeWidthStr::width(url),
            "combined fragment widths must equal URL display width",
        );
    }

    /// Long URL inside a blockquote.  The OverlayLink for the URL must
    /// cover continuation rows so OSC 8 is present on every wrapped row.
    ///
    /// KNOWN-BUG: the current
    /// `map_hyperlinks_to_overlay` accumulates `cumulative_col` using
    /// `line.content.width()`, which INCLUDES the `│ ` indent injected
    /// by `word_wrap_line_with_joiners` on continuation rows.  Two
    /// consequences for blockquote/list URL wraps:
    ///   (a) Cosmetic: the OverlayLink on continuation rows starts at
    ///       `content_x` (covering the `│ ` indent), so the indent
    ///       characters are OSC 8 wrapped and inherit the terminal's
    ///       auto-styling (underline/colour).
    ///   (b) Functional: because `cumulative_col` over-counts by
    ///       `indent_width` cells per continuation row, the last
    ///       `indent_width` cells of the URL on each continuation row
    ///       are NOT covered by an OverlayLink — those trailing chars
    ///       are not clickable.  With N continuation rows the
    ///       unclickable tail accumulates to `N * indent_width` cells.
    /// The original PR's invariant (OSC 8 present on every wrapped row
    /// of the URL) IS satisfied.  This test pins that invariant; once
    /// the bug is fixed (needs `BlockLine` to carry `subsequent_indent`
    /// width), tighten the assertions to `col_start == content_x +
    /// indent_width` on continuation rows and pin
    /// `sum(fragment_widths) == url.display_width()`.
    #[test]
    fn overlay_pretty_link_url_in_blockquote_wraps_correctly() {
        let url = "https://example.com/blockquote/path/with/many/hyphens-and-segments-here";
        let markdown = format!("> See [docs]({url}) for more.\n");
        let entries = vec![make_markdown_entry(&markdown)];

        let viewport = Rect::new(0, 0, 50, 10);
        let result = render_with_scratch(&entries, viewport, 0, None);
        let content_x = result.selection_model.content_area.x;

        let group = url_overlay_group(&result, url);
        assert!(
            group.len() >= 2,
            "blockquote URL should wrap; got fragments: {:?}",
            group
                .iter()
                .map(|o| (o.screen_row, o.col_start, o.col_end))
                .collect::<Vec<_>>(),
        );
        assert_consecutive_rows(&group);

        // All fragments must be inside the viewport content area.
        for frag in &group {
            assert!(
                frag.col_start >= content_x,
                "OverlayLink must start at or after content_x",
            );
            assert!(
                frag.col_end <= content_x + viewport.width,
                "OverlayLink must not exceed the viewport content width",
            );
        }
    }

    /// Long URL inside a list item.  Same OSC-coverage invariant as the
    /// blockquote test above; see that test for the rationale and the
    /// description of the related indent-inclusion
    /// bug (both the cosmetic indent-over-styling AND the functional
    /// "last `indent_width` cells of URL not clickable on continuation
    /// rows" symptoms apply here too).
    #[test]
    fn overlay_pretty_link_url_in_list_wraps_correctly() {
        let url = "https://example.com/list/item/path/with/many/hyphens-and-segments-here";
        let markdown = format!("- See [docs]({url}) for more.\n");
        let entries = vec![make_markdown_entry(&markdown)];

        let viewport = Rect::new(0, 0, 50, 10);
        let result = render_with_scratch(&entries, viewport, 0, None);
        let content_x = result.selection_model.content_area.x;

        let group = url_overlay_group(&result, url);
        assert!(
            group.len() >= 2,
            "list URL should wrap; got fragments: {:?}",
            group
                .iter()
                .map(|o| (o.screen_row, o.col_start, o.col_end))
                .collect::<Vec<_>>(),
        );
        assert_consecutive_rows(&group);

        for frag in &group {
            assert!(
                frag.col_start >= content_x,
                "OverlayLink must start at or after content_x",
            );
            assert!(
                frag.col_end <= content_x + viewport.width,
                "OverlayLink must not exceed the viewport content width",
            );
        }
    }

    /// Width changes trigger `set_max_table_width` resets inside
    /// `MarkdownContent::ensure_wrapped`.  URL hyperlinks must survive
    /// every step of a narrow→narrower→wider→narrow sequence (production
    /// terminal pane drags fire many such transitions).
    #[test]
    fn overlay_url_hyperlinks_survive_width_change() {
        let url = "https://example.com/long/url/path/with/many/segments-and-hyphens";
        let markdown = format!("[link]({url})\n");
        let mut entries = vec![make_markdown_entry(&markdown)];
        if let RenderBlock::AgentMessage(b) = &mut entries[0].block {
            b.finish();
        }
        // Exercise multiple width transitions: a wide viewport where
        // the URL fits on one row, a narrow viewport where it must
        // wrap onto multiple rows, and back-and-forth between them.
        // The URL must remain present at every step.
        //
        // The "wide" threshold (120) is chosen so the per-block content
        // area exceeds the URL's display width + leading "link (" prefix
        // even after the 10-cell timestamp reservation and ~4 cells of
        // layout chrome.  The "narrow" widths (30, 50) force wrapping.
        for width in [120u16, 50, 30, 50, 120, 30] {
            let result = render_with_scratch(&entries, Rect::new(0, 0, width, 10), 0, None);
            let group = url_overlay_group(&result, url);
            if width >= 120 {
                // Wide enough: URL fits on a single row.
                assert_eq!(
                    group.len(),
                    1,
                    "URL at width={width} should fit on one row; got: {:?}",
                    group
                        .iter()
                        .map(|o| (o.screen_row, o.col_start, o.col_end))
                        .collect::<Vec<_>>(),
                );
                assert_eq!(
                    (group[0].col_end - group[0].col_start) as usize,
                    url.len(),
                    "single-row URL fragment width must equal URL display width",
                );
            } else {
                // Narrow: URL must wrap onto consecutive rows, with
                // combined fragment widths summing to the URL width.
                assert!(
                    group.len() >= 2,
                    "URL at width={width} should wrap onto multiple rows; got: {:?}",
                    group
                        .iter()
                        .map(|o| (o.screen_row, o.col_start, o.col_end))
                        .collect::<Vec<_>>(),
                );
                assert_consecutive_rows(&group);
                let combined_width: u32 =
                    group.iter().map(|o| (o.col_end - o.col_start) as u32).sum();
                assert_eq!(
                    combined_width as usize,
                    url.len(),
                    "URL at width={width}: combined fragment widths must equal \
                     URL display width; got fragments: {:?}",
                    group
                        .iter()
                        .map(|o| (o.screen_row, o.col_start, o.col_end))
                        .collect::<Vec<_>>(),
                );
            }
        }
    }

    /// Short URL that fits on a single row produces exactly one
    /// `OverlayLink`, sized to the URL's display width.
    #[test]
    fn overlay_pretty_link_url_no_wrap_single_row() {
        let url = "https://a.example/x";
        let markdown = format!("See [docs]({url}) here.\n");
        let entries = vec![make_markdown_entry(&markdown)];

        let viewport = Rect::new(0, 0, 80, 10);
        let result = render_with_scratch(&entries, viewport, 0, None);

        let group = url_overlay_group(&result, url);
        assert_eq!(
            group.len(),
            1,
            "short URL must produce exactly one OverlayLink fragment",
        );
        assert_eq!(
            (group[0].col_end - group[0].col_start) as usize,
            url.len(),
            "single-row URL fragment width must equal URL display width",
        );
    }

    /// Two markdown links on the same pre-wrap line, both with URLs long
    /// enough to wrap their `(url)` suffixes; each URL must produce its
    /// own id-group and the id-groups must be distinct (no merging).
    #[test]
    fn overlay_pretty_two_wrapping_links_distinct_ids() {
        let url_a = "https://aaa.example/with-many-segments/and-more-hyphens/foo/bar/baz";
        let url_b = "https://bbb.example/another-set-of-segments/and-more-hyphens/qux/quux";
        let markdown = format!("See [link-a]({url_a}) and [link-b]({url_b}) end.\n");
        let entries = vec![make_markdown_entry(&markdown)];

        let viewport = Rect::new(0, 0, 50, 30);
        let result = render_with_scratch(&entries, viewport, 0, None);

        let group_a = url_overlay_group(&result, url_a);
        let group_b = url_overlay_group(&result, url_b);

        let id_a = group_a[0].id.expect("URL fragment must have id");
        let id_b = group_b[0].id.expect("URL fragment must have id");
        assert_ne!(
            id_a, id_b,
            "two distinct URLs must produce distinct OSC 8 ids",
        );
        assert!(group_a.len() >= 2, "URL A should wrap onto multiple rows");
        assert!(group_b.len() >= 2, "URL B should wrap onto multiple rows");
        // Wrapped fragments must land on strictly consecutive rows
        // (no silently-dropped middle row).
        assert_consecutive_rows(&group_a);
        assert_consecutive_rows(&group_b);

        // The full set of OverlayLink ids referencing either URL must
        // have at least 4 distinct entries: parser link-text id for "link-a",
        // url_scan id for `(url_a)`, parser link-text id for "link-b", and
        // url_scan id for `(url_b)`.  A regression where any two of these
        // collide (e.g. parser hyperlink IDs not advanced past url_scan IDs
        // across paragraphs) would silently merge OSC 8 hyperlinks.
        let ids: std::collections::HashSet<u32> = result
            .link_overlay
            .links()
            .iter()
            .filter(|l| {
                resolve_link_target(&l.target)
                    .and_then(|resolved| resolved.osc8_url)
                    .is_some_and(|url| url.as_ref() == url_a || url.as_ref() == url_b)
            })
            .filter_map(|l| l.id)
            .collect();
        assert!(
            ids.len() >= 4,
            "expected at least 4 distinct OSC 8 ids across the two links' \
             parser + url_scan groups; got {} distinct ids: {:?}",
            ids.len(),
            ids,
        );
    }

    /// A detected diagram exposes one affordance-row placement, anchored on a
    /// real screen row at the content-area x, carrying the diagram source — and
    /// never an inline image (no `inline_media` placement). Rendering is lazy, so
    /// the placement holds no path/state.
    #[test]
    fn diagram_emits_affordance_placement_not_inline_image() {
        crate::appearance::cache::set_render_mermaid(crate::appearance::RenderMermaid::On);

        let entry = make_markdown_entry("intro\n\n```mermaid\nA-->B\n```\n\nbye\n");

        let viewport = Rect::new(0, 0, 80, 60);
        let theme = Theme::current();
        let appearance = AppearanceConfig::default();
        let layouts = compute_layouts(std::slice::from_ref(&entry), viewport.width, &appearance);
        let refs: Vec<&ScrollbackEntry> = vec![&entry];
        let mut buf = Buffer::empty(viewport);
        let result = render_scrolled_entries_with_scratch(
            &mut buf,
            viewport,
            &refs,
            0,
            None,
            &theme,
            &appearance,
            &layouts,
            0,
            None,
            None,
            None,
            0,
            0,
            &[],
            None,
            None,
        );

        assert!(
            result.inline_media.is_empty(),
            "a diagram is never an inline image",
        );
        assert_eq!(
            result.diagram_affordances.len(),
            1,
            "a ready diagram emits one affordance placement",
        );
        let aff = &result.diagram_affordances[0];
        assert_eq!(aff.source, "A-->B\n");

        // Exact placement geometry: one row tall, anchored at the content-area x,
        // a non-empty width that stays within the content column band.
        let hlayout = HorizontalLayout::new(viewport, &appearance.scrollback.layout);
        assert_eq!(aff.screen_rect.height, 1, "the affordance row is one row");
        assert_eq!(
            aff.screen_rect.x, hlayout.content.x,
            "anchored at the content-area x",
        );
        assert!(aff.screen_rect.width > 0, "non-degenerate width");
        assert!(
            aff.screen_rect.x + aff.screen_rect.width <= hlayout.content.x + hlayout.content.width,
            "row stays within the content column band",
        );
        assert!(
            aff.screen_rect.y >= viewport.y && aff.screen_rect.y < viewport.y + viewport.height,
            "row is on-screen",
        );

        // The reserved affordance row renders blank here (the draw loop paints the
        // buttons), and the diagram source sits on a row above it.
        assert!(
            buffer_row_text(&buf, aff.screen_rect.y).trim().is_empty(),
            "the placement points at the reserved blank row",
        );
        assert!(
            (viewport.y..aff.screen_rect.y).any(|y| buffer_row_text(&buf, y).contains("A-->B")),
            "the diagram source renders above the affordance row",
        );
    }

    fn make_test_png(width: u32, height: u32) -> Vec<u8> {
        use image::{ImageBuffer, Rgba};
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(width, height, Rgba([128, 64, 32, 255]));
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    /// A tool-media overlay/image placement exposes the click-to-copy filepath as
    /// its `filepath_screen_rect`, anchored on the block's second output row (the
    /// filepath line) and sharing the image's content-column origin. This is the
    /// assertion that the removed `has_filepath_line` flag previously carried.
    #[test]
    fn tool_media_overlay_exposes_filepath_click_rect() {
        use crate::scrollback::blocks::tool::{OtherToolCallBlock, ToolCallBlock};
        use crate::terminal::image::{GraphicsProtocol, set_protocol_for_test};

        // Kitty overlay active → the block hosts its image overlay (no text
        // `[Open]` line), so the sole placement is the overlay one whose second
        // output line is the click-to-copy filepath.
        let _guard = set_protocol_for_test(GraphicsProtocol::Kitty);

        let dir = tempfile::tempdir().unwrap();
        let image_path = dir.path().join("referenced.png");
        std::fs::write(&image_path, make_test_png(120, 120)).unwrap();

        let entry = ScrollbackEntry::new(RenderBlock::ToolCall(ToolCallBlock::Other(
            OtherToolCallBlock::new("image_gen", "saved image").with_media_ref(&image_path, false),
        )));
        let viewport = Rect::new(0, 0, 80, 30);
        let result = render_with_scratch(std::slice::from_ref(&entry), viewport, 0, None);

        // Exactly the overlay/image placement (button row reserved); no separate
        // text-`[Open]` placement when the overlay hosts the buttons.
        assert_eq!(result.inline_media.len(), 1, "one overlay placement");
        let media = &result.inline_media[0];
        assert!(media.has_button_row, "overlay/image tool-media placement");

        // The tool block has no vpad, so its second output line (the filepath) is
        // screen row 1: a one-row click-to-copy target at the image's x.
        let rect = media
            .filepath_screen_rect
            .expect("tool media exposes the click-to-copy filepath rect");
        assert_eq!(
            rect.y, 1,
            "filepath rect on the block's second (filepath) line"
        );
        assert_eq!(rect.height, 1);
        assert!(rect.width > 0, "non-degenerate width");
        assert_eq!(
            rect.x, media.screen_rect.x,
            "shares the image's content-column x"
        );
        assert!(
            media.screen_rect.y > rect.y,
            "the image sits below its filepath line",
        );
    }
}
