//! Hook data types and rendering helpers for tool call blocks.
//!
//! Hook runs are displayed as part of tool call blocks rather than
//! as standalone scrollback entries. The tool header comes first,
//! then pre_tool_use hooks, then post_tool_use hooks.

use std::time::Duration;

use ratatui::text::{Line, Span};

use crate::scrollback::types::{BlockLine, DisplayMode};
use crate::theme::Theme;

// ── Data types ────────────────────────────────────────────────────────

/// Status of a single hook execution within a batch.
#[derive(Debug, Clone)]
pub enum HookRunStatus {
    Success {
        elapsed: Duration,
    },
    Skipped,
    /// The hook ran and blocked (a stop-gate decision, not a failure).
    Blocked {
        detail: String,
        elapsed: Duration,
    },
    Failed {
        error: String,
        elapsed: Duration,
    },
}

/// A single hook run entry for display.
#[derive(Debug, Clone)]
pub struct HookRunEntry {
    pub name: String,
    pub status: HookRunStatus,
    /// Truncated stdout/stderr from the hook command, if any.
    pub output: Option<String>,
}

/// Which phase of tool execution the hooks belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPhase {
    Pre,
    Post,
}

/// Hook data attached to a tool call block.
#[derive(Debug, Clone, Default)]
pub struct ToolCallHookData {
    pub pre_hooks: Vec<HookRunEntry>,
    pub post_hooks: Vec<HookRunEntry>,
    /// Lifecycle hooks (session_start, session_end, stop) — rendered with their own event name.
    pub lifecycle: Vec<(String, Vec<HookRunEntry>)>,
}

impl ToolCallHookData {
    pub fn is_empty(&self) -> bool {
        self.pre_hooks.is_empty() && self.post_hooks.is_empty() && self.lifecycle.is_empty()
    }

    pub fn has_content(&self) -> bool {
        self.pre_hooks
            .iter()
            .chain(self.post_hooks.iter())
            .any(|r| !matches!(r.status, HookRunStatus::Skipped))
            || self.lifecycle.iter().any(|(_, runs)| {
                runs.iter()
                    .any(|r| !matches!(r.status, HookRunStatus::Skipped))
            })
    }
}

// ── Rendering helpers ─────────────────────────────────────────────────

const INDENT: &str = "    ";

/// Count successes and failures across all hook entries.
fn count_hooks(entries: &[&[HookRunEntry]]) -> (usize, usize) {
    let mut success = 0usize;
    let mut failed = 0usize;
    for runs in entries {
        for r in *runs {
            match r.status {
                HookRunStatus::Success { .. } | HookRunStatus::Blocked { .. } => success += 1,
                HookRunStatus::Failed { .. } => failed += 1,
                HookRunStatus::Skipped => {}
            }
        }
    }
    (success, failed)
}

/// `[hooks: N/M]` spans (green successes, red failures) with a leading
/// two-space gap. Returns `None` when nothing ran.
fn hooks_count_spans(success: usize, failed: usize) -> Option<Vec<Span<'static>>> {
    if success == 0 && failed == 0 {
        return None;
    }
    let theme = Theme::current();
    let mut spans = vec![Span::styled("  [hooks: ", theme.muted())];
    if success > 0 {
        spans.push(Span::styled(
            format!("{}", success),
            theme
                .fg(theme.accent_success)
                .add_modifier(ratatui::style::Modifier::DIM),
        ));
    }
    if success > 0 && failed > 0 {
        spans.push(Span::styled("/", theme.muted()));
    }
    if failed > 0 {
        spans.push(Span::styled(
            format!("{}", failed),
            theme
                .fg(theme.accent_error)
                .add_modifier(ratatui::style::Modifier::DIM),
        ));
    }
    spans.push(Span::styled("]", theme.muted()));
    Some(spans)
}

/// Render an inline `[hooks: N/M]` suffix to append to the tool header line.
///
/// - Green number for successes, red for failures
/// - If no errors, only show success count
/// - If no successes, only show error count
/// - Returns None if no hooks ran
pub fn render_hooks_inline_suffix(data: &ToolCallHookData) -> Option<Vec<Span<'static>>> {
    let all_runs: Vec<&[HookRunEntry]> = [data.pre_hooks.as_slice(), data.post_hooks.as_slice()]
        .into_iter()
        .chain(data.lifecycle.iter().map(|(_, runs)| runs.as_slice()))
        .collect();
    let (success, failed) = count_hooks(&all_runs);
    hooks_count_spans(success, failed)
}

/// Right-side summary for stop hooks merged onto a turn-terminal marker line:
/// `stop  [hooks: 2]` per group (bold muted event name + colored counts),
/// groups joined by two spaces. Returns `None` when nothing ran.
pub fn render_stop_hooks_summary(
    groups: &[(String, Vec<HookRunEntry>)],
) -> Option<Vec<Span<'static>>> {
    let theme = Theme::current();
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (event_name, runs) in groups {
        let (success, failed) = count_hooks(&[runs.as_slice()]);
        let Some(count_spans) = hooks_count_spans(success, failed) else {
            continue;
        };
        if !spans.is_empty() {
            spans.push(Span::styled("  ", theme.muted()));
        }
        spans.push(Span::styled(
            event_name.clone(),
            theme.muted().add_modifier(ratatui::style::Modifier::BOLD),
        ));
        spans.extend(count_spans);
    }
    if spans.is_empty() { None } else { Some(spans) }
}

/// Render a separator line between tool output and hooks.
fn render_separator() -> BlockLine {
    let theme = Theme::current();
    Line::from(vec![Span::styled(
        format!("{}\u{2500}\u{2500}\u{2500}", INDENT),
        theme.muted(),
    )])
    .into()
}

/// Render hook details as expanded lines.
///
/// Format:
///   **pre_tool_use**
///     \u2713 hook-name (12ms)
///     \u2717 hook-name (3ms): error message
///   **post_tool_use**
///     \u2713 hook-name (5ms)
fn render_hooks_expanded(event: &str, runs: &[HookRunEntry]) -> Vec<BlockLine> {
    let theme = Theme::current();
    let mut lines = Vec::new();

    // If all hooks were skipped, render nothing.
    if runs
        .iter()
        .all(|r| matches!(r.status, HookRunStatus::Skipped))
    {
        return lines;
    }

    // Header: indented, bold, muted
    lines.push(
        Line::from(vec![Span::styled(
            format!("{}{}", INDENT, event),
            theme.muted().add_modifier(ratatui::style::Modifier::BOLD),
        )])
        .into(),
    );

    // Per-hook detail lines
    lines.extend(render_hooks_expanded_inner(runs));

    lines
}

/// Render per-hook detail lines without a section header.
fn render_hooks_expanded_inner(runs: &[HookRunEntry]) -> Vec<BlockLine> {
    let theme = Theme::current();
    let mut lines = Vec::new();

    for run in runs {
        match &run.status {
            HookRunStatus::Success { elapsed } => {
                lines.push(
                    Line::from(vec![
                        Span::styled(format!("{}  ", INDENT), theme.muted()),
                        Span::styled(
                            format!("{} ", crate::glyphs::check_mark()),
                            theme.fg(theme.accent_success),
                        ),
                        Span::styled(run.name.clone(), theme.muted()),
                        Span::styled(format!(" ({}ms)", elapsed.as_millis()), theme.muted()),
                    ])
                    .into(),
                );
            }
            HookRunStatus::Skipped => {
                lines.push(
                    Line::from(vec![
                        Span::styled(format!("{}  ", INDENT), theme.muted()),
                        Span::styled("- ", theme.muted()),
                        Span::styled(run.name.clone(), theme.muted()),
                        Span::styled(" skipped", theme.muted()),
                    ])
                    .into(),
                );
            }
            HookRunStatus::Blocked { detail, elapsed } => {
                lines.push(
                    Line::from(vec![
                        Span::styled(format!("{}  ", INDENT), theme.muted()),
                        Span::styled("\u{21a9} ", theme.fg(theme.accent_running)),
                        Span::styled(run.name.clone(), theme.muted()),
                        Span::styled(format!(" ({}ms)", elapsed.as_millis()), theme.muted()),
                    ])
                    .into(),
                );
                let detail_text = crate::render::line_utils::truncate_str(detail, 120);
                for detail_line in detail_text.lines().take(3) {
                    lines.push(
                        Line::from(vec![
                            Span::styled(format!("{}      ", INDENT), theme.muted()),
                            Span::styled(detail_line.to_string(), theme.fg(theme.accent_running)),
                        ])
                        .into(),
                    );
                }
            }
            HookRunStatus::Failed { error, elapsed } => {
                lines.push(
                    Line::from(vec![
                        Span::styled(format!("{}  ", INDENT), theme.muted()),
                        Span::styled(
                            format!("{} ", crate::glyphs::ballot_x()),
                            theme.fg(theme.accent_error),
                        ),
                        Span::styled(run.name.clone(), theme.muted()),
                        Span::styled(format!(" ({}ms)", elapsed.as_millis()), theme.muted()),
                    ])
                    .into(),
                );
                // Error text — strip redundant hook name prefix if present
                let cleaned = error
                    .strip_prefix(&format!("hook '{}' ", run.name))
                    .unwrap_or(error);
                let err_text = crate::render::line_utils::truncate_str(cleaned, 120);
                for err_line in err_text.lines().take(3) {
                    lines.push(
                        Line::from(vec![
                            Span::styled(format!("{}      ", INDENT), theme.muted()),
                            Span::styled(err_line.to_string(), theme.fg(theme.accent_error)),
                        ])
                        .into(),
                    );
                }
            }
        }

        // Truncated output (if present)
        if let Some(ref output) = run.output {
            let truncated = crate::render::line_utils::truncate_str(output, 120);
            for out_line in truncated.lines().take(3) {
                lines.push(
                    Line::from(vec![
                        Span::styled(format!("{}      ", INDENT), theme.muted()),
                        Span::styled(out_line.to_string(), theme.muted()),
                    ])
                    .into(),
                );
            }
        }
    }

    lines
}

/// Render hook lines for a given display mode.
pub fn render_hooks_for_mode(
    event: &str,
    runs: &[HookRunEntry],
    mode: DisplayMode,
) -> Vec<BlockLine> {
    if runs.is_empty() {
        return Vec::new();
    }
    match mode {
        DisplayMode::Collapsed => Vec::new(),
        DisplayMode::Expanded | DisplayMode::Truncated => render_hooks_expanded(event, runs),
    }
}

/// Render hook detail lines (no section header) for expanded/truncated modes.
///
/// Used by lifecycle blocks where the block header already shows the event name,
/// so repeating it as a section header would be redundant.
pub fn render_hooks_detail(runs: &[HookRunEntry], mode: DisplayMode) -> Vec<BlockLine> {
    if runs.is_empty() {
        return Vec::new();
    }
    match mode {
        DisplayMode::Collapsed => Vec::new(),
        DisplayMode::Expanded | DisplayMode::Truncated => render_hooks_expanded_inner(runs),
    }
}

/// Render a separator line (for use between tool output and hooks in expanded mode).
pub fn render_hook_separator() -> BlockLine {
    render_separator()
}
