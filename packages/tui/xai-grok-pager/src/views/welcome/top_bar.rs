//! Top bar component: renders cwd and git info.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use std::path::{Path, PathBuf};

use crate::git_info;
use crate::render::line_utils::truncate_line;
use crate::theme::Theme;

pub fn render_top_bar(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    announcement: Option<&xai_grok_announcements::RemoteAnnouncement>,
) {
    let line = truncate_line(location_line(theme), area.width as usize);
    let line_width = line.width() as u16;
    buf.set_line(area.x, area.y, &line, line_width.min(area.width));

    if let Some(a) = announcement
        && let Some(text) = a.message.as_deref()
        && area.height > 1
    {
        let text_style = Style::default().fg(theme.text_primary);
        let line = Line::from(Span::styled(text, text_style));
        Paragraph::new(line).render(
            Rect {
                y: area.y + 1,
                height: area.height.saturating_sub(1),
                ..area
            },
            buf,
        );
    }
}

/// Build the `{git branch} {worktree} {cwd}` line for the welcome top bar, reading the live process cwd.
pub(crate) fn location_line(theme: &Theme) -> Line<'static> {
    location_line_at(theme, &process_cwd())
}

/// As [`location_line`], but for an explicit `cwd`.
/// The dashboard header passes its staged `app.cwd` so the line tracks a `/cd` immediately.
/// That holds before (or even if) `Effect::SetWorkingDir` moves the process cwd.
/// Safe to call during render: it reads the per-cwd git cache and never blocks or spawns `git`.
/// The caller width-truncates the returned line.
pub(crate) fn location_line_at(theme: &Theme, cwd: &Path) -> Line<'static> {
    let info_style = Style::default().fg(theme.gray);

    let info = git_info::cwd_git_info_lazy(cwd);

    let mut parts: Vec<Span> = Vec::new();
    if let Some(branch) = info.as_ref().and_then(|i| i.branch.as_deref()) {
        let icon = git_info::branch_icon();
        let git_text = if branch.is_empty() {
            format!("{icon} detached")
        } else {
            format!("{icon} {branch}")
        };
        let git_style = Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::DIM);
        parts.push(Span::styled(git_text, git_style));
        parts.push(Span::styled(" ", info_style));
    }
    // Worktree badge: matches the session status bar's `worktree ` marker (accent_user) before the path when the cwd is a linked worktree
    if info.as_ref().is_some_and(|i| i.is_worktree) {
        parts.push(Span::styled(
            "worktree ",
            Style::default().fg(theme.accent_user),
        ));
    }
    let cwd_display = format_cwd_display(cwd, info.as_ref());
    let cwd_style = Style::default().fg(theme.gray_dim);
    parts.push(Span::styled(cwd_display, cwd_style));
    Line::from(parts)
}

fn process_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// The tilde-collapsed cwd, with a `(worktree of …)` suffix when `info` reports a linked worktree's main repo.
/// Matches the session status bar; the `worktree ` badge itself is painted by [`location_line`].
/// Pure formatting over the per-cwd git probe; nothing here spawns `git`.
fn format_cwd_display(cwd: &Path, info: Option<&git_info::CwdGitInfo>) -> String {
    let display = collapse_home(cwd);
    let main_repo = info.and_then(|i| i.main_repo.as_deref());
    format_cwd_parts(&display, main_repo)
}

/// Pure formatting for the cwd display; no global state.
fn format_cwd_parts(display: &str, main_repo: Option<&str>) -> String {
    if let Some(main_repo) = main_repo {
        format!("{display} (worktree of {main_repo})")
    } else {
        display.to_string()
    }
}

fn collapse_home(dir: &std::path::Path) -> String {
    // Match the session status bar: Path::strip_prefix via abbreviate_path, not a string prefix of USERPROFILE
    // `C:\Users\foo` must not collapse neighbor `C:\Users\foobar` to `~bar`
    crate::util::abbreviate_path(&dir.to_string_lossy()).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_cwd_plain_repo() {
        assert_eq!(format_cwd_parts("~/xai", None), "~/xai");
    }

    /// A linked worktree shows the `(worktree of …)` suffix (matching the session status bar) regardless of the worktree's human label.
    /// The label is no longer shown here; the `worktree ` badge stands in for it.
    #[test]
    fn format_cwd_worktree_shows_main_repo() {
        assert_eq!(
            format_cwd_parts("~/wt/session-1", Some("~/xai")),
            "~/wt/session-1 (worktree of ~/xai)"
        );
    }

    /// The header shows the ACTUAL cwd, not the git repo root: switching into a subdirectory of a repo reflects the subdirectory.
    /// (`/work/...` is outside `$HOME`, so `collapse_home` leaves it verbatim.)
    #[test]
    fn format_cwd_display_shows_subdir_not_repo_root() {
        let info = git_info::CwdGitInfo {
            branch: Some("main".into()),
            is_worktree: false,
            main_repo: None,
            worktree_label: None,
        };
        assert_eq!(
            format_cwd_display(Path::new("/work/xai/frontend/apps"), Some(&info)),
            "/work/xai/frontend/apps",
        );
    }

    /// A worktree subdirectory shows the `(worktree of …)` suffix (matching the session status bar) while still showing the real subdirectory path.
    #[test]
    fn format_cwd_display_worktree_subdir_shows_main_repo() {
        let info = git_info::CwdGitInfo {
            branch: Some("kevin/x".into()),
            is_worktree: true,
            main_repo: Some("~/xai".into()),
            worktree_label: Some("location-picker".into()),
        };
        assert_eq!(
            format_cwd_display(Path::new("/work/wt/location-picker/frontend"), Some(&info)),
            "/work/wt/location-picker/frontend (worktree of ~/xai)",
        );
    }

    /// On a cache miss (`info == None`) the header still shows the raw cwd.
    #[test]
    fn format_cwd_display_cache_miss_shows_raw_cwd() {
        assert_eq!(
            format_cwd_display(Path::new("/work/xai/frontend/apps"), None),
            "/work/xai/frontend/apps",
        );
    }

    /// String-prefix `strip_prefix($HOME)` would collapse a neighbor profile (`$HOMEbar`) into `~bar`.
    /// Path-component matching must leave it intact while still collapsing a real child of `$HOME`.
    #[test]
    fn collapse_home_does_not_eat_neighbor_profile() {
        let Some(home) = git_info::home_dir() else {
            return;
        };
        if home.is_empty() {
            return;
        }
        let neighbor = PathBuf::from(format!("{home}bar")).join("src");
        let collapsed_neighbor = collapse_home(&neighbor);
        assert_eq!(
            collapsed_neighbor,
            neighbor.display().to_string(),
            "string prefix would produce ~bar/src (or ~bar\\src)"
        );

        let child = PathBuf::from(&home).join("src");
        let expected = format!("~/{}", Path::new("src").display());
        assert_eq!(collapse_home(&child), expected);
    }
}
