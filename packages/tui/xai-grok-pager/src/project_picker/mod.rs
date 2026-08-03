//! Project picker: select a project directory on first prompt from a non-project dir.

pub mod detection {
    pub use xai_file_utils::workspace_classifier::is_project_dir;
}
pub mod sources;

use std::path::{Path, PathBuf};

use xai_grok_tools::implementations::grok_build::ask_user_question::{Question, QuestionOption};

/// `resolved_paths` is index-aligned with the leading `question.options`.
/// The trailing "Don't ask me again" option at `dont_ask_index` has no
/// corresponding path (selecting it continues in the current directory).
pub struct ProjectQuestion {
    pub question: Question,
    pub resolved_paths: Vec<PathBuf>,
    /// Option index of the "Don't ask me again" entry.
    pub dont_ask_index: usize,
}

const MAX_RECENT_DIRS: usize = 5;
pub fn build_project_question(
    recent_dirs: &[(PathBuf, chrono::DateTime<chrono::Utc>)],
    cwd: &Path,
) -> ProjectQuestion {
    let mut options = Vec::new();
    let mut resolved_paths = Vec::new();

    // First option: continue in the current directory.
    let is_home = dirs::home_dir().is_some_and(|h| h == cwd);
    let cwd_name = if is_home {
        "~"
    } else {
        cwd.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("current directory")
    };
    options.push(QuestionOption {
        label: format!("{cwd_name} (current)"),
        description: sources::display_path(cwd),
        preview: None,
        id: None,
    });
    resolved_paths.push(cwd.to_path_buf());

    // Recent project directories.
    for (path, ts) in recent_dirs
        .iter()
        .filter(|(p, _)| p != cwd)
        .take(MAX_RECENT_DIRS)
    {
        let raw_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let name = crate::render::line_utils::truncate_str(raw_name, 22);
        options.push(QuestionOption {
            label: name,
            description: format!(
                "{}  ({})",
                sources::display_path(path),
                crate::views::session_title::format_relative_time(
                    (chrono::Utc::now() - *ts).to_std().unwrap_or_default()
                ),
            ),
            preview: None,
            id: None,
        });
        resolved_paths.push(path.clone());
    }

    // Kept out of `resolved_paths` so the path options stay index-aligned.
    let dont_ask_index = options.len();
    options.push(QuestionOption {
        label: "Don't ask me again".to_string(),
        description: "Always start in the current directory (reset in config.toml)".to_string(),
        preview: None,
        id: None,
    });

    ProjectQuestion {
        question: Question {
            question: "Run Grok Build in a project directory?\n\n\
                 This gives Grok Build full context of your codebase for better results."
                .into(),
            id: None,
            options,
            multi_select: Some(false),
        },
        resolved_paths,
        dont_ask_index,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn no_recent_dirs_returns_only_cwd() {
        let pq = build_project_question(&[], Path::new("/home/user"));
        assert_eq!(pq.resolved_paths.len(), 1);
        assert_eq!(pq.resolved_paths[0], PathBuf::from("/home/user"));
    }

    #[test]
    fn recent_dirs_index_aligned_with_options() {
        let now = Utc::now();
        let recent = vec![
            (PathBuf::from("/projects/alpha"), now),
            (PathBuf::from("/projects/beta"), now),
        ];
        let pq = build_project_question(&recent, Path::new("/home/user"));
        // Options carry one extra trailing "Don't ask me again" entry beyond
        // the index-aligned path options.
        assert_eq!(pq.question.options.len(), pq.resolved_paths.len() + 1);
        assert_eq!(pq.resolved_paths[0], PathBuf::from("/home/user"));
        assert_eq!(pq.resolved_paths[1], PathBuf::from("/projects/alpha"));
        assert_eq!(pq.resolved_paths[2], PathBuf::from("/projects/beta"));
    }

    #[test]
    fn dont_ask_option_is_last_and_excluded_from_paths() {
        let pq = build_project_question(&[], Path::new("/home/user"));
        assert_eq!(pq.dont_ask_index, pq.resolved_paths.len());
        assert_eq!(pq.dont_ask_index, pq.question.options.len() - 1);
        assert_eq!(
            pq.question.options[pq.dont_ask_index].label,
            "Don't ask me again"
        );
    }
}
