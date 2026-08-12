//! Layout cache and lazy viewport measurement for [`ScrollbackState`].

use super::verb_group::{RunStep, run_step, scan_run_forward};
use super::*;

/// A width-stable anchor for the content at the viewport top, captured before a
/// width rebuild so the same content can be re-pinned afterward.
///
/// The position is stored as `(entry, logical_line, sub_rows)` rather than an
/// absolute wrapped-row count: a row count is meaningless after re-wrapping (the
/// whole transcript can be one giant entry), but the logical (newline-delimited)
/// line it sits on is width-independent. `sub_rows` is the signed wrapped-row
/// offset from that logical line's start (covers vpad / mid-paragraph anchors;
/// zero for the common non-wrapping top line). `sub_rows` is exact only for a
/// non-wrapping anchor line; if the anchor line itself re-wraps, restore clamps
/// the offset within the re-resolved line so the top can drift by at most that
/// one line's wrap delta and never spills into the next logical line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrollAnchor {
    entry_idx: usize,
    logical_line: usize,
    sub_rows: i64,
}

/// One-shot anchor for the content at the top of a manually scrolled
/// viewport, armed by a structural entry mutation (removal, insertion)
/// immediately BEFORE it invalidates the layout cache — the last moment
/// entry indices and the cache still agree — and consumed by the very next
/// `prepare_layout`. Unlike [`ScrollAnchor`] it is keyed by stable
/// [`EntryId`], because the arming mutation is exactly what shifts indices;
/// its raw row offset is only meaningful at an unchanged width, so width
/// changes re-anchor via [`ScrollAnchor`]'s logical-line mapping instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StructuralScrollAnchor {
    /// Entry at the viewport top when the arming mutation happened.
    id: EntryId,
    /// Wrapped rows from that entry's top down to the viewport top, measured
    /// over the entry's full layout span — content rows plus trailing gap,
    /// matching `entry_at_virtual_row`'s attribution of gap rows to the entry
    /// above, so a top parked on a gap row stays a gap row.
    rows_into_span: usize,
    /// `scroll_offset` at arm time. A mismatch at consume time means explicit
    /// navigation moved the viewport between the mutation and its frame; the
    /// anchor is stale and must not override that.
    armed_scroll_offset: usize,
}

/// Cached layout data for efficient navigation and rendering.
///
/// This is rebuilt when entries change or viewport width changes.
/// It provides O(1) lookup for sticky header heights needed by navigation.
#[derive(Debug, Clone, Default)]
pub(super) struct LayoutCache {
    /// Per-entry layout info (height + gap_after).
    pub(super) entries: Vec<EntryLayoutInfo>,
    /// Truncated height of each entry (for sticky header min_height).
    /// Separate from EntryLayoutInfo because it's only needed during
    /// PromptDescriptor building, not during rendering.
    pub(super) entry_truncated_heights: Vec<u16>,
    /// Whether each entry's cached `height`/`truncated_height` is an EXACT
    /// measurement (`true`) or a cheap estimate (`false`).
    ///
    /// On a bulk load every entry starts estimated; entries are measured
    /// exactly only when they enter (or are near) the viewport — see
    /// `settle_visible_measurements`. Parallel to `entries`.
    pub(super) measured: Vec<bool>,
    /// Virtual Y position of each entry (cumulative heights + gaps).
    pub(super) virtual_y: Vec<usize>,
    /// Prompt descriptors for sticky layout computation.
    pub(super) prompt_descriptors: Vec<PromptDescriptor>,
    /// Group spans computed by the last fold pass — the authoritative model
    /// the per-entry flags in `entries` are projected from (see
    /// `state::groups`). Stale between an incremental append and the next
    /// structural rebuild, exactly like those flags.
    pub(super) groups: Vec<groups::GroupSpan>,
    /// Width used to compute this cache.
    pub(super) width: u16,
}

impl LayoutCache {
    pub fn take(mut self) -> Self {
        // Used primarily to avoid reallocs when updating
        self.entries.clear();
        self.entry_truncated_heights.clear();
        self.measured.clear();
        self.virtual_y.clear();
        self.prompt_descriptors.clear();
        self.groups.clear();
        self.width = 0;
        self
    }

    /// Binary-search `virtual_y` to find the entry that contains `content_y`.
    ///
    /// `content_y` is an absolute position in the virtual content space.
    /// `valid_range` restricts the search to a subset of entries (e.g. visible range).
    ///
    /// Returns `Some(index)` if the position falls within an entry's area,
    /// or `None` if it falls in a gap between entries or outside valid range.
    fn entry_at_content_y(&self, content_y: usize, valid_range: Range<usize>) -> Option<usize> {
        if valid_range.is_empty() || self.virtual_y.is_empty() {
            return None;
        }

        let slice = &self.virtual_y[valid_range.clone()];

        // partition_point returns the first index where virtual_y > content_y,
        // so the entry we want is the one before that.
        let pos = slice.partition_point(|&y| y <= content_y);
        if pos == 0 {
            return None;
        }

        let idx = valid_range.start + pos - 1;
        let entry_start = self.virtual_y[idx];
        let entry_end = entry_start + self.entries[idx].height as usize;

        if content_y < entry_end {
            Some(idx)
        } else {
            // In the gap after this entry
            None
        }
    }
}

impl ScrollbackState {
    /// Invalidate and rebuild the layout cache from scratch.
    ///
    /// `ensure_layout_cache` skips work when the width and entry count are
    /// unchanged, so an in-place display-mode or group change must null the
    /// cache first for the next read to see fresh heights, gaps, and totals.
    pub(super) fn rebuild_layout(&mut self) {
        #[cfg(test)]
        {
            self.layout_rebuilds += 1;
        }
        self.gaps_may_be_dirty = true;
        self.layout_cache = None;
        if self.last_width > 0 {
            self.ensure_layout_cache(self.last_width);
            self.compute_total_height_from_cache();
        }
    }

    // Layout Cache Accessors
    //
    // These methods provide read-only access to cached layout data.
    // The cache is populated by prepare_layout() and should be valid during render.
    // Names use "cached" prefix to make it clear these are O(1) lookups, not computations.

    /// Get cached height for a single entry.
    ///
    /// Returns None if cache is invalid or index out of bounds.
    /// Call prepare_layout() before render to ensure cache is valid.
    pub fn get_cached_entry_height(&self, idx: usize) -> Option<u16> {
        self.layout_cache
            .as_ref()
            .and_then(|c| c.entries.get(idx).map(|e| e.height))
    }

    /// Group spans computed by the last fold pass — the model behind every
    /// group header and hidden row (see [`groups::GroupSpan`]). Empty when
    /// the layout cache is invalid; call `prepare_layout()` first.
    pub fn group_spans(&self) -> &[groups::GroupSpan] {
        self.layout_cache
            .as_ref()
            .map_or(&[], |c| c.groups.as_slice())
    }

    /// The group span containing the entry at `idx`, if the last fold pass
    /// folded it (header, hidden member, or visible tail of a truncation
    /// run). Same freshness contract as [`Self::group_spans`].
    pub fn span_at(&self, idx: usize) -> Option<&groups::GroupSpan> {
        groups::span_containing(self.group_spans(), idx)
    }

    /// Check if an entry is hidden by group truncation (height=0 in cache).
    ///
    /// Returns false if the cache is missing or the index is out of bounds
    /// (conservative: treat uncached entries as visible).
    pub(super) fn is_entry_hidden(&self, idx: usize) -> bool {
        self.layout_cache
            .as_ref()
            .and_then(|c| c.entries.get(idx))
            .is_some_and(|e| e.height == 0)
    }

    /// If the current selection is on a hidden entry (height=0), move it to
    /// the nearest visible entry. Prefers the group header (the first entry
    /// of the truncated run, which has height=1) since it's the expand affordance.
    pub(super) fn fixup_hidden_selection(&mut self) {
        let Some(sel) = self.selected else { return };
        if !self.is_entry_hidden(sel) {
            return;
        }
        // Walk backward to find the group header (group_header_count > 0)
        for idx in (0..sel).rev() {
            if let Some(ref cache) = self.layout_cache
                && let Some(info) = cache.entries.get(idx)
                && info.height > 0
            {
                self.selected = Some(idx);
                return;
            }
        }
        // Fallback: walk forward
        let n = self.entries.len();
        for idx in (sel + 1)..n {
            if let Some(ref cache) = self.layout_cache
                && let Some(info) = cache.entries.get(idx)
                && info.height > 0
            {
                self.selected = Some(idx);
                return;
            }
        }
    }

    /// Get all cached entry layout info (height + gap_after per entry).
    ///
    /// Returns None if cache is invalid.
    pub fn get_cached_entry_layouts(&self) -> Option<&[EntryLayoutInfo]> {
        self.layout_cache.as_ref().map(|c| c.entries.as_slice())
    }

    /// Get cached virtual Y positions for all entries.
    ///
    /// Each entry's virtual Y is its cumulative position in the scrollable content.
    /// Returns None if cache is invalid.
    pub fn get_cached_virtual_y(&self) -> Option<&[usize]> {
        self.layout_cache.as_ref().map(|c| c.virtual_y.as_slice())
    }

    /// Get cached prompt descriptors for sticky header layout.
    ///
    /// Returns None if cache is invalid.
    pub fn get_cached_prompt_descriptors(&self) -> Option<&[PromptDescriptor]> {
        self.layout_cache
            .as_ref()
            .map(|c| c.prompt_descriptors.as_slice())
    }

    /// Get cached truncated height for a single entry.
    ///
    /// Truncated height is the height when displayed in Truncated mode,
    /// used for sticky header min_height calculations.
    pub fn get_cached_truncated_height(&self, idx: usize) -> Option<u16> {
        self.layout_cache
            .as_ref()
            .and_then(|c| c.entry_truncated_heights.get(idx).copied())
    }

    /// Get entries in range as a Vec.
    /// Note: With IndexMap, we can't return a slice directly, so we collect references.
    pub fn entries_in_range(&self, range: Range<usize>) -> Vec<&ScrollbackEntry> {
        range
            .filter_map(|i| self.entries.get_index(i).map(|(_, v)| v))
            .collect()
    }

    /// Get entries in range (deprecated, use entries_in_range instead).
    /// This allocates a Vec to maintain compatibility.
    #[deprecated(note = "Use entries_in_range() instead")]
    pub fn entries_slice(&self, range: Range<usize>) -> Vec<&ScrollbackEntry> {
        self.entries_in_range(range)
    }

    /// Map a screen row to an entry index.
    ///
    /// Given a screen Y coordinate and the scrollback area rect, determines
    /// which entry (if any) is at that position. This covers both:
    /// - Content entries rendered in the scrollable area below the header
    /// - Prompt entries rendered as sticky headers (pinned or disappearing)
    ///
    /// Returns `None` if the row falls on a gap between entries, on the
    /// header/content separator gap, or outside the scrollback area entirely.
    ///
    /// Requires `prepare_layout()` to have been called (layout cache must be valid).
    pub fn entry_index_at_screen_row(
        &self,
        screen_row: u16,
        scrollback_area: Rect,
    ) -> Option<usize> {
        if screen_row < scrollback_area.y
            || screen_row >= scrollback_area.y + scrollback_area.height
        {
            return None;
        }

        let cache = self.layout_cache.as_ref()?;
        let visible_range = self.visible_entry_range();
        if visible_range.is_empty() {
            return None;
        }

        let sticky = self.current_sticky_layout(cache, &visible_range);
        let row_in_area = screen_row - scrollback_area.y;
        let header_rows = sticky.header_screen_rows();

        if row_in_area < header_rows {
            // In the header zone — check if we hit a pushed or pinned prompt
            return sticky.entry_at_header_row(row_in_area);
        }

        // Convert screen row to absolute content-space Y
        let base_y = cache.virtual_y[visible_range.start];
        let content_y = base_y + (screen_row - scrollback_area.y) as usize + self.scroll_offset;

        cache.entry_at_content_y(content_y, visible_range)
    }

    /// Compute the screen area for an entry at the given index.
    ///
    /// Returns `(area, top_clipped, bottom_clipped)` where `area` is the
    /// visible portion of the entry's selection box area on screen.
    ///
    /// Handles both content entries (below the header) and prompt entries
    /// rendered as sticky headers (pushed/pinned). For header prompts,
    /// returns the header area; for content, clips to exclude the header zone.
    ///
    /// Returns `None` if the entry is not visible.
    ///
    /// Requires `prepare_layout()` to have been called.
    pub fn entry_screen_area(
        &self,
        entry_idx: usize,
        scrollback_area: Rect,
    ) -> Option<(Rect, bool, bool)> {
        let cache = self.layout_cache.as_ref()?;
        let visible_range = self.visible_entry_range();
        if !visible_range.contains(&entry_idx) {
            return None;
        }

        let layout = HorizontalLayout::new(scrollback_area, &self.appearance.scrollback.layout);
        let sel = layout.selection_area();

        // Check if entry is rendered as a sticky header prompt
        let sticky = self.current_sticky_layout(cache, &visible_range);
        if let Some((header_y, header_h, is_pushed)) = sticky.header_entry_area(entry_idx) {
            // Pushed prompts have their top clipped (disappearing upward)
            let top_clipped = is_pushed && sticky.pushed.is_some_and(|p| p.clip_top > 0);
            return Some((
                Rect {
                    x: sel.x,
                    y: scrollback_area.y + header_y,
                    width: sel.width,
                    height: header_h,
                },
                top_clipped,
                false, // header prompts are never bottom-clipped
            ));
        }

        // Content entry: compute from virtual_y coordinates. Keep the cumulative
        // positions in usize (tall sessions exceed u16::MAX); the final
        // screen y / height are viewport-relative and provably fit in u16.
        let base_y = cache.virtual_y[visible_range.start];
        let entry_start = cache.virtual_y[entry_idx] - base_y;
        let entry_height = cache.entries[entry_idx].height;
        let entry_end = entry_start + entry_height as usize;

        // Check if entry is within viewport
        let vp_start = self.scroll_offset;
        let vp_end = self.scroll_offset + self.viewport_height as usize;
        if entry_end <= vp_start || entry_start >= vp_end {
            return None;
        }

        let top_clipped = entry_start < vp_start;
        let bottom_clipped = entry_end > vp_end;

        // Screen coordinates: viewport-relative deltas always fit in u16.
        let mut screen_y = if top_clipped {
            scrollback_area.y
        } else {
            scrollback_area.y + (entry_start - vp_start) as u16
        };

        let mut visible_height = if top_clipped && bottom_clipped {
            self.viewport_height
        } else if top_clipped {
            (entry_end - vp_start) as u16
        } else if bottom_clipped {
            (vp_end - entry_start) as u16
        } else {
            entry_height
        };

        // Clip to below the sticky header
        let header_rows = sticky.header_screen_rows();
        let content_top = scrollback_area.y + header_rows;
        if screen_y + visible_height <= content_top {
            return None; // Entirely behind header
        }
        let mut top_clipped = top_clipped;
        if screen_y < content_top {
            let clip = content_top - screen_y;
            visible_height = visible_height.saturating_sub(clip);
            screen_y = content_top;
            top_clipped = true;
        }
        if visible_height == 0 {
            return None;
        }

        Some((
            Rect {
                x: sel.x,
                y: screen_y,
                width: sel.width,
                height: visible_height,
            },
            top_clipped,
            bottom_clipped,
        ))
    }

    // Lazy viewport height measurement (see `rebuild_layout_cache` for the
    // estimate side; these upgrade the on/near-screen entries to exact heights).

    /// Virtual-space `(top, bottom)` of the viewport (relative to entry 0):
    /// `top` is the current scroll position, `bottom` is one past the last
    /// visible row. `None` when the cache is absent, the visible range is empty,
    /// or the cache is stale (range start past `virtual_y`).
    pub(super) fn viewport_virtual_bounds(&self) -> Option<(usize, usize)> {
        let cache = self.layout_cache.as_ref()?;
        let range = self.visible_entry_range();
        if range.is_empty() {
            return None;
        }
        let base_y = cache.virtual_y.get(range.start).copied()?;
        let top = base_y + self.scroll_offset;
        let bottom = top + self.viewport_height as usize;
        Some((top, bottom))
    }

    /// Maximum valid `scroll_offset`: content height that doesn't fit the
    /// viewport. `scroll_offset` is always clamped to `[0, max_scroll_offset()]`.
    pub(super) fn max_scroll_offset(&self) -> usize {
        self.total_height
            .saturating_sub(self.viewport_height as usize)
    }

    /// Capture a width-stable [`ScrollAnchor`] for the content at the viewport
    /// top, from the CURRENT layout cache.
    ///
    /// Captured before a width rebuild — which re-wraps every entry, so the
    /// absolute wrapped-row `scroll_offset` points at different content
    /// afterward — so the same content can be re-pinned to the viewport top via
    /// [`restore_scroll_anchor`]. The display-row offset into the top entry is
    /// converted to a logical line + signed sub-row offset, which survives the
    /// entry's own re-wrapping (the whole transcript can be one giant entry).
    pub(super) fn capture_scroll_anchor(&self) -> Option<ScrollAnchor> {
        // `entry_at_virtual_row` resolves the viewport-top entry deterministically
        // — including a gap row, which it attributes to the entry above (no
        // special case, unlike `entry_at_content_y` which returns None in a gap).
        let (top_content_y, _) = self.viewport_virtual_bounds()?;
        let entry_idx = self.entry_at_virtual_row(top_content_y)?;
        let cache = self.layout_cache.as_ref()?;
        let entry_y = *cache.virtual_y.get(entry_idx)?;
        let rows_into_entry = top_content_y.saturating_sub(entry_y);

        // Convert the display-row offset within the entry to a logical line +
        // signed sub-row offset, both resolved at the cache's (old) width.
        let area_width = self.entry_area_width(cache.width);
        let (_, entry) = self.entries.get_index(entry_idx)?;
        let theme = Theme::current();
        let renderer = EntryRenderer::new(entry, &theme)
            .with_appearance_ref(&self.appearance)
            .with_cwd(self.cwd());
        let rows = u16::try_from(rows_into_entry).unwrap_or(u16::MAX);
        let logical_line = renderer.logical_line_of_rendered_row(area_width, rows);
        let line_start = renderer.rendered_row_of_logical_line(area_width, logical_line);
        let sub_rows = rows as i64 - line_start as i64;
        Some(ScrollAnchor {
            entry_idx,
            logical_line,
            sub_rows,
        })
    }

    /// Re-derive `scroll_offset` so the content captured by
    /// [`capture_scroll_anchor`] sits at the viewport top again. Call after the
    /// layout cache and `total_height` are rebuilt at the new width, before
    /// `settle` re-pins.
    pub(super) fn restore_scroll_anchor(&mut self, anchor: ScrollAnchor) {
        let ScrollAnchor {
            entry_idx,
            logical_line,
            sub_rows,
        } = anchor;
        // Re-resolve the logical line's start row at the NEW width — this is what
        // makes a re-wrapping top entry re-anchor correctly: the wrapped rows
        // above the anchor line grow/shrink, and the rebuilt start rows account
        // for that. `sub_rows` is width-stable for a non-wrapping line.
        let Some(width) = self.layout_cache.as_ref().map(|c| c.width) else {
            return;
        };
        let area_width = self.entry_area_width(width);
        let (new_line_start, line_last_row) = {
            let Some((_, entry)) = self.entries.get_index(entry_idx) else {
                return;
            };
            let theme = Theme::current();
            let renderer = EntryRenderer::new(entry, &theme)
                .with_appearance_ref(&self.appearance)
                .with_cwd(self.cwd());
            let (starts, last_content_row) = renderer.logical_line_start_rows(area_width);
            let new_line_start = starts
                .get(logical_line)
                .copied()
                .unwrap_or(last_content_row);
            // Last rendered row of the anchor line: one before the NEXT line's
            // start, or the entry's last row when the anchor is the final line.
            let line_last_row = starts
                .get(logical_line + 1)
                .map(|&next| next.saturating_sub(1))
                .unwrap_or(last_content_row);
            (new_line_start, line_last_row.max(new_line_start))
        };
        // Clamp the intra-line offset to the anchor line's wrapped extent at the
        // new width so a re-wrapped (now shorter) anchor line can't push the
        // viewport top past itself into the next logical line.
        let new_rows_into_entry =
            (new_line_start as i64 + sub_rows).clamp(0, line_last_row as i64) as usize;

        let Some(cache) = self.layout_cache.as_ref() else {
            return;
        };
        let range = self.visible_entry_range();
        let (Some(&base_y), Some(&entry_y)) = (
            cache.virtual_y.get(range.start),
            cache.virtual_y.get(entry_idx),
        ) else {
            return;
        };
        let new_top_content_y = entry_y + new_rows_into_entry;
        self.scroll_offset = new_top_content_y
            .saturating_sub(base_y)
            .min(self.max_scroll_offset());
    }

    /// Arm a one-shot [`StructuralScrollAnchor`] for the current viewport top.
    /// Call BEFORE mutating the entries map. The first mutation before a
    /// frame wins — it saw the on-screen geometry; later mutations have no
    /// cache to anchor against (removals keep the armed anchor honest via
    /// [`Self::migrate_structural_anchor_past_removal`]).
    pub(super) fn arm_structural_scroll_anchor(&mut self) {
        if self.structural_scroll_anchor.is_some() {
            return;
        }
        let Some((entry_idx, rows_into_span)) = self.viewport_top_anchor_point() else {
            return;
        };
        let Some((id, _)) = self.entries.get_index(entry_idx) else {
            return;
        };
        self.structural_scroll_anchor = Some(StructuralScrollAnchor {
            id: *id,
            rows_into_span,
            armed_scroll_offset: self.scroll_offset,
        });
    }

    /// Keep an armed anchor meaningful across the removal that follows it
    /// (and any further removals before the next frame): when the anchored
    /// entry itself was just removed, re-point at the first LATER survivor —
    /// the entry that shifted into `removed_index` — pinned to the vacated
    /// viewport-top row. With nothing surviving below the removal, the anchor
    /// is dropped and the plain max-offset clamp takes over.
    pub(super) fn migrate_structural_anchor_past_removal(
        &mut self,
        removed_id: EntryId,
        removed_index: usize,
    ) {
        let Some(anchor) = self.structural_scroll_anchor else {
            return;
        };
        if anchor.id != removed_id {
            return;
        }
        let survivor = self.entries.get_index(removed_index).map(|(id, _)| *id);
        self.structural_scroll_anchor = survivor.map(|id| StructuralScrollAnchor {
            id,
            rows_into_span: 0,
            armed_scroll_offset: anchor.armed_scroll_offset,
        });
    }

    /// Drop an armed anchor whose entry no longer exists. For bulk tail
    /// removal (`remove_from`), where everything at and after the anchor can
    /// vanish at once with no later survivor to migrate to.
    pub(super) fn prune_dead_structural_anchor(&mut self) {
        if let Some(anchor) = self.structural_scroll_anchor
            && self.index_of_id(anchor.id).is_none()
        {
            self.structural_scroll_anchor = None;
        }
    }

    /// Apply a taken [`StructuralScrollAnchor`] after the same-width full
    /// rebuild that the arming mutation forced, re-pinning the pre-mutation
    /// viewport-top content. Skipped when follow took over or when explicit
    /// navigation moved the viewport since arming (`armed_scroll_offset`
    /// mismatch — user intent wins).
    pub(super) fn apply_structural_scroll_anchor(
        &mut self,
        anchor: Option<StructuralScrollAnchor>,
        width: u16,
    ) {
        let Some(anchor) = anchor else {
            return;
        };
        if self.follow_mode || self.scroll_offset != anchor.armed_scroll_offset {
            return;
        }
        let Some(entry_idx) = self.index_of_id(anchor.id) else {
            return;
        };
        // The rebuild reset every height to a cheap ESTIMATE, while
        // `rows_into_span` was measured against the entry's EXACT pre-mutation
        // layout. Measure the anchor entry exactly first, or the span clamp in
        // the re-pin would squeeze an exact row against a transient
        // under-estimate and jump within an entry whose content never changed.
        self.measure_span_and_rebuild(entry_idx, entry_idx, width);
        self.repin_viewport_top_to_entry(entry_idx, anchor.rows_into_span);
    }

    /// Identity of the viewport-top row as `(entry_idx, rows_into_span)`: the
    /// entry owning the top row (gap rows attribute to the entry above, per
    /// `entry_at_virtual_row`) and the row offset from that entry's top over
    /// its full layout span — content rows plus trailing gap — so a top parked
    /// on a gap row round-trips as that same gap row. `None` when there is
    /// nothing to anchor: following (the bottom re-pins itself each frame), an
    /// unscrolled top, or no layout.
    pub(super) fn viewport_top_anchor_point(&self) -> Option<(usize, usize)> {
        if self.follow_mode || self.scroll_offset == 0 {
            return None;
        }
        let (top, _) = self.viewport_virtual_bounds()?;
        let entry_idx = self.entry_at_virtual_row(top)?;
        let entry_y = *self.layout_cache.as_ref()?.virtual_y.get(entry_idx)?;
        Some((entry_idx, top.saturating_sub(entry_y)))
    }

    /// Re-derive `scroll_offset` so the row `rows_into_span` below entry
    /// `entry_idx`'s top sits at the viewport top again, after `virtual_y`
    /// changed at an unchanged width. A change that only touched geometry at
    /// or below the viewport top re-derives the exact same offset, so
    /// below-viewport mutations never move the viewport.
    ///
    /// The row offset is clamped within the entry's CURRENT layout span
    /// (content rows plus trailing gap — a gap-row park is never squeezed onto
    /// a content row) so a genuinely shrunken anchor entry cannot spill the
    /// top into unrelated content, and to `max_scroll_offset`.
    pub(super) fn repin_viewport_top_to_entry(&mut self, entry_idx: usize, rows_into_span: usize) {
        let Some(cache) = self.layout_cache.as_ref() else {
            return;
        };
        let range = self.visible_entry_range();
        if !range.contains(&entry_idx) {
            return;
        }
        let (Some(&base_y), Some(&entry_y)) = (
            cache.virtual_y.get(range.start),
            cache.virtual_y.get(entry_idx),
        ) else {
            return;
        };
        let span_rows = cache
            .entries
            .get(entry_idx)
            .map_or(0, |e| e.height as usize + e.gap_after as usize);
        let rows_into_span = rows_into_span.min(span_rows.saturating_sub(1));
        self.scroll_offset = (entry_y + rows_into_span)
            .saturating_sub(base_y)
            .min(self.max_scroll_offset());
    }

    /// Index range `[start, end]` of entries to measure exactly for the current
    /// viewport: every on-screen entry plus a small below-margin.
    ///
    /// There is deliberately NO above-margin: entries above the first visible
    /// one stay estimated, so their cumulative offset — and therefore the
    /// on-screen position of the first visible entry — does not shift when we
    /// measure. That keeps the top anchored on manual scroll-up.
    fn measurement_window(&self) -> Option<(usize, usize)> {
        // `start` is the first visible entry (shared, canonical predicate).
        let start = self.first_visible_entry()?;
        let (_, bottom) = self.viewport_virtual_bounds()?;
        let cache = self.layout_cache.as_ref()?;
        let range = self.visible_entry_range();
        let vy = cache.virtual_y.get(range.clone())?;

        // Last entry whose start is before the viewport bottom, plus a small
        // below-margin (no above-margin — see the doc comment above).
        let last_rel = vy.partition_point(|&y| y < bottom).saturating_sub(1);
        let last_visible = (range.start + last_rel).max(start);
        let end = (last_visible + MEASURE_MARGIN_ENTRIES).min(range.end - 1);
        Some((start, end))
    }

    /// Whether the entry at `idx` falls inside the current viewport window
    /// (visible rows plus the small below-margin from `measurement_window`).
    ///
    /// Conservative: with no layout yet (before the first draw / after an
    /// invalidation), every entry counts as visible so animation gating never
    /// starves a redraw it can't reason about.
    pub(super) fn entry_index_in_viewport(&self, idx: usize) -> bool {
        // A wedged offset (viewport top at/past the end of the content, e.g.
        // after a shrink under a follow pin) yields a degenerate window that
        // contains no entry at all; treat it like an absent layout so
        // animation gating can't mute the redraws that heal the state. (A
        // legit page-flip pin keeps `scroll_offset < total_height` — see the
        // re-clamp in `follow_scroll_to_bottom` — so it never hits this arm.)
        if self.scroll_offset >= self.total_height {
            return true;
        }
        match self.measurement_window() {
            Some((start, end)) => idx >= start && idx <= end,
            None => true,
        }
    }

    /// Evict heavyweight render caches from entries far outside the viewport.
    ///
    /// Long sessions pin a fully styled+wrapped copy of every entry ever
    /// rendered (`cached_output`, plus the markdown wrap cache inside the
    /// block) — for a multi-MB transcript that is easily hundreds of MB that
    /// can never be seen without scrolling. This sweeps everything outside
    /// the measurement window padded by [`EVICT_KEEP_MARGIN_ENTRIES`] on both
    /// sides. Heights are cached separately (`cached_truncated_height` /
    /// `cached_estimate_lines` / the layout cache) and are deliberately kept,
    /// so scroll geometry is unaffected; a swept entry re-renders
    /// transparently when it scrolls back into the window.
    ///
    /// The selected entry is skipped (its output can be consulted off-screen
    /// for copy/selection). Returns the number of entries whose cached output
    /// was dropped.
    pub(crate) fn evict_offscreen_render_caches(&self) -> usize {
        let Some((win_start, win_end)) = self.measurement_window() else {
            // No layout (nothing rendered yet) — nothing worth sweeping.
            return 0;
        };
        let keep_start = win_start.saturating_sub(EVICT_KEEP_MARGIN_ENTRIES);
        let keep_end = win_end.saturating_add(EVICT_KEEP_MARGIN_ENTRIES);
        let mut evicted = 0usize;
        for (idx, (_, entry)) in self.entries.iter().enumerate() {
            if idx >= keep_start && idx <= keep_end {
                continue;
            }
            if self.selected == Some(idx) {
                continue;
            }
            if entry.evict_render_cache() {
                evicted += 1;
            }
        }
        evicted
    }

    /// Full entry render-area width (accent + padding + content) for a viewport
    /// of `width` — i.e. the width handed to `EntryRenderer`, which subtracts
    /// chrome itself to reach the content width. Centralizes the layout
    /// round-trip so the reveal row mapping, exact height measurement, and
    /// prompt-descriptor layout can't drift apart.
    pub(super) fn entry_area_width(&self, width: u16) -> u16 {
        let simulated_area = Rect::new(0, 0, width, 1);
        HorizontalLayout::new(simulated_area, &self.appearance.scrollback.layout)
            .entry_content_area()
            .width
    }

    /// Content-column width (excluding accent bar and block padding) for a
    /// full scrollback width. Used to size the inline edit textarea.
    pub fn entry_text_column_width(&self, width: u16) -> u16 {
        let simulated_area = Rect::new(0, 0, width, 1);
        HorizontalLayout::new(simulated_area, &self.appearance.scrollback.layout).content_width()
    }

    /// Measure exact heights for not-yet-measured entries in `[start, end]`.
    ///
    /// Returns `true` if any entry was newly measured (i.e. an estimate was
    /// replaced by an exact height). Hidden (group-truncated, height 0) and
    /// synthetic group-header rows render no markdown — their height is owned by
    /// group truncation, not measurement — so they are skipped.
    fn measure_window_exact(&mut self, width: u16, start: usize, end: usize) -> bool {
        // Cheap pre-scan: bail before building a Theme + layout when every in-window
        // entry is already measured or is a non-rendered (hidden / group-header) row.
        {
            let Some(cache) = self.layout_cache.as_ref() else {
                return false;
            };
            let needs_measure = (start..=end).any(|idx| {
                cache.entries.get(idx).is_some_and(|info| {
                    !cache.measured[idx]
                        && info.height != 0
                        && (!info.is_group_header() || info.is_expanded_verb_header())
                })
            });
            if !needs_measure {
                return false;
            }
        }

        let theme = Theme::current();
        let entry_area_width = self.entry_area_width(width);
        let cwd = self.cwd.as_deref();
        let inline_edit_height = self.inline_edit_height;

        let Some(cache) = self.layout_cache.as_mut() else {
            return false;
        };

        let mut measured_any = false;
        for idx in start..=end {
            if idx >= cache.entries.len() {
                break;
            }
            if cache.measured[idx] {
                continue;
            }
            let info = cache.entries[idx];
            // Estimated entries always have height >= 1, so a height of 0 here
            // means group truncation hid this entry. Synthetic-only headers need
            // no block render; an expanded verb header also owns member 0 rows.
            if info.height == 0 || (info.is_group_header() && !info.is_expanded_verb_header()) {
                continue;
            }
            let Some((entry_id, entry)) = self.entries.get_index(idx) else {
                continue;
            };
            let renderer = EntryRenderer::new(entry, &theme)
                .with_appearance_ref(&self.appearance)
                .with_cwd(cwd);
            let member_height = match inline_edit_height {
                Some((edit_id, h)) if edit_id == *entry_id => h,
                _ => renderer.desired_height(entry_area_width),
            };
            cache.entries[idx].height = info.with_verb_header_row(member_height);
            // Truncated height only feeds prompt sticky-header min_height, so
            // only prompts pay for the extra Truncated-mode render; others keep
            // their seeded value (unused for non-prompts).
            if entry.block.is_user_prompt() {
                cache.entry_truncated_heights[idx] =
                    renderer.compute_truncated_height(entry_area_width);
            }
            cache.measured[idx] = true;
            measured_any = true;
        }
        measured_any
    }

    /// Upgrade the on-screen entries from estimated to exact heights and re-anchor
    /// the viewport so what the user is looking at stays put.
    ///
    /// Iterates because an exact height shifts later entries, which can reveal a
    /// new entry at the bottom edge. `measured` grows monotonically so it
    /// terminates; the loop bound is a defensive cap.
    pub(super) fn settle_visible_measurements(&mut self, width: u16) {
        if self.viewport_height == 0 || self.last_width == 0 {
            return;
        }
        let max_iters = self.entries.len().saturating_add(2);
        for _ in 0..max_iters {
            let Some((start, end)) = self.measurement_window() else {
                return;
            };
            if !self.measure_window_exact(width, start, end) {
                // Everything visible is exact: render will match the layout.
                return;
            }
            // Estimates became exact — rebuild offsets (cheap arithmetic, no
            // markdown) and re-pin the viewport.
            self.rebuild_virtual_y_from_heights();
            self.compute_total_height_from_cache();
            if self.follow_mode {
                // Bottom-anchored: re-pin to the (now exact) bottom.
                self.follow_scroll_to_bottom();
            } else {
                // Top-anchored: the first visible entry's offset is unchanged
                // (nothing above it was measured), so scroll stays put. Only
                // clamp if the content shrank past the end.
                let max_offset = self
                    .total_height
                    .saturating_sub(self.viewport_height as usize);
                if self.scroll_offset > max_offset {
                    self.scroll_offset = max_offset;
                }
            }
        }
    }

    /// One-shot warm-up after a bottom-pinned full rebuild (resume): measure the
    /// `RESUME_WARM_PAGES` pages of entries directly above the viewport so an
    /// immediate scroll-up reveals already-exact heights instead of triggering an
    /// estimate->exact rebuild (which could jump).
    ///
    /// Only safe while the viewport is pinned to the BOTTOM: measuring above
    /// shifts every offset uniformly, which the following re-pin cancels. Skipped
    /// in `follow_preserve_scroll` (a prompt pinned at the TOP —
    /// `follow_scroll_to_bottom` keeps it put, so the shift would move it down: a
    /// jump) and outside `follow_mode` (a manual top-anchored scroll position).
    pub(super) fn warm_measure_pages_above(&mut self, width: u16) {
        if !self.follow_mode
            || self.follow_preserve_scroll
            || self.viewport_height == 0
            || width == 0
        {
            return;
        }
        let Some((top, _)) = self.viewport_virtual_bounds() else {
            return;
        };
        let Some(first_visible) = self.entry_at_virtual_row(top) else {
            return;
        };
        let warm_top =
            top.saturating_sub(RESUME_WARM_PAGES as usize * self.viewport_height as usize);
        let Some(start) = self.entry_at_virtual_row(warm_top) else {
            return;
        };
        self.measure_span_and_rebuild(start, first_visible, width);
        self.follow_scroll_to_bottom();
    }

    /// Index of the entry whose span contains virtual row `row` (the last entry
    /// starting at or before it), or `None` if the cache/viewport is empty.
    pub(super) fn entry_at_virtual_row(&self, row: usize) -> Option<usize> {
        let cache = self.layout_cache.as_ref()?;
        let range = self.visible_entry_range();
        if range.is_empty() {
            return None;
        }
        let vy = cache.virtual_y.get(range.clone())?;
        if vy.is_empty() {
            return None;
        }
        let rel = vy.partition_point(|&y| y <= row).saturating_sub(1);
        let idx = range.start.saturating_add(rel);
        // Guard stale cache vs range drift — never return an OOB index.
        if idx >= range.end || idx >= cache.virtual_y.len() || idx >= cache.entries.len() {
            return None;
        }
        Some(idx)
    }

    /// Index of the entry at the top of the current viewport (the one whose span
    /// contains the scroll top), or `None` if the cache/viewport is empty.
    fn first_visible_entry(&self) -> Option<usize> {
        if self.viewport_height == 0 {
            return None;
        }
        let (top, _) = self.viewport_virtual_bounds()?;
        self.entry_at_virtual_row(top)
    }

    /// Measure exact heights for entries in `[start, end]` (clamped to the
    /// visible range) and rebuild cached offsets if anything was newly measured.
    fn measure_span_and_rebuild(&mut self, start: usize, end: usize, width: u16) {
        if self.viewport_height == 0 || self.layout_cache.is_none() {
            return;
        }
        let range = self.visible_entry_range();
        if range.is_empty() {
            return;
        }
        let start = start.max(range.start);
        let end = end.min(range.end - 1);
        if start > end {
            return;
        }
        if self.measure_window_exact(width, start, end) {
            self.rebuild_virtual_y_from_heights();
            self.compute_total_height_from_cache();
        }
    }

    /// Measure exact heights for entries within ~one viewport of `entry_idx`
    /// (a bounded window: each entry is >= 1 row, so H viewport rows span at most
    /// H entries, and measuring H on each side covers any window that could land
    /// on screen).
    ///
    /// Callers that SET `scroll_offset` from the post-measure offsets
    /// (`scroll_to_entry_top` / `_center`) re-derive scroll from the now-exact
    /// `virtual_y`, so measuring above the viewport doesn't desync.
    /// `ensure_selected_visible` calls this ONLY for an OFF-viewport selection
    /// (an on-viewport selection measures nothing): measuring above an
    /// on-viewport selection would shift `virtual_y` while its fully-visible
    /// early return leaves `scroll_offset` unchanged — a jump.
    pub(super) fn measure_around_entry(&mut self, entry_idx: usize, width: u16) {
        if !self.visible_entry_range().contains(&entry_idx) {
            return;
        }
        let span = self.viewport_height as usize;
        self.measure_span_and_rebuild(
            entry_idx.saturating_sub(span),
            entry_idx.saturating_add(span),
            width,
        );
    }

    /// Measure everything a scroll-to-target computation reads: the window around
    /// the target, plus — in SingleTurn mode — the turn's sticky prompt (at the
    /// visible range start), which drives the sticky-header height in the scroll
    /// math but can sit far above the target window.
    pub(super) fn measure_scroll_target(&mut self, target: usize, width: u16) {
        self.measure_around_entry(target, width);
        if self.view_mode == ViewMode::SingleTurn {
            let start = self.visible_entry_range().start;
            self.measure_span_and_rebuild(start, start, width);
        }
    }

    // Turn Pinning

    /// Check if the current turn's prompt should be pinned as a sticky header.
    pub fn should_pin_prompt(&self) -> bool {
        self.pinned_prompt_index().is_some()
    }

    /// Get the index of the prompt that should be pinned (if any).
    ///
    /// Returns the prompt entry index if it should be pinned.
    /// Pins when scroll_offset > 0 to keep the turn's prompt visible.
    pub fn pinned_prompt_index(&self) -> Option<usize> {
        // Only pin in SingleTurn mode or when we have a current turn
        let turn_idx = self.current_turn?;
        let turn = self.turns.get(turn_idx)?;
        let prompt_idx = turn.prompt_index;

        // Check if prompt is in the visible range
        let visible_range = self.visible_entry_range();
        if !visible_range.contains(&prompt_idx) {
            return None;
        }

        // Pin the prompt when we've scrolled at all
        // This keeps the turn's prompt visible while scrolling through content
        if prompt_idx == visible_range.start && self.scroll_offset > 0 {
            Some(prompt_idx)
        } else {
            None
        }
    }

    /// Get scroll info for scrollbar rendering.
    ///
    /// Returns `(scroll_offset, viewport_height, total_height)`. The two
    /// cumulative quantities are `usize` (tall sessions exceed `u16::MAX`);
    /// `viewport_height` stays `u16`.
    pub fn scroll_info(&self) -> (usize, u16, usize) {
        (self.scroll_offset, self.viewport_height, self.total_height)
    }

    // Layout Cache (for navigation with sticky headers)

    /// Invalidate the layout cache (call when entries change).
    pub(super) fn invalidate_layout_cache(&mut self) {
        self.layout_cache = None;
        // Mark all heights as dirty
        self.dirty_heights = self.entries.keys().copied().collect();
        self.gaps_may_be_dirty = true;
    }

    /// Ensure layout cache is valid for the given width.
    /// Rebuilds the cache if needed.
    pub(super) fn ensure_layout_cache(&mut self, width: u16) {
        // Check if cache is valid
        if let Some(ref cache) = self.layout_cache
            && cache.width == width
            && cache.entries.len() == self.entries.len()
        {
            return; // Cache is valid
        }

        // Rebuild the cache
        self.rebuild_layout_cache(width);
    }

    /// Compute total content height from the layout cache.
    ///
    /// Call after `ensure_layout_cache()` to derive total_height from cached entry heights.
    /// This replaces the old `precompute_total_height()` approach.
    ///
    /// Only sums heights for entries in `visible_entry_range()`. In SingleTurn mode,
    /// this means only the current turn's entries are counted, preventing scroll_down
    /// from allowing scrolling past the end of the visible content.
    ///
    /// `total_height`/`scroll_offset` are `usize`, matching `virtual_y`
    /// (`Vec<usize>`), so the summed rows are never truncated. Capping the total
    /// at `u16::MAX` here is what stranded the bottom of very long sessions:
    /// once content exceeded 65 535 rows, `scroll_offset`/`max_offset`
    /// could not point past the cap and the last rows were unreachable.
    pub(super) fn compute_total_height_from_cache(&mut self) {
        if let Some(ref cache) = self.layout_cache {
            let range = self.visible_entry_range();
            let layouts = &cache.entries[range.clone()];
            // Sum entry heights + gap_after values in the visible range.
            // Per-entry heights are u16; accumulate into usize so a long
            // session (many entries / tall content) is not truncated.
            let total: usize = layouts
                .iter()
                .map(|e| e.height as usize + e.gap_after as usize)
                .sum();
            // gap_after of the last entry acts as the trailing gap (always 1)
            // for selection box bottom corner space, so total is correct as-is.
            self.total_height = total;
        }
    }

    /// Update heights for dirty entries only.
    ///
    /// Returns a list of `(entry_index, height_delta)` for entries whose height
    /// actually changed. The delta is `new_height as i32 - old_height as i32`.
    /// An empty vec means no heights changed.
    pub(super) fn update_dirty_entry_heights(&mut self, width: u16) -> Vec<(usize, i32)> {
        let entry_area_width = self.entry_area_width(width);
        let cwd = self.cwd.as_deref();
        let inline_edit_height = self.inline_edit_height;
        let Some(cache) = self.layout_cache.as_mut() else {
            return Vec::new();
        };

        let theme = Theme::current();

        let mut changes = Vec::new();

        // Collect (id, idx) pairs first to avoid borrow issues
        let dirty_entries: Vec<(EntryId, usize)> = self
            .dirty_heights
            .iter()
            .filter_map(|&id| self.entries.get_index_of(&id).map(|idx| (id, idx)))
            .collect();

        for (id, idx) in dirty_entries {
            if idx >= cache.entries.len() {
                continue; // Entry added after cache was built
            }

            let Some((_, entry)) = self.entries.get_index(idx) else {
                continue;
            };
            let info = cache.entries[idx];
            let renderer = EntryRenderer::new(entry, &theme)
                .with_appearance_ref(&self.appearance)
                .with_cwd(cwd);
            let member_height = match inline_edit_height {
                Some((edit_id, h)) if edit_id == id => h,
                _ => renderer.desired_height(entry_area_width),
            };
            let new_height = info.with_verb_header_row(member_height);
            let old_height = cache.entries[idx].height;
            // This entry now has an exact (re)measured height, so it no longer
            // needs the lazy viewport measurement pass.
            cache.measured[idx] = true;

            // A measured prompt's exact truncated height feeds sticky min_height;
            // refresh it unconditionally (the height can be unchanged while the
            // seed is still the conservative MAX) — matching the sibling measure
            // paths. Cheap: prompts are rarely re-dirtied.
            if entry.block.is_user_prompt() {
                cache.entry_truncated_heights[idx] =
                    renderer.compute_truncated_height(entry_area_width);
            }

            if new_height != old_height {
                cache.entries[idx].height = new_height;
                changes.push((idx, new_height as i32 - old_height as i32));
            }
        }

        changes
    }

    /// Rebuild virtual_y positions and gap_after values from cached entry layout info.
    ///
    /// Called after dirty height updates or lazy viewport measurement. Recomputes
    /// gap_after (because display_mode changes affect the pairwise gap rule) and
    /// then rebuilds virtual_y.
    pub(super) fn rebuild_virtual_y_from_heights(&mut self) {
        let Some(cache) = self.layout_cache.as_mut() else {
            return;
        };

        // Recompute gap_after — display_mode may have changed
        Self::recompute_gap_after(&self.entries, &mut cache.entries);

        // Re-apply verb-group folding + group truncation after gap recomputation
        let max_visible = self.appearance.scrollback.display.group_max_visible as usize;
        cache.groups = groups::apply(
            &self.entries,
            &mut cache.entries,
            max_visible,
            &self.expanded_groups,
        );

        cache.virtual_y.clear();
        cache.prompt_descriptors.clear();

        let mut y = 0usize;

        for (idx, layout) in cache.entries.iter().enumerate() {
            cache.virtual_y.push(y);

            if let Some((_, entry)) = self.entries.get_index(idx)
                && entry.block.is_user_prompt()
            {
                let truncated_height = cache.entry_truncated_heights[idx];
                let min_height = truncated_height.min(MAX_TRUNCATED_HEADER_HEIGHT);
                // Expanded foldable prompts participate in push calculations
                // but don't stick themselves — they scroll away normally.
                let sticky =
                    !(entry.block.is_foldable() && entry.display_mode == DisplayMode::Expanded);
                cache.prompt_descriptors.push(PromptDescriptor {
                    entry_idx: idx,
                    y_virtual: y,
                    full_height: layout.height,
                    min_height,
                    sticky,
                });
            }

            y += layout.height as usize + layout.gap_after as usize;
        }
    }

    /// Incrementally patch virtual_y positions after height-only changes.
    ///
    /// This is the fast path for streaming: when only entry heights changed (no
    /// display_mode or structural changes), we can skip `recompute_gap_after`
    /// entirely and just shift the virtual_y entries after the earliest change.
    ///
    /// `changes` is a list of `(entry_index, height_delta)` from
    /// `update_dirty_entry_heights`. Returns the total height delta (sum of all
    /// individual deltas), useful for O(1) total_height update.
    pub(super) fn patch_virtual_y_for_dirty(&mut self, changes: &[(usize, i32)]) -> i32 {
        if changes.is_empty() {
            return 0;
        }

        let Some(cache) = self.layout_cache.as_mut() else {
            return 0;
        };

        // Find the earliest changed index. All virtual_y entries after it
        // need shifting by the cumulative delta up to that point.
        //
        // For the common streaming case (one entry at the end), this loop
        // touches zero virtual_y entries.
        let earliest_idx = changes.iter().map(|&(idx, _)| idx).min().unwrap_or(0);

        // Build a cumulative delta: for each position from earliest_idx onward,
        // the delta is the sum of all changes at or before that position.
        // Sort changes by index to apply them in order.
        let mut sorted_changes = changes.to_vec();
        sorted_changes.sort_unstable_by_key(|&(idx, _)| idx);

        let total_delta: i32 = sorted_changes.iter().map(|&(_, d)| d).sum();

        // Apply deltas to virtual_y. Walk from earliest_idx+1 to the end,
        // accumulating the delta as we pass each change point.
        let mut change_iter = sorted_changes.iter().peekable();
        let mut cumulative_delta: i64 = 0;

        // Skip changes before earliest_idx (shouldn't happen, but defensive)
        while change_iter
            .peek()
            .is_some_and(|&&(idx, _)| idx < earliest_idx)
        {
            let &(_, d) = change_iter.next().unwrap();
            cumulative_delta += d as i64;
        }

        // Apply the delta at earliest_idx itself (affects entries after it)
        if change_iter
            .peek()
            .is_some_and(|&&(idx, _)| idx == earliest_idx)
        {
            let &(_, d) = change_iter.next().unwrap();
            cumulative_delta += d as i64;
        }

        // Now shift virtual_y[earliest_idx+1..] and update prompt_descriptors
        for idx in (earliest_idx + 1)..cache.virtual_y.len() {
            // Check if this index has its own height change
            if change_iter.peek().is_some_and(|&&(cidx, _)| cidx == idx) {
                let &(_, d) = change_iter.next().unwrap();
                // Apply delta from earlier changes first, then add this one
                cache.virtual_y[idx] = (cache.virtual_y[idx] as i64 + cumulative_delta) as usize;
                cumulative_delta += d as i64;
            } else {
                cache.virtual_y[idx] = (cache.virtual_y[idx] as i64 + cumulative_delta) as usize;
            }
        }

        // Update prompt_descriptors y_virtual values for affected prompts
        for pd in cache.prompt_descriptors.iter_mut() {
            if pd.entry_idx > earliest_idx {
                pd.y_virtual = (pd.y_virtual as i64 + total_delta as i64) as usize;
            } else if pd.entry_idx == earliest_idx {
                // The prompt itself didn't move, but its full_height may have changed
                // (update from the cache which was already patched by update_dirty_entry_heights)
                pd.full_height = cache.entries[pd.entry_idx].height;
            }
        }

        // Also update full_height for any prompts at dirty indices
        for &(idx, _) in changes {
            for pd in cache.prompt_descriptors.iter_mut() {
                if pd.entry_idx == idx {
                    pd.full_height = cache.entries[idx].height;
                }
            }
        }

        total_delta
    }

    /// Try to extend an existing layout cache for a single newly appended entry.
    ///
    /// Returns `true` on success. Returns `false` if the cache doesn't exist
    /// (or appears out of sync) and the caller should fall back to nuking it.
    ///
    /// This avoids the O(N) full rebuild that `invalidate_layout_cache` would
    /// otherwise force on the next `prepare_layout` call. That rebuild is the
    /// dominant per-frame cost during heavy subagent streaming, where dozens
    /// of new blocks are pushed per second; a fresh full-N rebuild on each
    /// push is what drops the subagent fullscreen view to single-digit FPS
    /// while scrolling.
    ///
    /// Updates:
    /// - `cache.entries`: appends an `EntryLayoutInfo` for the new entry, and
    ///   recomputes the previous entry's `gap_after` (it's no longer the
    ///   trailing entry, so the pairwise grouping rule applies).
    /// - `cache.entry_truncated_heights`: appends the new entry's truncated height.
    /// - `cache.virtual_y`: appends the new entry's start position.
    /// - `cache.prompt_descriptors`: appends a descriptor if the new entry is
    ///   a user prompt.
    ///
    /// `total_height` is intentionally NOT updated here -- the next
    /// `prepare_layout` Case 3 path recomputes it from `visible_entry_range()`.
    /// The previous entry's `gap_after` change does not require updating any
    /// earlier `virtual_y` values: only the new entry's position depends on
    /// it, and we compute that here directly.
    pub(super) fn extend_layout_cache_with_new_entry(&mut self, new_idx: usize) -> bool {
        // Read the cache's own width before the mutable borrow below, so the
        // shared entry_area_width helper (which borrows &self) doesn't clash.
        let Some(width) = self.layout_cache.as_ref().map(|c| c.width) else {
            return false;
        };
        let entry_area_width = self.entry_area_width(width);
        let cwd = self.cwd.as_deref();
        let Some(cache) = self.layout_cache.as_mut() else {
            return false;
        };

        // Defensive: cache should be in sync with entries up to but not including new_idx.
        if cache.entries.len() != new_idx
            || cache.virtual_y.len() != new_idx
            || cache.entry_truncated_heights.len() != new_idx
            || cache.measured.len() != new_idx
            || new_idx >= self.entries.len()
        {
            // Cache is out of sync (concurrent state mutation, bug, or batch).
            // Bail out and let the caller invalidate.
            return false;
        }

        let theme = Theme::current();

        // Borrow the new entry to compute its layout info.
        let Some((_, new_entry)) = self.entries.get_index(new_idx) else {
            return false;
        };

        let renderer = EntryRenderer::new(new_entry, &theme)
            .with_appearance_ref(&self.appearance)
            .with_cwd(cwd);
        let height = renderer.desired_height(entry_area_width);
        let is_prompt = new_entry.block.is_user_prompt();
        // Truncated height only feeds prompt sticky-header min_height; only
        // prompts pay for the extra Truncated-mode render (others seed the MAX).
        let truncated_height = if is_prompt {
            renderer.compute_truncated_height(entry_area_width)
        } else {
            MAX_TRUNCATED_HEADER_HEIGHT
        };
        let is_foldable = new_entry.block.is_foldable();
        let new_groupable = new_entry.block.is_groupable();
        let new_collapsed = new_entry.display_mode == DisplayMode::Collapsed;
        let new_display_mode = new_entry.display_mode;

        // Recompute the previous entry's gap_after now that it's no longer the
        // trailing entry. Same pairwise rule as `recompute_gap_after`.
        // The defensive check above guarantees `new_idx < self.entries.len()`,
        // so when `new_idx > 0` the previous entry is in range -- but we still
        // use `if let Some(...)` to keep the access panic-free.
        if new_idx > 0
            && let Some((_, prev_entry)) = self.entries.get_index(new_idx - 1)
        {
            let both_groupable = prev_entry.block.is_groupable() && new_groupable;
            let both_collapsed = prev_entry.display_mode == DisplayMode::Collapsed && new_collapsed;
            cache.entries[new_idx - 1].gap_after = if both_groupable && both_collapsed {
                0
            } else {
                1
            };
        }

        // Compute the new entry's virtual_y (start position) using the
        // previous entry's (now-correct) gap_after.
        let new_y = if new_idx == 0 {
            0
        } else {
            cache.virtual_y[new_idx - 1]
                + cache.entries[new_idx - 1].height as usize
                + cache.entries[new_idx - 1].gap_after as usize
        };

        // Append the new entry. It's now the trailing entry, so gap_after = 1.
        cache.entries.push(EntryLayoutInfo {
            height,
            gap_after: 1,
            group_header_count: 0,
            group_collapse_header: false,
            verb_group_header: false,
        });
        cache.entry_truncated_heights.push(truncated_height);
        // New entries append at the bottom (visible/streaming) and are measured
        // exactly above via `desired_height`, so mark them measured.
        cache.measured.push(true);
        cache.virtual_y.push(new_y);

        if is_prompt {
            let min_height = truncated_height.min(MAX_TRUNCATED_HEADER_HEIGHT);
            // Expanded foldable prompts participate in push calculations
            // but don't stick themselves -- they scroll away normally.
            let sticky = !(is_foldable && new_display_mode == DisplayMode::Expanded);
            cache.prompt_descriptors.push(PromptDescriptor {
                entry_idx: new_idx,
                y_virtual: new_y,
                full_height: height,
                min_height,
                sticky,
            });
        }

        true
    }

    /// Rebuild the layout cache for the given width.
    ///
    /// Entry heights start as cheap ESTIMATES (no markdown render) so this stays
    /// O(history) in arithmetic, not O(history) markdown renders. The on-screen
    /// entries are upgraded to EXACT heights by `settle_visible_measurements`
    /// (driven from `prepare_layout`). Also builds prompt descriptors, used for:
    /// - Sticky header height computation (for navigation)
    /// - Scroll position calculations
    ///
    /// Reuses existing Vec allocations when possible to avoid repeated allocations.
    fn rebuild_layout_cache(&mut self, width: u16) {
        let theme = Theme::current();
        let entry_area_width = self.entry_area_width(width);

        // Reuse existing cache's Vecs to avoid allocations
        let mut cache = self.layout_cache.take().unwrap_or_default().take();
        cache.width = width;

        // Pass 1: Compute a CHEAP height ESTIMATE for every entry (no markdown
        // render / word-wrap). This keeps the bulk-load rebuild O(history) in
        // cheap arithmetic instead of O(history) markdown renders. Exact heights
        // are filled in for the visible viewport by `settle_visible_measurements`
        // (called from `prepare_layout`); off-screen entries stay estimated until
        // they scroll in. gap_after is a placeholder (1), fixed up in pass 2.
        for entry in self.entries.values() {
            let renderer = EntryRenderer::new(entry, &theme)
                .with_appearance_ref(&self.appearance)
                .with_cwd(self.cwd());
            let height = renderer.estimate_height(entry_area_width);
            cache.entries.push(EntryLayoutInfo {
                height,
                gap_after: 1,
                group_header_count: 0,
                group_collapse_header: false,
                verb_group_header: false,
            });
            // Truncated height only feeds prompt sticky-header min_height, and is
            // an ESTIMATE until the entry is measured. Seed it with the MAX so an
            // as-yet-unmeasured pinned prompt never UNDER-reserves and overlaps
            // its content; the exact value is filled in on measurement.
            cache
                .entry_truncated_heights
                .push(MAX_TRUNCATED_HEADER_HEIGHT);
            cache.measured.push(false);
        }

        // Pass 2: Compute gap_after using the pairwise grouping rule.
        Self::recompute_gap_after(&self.entries, &mut cache.entries);

        // Pass 2b: Apply verb-group folding + group truncation.
        let max_visible = self.appearance.scrollback.display.group_max_visible as usize;
        cache.groups = groups::apply(
            &self.entries,
            &mut cache.entries,
            max_visible,
            &self.expanded_groups,
        );

        // Pass 3: Build virtual_y and prompt descriptors from heights + gaps.
        let mut y: usize = 0;
        for (idx, entry_layout) in cache.entries.iter().enumerate() {
            cache.virtual_y.push(y);

            if let Some((_, entry)) = self.entries.get_index(idx)
                && entry.block.is_user_prompt()
            {
                let truncated_height = cache.entry_truncated_heights[idx];
                let min_height = truncated_height.min(MAX_TRUNCATED_HEADER_HEIGHT);
                let sticky =
                    !(entry.block.is_foldable() && entry.display_mode == DisplayMode::Expanded);
                cache.prompt_descriptors.push(PromptDescriptor {
                    entry_idx: idx,
                    y_virtual: y,
                    full_height: entry_layout.height,
                    min_height,
                    sticky,
                });
            }

            y += entry_layout.height as usize + entry_layout.gap_after as usize;
        }

        self.layout_cache = Some(cache);
    }

    /// Compute gap_after for all entries using the pairwise grouping rule.
    ///
    /// Rule: gap between entry[i] and entry[i+1] is 0 if both are groupable AND
    /// both are collapsed; otherwise 1. The last entry always gets gap_after=1
    /// (trailing gap for selection box bottom corner).
    ///
    /// Hidden thinking (height 0) is transparent for spacing: its own
    /// `gap_after` is 0, and the previous visible entry gaps to the *next
    /// visible* neighbor (skipping a run of hidden thinking) so we do not
    /// leave a double spacer (gap into thinking + gap out).
    fn recompute_gap_after(
        entries: &IndexMap<EntryId, ScrollbackEntry>,
        cached_entries: &mut [EntryLayoutInfo],
    ) {
        let n = cached_entries.len();
        if n == 0 {
            return;
        }

        let show_thinking = crate::appearance::cache::load_show_thinking_blocks();

        for (i, cached) in cached_entries.iter_mut().enumerate() {
            let (_, a) = entries.get_index(i).unwrap();
            if a.is_hidden_thinking(show_thinking) {
                cached.gap_after = 0;
                continue;
            }

            // Skip over a run of hidden thinking to the next visible neighbor.
            let mut j = i + 1;
            while j < n {
                let (_, mid) = entries.get_index(j).unwrap();
                if !mid.is_hidden_thinking(show_thinking) {
                    break;
                }
                j += 1;
            }

            if j >= n {
                // Only trailing hidden thinking after `a` (or `a` is last).
                cached.gap_after = 1;
                continue;
            }

            let (_, b) = entries.get_index(j).unwrap();
            let both_groupable = a.block.is_groupable() && b.block.is_groupable();
            let both_collapsed = a.display_mode == DisplayMode::Collapsed
                && b.display_mode == DisplayMode::Collapsed;
            cached.gap_after = if both_groupable && both_collapsed {
                0
            } else {
                1
            };
        }
    }

    /// Compute the group range containing the entry at `idx`.
    ///
    /// A group is a maximal run of adjacent groupable blocks. This walks
    /// forward/backward from `idx` to find the boundaries.
    ///
    /// # Parameters
    /// - `idx`: The entry index to find the group for.
    /// - `collapsed_only`: When `true` (Mode B), only includes adjacent entries
    ///   that are both groupable AND collapsed. An expanded groupable block breaks
    ///   the run. When `false` (Mode A), includes all adjacent groupable blocks
    ///   regardless of display mode.
    ///
    /// # Returns
    /// - If the entry at `idx` is not groupable (or not collapsed when `collapsed_only`
    ///   is true), returns `idx..idx+1` (singleton).
    /// - Otherwise, returns the range of the contiguous group. The walk is
    ///   bounded by [`Self::joins_dense_run`], so the dense range agrees with
    ///   the truncation pass's claimed-entry breaks (leading hidden thinking
    ///   can still skew `start` off the truncation header — pre-existing).
    pub fn group_range_of(&self, idx: usize, collapsed_only: bool) -> Range<usize> {
        // A verb-group run is its own group regardless of `collapsed_only`
        // (members are collapsed by construction; the run stays the toggle /
        // collapse / selection unit while expanded).
        if let Some(range) = self.verb_group_span_range(idx) {
            return range;
        }

        let Some((_, entry)) = self.entries.get_index(idx) else {
            return idx..idx + 1;
        };

        if !entry.block.is_groupable() {
            return idx..idx + 1;
        }
        if collapsed_only && entry.display_mode != DisplayMode::Collapsed {
            return idx..idx + 1;
        }

        let matches = |i: usize| self.joins_dense_run(i, collapsed_only);

        let mut start = idx;
        while start > 0 && matches(start - 1) {
            start -= 1;
        }
        let mut end = idx + 1;
        while end < self.entries.len() && matches(end) {
            end += 1;
        }
        start..end
    }

    /// Whether the entry at `i` joins a dense (non-verb) run walk — the one
    /// membership predicate shared by every dense-run re-derivation
    /// (`group_range_of`, `expand_all_groups`), so their run shapes can't
    /// drift apart. Verb-claimed entries never join: truncation breaks its
    /// runs at claimed entries, and a walk that disagrees keys
    /// expand/collapse on the wrong header id. Unclaimed entries
    /// (pure-thought runs, flag off) stay in, as in truncation.
    pub(super) fn joins_dense_run(&self, i: usize, collapsed_only: bool) -> bool {
        if let Some((_, e)) = self.entries.get_index(i) {
            e.block.is_groupable()
                && (!collapsed_only || e.display_mode == DisplayMode::Collapsed)
                && self.verb_group_range_of(i).is_none()
        } else {
            false
        }
    }

    /// The folded verb run containing the claimed entry at `idx`, read from
    /// the last fold pass's spans ([`Self::span_at`]). Transparent entries
    /// inside the span (live/opened thinking, opened members) keep their own
    /// rows and stay outside the toggle unit, mirroring the walk's anchor
    /// check in [`Self::verb_group_range_of`]. Post-layout query paths
    /// (toggle / collapse / reveal / selection grouping) use this; paths that
    /// run mid-mutation, before the next fold — `rekey_verb_group_expansion`,
    /// `joins_dense_run` — keep the walk, which predicts the NEXT fold from
    /// current entry state.
    fn verb_group_span_range(&self, idx: usize) -> Option<Range<usize>> {
        let span = self.span_at(idx)?;
        let groups::GroupKind::VerbRun { .. } = span.kind else {
            return None;
        };
        let (_, entry) = self.entries.get_index(idx)?;
        let show_thinking = crate::appearance::cache::load_show_thinking_blocks();
        match run_step(entry, show_thinking) {
            RunStep::Member(_) | RunStep::ThoughtMember => Some(span.range.clone()),
            RunStep::Transparent | RunStep::Break => None,
        }
    }

    /// The folding verb-group run (per `RunScan::folds`, `group_tool_verbs`
    /// on) containing the claimed entry (member or thought member) at `idx`,
    /// else `None`. Walks with the fold's own predicate + thinking
    /// transparency so toggle/collapse/reveal operate on the exact folded
    /// range, not the broader dense-group run (which would leak across
    /// separators like Edit). Predicts the fold from CURRENT entry state —
    /// mid-mutation callers rely on this; post-layout queries go through
    /// [`Self::verb_group_span_range`] instead.
    pub(super) fn verb_group_range_of(&self, idx: usize) -> Option<Range<usize>> {
        if !crate::appearance::cache::load_group_tool_verbs() {
            return None;
        }
        let show_thinking = crate::appearance::cache::load_show_thinking_blocks();
        // An unclaimable `idx` has no range, even when its neighbors form a
        // run.
        let (_, entry) = self.entries.get_index(idx)?;
        match run_step(entry, show_thinking) {
            RunStep::Member(_) | RunStep::ThoughtMember => {}
            RunStep::Transparent | RunStep::Break => return None,
        }

        // Backward half only finds the run's start; the shared forward scan
        // from `start` then measures the whole run in one pass.
        let mut start = idx;
        let mut scan = idx;
        while scan > 0 {
            let (_, e) = self.entries.get_index(scan - 1)?;
            match run_step(e, show_thinking) {
                RunStep::Member(_) | RunStep::ThoughtMember => start = scan - 1,
                RunStep::Transparent => {}
                RunStep::Break => break,
            }
            scan -= 1;
        }

        let entry_at = |i: usize| self.entries.get_index(i).map(|(_, e)| e);
        let run = scan_run_forward(entry_at, start, show_thinking)?;
        run.folds().then_some(start..run.end)
    }

    /// Paint window for one scroll frame: the sub-range of `visible_range`
    /// whose entries can intersect the content viewport, plus the window's
    /// starting virtual-y (relative to `visible_range.start`).
    ///
    /// Thin wrapper over [`compute_paint_window`] fed from the layout cache;
    /// group-header runs (verb and truncation) extend through their fold
    /// span ([`Self::span_at`]) so the aggregated header labels still see
    /// off-screen members.
    ///
    /// # Panics
    /// Panics if the layout cache is invalid (call `prepare_layout()` first)
    /// or `visible_range` is out of bounds for it.
    pub fn paint_window(
        &self,
        visible_range: Range<usize>,
        scroll: usize,
        viewport_h: usize,
    ) -> (Range<usize>, usize) {
        let virtual_y = self
            .get_cached_virtual_y()
            .expect("layout cache must be valid - was prepare_layout() called?");
        let layouts = self
            .get_cached_entry_layouts()
            .expect("layout cache must be valid - was prepare_layout() called?");
        compute_paint_window(virtual_y, layouts, visible_range, scroll, viewport_h, |i| {
            self.span_at(i).map_or(i + 1, |span| span.range.end)
        })
    }

    /// Get the sticky header layout for the current scroll position.
    ///
    /// Works for BOTH AllTurns and SingleTurn modes using unified sticky logic.
    /// Returns None if no entries or no sticky header at current position.
    pub fn sticky_layout(&mut self) -> Option<StickyHeaderLayout> {
        if self.entries.is_empty() || self.last_width == 0 || self.viewport_height == 0 {
            return None;
        }

        self.ensure_layout_cache(self.last_width);

        let cache = self.layout_cache.as_ref()?;
        let visible_range = self.visible_entry_range();
        let relative_prompts = self.build_relative_prompt_descriptors(cache, &visible_range);

        let sticky =
            compute_sticky_layout(self.scroll_offset, self.viewport_height, &relative_prompts);

        if sticky.has_header() {
            Some(sticky)
        } else {
            None
        }
    }

    /// Get cached prompt descriptors (for rendering).
    /// Returns None if cache is not valid.
    pub fn prompt_descriptors(&mut self) -> Option<Vec<PromptDescriptor>> {
        if self.last_width == 0 {
            return None;
        }
        self.ensure_layout_cache(self.last_width);
        self.layout_cache
            .as_ref()
            .map(|c| c.prompt_descriptors.clone())
    }
}

/// Compute the paint window for one scroll frame: the sub-range of
/// `visible_range` whose entries can intersect the viewport rows
/// `scroll..scroll + viewport_h` (in virtual-y space relative to
/// `visible_range.start`), plus the window's starting virtual-y in that same
/// space (`content_y0` for the renderer).
///
/// O(log n) via `partition_point` over the cached prefix-sum `virtual_y`,
/// instead of collecting/walking the full history each frame. Backs off one
/// entry when the previous entry straddles the viewport top (entries never
/// overlap, so one is enough). A group header inside the window (verb or
/// truncation) extends the window end through `run_end(header_idx)`
/// (exclusive run end, clamped to `visible_range.end`) so the aggregated
/// header labels still see off-screen members (counts/tense/failures);
/// `run_end` is only called for visible header rows.
///
/// Invariants (violations panic loudly rather than being papered over):
/// `virtual_y` and `layouts` are the full-history parallel layout-cache slices
/// (`virtual_y[i+1] = virtual_y[i] + height[i] + gap_after[i]`), and
/// `visible_range` is in bounds for them. The returned range is always within
/// `visible_range`.
pub fn compute_paint_window(
    virtual_y: &[usize],
    layouts: &[EntryLayoutInfo],
    visible_range: Range<usize>,
    scroll: usize,
    viewport_h: usize,
    run_end: impl Fn(usize) -> usize,
) -> (Range<usize>, usize) {
    debug_assert_eq!(virtual_y.len(), layouts.len());
    if visible_range.is_empty() {
        return (visible_range.start..visible_range.start, 0);
    }
    let base_y = virtual_y[visible_range.start];
    let vp_start = base_y + scroll;
    let vp_end = vp_start + viewport_h;
    let range_vy = &virtual_y[visible_range.clone()];
    let mut first_rel = range_vy.partition_point(|&y| y < vp_start);
    if first_rel > 0 {
        let prev = visible_range.start + first_rel - 1;
        if virtual_y[prev] + layouts[prev].height as usize > vp_start {
            first_rel -= 1;
        }
    }
    let paint_start = visible_range.start + first_rel;
    let mut paint_end = visible_range.start + range_vy.partition_point(|&y| y < vp_end);
    let mut i = paint_start;
    while i < paint_end {
        // Any group header row (verb or truncation) aggregates entries that
        // can sit past the viewport edge; extend so the label walks see them.
        if layouts[i].height > 0 && layouts[i].is_group_header() {
            // The run walk is range-agnostic; keep the window inside the
            // visible range so index remapping downstream stays valid.
            paint_end = paint_end.max(run_end(i).min(visible_range.end));
        }
        i += 1;
    }
    let content_y0 = if paint_start < paint_end {
        virtual_y[paint_start] - base_y
    } else {
        0
    };
    (paint_start..paint_end, content_y0)
}

#[cfg(test)]
mod tests {
    use super::super::test_util::*;
    use super::*;
    use crate::scrollback::entry::EntryId;
    use crate::theme::cache::pin_theme;
    use pretty_assertions::assert_eq;
    use ratatui::style::Color;

    /// After the first `prepare_layout`, subsequent `push_block` calls should
    /// EXTEND the layout cache instead of nuking it. This prevents the O(N)
    /// full rebuild that caused subagent fullscreen scrolling to drop to 0 FPS
    /// during streaming.
    #[test]
    fn test_push_extends_layout_cache_when_present() {
        let mut state = ScrollbackState::new();
        state.push_block(stub_block("first"));
        state.push_block(stub_block("second"));
        state.prepare_layout(80, 20);

        // Cache exists after prepare_layout.
        assert!(
            state.layout_cache.is_some(),
            "cache populated after prepare_layout"
        );
        let pre_len = state
            .layout_cache
            .as_ref()
            .map(|c| c.entries.len())
            .unwrap();
        assert_eq!(pre_len, 2);

        // Push a new entry. Cache should still exist and grow by exactly one slot.
        state.push_block(stub_block("third"));
        let cache = state
            .layout_cache
            .as_ref()
            .expect("cache should NOT be nuked by push_block");
        assert_eq!(cache.entries.len(), 3);
        assert_eq!(cache.virtual_y.len(), 3);
        assert_eq!(cache.entry_truncated_heights.len(), 3);
    }

    /// After an incremental extend, the next `prepare_layout` must NOT do a
    /// Case 1 full rebuild. We assert this indirectly: dirty_heights stays
    /// empty (push doesn't dirty existing entries), gaps_may_be_dirty is
    /// false (we updated the gap inline), and the cache pointer is preserved.
    #[test]
    fn test_push_does_not_set_gaps_may_be_dirty_after_successful_extend() {
        let mut state = ScrollbackState::new();
        state.push_block(stub_block("first"));
        state.prepare_layout(80, 20);

        assert!(!state.gaps_may_be_dirty, "clean after prepare_layout");
        assert!(state.dirty_heights.is_empty());

        state.push_block(stub_block("second"));

        assert!(
            !state.gaps_may_be_dirty,
            "extend handles gap inline; gaps_may_be_dirty must stay false \
             so the next streaming chunk's Case 2 takes the fast path"
        );
        assert!(state.dirty_heights.is_empty(), "push doesn't dirty heights");
        assert!(state.layout_cache.is_some(), "cache preserved");
    }

    /// After extension, virtual_y for the new entry must equal the previous
    /// entry's start + its height + its (possibly recomputed) gap_after.
    #[test]
    fn test_push_extends_virtual_y_correctly() {
        let mut state = ScrollbackState::new();
        state.push_block(stub_block("a"));
        state.prepare_layout(80, 20);

        let (prev_start, prev_height, prev_gap) = {
            let cache = state.layout_cache.as_ref().unwrap();
            (
                cache.virtual_y[0],
                cache.entries[0].height,
                cache.entries[0].gap_after,
            )
        };

        state.push_block(stub_block("b"));

        let cache = state.layout_cache.as_ref().unwrap();
        // Index 1 should start exactly where the previous entry's content ended +
        // the (possibly recomputed) gap.
        let expected_y = prev_start + prev_height as usize + cache.entries[0].gap_after as usize;
        assert_eq!(cache.virtual_y[1], expected_y);

        // Sanity: extending shouldn't have shifted the previous entry's start.
        assert_eq!(cache.virtual_y[0], prev_start);
        assert_eq!(cache.entries[0].height, prev_height);
        // gap_after of the previous entry MAY change (e.g. 1 -> 0 for two
        // groupable+collapsed blocks), so we don't assert it's still prev_gap.
        let _ = prev_gap;
    }

    /// Extension must also append a `PromptDescriptor` when the new entry is
    /// a UserPrompt, so sticky-header navigation still works without a rebuild.
    #[test]
    fn test_push_user_prompt_appends_prompt_descriptor() {
        let mut state = ScrollbackState::new();
        state.push_block(stub_block("a"));
        state.prepare_layout(80, 20);

        let pre = state
            .layout_cache
            .as_ref()
            .map(|c| c.prompt_descriptors.len())
            .unwrap();
        assert_eq!(pre, 0);

        let prompt_id = state.push_block(user_block("Hello"));

        let cache = state.layout_cache.as_ref().unwrap();
        assert_eq!(cache.prompt_descriptors.len(), 1);
        let pd = &cache.prompt_descriptors[0];
        let prompt_idx = state.index_of_id(prompt_id).unwrap();
        assert_eq!(pd.entry_idx, prompt_idx);
        assert_eq!(pd.y_virtual, cache.virtual_y[prompt_idx]);
    }

    /// Build a LayoutCache with the given entry heights.
    /// virtual_y is computed with 1-row gaps between entries (matching current gap_after=1).
    fn make_cache(heights: &[u16]) -> LayoutCache {
        let mut entries = Vec::with_capacity(heights.len());
        let mut virtual_y = Vec::with_capacity(heights.len());
        let mut y = 0usize;
        for &h in heights {
            virtual_y.push(y);
            let gap_after = 1u16; // constant for now
            entries.push(EntryLayoutInfo {
                height: h,
                gap_after,
                group_header_count: 0,
                group_collapse_header: false,
                verb_group_header: false,
            });
            y += h as usize + gap_after as usize;
        }
        LayoutCache {
            measured: vec![true; heights.len()],
            entries,
            entry_truncated_heights: heights.to_vec(),
            virtual_y,
            prompt_descriptors: vec![],
            groups: vec![],
            width: 80,
        }
    }

    #[test]
    fn test_entry_at_content_y_basic() {
        // 3 entries: heights 3, 2, 4.  gap=1
        // Layout:  [0..3) entry0, [3] gap, [4..6) entry1, [6] gap, [7..11) entry2
        let cache = make_cache(&[3, 2, 4]);
        let all = 0..3;

        // Entry 0 occupies rows 0, 1, 2
        assert_eq!(cache.entry_at_content_y(0, all.clone()), Some(0));
        assert_eq!(cache.entry_at_content_y(2, all.clone()), Some(0));

        // Row 3 is the gap after entry 0
        assert_eq!(cache.entry_at_content_y(3, all.clone()), None);

        // Entry 1 occupies rows 4, 5
        assert_eq!(cache.entry_at_content_y(4, all.clone()), Some(1));
        assert_eq!(cache.entry_at_content_y(5, all.clone()), Some(1));

        // Row 6 is the gap after entry 1
        assert_eq!(cache.entry_at_content_y(6, all.clone()), None);

        // Entry 2 occupies rows 7, 8, 9, 10
        assert_eq!(cache.entry_at_content_y(7, all.clone()), Some(2));
        assert_eq!(cache.entry_at_content_y(10, all.clone()), Some(2));

        // Past the end
        assert_eq!(cache.entry_at_content_y(11, all.clone()), None);
        assert_eq!(cache.entry_at_content_y(100, all.clone()), None);
    }

    #[test]
    fn test_entry_at_content_y_single_entry() {
        let cache = make_cache(&[5]);
        let all = 0..1;

        assert_eq!(cache.entry_at_content_y(0, all.clone()), Some(0));
        assert_eq!(cache.entry_at_content_y(4, all.clone()), Some(0));
        assert_eq!(cache.entry_at_content_y(5, all.clone()), None);
    }

    #[test]
    fn test_entry_at_content_y_restricted_range() {
        // 5 entries, but only search within range 2..4
        let cache = make_cache(&[2, 2, 3, 4, 2]);
        // virtual_y: [0, 3, 6, 10, 15]

        // Entry 2 starts at virtual_y=6, height=3 → occupies [6..9)
        assert_eq!(cache.entry_at_content_y(6, 2..4), Some(2));
        assert_eq!(cache.entry_at_content_y(8, 2..4), Some(2));

        // Gap at 9
        assert_eq!(cache.entry_at_content_y(9, 2..4), None);

        // Entry 3 starts at virtual_y=10, height=4 → occupies [10..14)
        assert_eq!(cache.entry_at_content_y(10, 2..4), Some(3));
        assert_eq!(cache.entry_at_content_y(13, 2..4), Some(3));

        // Entry 0 is outside the range
        assert_eq!(cache.entry_at_content_y(0, 2..4), None);

        // Entry 4 is outside the range
        assert_eq!(cache.entry_at_content_y(15, 2..4), None);
    }

    #[test]
    fn test_entry_at_content_y_empty_range() {
        let cache = make_cache(&[3, 2]);
        assert_eq!(cache.entry_at_content_y(0, 0..0), None);
    }

    #[test]
    fn test_entry_at_content_y_height_one_entries() {
        // Entries of height 1 with gaps between → alternating entry/gap
        let cache = make_cache(&[1, 1, 1]);
        // virtual_y: [0, 2, 4]  (each entry=1 + gap=1)
        let all = 0..3;

        assert_eq!(cache.entry_at_content_y(0, all.clone()), Some(0));
        assert_eq!(cache.entry_at_content_y(1, all.clone()), None); // gap
        assert_eq!(cache.entry_at_content_y(2, all.clone()), Some(1));
        assert_eq!(cache.entry_at_content_y(3, all.clone()), None); // gap
        assert_eq!(cache.entry_at_content_y(4, all.clone()), Some(2));
        assert_eq!(cache.entry_at_content_y(5, all.clone()), None); // past end
    }

    // ── Hit-testing with sticky headers ──────────────────────────────

    /// Set up a scrollback state with a prompt + N response blocks,
    /// prepare layout, and return it.
    ///
    /// Uses no-vpad appearance so heights are predictable:
    ///   user_block("prompt") → height 1
    ///   stub_block("resp")   → height 1
    ///
    /// With ENTRY_GAP=1, a 2-entry layout is:
    ///   row 0: prompt (entry 0)
    ///   row 1: gap
    ///   row 2: response (entry 1)
    fn make_scrollback_for_hittest(
        response_count: usize,
        viewport_width: u16,
        viewport_height: u16,
    ) -> ScrollbackState {
        use crate::appearance::AppearanceConfig;

        let mut state = ScrollbackState::new();

        // Disable prompt vpad for predictable 1-row heights
        let mut appearance = AppearanceConfig::default();
        appearance.scrollback.blocks.prompt.vpad = false;
        state.set_appearance(appearance);

        state.push_block(user_block("prompt"));
        for i in 0..response_count {
            state.push_block(stub_block(&format!("resp{i}")));
        }

        state.prepare_layout(viewport_width, viewport_height);
        state
    }

    #[test]
    fn ffmpeg_install_midsession_expands_video_reservation() {
        use crate::inline_media_ffmpeg::set_ffmpeg_available_for_test;
        use crate::scrollback::block::RenderBlock;
        use crate::scrollback::blocks::{OtherToolCallBlock, ToolCallBlock};
        use crate::terminal::image::{GraphicsProtocol, set_protocol_for_test};

        // Inline video posters only reserve rows on a Kitty-capable terminal.
        let _proto = set_protocol_for_test(GraphicsProtocol::Kitty);

        // Building a video ref requires a real file with a video extension.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mp4");
        std::fs::write(&path, b"x").unwrap();

        let mut state = ScrollbackState::new();
        state.push_block(RenderBlock::ToolCall(ToolCallBlock::Other(
            OtherToolCallBlock::new("image_to_video", "clip").with_media_ref(path.clone(), true),
        )));

        // Without ffmpeg the entry reserves only the compact banner.
        let banner_total = {
            let _no_ffmpeg = set_ffmpeg_available_for_test(false);
            state.prepare_layout(80, 40);
            state.scroll_info().2
        };

        // Installing ffmpeg mid-session must rebuild the layout so the poster
        // claims full height — otherwise it paints over the text below the
        // (stale) banner-sized reservation.
        let poster_total = {
            let _ffmpeg = set_ffmpeg_available_for_test(true);
            state.prepare_layout(80, 40);
            state.scroll_info().2
        };

        assert!(
            poster_total > banner_total,
            "video reservation must grow when ffmpeg appears \
             (banner={banner_total}, poster={poster_total})"
        );
    }

    #[test]
    fn test_hit_test_no_scroll_no_header() {
        // No scroll → no sticky header → all entries hittable
        let state = make_scrollback_for_hittest(2, 80, 20);
        let area = Rect::new(0, 0, 80, 20);

        // Check actual cached heights for debugging
        let heights: Vec<u16> = (0..state.len())
            .map(|i| state.get_cached_entry_height(i).unwrap())
            .collect();

        // With no-vpad prompt (height 1) and stub blocks (height 1), and ENTRY_GAP=1:
        //   virtual_y[0]=0, virtual_y[1]=2, virtual_y[2]=4
        let virtual_y: Vec<usize> = state.layout_cache.as_ref().unwrap().virtual_y.clone();

        // Entry 0 (prompt) at virtual_y[0]
        assert_eq!(
            state.entry_index_at_screen_row(0, area),
            Some(0),
            "heights={heights:?}, virtual_y={virtual_y:?}"
        );

        // Find where entry 1 starts on screen
        let entry1_screen_row = virtual_y[1] as u16;
        assert_eq!(
            state.entry_index_at_screen_row(entry1_screen_row, area),
            Some(1),
            "Entry 1 should be at screen row {entry1_screen_row}, heights={heights:?}, virtual_y={virtual_y:?}"
        );

        // Gap between entry 0 and entry 1
        let gap_row = heights[0]; // right after entry 0 ends
        assert_eq!(
            state.entry_index_at_screen_row(gap_row, area),
            None,
            "Row {gap_row} should be a gap, heights={heights:?}, virtual_y={virtual_y:?}"
        );
    }

    #[test]
    fn test_hit_test_with_sticky_header_excludes_header_rows() {
        let mut state = make_scrollback_for_hittest(5, 80, 10);
        let area = Rect::new(0, 0, 80, 10);

        // Scroll past the prompt
        state.scroll_down(3);

        let cache = state.layout_cache.as_ref().unwrap();
        let visible_range = state.visible_entry_range();
        let header_rows = state
            .current_sticky_layout(cache, &visible_range)
            .header_screen_rows();

        let heights: Vec<u16> = (0..state.len())
            .map(|i| state.get_cached_entry_height(i).unwrap())
            .collect();
        let virtual_y = &cache.virtual_y;

        // Sticky header should be present
        assert!(
            header_rows >= 1,
            "Expected sticky header, got {header_rows} rows. heights={heights:?}, virtual_y={virtual_y:?}, scroll_offset={}",
            state.scroll_offset
        );

        // Rows in the header area should hit the pinned prompt (entry 0),
        // except for gap rows which return None.
        let sticky = state.current_sticky_layout(cache, &visible_range);
        for row in 0..header_rows {
            let result = state.entry_index_at_screen_row(row, area);
            let expected = sticky.entry_at_header_row(row);
            assert_eq!(
                result, expected,
                "Row {row} in header: expected {expected:?}, got {result:?} (header_rows={header_rows})"
            );
        }

        // Scan from header down to find the first row that hits an entry
        let mut found_entry = false;
        for row in header_rows..area.height {
            if let Some(idx) = state.entry_index_at_screen_row(row, area) {
                found_entry = true;
                assert!(
                    row >= header_rows,
                    "Entry {idx} hit at row {row} which is in header area ({header_rows} rows)"
                );
                break;
            }
        }
        assert!(
            found_entry,
            "No entry found below header. header_rows={header_rows}, heights={heights:?}, virtual_y={virtual_y:?}, scroll={}",
            state.scroll_offset
        );
    }

    #[test]
    fn test_entry_screen_area_clipped_by_sticky_header() {
        // Set up with a scrolled-down state so sticky header is active
        let mut state = make_scrollback_for_hittest(5, 80, 10);
        let area = Rect::new(0, 0, 80, 10);

        // Scroll down so the prompt is pinned as sticky header,
        // and the first response is partially behind the header
        state.scroll_down(1);

        let cache = state.layout_cache.as_ref().unwrap();
        let visible_range = state.visible_entry_range();
        let header_rows = state
            .current_sticky_layout(cache, &visible_range)
            .header_screen_rows();

        if header_rows > 0 {
            // Get the screen area for an entry that's visible below the header
            // Entry 1 (resp0) should be at or near the top of content area
            if let Some((entry_area, _top_clipped, _bottom_clipped)) =
                state.entry_screen_area(1, area)
            {
                // The entry area must NOT extend into the header
                assert!(
                    entry_area.y >= header_rows,
                    "Entry area y={} extends into header (header_rows={})",
                    entry_area.y,
                    header_rows
                );
            }
        }
    }

    #[test]
    fn test_entry_screen_area_behind_header_returns_none() {
        // Scroll down far enough that entry 0 is entirely behind the sticky header
        let mut state = make_scrollback_for_hittest(10, 80, 10);
        let area = Rect::new(0, 0, 80, 10);

        // Entry 0 (prompt) has height 1 at virtual_y=0.
        // Scrolling past it means it becomes the sticky header.
        state.scroll_down(5);

        // Entry 0 is the pinned sticky header — entry_screen_area should
        // return its header area (it IS visible, just in the header zone).
        let result = state.entry_screen_area(0, area);
        assert!(
            result.is_some(),
            "Entry 0 is the pinned sticky header, should be hittable"
        );
        let (entry_area, _top_clipped, _bottom_clipped) = result.unwrap();
        // The header area should start at row 0 (top of scrollback)
        assert_eq!(entry_area.y, 0, "Pinned header should start at top");
        assert!(entry_area.height > 0, "Pinned header should have height");
    }

    /// Regression: a lazily-resumed session must not pad a pinned sticky-header
    /// prompt with empty rows. Old (above-viewport) prompts are never in the
    /// measurement window, so their `entry_truncated_heights` stays at the
    /// `MAX_TRUNCATED_HEADER_HEIGHT` seed; the sticky layout must still collapse
    /// a short pinned prompt to its real (full) height rather than the 6-row
    /// seed. See `sticky::calculate_render_height`'s full-height clamp.
    #[test]
    fn lazy_resumed_pinned_prompt_collapses_to_real_height() {
        use crate::appearance::AppearanceConfig;
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig {
            show_timestamps: false,
            ..Default::default()
        };
        appearance.scrollback.blocks.prompt.vpad = false;
        state.set_appearance(appearance);

        // Resume: short 1-line prompts + tall responses, bulk-loaded (lazy).
        state.begin_batch();
        for i in 0..10 {
            state.push_block(RenderBlock::user_prompt(format!("q{i}")));
            state.push_block(RenderBlock::stub(
                format!("resp{i}\na\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm"),
                Color::Blue,
            ));
        }
        state.end_batch();
        state.prepare_layout(80, 12);

        // Scroll up so an OLD prompt becomes the pinned sticky header.
        state.scroll_up(40);
        let _ = state.prepare_layout(80, 12);

        let cache = state.layout_cache.as_ref().unwrap();
        let range = state.visible_entry_range();
        let sticky = state.current_sticky_layout(cache, &range);
        let pinned = sticky
            .pinned
            .expect("an old prompt should be pinned after scrolling up");

        // The pinned prompt was never measured (it sits above the viewport), so
        // its seeded truncated height is the 6-row MAX. The collapsed sticky
        // header must still match the prompt's real height (1 row), proving the
        // seed no longer leaks empty padding rows.
        assert!(
            !cache.measured[pinned.entry_idx],
            "precondition: pinned prompt must be unmeasured (lazy seed in play)"
        );
        let full_height = cache.entries[pinned.entry_idx].height;
        assert!(
            pinned.visible_height() <= full_height,
            "sticky header ({}) must not exceed the prompt's full height ({full_height})",
            pinned.visible_height(),
        );
        assert_eq!(
            pinned.visible_height(),
            1,
            "a 1-row pinned prompt must collapse to 1 row, not the 6-row seed"
        );
    }

    // ── Lazy viewport height measurement (fast large-session resume) ──

    /// Number of entries that have actually been laid out (markdown-rendered).
    /// Estimated entries never populate the entry's output cache.
    fn laid_out_count(state: &ScrollbackState) -> usize {
        (0..state.len())
            .filter(|&i| state.entry(i).is_some_and(|e| e.has_cached_output()))
            .count()
    }

    fn measured_at(state: &ScrollbackState, idx: usize) -> bool {
        state.layout_cache.as_ref().unwrap().measured[idx]
    }

    /// Recompute total height directly from the layout cache (mix of estimated
    /// and exact heights) to assert internal consistency with `total_height`.
    fn cache_total(state: &ScrollbackState) -> u32 {
        let cache = state.layout_cache.as_ref().unwrap();
        let range = state.visible_entry_range();
        cache.entries[range]
            .iter()
            .map(|e| e.height as u32 + e.gap_after as u32)
            .sum()
    }

    fn exact_height(state: &ScrollbackState, idx: usize, width: u16) -> u16 {
        let theme = Theme::current();
        let entry = state.entry(idx).unwrap();
        EntryRenderer::new(entry, &theme)
            .with_appearance(state.appearance().clone())
            .with_cwd(state.cwd())
            .desired_height(width)
    }

    /// Bulk-load `n` multi-row stub entries (begin_batch/end_batch like resume).
    fn bulk_load_stubs(state: &mut ScrollbackState, n: usize) {
        state.begin_batch();
        for i in 0..n {
            state.push_block(RenderBlock::stub(
                format!("entry {i} alpha\nbeta line\ngamma line"),
                Color::Blue,
            ));
        }
        state.end_batch();
    }

    /// Bulk-load `n` agent messages whose word-heavy text word-wraps at a narrow
    /// width, so the estimate (char-ceil) and exact (word-wrap) heights differ.
    fn bulk_load_wrapping(n: usize) -> ScrollbackState {
        use crate::appearance::AppearanceConfig;
        let mut state = ScrollbackState::new();
        let appearance = AppearanceConfig {
            show_timestamps: false,
            ..Default::default()
        };
        state.set_appearance(appearance);
        state.begin_batch();
        for i in 0..n {
            state.push_block(RenderBlock::agent_message(format!(
                "msg{i} aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd eeeeeeeeee ffffffffff"
            )));
        }
        state.end_batch();
        state
    }

    #[test]
    fn lazy_bulk_load_lays_out_viewport_plus_warm_pages_not_history() {
        let _theme = pin_theme();
        // Resuming a large session must NOT render every entry — only the visible
        // tail plus the small RESUME_WARM_PAGES band above it.
        let mut state = ScrollbackState::new();
        bulk_load_stubs(&mut state, 200);
        state.prepare_layout(80, 20);

        let count = laid_out_count(&state);
        assert!(count >= 1, "the visible tail must be laid out");
        // Bounded: viewport + warm pages (each stub is 6 rows), never history.
        assert!(
            count <= 30 && count < state.len(),
            "laid-out count must be ~viewport+warm, not history (got {count})"
        );
        // The bottom (visible) entry is laid out; the far top is not.
        assert!(
            state.entry(199).unwrap().has_cached_output(),
            "last entry (visible) must be laid out"
        );
        assert!(
            !state.entry(0).unwrap().has_cached_output(),
            "first entry (far off-screen) must NOT be laid out"
        );
    }

    #[test]
    fn lazy_bulk_load_warms_pages_above_the_viewport() {
        let _theme = pin_theme();
        let mut state = ScrollbackState::new();
        bulk_load_stubs(&mut state, 200);
        state.prepare_layout(80, 20);

        let visible_top = state.first_visible_entry().unwrap();
        assert!(measured_at(&state, 199), "bottom entry measured exactly");
        // The warm-up measured a band ABOVE the visible window...
        let warmed: Vec<usize> = (0..visible_top)
            .filter(|&i| measured_at(&state, i))
            .collect();
        assert!(
            !warmed.is_empty() && *warmed.first().unwrap() < visible_top,
            "entries above the viewport are pre-measured (warmed={warmed:?}, visible_top={visible_top})"
        );
        // ...but the far history stays estimated (bounded, not O(history)).
        assert!(!measured_at(&state, 0), "far-above history left estimated");
    }

    #[test]
    fn resize_defers_warm_above_until_the_width_settles() {
        let _theme = pin_theme();
        let mut state = ScrollbackState::new();
        bulk_load_stubs(&mut state, 200);
        state.begin_frame();
        state.prepare_layout(80, 20);
        let visible_top = state.first_visible_entry().unwrap();
        assert!(
            (0..visible_top).any(|i| measured_at(&state, i)),
            "the initial layout still warms above the viewport"
        );

        for width in [79u16, 78, 77] {
            state.begin_frame();
            state.prepare_layout(width, 20);
            let top = state.first_visible_entry().unwrap();
            assert!(
                !(0..top).any(|i| measured_at(&state, i)),
                "width {width}: nothing above the viewport is measured mid-drag"
            );
        }

        state.begin_frame();
        state.prepare_layout(77, 20);
        let top = state.first_visible_entry().unwrap();
        assert!(
            (0..top).any(|i| measured_at(&state, i)),
            "the deferred warm-up runs once the width stops changing"
        );
    }

    /// A fullscreen frame prepares layout twice whenever the timeline rail is
    /// on, and the second pass sees an unchanged width.
    #[test]
    fn resize_defers_warm_above_across_a_frames_extra_layout_passes() {
        let _theme = pin_theme();
        let mut state = ScrollbackState::new();
        bulk_load_stubs(&mut state, 200);
        state.begin_frame();
        state.prepare_layout(80, 20);
        state.prepare_layout(80, 20);

        for width in [79u16, 78, 77] {
            state.begin_frame();
            state.prepare_layout(width, 20);
            state.prepare_layout(width, 20);
            let top = state.first_visible_entry().unwrap();
            assert!(
                !(0..top).any(|i| measured_at(&state, i)),
                "width {width}: the paint pass must not run the warm-up the \
                 rail pass deferred"
            );
        }

        state.begin_frame();
        state.prepare_layout(77, 20);
        let top = state.first_visible_entry().unwrap();
        assert!(
            (0..top).any(|i| measured_at(&state, i)),
            "the deferred warm-up runs on the first frame after the drag"
        );
    }

    #[test]
    fn lazy_resume_scroll_up_lands_on_prewarmed_exact_entries() {
        let _theme = pin_theme();
        // The point of the warm-up: scrolling up one page right after resume must
        // land on already-exact entries (measured before the scroll), so there is
        // no estimate->exact rebuild and no jump.
        let mut state = ScrollbackState::new();
        bulk_load_stubs(&mut state, 200);
        state.prepare_layout(80, 20);
        let before = state.layout_cache.as_ref().unwrap().measured.clone();

        state.page_up();
        state.prepare_layout(80, 20);

        let top = state.first_visible_entry().unwrap();
        assert!(
            before[top],
            "one page-up lands inside the pre-warmed region (entry {top} was exact before the scroll)"
        );
    }

    #[test]
    fn lazy_warm_up_is_skipped_in_preserve_mode() {
        let _theme = pin_theme();
        // Regression: the warm-up measures pages ABOVE the viewport and relies on
        // the bottom re-pin to cancel the uniform shift. In follow_preserve_scroll
        // (a prompt pinned at the TOP) follow_scroll_to_bottom keeps the scroll
        // put, so warming above would shift the pin down — a jump. The warm-up
        // must skip preserve mode.
        let mut state = bulk_load_wrapping(200);
        state.prepare_layout(20, 12);

        // Fresh all-estimated, bottom-pinned cache (no settle/warm yet).
        state.invalidate_layout_cache();
        state.ensure_layout_cache(20);
        state.compute_total_height_from_cache();
        state.handle_follow_mode();
        let top = state.first_visible_entry().unwrap();
        assert!(
            top >= 1 && !measured_at(&state, top - 1),
            "entries just above the viewport are estimated (warm-up has work to do)"
        );

        // Preserve mode: warm-up must measure nothing above the viewport.
        state.follow_preserve_scroll = true;
        state.warm_measure_pages_above(20);
        assert!(
            (0..top).all(|i| !measured_at(&state, i)),
            "preserve mode: warm-up must not measure above the viewport"
        );

        // Same state without preserve: the warm-up DOES measure pages above (the
        // preserve guard is the only difference) — proves the test is load-bearing.
        state.follow_preserve_scroll = false;
        state.warm_measure_pages_above(20);
        assert!(
            (0..top).any(|i| measured_at(&state, i)),
            "non-preserve: warm-up measures pages above the viewport"
        );
    }

    /// A wedged offset (viewport top at/past the end of the content) has a
    /// degenerate window containing no entry: animation gating must fail
    /// open rather than mute the healing redraws (see
    /// `entry_index_in_viewport`).
    #[test]
    fn entry_index_in_viewport_fails_open_when_scrolled_past_end() {
        let _theme = pin_theme();
        let mut state = ScrollbackState::new();
        bulk_load_stubs(&mut state, 50);
        state.prepare_layout(80, 10);
        assert!(
            !state.entry_index_in_viewport(0),
            "normal bottom-pinned window still gates far-above entries"
        );

        // Wedge the offset past the end of the content.
        state.scroll_offset = state.total_height + 5;
        assert!(
            state.entry_index_in_viewport(0),
            "degenerate window must fail open, not gate repaints"
        );
    }

    #[test]
    fn lazy_scroll_up_measures_on_demand() {
        let _theme = pin_theme();
        let mut state = ScrollbackState::new();
        bulk_load_stubs(&mut state, 200);
        state.prepare_layout(80, 20);
        assert!(!measured_at(&state, 0), "top starts estimated");

        // Scroll to the very top and render again.
        state.goto_top();
        state.prepare_layout(80, 20);

        assert!(
            measured_at(&state, 0),
            "top entry measured after scrolling to it"
        );
        assert!(
            state.entry(0).unwrap().has_cached_output(),
            "top entry laid out after scrolling to it"
        );
        // Bottom entries measured earlier stay measured (monotonic).
        assert!(measured_at(&state, 199));
    }

    #[test]
    fn lazy_total_height_is_internally_consistent_and_refines_on_measure() {
        let _theme = pin_theme();
        // Mixed estimated/exact entries: total_height must equal the cache sum,
        // and measuring everything (tall viewport) must refine it upward
        // (word-wrap exact >= char-ceil estimate) while staying consistent.
        let mut state = bulk_load_wrapping(40);
        state.prepare_layout(20, 6);

        let total_mixed = state.scroll_info().2;
        assert_eq!(
            total_mixed as u32,
            cache_total(&state),
            "total_height must equal the sum of cached heights+gaps"
        );
        // The visible bottom entry is EXACT (never an estimate).
        let last = state.len() - 1;
        assert!(measured_at(&state, last));
        assert_eq!(
            state.get_cached_entry_height(last).unwrap(),
            exact_height(&state, last, 20)
        );

        // Scroll to the top with a viewport taller than the content so the whole
        // range falls in the measurement window and every entry is measured.
        state.set_scroll_offset(0);
        state.prepare_layout(20, 10_000);
        let total_exact = state.scroll_info().2;
        assert_eq!(total_exact as u32, cache_total(&state));
        assert!(
            state
                .layout_cache
                .as_ref()
                .unwrap()
                .measured
                .iter()
                .all(|&m| m),
            "all entries measured under a viewport taller than the content"
        );
        assert!(
            total_exact >= total_mixed,
            "measuring refines total upward (exact={total_exact}, mixed={total_mixed})"
        );
    }

    #[test]
    fn lazy_scroll_to_bottom_is_exact() {
        let _theme = pin_theme();
        // Resume pins to the bottom; the visible bottom must render from EXACT
        // heights and the last entry must sit flush at the content bottom.
        let mut state = bulk_load_wrapping(40);
        state.prepare_layout(20, 6);

        let last = state.len() - 1;
        assert_eq!(
            state.get_cached_entry_height(last).unwrap(),
            exact_height(&state, last, 20),
            "bottom entry must be measured exactly, not estimated"
        );
        let (scroll, vp, total) = state.scroll_info();
        assert_eq!(
            scroll,
            total.saturating_sub(vp as usize),
            "follow mode pins the viewport to the exact bottom"
        );
        // The last entry's bottom edge plus its trailing gap == total height.
        let cache = state.layout_cache.as_ref().unwrap();
        let last_bottom = cache.virtual_y[last] + cache.entries[last].height as usize;
        assert_eq!(
            last_bottom + cache.entries[last].gap_after as usize,
            total,
            "last entry ends at the content bottom (only the trailing gap follows)"
        );
    }

    #[test]
    fn lazy_live_append_is_measured_immediately() {
        let _theme = pin_theme();
        // The streaming path: a new entry appended at the bottom is visible and
        // must be measured exactly right away (not left as an estimate).
        let mut state = ScrollbackState::new();
        bulk_load_stubs(&mut state, 60);
        state.prepare_layout(80, 20);

        let id = state.push_block(RenderBlock::stub(
            "freshly appended\nsecond line",
            Color::Blue,
        ));
        let idx = state.index_of_id(id).unwrap();

        assert!(measured_at(&state, idx), "appended entry measured on push");
        assert!(
            state.entry(idx).unwrap().has_cached_output(),
            "appended entry laid out on push"
        );

        // A following render keeps it measured and at the bottom.
        state.prepare_layout(80, 20);
        assert!(measured_at(&state, idx));
        assert!(state.get_cached_entry_height(idx).unwrap() > 0);
    }

    #[test]
    fn lazy_width_change_re_estimates_then_measures_viewport() {
        let _theme = pin_theme();
        // A width change invalidates everything; the rebuild must re-estimate
        // (not re-render all) and only the new viewport is laid out exactly.
        let mut state = ScrollbackState::new();
        bulk_load_stubs(&mut state, 200);
        state.prepare_layout(80, 20);
        assert!(measured_at(&state, 199));

        // Resize: full rebuild at the new width.
        state.prepare_layout(100, 20);
        assert!(
            !measured_at(&state, 0),
            "off-screen entries re-estimated after resize, not all re-rendered"
        );
        assert!(
            measured_at(&state, 199),
            "viewport re-measured at new width"
        );
        assert!(laid_out_count(&state) < state.len());
    }

    /// Screen row (relative to the viewport top) of entry `idx`, from the cache.
    fn screen_row_of(state: &ScrollbackState, idx: usize) -> i64 {
        let cache = state.layout_cache.as_ref().unwrap();
        let range = state.visible_entry_range();
        let base_y = cache.virtual_y[range.start] as i64;
        cache.virtual_y[idx] as i64 - base_y - state.scroll_offset as i64
    }

    /// Independent total-height oracle: Σ exact `desired_height` + structural gap
    /// over the visible range. NOT a re-sum of the cache, so it catches a cache
    /// that is internally consistent but built from wrong (estimated) heights.
    fn exact_total_oracle(state: &ScrollbackState, width: u16) -> u32 {
        let range = state.visible_entry_range();
        let cache = state.layout_cache.as_ref().unwrap();
        range
            .map(|i| exact_height(state, i, width) as u32 + cache.entries[i].gap_after as u32)
            .sum()
    }

    #[test]
    fn lazy_scroll_to_entry_center_keeps_target_centered() {
        let _theme = pin_theme();
        // Regression for the off-screen-center drift: an estimated target was
        // positioned from estimated offsets and the next settle (which only
        // re-pins top/bottom) left it off-center. With the target region measured
        // first, the target sits at the exact viewport center and stays there.
        let mut state = bulk_load_wrapping(60);
        state.prepare_layout(20, 8); // bottom-pinned; target is off-screen
        let target = 15;
        assert!(
            !measured_at(&state, target),
            "target starts estimated/off-screen"
        );

        state.scroll_to_entry_center(target);
        state.prepare_layout(20, 8); // settle runs here; target must NOT drift

        assert!(measured_at(&state, target), "target measured exactly");
        assert_eq!(
            screen_row_of(&state, target),
            (8 / 2) as i64,
            "centered target stays at the viewport center after settle"
        );
    }

    #[test]
    fn lazy_scroll_to_entry_top_lands_at_top() {
        let _theme = pin_theme();
        let mut state = bulk_load_wrapping(60);
        state.prepare_layout(20, 8);
        let target = 20;

        state.scroll_to_entry_top(target);
        state.prepare_layout(20, 8);

        assert!(measured_at(&state, target));
        assert_eq!(
            screen_row_of(&state, target),
            0,
            "target lands (and stays) at the viewport top"
        );
    }

    /// State for the resize-preservation tests: a block of long, wrapping agent
    /// messages (re-wrap to different row counts per width) above a run of short,
    /// non-wrapping ones (stable at any width). Anchoring a short entry past the
    /// wrapping block isolates the resize jump to the (changing) content above it.
    fn resize_anchor_state() -> ScrollbackState {
        use crate::appearance::AppearanceConfig;
        let mut state = ScrollbackState::new();
        state.set_appearance(AppearanceConfig {
            show_timestamps: false,
            ..Default::default()
        });
        state.begin_batch();
        for i in 0..8 {
            state.push_block(RenderBlock::agent_message(format!(
                "wrap{i} {}",
                "alpha bravo charlie delta echo foxtrot golf hotel ".repeat(3)
            )));
        }
        for i in 0..40 {
            state.push_block(RenderBlock::agent_message(format!("short-{i:02}")));
        }
        state.end_batch();
        state
    }

    /// A width-only resize while scrolled into the middle (NOT following) must
    /// keep the anchored content at the viewport top: the wrapped-row count above
    /// the anchor changes, but the content the user is looking at stays put.
    fn assert_resize_keeps_anchor_at_top(from_width: u16, to_width: u16) {
        let mut state = resize_anchor_state();
        let height = 20u16;
        let anchor = 10usize; // a short, non-wrapping entry past the wrapping block

        state.prepare_layout(from_width, height);
        let top = {
            let range = state.visible_entry_range();
            let vy = state.get_cached_virtual_y().unwrap();
            vy[anchor] - vy[range.start]
        };
        state.set_scroll_offset(top);
        state.prepare_layout(from_width, height); // settle the "before" layout

        assert!(!state.is_follow_mode());
        assert!(
            state.scroll_offset() > 0,
            "must be scrolled into the middle (the bug regime)"
        );
        assert_eq!(
            screen_row_of(&state, anchor),
            0,
            "anchor at the viewport top before the {from_width}->{to_width} resize"
        );

        state.prepare_layout(to_width, height);
        assert_eq!(
            screen_row_of(&state, anchor),
            0,
            "anchor stays at the viewport top across the {from_width}->{to_width} resize"
        );
    }

    #[test]
    fn resize_narrower_preserves_scroll_when_not_following() {
        let _theme = pin_theme();
        assert_resize_keeps_anchor_at_top(80, 40);
    }

    #[test]
    fn resize_wider_preserves_scroll_when_not_following() {
        let _theme = pin_theme();
        assert_resize_keeps_anchor_at_top(40, 80);
    }

    /// Follow mode re-pins to the bottom every frame, so a resize must leave it
    /// pinned (the fix only touches the not-following path).
    #[test]
    fn resize_keeps_follow_mode_pinned_to_bottom() {
        let _theme = pin_theme();
        let mut state = resize_anchor_state();
        let height = 20u16;

        state.prepare_layout(80, height);
        assert!(state.is_follow_mode(), "default state follows new content");
        let max_before = state.total_height.saturating_sub(height as usize);
        assert_eq!(
            state.scroll_offset(),
            max_before,
            "pinned to bottom before resize"
        );

        state.prepare_layout(40, height);
        assert!(state.is_follow_mode(), "still following after resize");
        let max_after = state.total_height.saturating_sub(height as usize);
        assert_eq!(
            state.scroll_offset(),
            max_after,
            "still pinned to bottom after resize"
        );
    }

    fn no_vpad_no_sticky_state() -> ScrollbackState {
        use crate::appearance::AppearanceConfig;
        let mut state = ScrollbackState::new();
        let mut appearance = AppearanceConfig {
            show_timestamps: false,
            ..Default::default()
        };
        appearance.scrollback.blocks.prompt.vpad = false;
        appearance.scrollback.display.sticky_headers = false;
        state.set_appearance(appearance);
        state
    }

    fn push_anchor_fillers(state: &mut ScrollbackState, n: usize) {
        for i in 0..n {
            state.push_block(agent_block(&format!("filler-{i}")));
        }
    }

    /// Measure `id` exactly (scroll-to-top runs the target measure path), then
    /// leave it measured even after later parking a downstream marker.
    fn measure_entry_exact(
        state: &mut ScrollbackState,
        id: EntryId,
        width: u16,
        height: u16,
    ) -> usize {
        state.prepare_layout(width, height);
        let idx = state.index_of_id(id).expect("entry must exist");
        state.scroll_to_entry_top(idx);
        state.prepare_layout(width, height);
        let idx = state.index_of_id(id).expect("entry must exist");
        assert!(
            measured_at(state, idx),
            "precondition: entry {idx} must be exactly measured"
        );
        idx
    }

    fn park_entry_at_row_zero(
        state: &mut ScrollbackState,
        id: EntryId,
        width: u16,
        height: u16,
    ) -> usize {
        state.prepare_layout(width, height);
        let idx = state.index_of_id(id).expect("park target must exist");
        state.scroll_to_entry_top(idx);
        state.prepare_layout(width, height);
        let idx = state.index_of_id(id).expect("park target must exist");
        assert!(
            !state.is_follow_mode(),
            "parking at the viewport top must disable follow"
        );
        assert_eq!(
            screen_row_of(state, idx),
            0,
            "precondition: parked entry must sit at screen row 0"
        );
        idx
    }

    /// Growing a streaming entry above a manually parked viewport must keep that
    /// marker at screen row 0. Missing semantic-anchor compensation lets
    /// `patch_virtual_y_for_dirty` shift later `virtual_y` while `scroll_offset`
    /// stays put, so the marker jolts downward.
    #[test]
    fn growing_entry_above_manual_viewport_keeps_marker_at_screen_row_zero() {
        let _theme = pin_theme();
        const W: u16 = 80;
        const H: u16 = 12;
        let mut state = no_vpad_no_sticky_state();

        let stream_id = state.start_streaming_agent();
        assert!(state.push_chunk_to_agent(stream_id, "seed paragraph\n\n"));
        let marker_id = state.push_block(agent_block("SCROLL-ANCHOR-MARKER"));
        push_anchor_fillers(&mut state, 40);

        measure_entry_exact(&mut state, stream_id, W, H);
        park_entry_at_row_zero(&mut state, marker_id, W, H);

        let stream_idx = state.index_of_id(stream_id).unwrap();
        let marker_idx = state.index_of_id(marker_id).unwrap();
        assert!(
            stream_idx < marker_idx,
            "precondition: streaming entry must sit above the marker"
        );
        assert!(
            measured_at(&state, stream_idx),
            "precondition: upstream streaming entry must stay exactly measured after park"
        );
        let height_before = state.get_cached_entry_height(stream_idx).unwrap();
        let marker_vy_before = state.get_cached_virtual_y().unwrap()[marker_idx];
        let scroll_before = state.scroll_offset();

        for i in 0..20 {
            assert!(state.push_chunk_to_agent(stream_id, &format!("grow paragraph {i}\n\n")));
        }
        state.prepare_layout(W, H);

        let stream_idx = state.index_of_id(stream_id).unwrap();
        let height_after = state.get_cached_entry_height(stream_idx).unwrap();
        assert!(
            height_after > height_before,
            "precondition: exactly-measured upstream streaming entry must grow \
             (before={height_before}, after={height_after})"
        );
        assert!(
            measured_at(&state, stream_idx),
            "upstream entry must remain exactly measured after growth"
        );

        let marker_idx = state
            .index_of_id(marker_id)
            .expect("marker EntryId must survive growth");
        let marker_vy_after = state.get_cached_virtual_y().unwrap()[marker_idx];
        let row = screen_row_of(&state, marker_idx);
        assert_eq!(
            row,
            0,
            "manual viewport must keep marker at screen row 0 after upstream \
             streaming growth; observed row {row} (delta {row} from row 0); \
             marker virtual_y {marker_vy_before} → {marker_vy_after}, \
             scroll_offset {scroll_before} → {}",
            state.scroll_offset()
        );
    }

    /// Removing a multi-row entry above a manually parked viewport (edit
    /// coalesce / collapse) must keep that marker at screen row 0. A full-cache
    /// rebuild that leaves `scroll_offset` uncompensated jolts the marker.
    /// Negative `screen_row_of` is valid failure evidence (marker now above the
    /// viewport); this must not panic or clamp-to-top into a false pass.
    #[test]
    fn removing_entry_above_manual_viewport_keeps_marker_at_screen_row_zero() {
        let _theme = pin_theme();
        const W: u16 = 80;
        const H: u16 = 12;
        let mut state = no_vpad_no_sticky_state();

        let removable_id = state.push_block(RenderBlock::stub(
            (0..12)
                .map(|i| format!("removable-line-{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            Color::Blue,
        ));
        let marker_id = state.push_block(agent_block("SCROLL-ANCHOR-MARKER"));
        push_anchor_fillers(&mut state, 40);

        let removable_idx = measure_entry_exact(&mut state, removable_id, W, H);
        let removable_h = state.get_cached_entry_height(removable_idx).unwrap();
        assert!(
            removable_h > 1,
            "precondition: removable entry must be multi-row (height={removable_h})"
        );
        park_entry_at_row_zero(&mut state, marker_id, W, H);
        assert!(
            measured_at(&state, state.index_of_id(removable_id).unwrap()),
            "precondition: removable entry must stay exactly measured after park"
        );

        let marker_idx = state.index_of_id(marker_id).unwrap();
        let prefix_before = state.get_cached_virtual_y().unwrap()[marker_idx];
        let scroll_before = state.scroll_offset();
        assert!(
            prefix_before > 0,
            "precondition: removable prefix must occupy rows above the marker"
        );

        assert!(state.remove_entry(removable_id));
        state.prepare_layout(W, H);

        assert!(
            state.total_height > H as usize,
            "post-removal transcript must still overflow the viewport \
             (total={}, vh={H}); max_offset=0 would clamp to row 0 and fake a pass",
            state.total_height
        );

        let marker_idx = state
            .index_of_id(marker_id)
            .expect("marker EntryId must survive removal");
        let prefix_after = state.get_cached_virtual_y().unwrap()[marker_idx];
        assert!(
            prefix_before > prefix_after,
            "precondition: real prefix height must be removed \
             (before={prefix_before}, after={prefix_after})"
        );

        let row = screen_row_of(&state, marker_idx);
        assert_eq!(
            row,
            0,
            "manual viewport must keep marker at screen row 0 after upstream \
             removal; observed row {row} (negative = marker above viewport); \
             prefix virtual_y {prefix_before} → {prefix_after}, \
             scroll_offset {scroll_before} → {}",
            state.scroll_offset()
        );
    }

    /// Removing the viewport-top entry itself must fall back deterministically:
    /// the next surviving entry gets pinned to the vacated viewport-top row
    /// instead of the stale offset landing on whatever content drifted there.
    #[test]
    fn removing_viewport_top_entry_pins_next_survivor_at_screen_row_zero() {
        let _theme = pin_theme();
        const W: u16 = 80;
        const H: u16 = 12;
        let mut state = no_vpad_no_sticky_state();

        state.push_block(agent_block("above-the-marker"));
        let marker_id = state.push_block(agent_block("SCROLL-ANCHOR-MARKER"));
        let survivor_id = state.push_block(agent_block("next-survivor"));
        push_anchor_fillers(&mut state, 40);

        park_entry_at_row_zero(&mut state, marker_id, W, H);

        assert!(state.remove_entry(marker_id));
        state.prepare_layout(W, H);

        assert!(
            state.total_height > H as usize,
            "post-removal transcript must still overflow the viewport \
             (total={}, vh={H}); max_offset=0 would clamp to row 0 and fake a pass",
            state.total_height
        );
        let survivor_idx = state
            .index_of_id(survivor_id)
            .expect("survivor EntryId must exist");
        assert_eq!(
            screen_row_of(&state, survivor_idx),
            0,
            "next surviving entry must be pinned to the vacated viewport top \
             (scroll_offset {})",
            state.scroll_offset()
        );
    }

    /// A content mutation below a manually parked viewport must not move it:
    /// `scroll_offset` stays put and the parked marker keeps its screen row.
    #[test]
    fn growth_below_manual_viewport_leaves_scroll_offset_unchanged() {
        let _theme = pin_theme();
        const W: u16 = 80;
        const H: u16 = 12;
        let mut state = no_vpad_no_sticky_state();

        state.push_block(agent_block("above-the-marker"));
        let marker_id = state.push_block(agent_block("SCROLL-ANCHOR-MARKER"));
        push_anchor_fillers(&mut state, 40);
        let stream_id = state.start_streaming_agent();
        assert!(state.push_chunk_to_agent(stream_id, "seed paragraph\n\n"));

        park_entry_at_row_zero(&mut state, marker_id, W, H);
        let marker_idx = state.index_of_id(marker_id).unwrap();
        let stream_idx = state.index_of_id(stream_id).unwrap();
        assert!(
            marker_idx < stream_idx,
            "precondition: streaming entry must sit below the marker"
        );
        let scroll_before = state.scroll_offset();

        for i in 0..10 {
            assert!(state.push_chunk_to_agent(stream_id, &format!("tail growth {i}\n\n")));
        }
        state.prepare_layout(W, H);

        assert_eq!(
            state.scroll_offset(),
            scroll_before,
            "growth below the viewport must not move a manually parked viewport"
        );
        let marker_idx = state.index_of_id(marker_id).unwrap();
        assert_eq!(
            screen_row_of(&state, marker_idx),
            0,
            "marker must hold screen row 0 while content grows below"
        );
    }

    /// A viewport top parked on the inter-entry GAP row (attributed to the
    /// entry above, per `entry_at_virtual_row`) must also be a strict no-op
    /// under below-viewport growth: the span-based re-pin keeps the gap row a
    /// gap row instead of clamping it onto the owner's last content row.
    #[test]
    fn growth_below_gap_parked_viewport_keeps_gap_row_at_top() {
        let _theme = pin_theme();
        const W: u16 = 80;
        const H: u16 = 12;
        let mut state = no_vpad_no_sticky_state();

        let gap_owner_id = state.push_block(RenderBlock::stub(
            "gap-owner-0\ngap-owner-1\ngap-owner-2".to_string(),
            Color::Blue,
        ));
        let below_id = state.push_block(agent_block("first-below-the-gap"));
        push_anchor_fillers(&mut state, 40);
        let stream_id = state.start_streaming_agent();
        assert!(state.push_chunk_to_agent(stream_id, "seed paragraph\n\n"));

        // Park the viewport top exactly on the 1-row gap after the owner.
        measure_entry_exact(&mut state, gap_owner_id, W, H);
        let below_idx = state.index_of_id(below_id).unwrap();
        let gap_top = {
            let range = state.visible_entry_range();
            let vy = state.get_cached_virtual_y().unwrap();
            (vy[below_idx] - vy[range.start]) - 1
        };
        state.set_scroll_offset(gap_top);
        state.prepare_layout(W, H);
        assert!(
            !state.is_follow_mode() && state.scroll_offset() == gap_top && gap_top > 0,
            "precondition: manually parked on the gap row (offset {gap_top})"
        );
        assert_eq!(
            screen_row_of(&state, below_idx),
            1,
            "precondition: viewport top is the gap row (next entry at row 1)"
        );

        for i in 0..10 {
            assert!(state.push_chunk_to_agent(stream_id, &format!("tail growth {i}\n\n")));
        }
        state.prepare_layout(W, H);

        assert_eq!(
            state.scroll_offset(),
            gap_top,
            "below-viewport growth must not move a gap-parked viewport"
        );
        assert_eq!(
            screen_row_of(&state, state.index_of_id(below_id).unwrap()),
            1,
            "the entry below the gap must stay at screen row 1"
        );
    }

    /// Same-width full rebuild (upstream removal) with the viewport parked
    /// several rows INTO a word-wrapping entry: the re-pin must measure the
    /// anchor entry exactly instead of clamping the exact row offset against
    /// the rebuild's transient (smaller) estimate, which would jump upward
    /// within an entry whose own content never changed.
    #[test]
    fn removal_above_wrapped_park_keeps_row_inside_wrapping_entry() {
        let _theme = pin_theme();
        const W: u16 = 20;
        const H: u16 = 12;
        let mut state = no_vpad_no_sticky_state();

        let removable_id = state.push_block(RenderBlock::stub(
            (0..6)
                .map(|i| format!("rm-{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            Color::Blue,
        ));
        // One long paragraph of words too wide to pair up on a 20-col line:
        // word-wrap burns ~half of each line, so the char-ceil estimate
        // undershoots the exact wrapped height — what makes the clamp bite.
        let wrap_id = state.push_block(agent_block(&"aaaaaaaaaaa ".repeat(30)));
        push_anchor_fillers(&mut state, 60);

        measure_entry_exact(&mut state, removable_id, W, H);
        let wrap_idx = measure_entry_exact(&mut state, wrap_id, W, H);
        let exact = state.get_cached_entry_height(wrap_idx).unwrap() as usize;
        let estimate = {
            let theme = Theme::current();
            let entry = state.entry(wrap_idx).unwrap();
            EntryRenderer::new(entry, &theme)
                .with_appearance_ref(state.appearance())
                .with_cwd(state.cwd())
                .estimate_height(state.entry_area_width(W)) as usize
        };
        // Park deeper into the entry than the estimate reaches, so a clamp
        // against the transient estimate would provably move the row.
        let rows_into = exact - 2;
        assert!(
            estimate < rows_into,
            "precondition: rebuild estimate ({estimate}) must undershoot the \
             park depth ({rows_into} of {exact} exact rows) or the clamp \
             cannot bite"
        );
        state.set_scroll_offset(state.scroll_offset() + rows_into);
        state.prepare_layout(W, H);
        assert_eq!(
            screen_row_of(&state, wrap_idx),
            -(rows_into as i64),
            "precondition: parked {rows_into} rows into the wrapping entry"
        );

        assert!(state.remove_entry(removable_id));
        state.prepare_layout(W, H);

        let wrap_idx = state.index_of_id(wrap_id).unwrap();
        assert!(
            measured_at(&state, wrap_idx),
            "anchor entry must be measured exactly before the re-pin clamps"
        );
        assert_eq!(
            screen_row_of(&state, wrap_idx),
            -(rows_into as i64),
            "removal above must keep the same wrapped row of the anchor \
             entry at the viewport top (exact {exact}, estimate {estimate})"
        );
    }

    /// The anchored entry AND its immediate successor both removed before the
    /// next frame (edit coalescing, reconnect cleanup): the armed anchor must
    /// keep migrating to the first later survivor rather than giving up after
    /// one hop.
    #[test]
    fn removing_top_entry_and_successor_pins_first_later_survivor() {
        let _theme = pin_theme();
        const W: u16 = 80;
        const H: u16 = 12;
        let mut state = no_vpad_no_sticky_state();

        state.push_block(agent_block("above-the-marker"));
        let marker_id = state.push_block(agent_block("SCROLL-ANCHOR-MARKER"));
        let successor_id = state.push_block(agent_block("immediate-successor"));
        let survivor_id = state.push_block(agent_block("first-later-survivor"));
        push_anchor_fillers(&mut state, 40);

        park_entry_at_row_zero(&mut state, marker_id, W, H);

        assert!(state.remove_entry(marker_id));
        assert!(state.remove_entry(successor_id));
        state.prepare_layout(W, H);

        assert!(
            state.total_height > H as usize,
            "post-removal transcript must still overflow the viewport \
             (total={}, vh={H})",
            state.total_height
        );
        let survivor_idx = state.index_of_id(survivor_id).expect("survivor must exist");
        assert_eq!(
            screen_row_of(&state, survivor_idx),
            0,
            "anchor must migrate past BOTH removals to the first later \
             survivor (scroll_offset {})",
            state.scroll_offset()
        );
    }

    /// Index of the entry the viewport top currently sits in (gap rows attribute
    /// to the entry above, matching `entry_at_virtual_row`).
    fn entry_at_top(state: &ScrollbackState) -> usize {
        let range = state.visible_entry_range();
        let vy = state.get_cached_virtual_y().unwrap();
        let top = vy[range.start] + state.scroll_offset();
        vy.partition_point(|&y| y <= top).saturating_sub(1)
    }

    /// A mid-paragraph anchor (`sub_rows > 0`) whose own logical line RE-WRAPS
    /// shorter on widen: the intra-line clamp must keep the viewport top inside
    /// the anchor entry instead of letting the stale row count spill past it into
    /// a later entry.
    #[test]
    fn resize_clamps_subrow_within_rewrapping_anchor_line() {
        let _theme = pin_theme();
        let mut state = resize_anchor_state();
        let height = 12u16;
        let narrow = 30u16;
        let wide = 120u16;
        let anchor = 0usize; // a long wrapping entry (one wrapping logical line)

        state.prepare_layout(narrow, height);
        // Measure the anchor exactly by putting it at the top, then park the
        // viewport top at its LAST row — deep inside the wrapping line.
        state.set_scroll_offset(0);
        state.prepare_layout(narrow, height);
        let anchor_h = state.get_cached_entry_height(anchor).unwrap() as usize;
        state.set_scroll_offset(anchor_h.saturating_sub(1));
        state.prepare_layout(narrow, height);

        assert!(state.scroll_offset() > 0 && !state.is_follow_mode());
        assert_eq!(
            entry_at_top(&state),
            anchor,
            "anchor entry at the top before widen"
        );
        assert!(
            screen_row_of(&state, anchor) < 0,
            "viewport top is mid-paragraph (sub_rows > 0), not at the entry's top"
        );

        // Widen: the anchor paragraph re-wraps to far fewer rows. Without the
        // clamp the stale `sub_rows` would push the top past the entry; the clamp
        // keeps it inside.
        state.prepare_layout(wide, height);
        assert_eq!(
            entry_at_top(&state),
            anchor,
            "anchor entry still at the top after widen (sub_rows clamped within its line)"
        );
        assert!(
            screen_row_of(&state, anchor) < 0,
            "viewport top still inside the (now shorter) anchor line"
        );
    }

    /// Viewport top parked in the 1-row inter-entry GAP: capture attributes the
    /// gap to the entry above (via `entry_at_virtual_row`), and a resize keeps
    /// that content anchored within tolerance — exercising the gap path.
    #[test]
    fn resize_anchors_gap_row_to_entry_above() {
        let _theme = pin_theme();
        let mut state = resize_anchor_state();
        let height = 20u16;
        let anchor = 10usize; // short, non-wrapping; the gap after it is 1 row

        state.prepare_layout(80, height);
        // Park the top on the 1-row gap after entry 10 (the row just before 11).
        let gap_top = {
            let range = state.visible_entry_range();
            let vy = state.get_cached_virtual_y().unwrap();
            (vy[anchor + 1] - vy[range.start]) - 1
        };
        state.set_scroll_offset(gap_top);
        state.prepare_layout(80, height);

        assert!(state.scroll_offset() > 0 && !state.is_follow_mode());
        assert_eq!(
            entry_at_top(&state),
            anchor,
            "gap row attributes to the entry above"
        );
        assert_eq!(
            screen_row_of(&state, anchor + 1),
            1,
            "next entry sits just below the gap row at the top"
        );
        let before = screen_row_of(&state, anchor);

        // Resize narrower: the wrapping block above grows; the gap anchor must
        // keep entry 10 within tolerance rather than jumping with the stale offset.
        state.prepare_layout(40, height);
        assert_eq!(
            entry_at_top(&state),
            anchor,
            "still anchored to entry 10 after resize"
        );
        assert!(
            (screen_row_of(&state, anchor) - before).abs() <= 2,
            "gap-anchored entry stays within tolerance ({} -> {})",
            before,
            screen_row_of(&state, anchor)
        );
    }

    #[test]
    fn lazy_measurement_window_boundaries_are_exact() {
        let _theme = pin_theme();
        let mut state = ScrollbackState::new();
        bulk_load_stubs(&mut state, 200);
        let viewport = 20usize;
        state.prepare_layout(80, viewport as u16);

        // Derive the uniform stub stride from a measured entry (height + the
        // trailing gap of 1) rather than hard-coding it.
        let stride = state.get_cached_entry_height(199).unwrap() as usize + 1;
        let top_idx = 100usize;

        // Put entry `top_idx` exactly at the viewport top (virtual_y[k] = k*stride).
        state.set_scroll_offset(top_idx * stride);
        state.prepare_layout(80, viewport as u16);

        // Window = [first_visible ..= last_visible + MEASURE_MARGIN_ENTRIES], with
        // NO above-margin. last_visible is the last entry starting before bottom.
        let bottom = top_idx * stride + viewport;
        let last_visible = (bottom - 1) / stride;
        let win_end = (last_visible + MEASURE_MARGIN_ENTRIES).min(state.len() - 1);

        for idx in top_idx..=win_end {
            assert!(measured_at(&state, idx), "entry {idx} (in window) measured");
        }
        assert!(
            !measured_at(&state, top_idx - 1),
            "first_visible-1 NOT measured (no above-margin keeps the top anchored)"
        );
        assert!(
            !measured_at(&state, win_end + 1),
            "beyond the below-margin NOT measured"
        );
    }

    #[test]
    fn lazy_second_prepare_layout_is_a_noop() {
        let _theme = pin_theme();
        // settle converges: a second prepare_layout with identical dims must not
        // move scroll / total / the measured set, for follow and for mid-scroll.
        let mut follow = bulk_load_wrapping(40);
        follow.prepare_layout(20, 8);
        let (s, _, t) = follow.scroll_info();
        let measured = follow.layout_cache.as_ref().unwrap().measured.clone();
        follow.prepare_layout(20, 8);
        assert_eq!(
            follow.scroll_info(),
            (s, 8, t),
            "follow: stable scroll/total"
        );
        assert_eq!(
            follow.layout_cache.as_ref().unwrap().measured,
            measured,
            "follow: stable measured set"
        );

        let mut manual = ScrollbackState::new();
        bulk_load_stubs(&mut manual, 200);
        manual.prepare_layout(80, 20);
        manual.set_scroll_offset(600);
        manual.prepare_layout(80, 20);
        let (s2, _, t2) = manual.scroll_info();
        let measured2 = manual.layout_cache.as_ref().unwrap().measured.clone();
        manual.prepare_layout(80, 20);
        assert_eq!(
            manual.scroll_info(),
            (s2, 20, t2),
            "manual: stable scroll/total"
        );
        assert_eq!(
            manual.layout_cache.as_ref().unwrap().measured,
            measured2,
            "manual: stable measured set"
        );
    }

    #[test]
    fn lazy_total_height_matches_independent_exact_oracle() {
        let _theme = pin_theme();
        let mut state = bulk_load_wrapping(40);
        // Measure every entry: scroll to top with a viewport taller than content.
        state.set_scroll_offset(0);
        state.prepare_layout(20, 10_000);

        assert!(
            state
                .layout_cache
                .as_ref()
                .unwrap()
                .measured
                .iter()
                .all(|&m| m),
            "tall viewport measures all entries"
        );
        let oracle = exact_total_oracle(&state, 20).min(u16::MAX as u32);
        assert_eq!(
            state.scroll_info().2 as u32,
            oracle,
            "total_height equals the independent Σ-exact oracle"
        );
        // cached == exact for several entries, not just the last.
        for idx in [0usize, 9, 21, 33, 39] {
            assert_eq!(
                state.get_cached_entry_height(idx).unwrap(),
                exact_height(&state, idx, 20),
                "entry {idx} cached height is exact"
            );
        }
    }

    #[test]
    fn lazy_empty_scrollback_and_oversized_viewport() {
        let _theme = pin_theme();
        // Empty: no panic, zero height.
        let mut empty = ScrollbackState::new();
        empty.prepare_layout(80, 20);
        assert_eq!(empty.scroll_info().2, 0);

        // Viewport taller than content: stays at the top, everything measured.
        let mut small = bulk_load_wrapping(5);
        small.prepare_layout(20, 1000);
        assert_eq!(small.scroll_offset, 0, "no scroll when content < viewport");
        assert!(
            small
                .layout_cache
                .as_ref()
                .unwrap()
                .measured
                .iter()
                .all(|&m| m)
        );

        // Single entry taller than the viewport: measured, no panic, pinned bottom.
        let mut tall = ScrollbackState::new();
        tall.push_block(RenderBlock::stub(
            "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8",
            Color::Blue,
        ));
        tall.prepare_layout(80, 3);
        assert!(measured_at(&tall, 0));
        let (scroll, vp, total) = tall.scroll_info();
        assert_eq!(
            scroll,
            total.saturating_sub(vp as usize),
            "pinned to the bottom"
        );
    }

    /// A long session can render past 65 535 rows, and the bottom must
    /// stay reachable.
    ///
    /// Before the fix, `ScrollbackState::scroll_offset`/`total_height` were
    /// `u16` and `compute_total_height_from_cache` capped the total at
    /// `u16::MAX`, so once content exceeded 65 535 rows `goto_bottom` could not
    /// scroll past that ceiling and the final entries were stranded. With the
    /// cumulative scroll state widened to `usize`, the full height is preserved
    /// and the last entry is on screen at the bottom.
    ///
    /// This test FAILS pre-fix: `total_height` saturates at 65 535, so the
    /// `total_height > 65_535` assertion fails (and the last entry would sit
    /// below the reachable `scroll_offset`).
    #[test]
    fn goto_bottom_reaches_end_past_u16_max_rows_gb3236() {
        let _theme = pin_theme();
        let mut state = ScrollbackState::new();

        // Stub blocks render one screen row per source line (no markdown
        // soft-wrapping) and are not collapsed off-screen, so their height
        // ESTIMATE is the full line count and counts toward total_height.
        // ~400 entries of ~200 lines each → ~80 000 rows, comfortably past
        // u16::MAX (65 535).
        let body = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        for _ in 0..400 {
            state.push_block(stub_block(&body));
        }

        let width = 100u16;
        let height = 40u16;
        state.prepare_layout(width, height);

        let (_, _, total_height) = state.scroll_info();
        // Pre-fix this saturated at u16::MAX, so the assert failed.
        assert!(
            total_height > 65_535,
            "total_height should exceed the old u16 cap, got {total_height}"
        );

        // Pin to the bottom and confirm the final rows are actually on screen.
        state.goto_bottom();
        let (scroll_offset, viewport_height, total_height) = state.scroll_info();
        assert!(
            scroll_offset + viewport_height as usize >= total_height,
            "bottom unreachable: scroll_offset({scroll_offset}) + viewport({viewport_height}) \
             < total_height({total_height})"
        );
        // The scroll position itself is past the old u16 ceiling — direct proof
        // that content below row 65 535 is now reachable.
        assert!(
            scroll_offset > 65_535,
            "scroll_offset should be past the old u16 cap, got {scroll_offset}"
        );

        // The last entry's painted rows overlap the viewport (it is on screen).
        let virtual_y = state.get_cached_virtual_y().expect("layout cache");
        let last = state.len() - 1;
        let last_top = virtual_y[last];
        let last_height = state.get_cached_entry_height(last).expect("cached height") as usize;
        let viewport_bottom = scroll_offset + viewport_height as usize;
        assert!(
            last_top < viewport_bottom && last_top + last_height > scroll_offset,
            "last entry [{last_top}, {}) must overlap viewport [{scroll_offset}, {viewport_bottom})",
            last_top + last_height
        );
    }

    #[test]
    fn lazy_dirty_case2_settle_measures_revealed_region() {
        let _theme = pin_theme();
        // A streaming chunk (Case 2: dirty heights, cache kept) while scrolled up
        // into an unmeasured region must still measure the visible region.
        let mut state = bulk_load_wrapping(200);
        state.prepare_layout(20, 10); // measures only the bottom

        // Scroll up WITHOUT a render so the middle stays estimated.
        state.set_scroll_offset(300);
        let (win_start, _) = state.measurement_window().unwrap();
        assert!(
            !measured_at(&state, win_start),
            "visible region still estimated before the dirty frame"
        );

        // Dirty entry 0 (off-screen) to take the Case 2 path on the next frame.
        let id = state.entry(0).unwrap().id;
        assert!(state.push_chunk_to_agent_deferred(id, "more"));
        state.prepare_layout(20, 10);

        assert!(
            measured_at(&state, win_start),
            "Case 2 settle measured the on-screen region"
        );
    }

    #[test]
    fn lazy_fold_anchor_settles_visible_region_on_estimated_session() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let _theme = pin_theme();
        // fold_selected_impl nulls the cache and rebuilds to ESTIMATES, then
        // settles the visible region exactly BEFORE its scroll-anchor math reads
        // virtual_y. This asserts that settle ran: the on-screen entries are
        // `measured` immediately after the fold (before any later prepare_layout)
        // — without the in-fold settle they'd all be estimates. (Load-bearing:
        // verified to fail when the settle at fold_selected_impl is removed.)
        let mut state = ScrollbackState::new();
        let appearance = crate::appearance::AppearanceConfig {
            show_timestamps: false,
            ..Default::default()
        };
        state.set_appearance(appearance);
        state.begin_batch();
        for i in 0..80 {
            let id = state.push_block(RenderBlock::thinking(format!(
                "th{i} aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd eeeeeeeeee"
            )));
            if let Some(e) = state.get_by_id_mut(id) {
                e.set_display_mode(DisplayMode::Expanded);
            }
        }
        state.end_batch();
        state.prepare_layout(20, 12);

        let target = 40;
        state.scroll_to_entry_top(target);
        state.prepare_layout(20, 12);
        assert_eq!(
            screen_row_of(&state, target),
            0,
            "target at top before fold"
        );

        // Fold (collapse) the target. Do NOT prepare_layout after — that would
        // settle and mask whether the in-fold settle ran.
        state.set_selected(Some(target));
        state.toggle_fold_selected();
        assert!(
            measured_at(&state, target),
            "in-fold settle measured the folded entry (else cache is all estimates)"
        );
        let below = state.first_visible_entry().unwrap() + 1;
        assert!(
            measured_at(&state, below),
            "in-fold settle measured the rest of the visible region"
        );
        assert_eq!(
            screen_row_of(&state, target),
            0,
            "anchored fold keeps the entry at its screen row"
        );

        // Unfold (expand) again — settle must re-measure and the anchor hold.
        state.toggle_fold_selected();
        assert!(
            measured_at(&state, target),
            "in-fold settle measured after unfold"
        );
        assert_eq!(
            screen_row_of(&state, target),
            0,
            "anchored unfold keeps the entry at its screen row"
        );
    }

    #[test]
    fn lazy_ensure_selected_visible_does_not_jump_on_upward_nav() {
        let _theme = pin_theme();
        // Regression: routing `ensure_selected_visible` through a SYMMETRIC
        // measure (above + below) and rebuilding virtual_y, while its
        // fully-visible early return leaves scroll_offset unchanged, jumped the
        // viewport on `k`. Measuring downward-only keeps the top anchored.
        let mut state = bulk_load_wrapping(80);
        state.prepare_layout(20, 20);
        // Position the viewport in the middle so entries above the top stay
        // estimated (only the visible window + below-margin gets measured).
        state.set_scroll_offset(200);
        state.prepare_layout(20, 20);

        let top = state.first_visible_entry().unwrap();
        assert!(top >= 2, "need estimated entries above the viewport top");
        assert!(
            !measured_at(&state, top - 1),
            "entries above the top are estimated (would shift virtual_y if measured)"
        );
        let top_row_before = screen_row_of(&state, top);

        // Select a clearly-interior, fully-visible entry, then navigate UP one
        // (it stays visible → ensure_selected_visible takes its early return).
        state.set_selected(Some(top + 2));
        state.select_prev();

        assert!(
            state.selected().is_some_and(|s| s < top + 2),
            "select_prev moved the selection up"
        );
        assert_eq!(
            screen_row_of(&state, top),
            top_row_before,
            "upward nav that keeps the selection visible must not shift the viewport"
        );
    }

    #[test]
    fn lazy_page_scroll_measures_revealed_entries() {
        let _theme = pin_theme();
        // PageUp onto estimated entries must measure (and lay out) them.
        let mut state = bulk_load_wrapping(120);
        state.prepare_layout(20, 12);
        let mid = 60;
        assert!(!measured_at(&state, mid), "mid is estimated at the bottom");

        for _ in 0..200 {
            if measured_at(&state, mid) {
                break;
            }
            state.page_up();
            state.prepare_layout(20, 12);
        }

        assert!(
            measured_at(&state, mid),
            "page-up measured the revealed region"
        );
        assert!(
            state.entry(mid).unwrap().has_cached_output(),
            "page-up laid out the revealed region"
        );
    }

    #[test]
    fn lazy_page_down_measures_revealed_entries() {
        let _theme = pin_theme();
        // PageDown onto estimated entries must measure (and lay out) them.
        let mut state = bulk_load_wrapping(120);
        state.prepare_layout(20, 12);
        state.goto_top();
        state.prepare_layout(20, 12);
        let mid = 60;
        assert!(!measured_at(&state, mid), "mid is estimated at the top");

        for _ in 0..200 {
            if measured_at(&state, mid) {
                break;
            }
            state.page_down();
            state.prepare_layout(20, 12);
        }

        assert!(
            measured_at(&state, mid),
            "page-down measured the revealed region"
        );
        assert!(
            state.entry(mid).unwrap().has_cached_output(),
            "page-down laid out the revealed region"
        );
    }

    #[test]
    fn lazy_ensure_selected_visible_measure_is_bounded() {
        let _theme = pin_theme();
        // An earlier fix measured [first_visible, selected] — UNBOUNDED. After
        // jumping the viewport to the top with the selection parked far below,
        // one select step must measure EXACTLY the bounded window around the new
        // (off-viewport) selection — [sel-vp, sel+vp] — never the whole prefix
        // (the O(history) freeze being removed). Asserting the exact
        // measured INDEX SPAN (not a loose global count) is both deterministic
        // and attributable: a regression to the unbounded span fails here with a
        // span mismatch, not an ambiguous count.
        let vp = 12u16;
        let mut state = bulk_load_wrapping(200);
        state.prepare_layout(20, vp);
        // Park the selection mid-session, then jump the VIEWPORT to the top
        // WITHOUT moving the selection (set_scroll_offset doesn't select).
        state.set_selected(Some(150));
        state.set_scroll_offset(0);
        state.prepare_layout(20, vp);
        assert!(
            !measured_at(&state, 150),
            "the parked selection is far off-screen / estimated"
        );
        let before = state.layout_cache.as_ref().unwrap().measured.clone();

        // One step down → ensure_selected_visible scrolls to 151 and measures
        // EXACTLY the bounded window [151-vp, 151+vp] (all 25 are plain agent
        // messages — none hidden / group headers — so the whole window flips).
        state.select_next();
        let selected = state.selected().unwrap();
        assert_eq!(selected, 151, "select_next advanced the parked selection");

        let after = &state.layout_cache.as_ref().unwrap().measured;
        let newly: Vec<usize> = (0..after.len())
            .filter(|&i| after[i] && !before[i])
            .collect();

        let lo = selected.saturating_sub(vp as usize);
        let hi = (selected + vp as usize).min(state.len() - 1);
        let expected: Vec<usize> = (lo..=hi).collect();
        assert_eq!(
            newly, expected,
            "ensure_selected_visible measured exactly the bounded window [{lo}, {hi}]"
        );
        assert_eq!(
            newly.len(),
            2 * vp as usize + 1,
            "bounded window is exactly 2*viewport + 1 entries"
        );
    }

    #[test]
    fn lazy_fold_no_anchor_does_not_jump_on_estimated_session() {
        crate::appearance::cache::set_show_thinking_blocks(true);
        let _theme = pin_theme();
        // With anchor_on_fold = false, folding must NOT measure above the viewport
        // without re-anchoring (that jumps). The top entry must stay put.
        let mut state = ScrollbackState::new();
        let mut appearance = crate::appearance::AppearanceConfig {
            show_timestamps: false,
            ..Default::default()
        };
        appearance.scrollback.scroll.anchor_on_fold = false;
        state.set_appearance(appearance);
        state.begin_batch();
        for i in 0..80 {
            let id = state.push_block(RenderBlock::thinking(format!(
                "th{i} aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd eeeeeeeeee"
            )));
            if let Some(e) = state.get_by_id_mut(id) {
                e.set_display_mode(DisplayMode::Expanded);
            }
        }
        state.end_batch();
        state.prepare_layout(20, 12);

        // Jump the viewport to a middle region WITHOUT measuring the prefix above
        // it (set_scroll_offset doesn't measure), so it stays estimated.
        state.set_scroll_offset(200);
        state.prepare_layout(20, 12);
        let top = state.first_visible_entry().unwrap();
        assert!(
            top >= 2 && !measured_at(&state, top - 1),
            "prefix above the viewport top is estimated"
        );
        let top_row_before = screen_row_of(&state, top);

        // Fold a fully-visible entry BELOW the top (collapse).
        state.set_selected(Some(top + 1));
        state.toggle_fold_selected();
        state.prepare_layout(20, 12);

        assert_eq!(
            screen_row_of(&state, top),
            top_row_before,
            "!anchor fold of a lower entry must not jump the viewport top"
        );
    }

    #[test]
    fn lazy_single_turn_center_measures_sticky_prompt() {
        let _theme = pin_theme();
        // measure_scroll_target's SingleTurn branch measures the turn's sticky
        // prompt (visible_range.start) — far above the centered target — so the
        // sticky-header height in the centering math is exact.
        let mut state = ScrollbackState::new();
        let appearance = crate::appearance::AppearanceConfig {
            show_timestamps: false,
            ..Default::default()
        };
        state.set_appearance(appearance);
        state.begin_batch();
        state.push_block(RenderBlock::user_prompt("the turn prompt"));
        for i in 0..60 {
            state.push_block(RenderBlock::agent_message(format!(
                "msg{i} aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd eeeeeeeeee"
            )));
        }
        state.end_batch();
        state.view_mode = ViewMode::SingleTurn;
        state.prepare_layout(20, 10);

        let prompt_idx = state.visible_entry_range().start;
        assert!(
            !measured_at(&state, prompt_idx),
            "sticky prompt is far above the target and starts estimated"
        );

        let target = 40;
        state.scroll_to_entry_center(target);

        assert!(
            measured_at(&state, prompt_idx),
            "SingleTurn centering measured the sticky prompt"
        );
        assert!(measured_at(&state, target), "target measured");

        // Observable result (mirrors the AllTurns center test): the target lands
        // at the viewport center, offset down by the pinned prompt's sticky
        // header. `current_sticky_layout` reports the exact header at the final
        // scroll, derived independently of the centering math under test, so a
        // centering-math regression in SingleTurn mode is caught here.
        let header = {
            let cache = state.layout_cache.as_ref().unwrap();
            let range = state.visible_entry_range();
            state
                .current_sticky_layout(cache, &range)
                .header_screen_rows() as i64
        };
        let center = (10 / 2) as i64;
        let row = screen_row_of(&state, target);
        assert!(
            (row - (center + header)).abs() <= 1,
            "centered target sits at the viewport center plus the sticky header \
             (row={row}, center={center}, header={header})"
        );
    }

    // ── Paint window (per-frame viewport sub-range) ──

    /// Build parallel `(virtual_y, layouts)` fixtures from `(height, gap_after)`
    /// rows, marking `verb_headers` indices as verb-group headers. Headers
    /// carry a nonzero `group_header_count` like every production header row,
    /// so the paint-window gate (`is_group_header`) sees them.
    fn window_fixture(
        rows: &[(u16, u16)],
        verb_headers: &[usize],
    ) -> (Vec<usize>, Vec<EntryLayoutInfo>) {
        let mut virtual_y = Vec::with_capacity(rows.len());
        let mut layouts = Vec::with_capacity(rows.len());
        let mut y = 0usize;
        for (i, &(height, gap_after)) in rows.iter().enumerate() {
            virtual_y.push(y);
            y += height as usize + gap_after as usize;
            let is_header = verb_headers.contains(&i);
            layouts.push(EntryLayoutInfo {
                height,
                gap_after,
                verb_group_header: is_header,
                group_header_count: u16::from(is_header),
                ..Default::default()
            });
        }
        (virtual_y, layouts)
    }

    #[test]
    fn compute_paint_window_straddle_backs_off_one_entry() {
        let (vy, layouts) = window_fixture(&[(3, 1); 5], &[]);
        let no_run = |_: usize| -> usize { unreachable!("no verb headers in fixture") };
        // vy = [0, 4, 8, 12, 16]; rows 5..9: entry 1 (rows 4..7) straddles the top.
        let (range, y0) = compute_paint_window(&vy, &layouts, 0..5, 5, 4, no_run);
        assert_eq!(range, 1..3);
        assert_eq!(y0, 4);
        // Rows 7..11: entry 1 ends exactly at the viewport top — no back-off.
        let (range, y0) = compute_paint_window(&vy, &layouts, 0..5, 7, 4, no_run);
        assert_eq!(range, 2..3);
        assert_eq!(y0, 8);
    }

    #[test]
    fn compute_paint_window_empty_past_content_end() {
        let (vy, layouts) = window_fixture(&[(3, 1); 5], &[]);
        let (range, y0) = compute_paint_window(&vy, &layouts, 0..5, 100, 4, |_| {
            unreachable!("no verb headers in fixture")
        });
        assert_eq!(range, 5..5);
        assert_eq!(y0, 0);
    }

    #[test]
    fn compute_paint_window_empty_visible_range() {
        let (vy, layouts) = window_fixture(&[(3, 1); 5], &[]);
        let (range, y0) = compute_paint_window(&vy, &layouts, 2..2, 0, 10, |_| {
            unreachable!("empty range never consults run_end")
        });
        assert_eq!(range, 2..2);
        assert_eq!(y0, 0);
    }

    #[test]
    fn compute_paint_window_verb_header_extends_through_run_end() {
        // Folded run: 1-row header at 2, three height-0 members, then a break.
        let rows = [(3, 1), (2, 1), (1, 0), (0, 0), (0, 0), (0, 1), (3, 1)];
        let (vy, layouts) = window_fixture(&rows, &[2]);
        // vy = [0, 4, 7, 8, 8, 8, 9]; rows 0..8 end right after the header row,
        // so every member sits past the window bottom.
        let (range, y0) = compute_paint_window(&vy, &layouts, 0..7, 0, 8, |i| {
            assert_eq!(i, 2, "run_end is only consulted for the header");
            6
        });
        assert_eq!(
            range,
            0..6,
            "window covers the full run, not just on-screen"
        );
        assert_eq!(y0, 0);
        // A run walk past the visible range is clamped to it.
        let (range, _) = compute_paint_window(&vy, &layouts, 0..4, 0, 8, |_| 100);
        assert_eq!(range, 0..4);
    }

    #[test]
    fn compute_paint_window_truncation_header_extends_through_run_end() {
        // Collapsed truncation run: count-marked header at 1 (NOT a verb
        // header — the gate must fire on `is_group_header` alone), two
        // height-0 hidden rows sharing the tail's virtual_y past the window
        // bottom, then the visible tail.
        let rows = [(3, 1), (1, 0), (0, 0), (0, 0), (1, 0), (1, 1)];
        let (vy, mut layouts) = window_fixture(&rows, &[]);
        layouts[1].group_header_count = 2;
        // vy = [0, 4, 5, 5, 5, 6]; rows 0..5 end right after the header row.
        let (range, _) = compute_paint_window(&vy, &layouts, 0..6, 0, 5, |i| {
            assert_eq!(i, 1, "run_end is only consulted for the header");
            6
        });
        assert_eq!(
            range,
            0..6,
            "window covers the hidden prefix and tail, not just on-screen"
        );
    }

    /// Wrapper + real fold: a verb-group header on the viewport's last row
    /// pulls the whole off-screen run into the paint window via the canonical
    /// `group_range_of` walk (trailing hidden thinking stays outside).
    #[test]
    fn paint_window_extends_through_offscreen_verb_group_members() {
        let _theme = pin_theme();
        crate::appearance::cache::set_group_tool_verbs(true);
        crate::appearance::cache::set_show_thinking_blocks(false);
        let mut state = ScrollbackState::new();
        for i in 0..20 {
            state.push_block(RenderBlock::agent_message(format!("filler {i}")));
        }
        let header = state.len();
        for i in 0..50 {
            state.push_block(RenderBlock::read(format!("f{i}.rs"), None));
        }
        state.push_block(RenderBlock::thinking("trailing hidden thinking"));
        state.push_block(RenderBlock::agent_message("after the run"));
        state.prepare_layout(80, 24);

        let layouts = state.get_cached_entry_layouts().unwrap();
        assert!(layouts[header].verb_group_header, "run folded to a header");
        let virtual_y = state.get_cached_virtual_y().unwrap();
        // Header row on the viewport's last row: all members are off-screen.
        let scroll = virtual_y[header] + 1 - 24;
        let (range, content_y0) = state.paint_window(0..state.len(), scroll, 24);
        assert!(
            range.start > 0 && range.contains(&header),
            "window starts mid-history and includes the header: {range:?}"
        );
        assert_eq!(content_y0, virtual_y[range.start]);
        assert_eq!(range.end, header + 50);
    }
}
