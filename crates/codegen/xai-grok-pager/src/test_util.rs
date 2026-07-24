//! Shared test utilities for the pager crate.
//!
//! Compiled only in `#[cfg(test)]` builds. Import via `crate::test_util`.
/// Minimal `AgentView` for unit tests outside the dispatch/handler modules
/// (which keep their own richer factories).
pub fn make_agent_view(session_id: Option<&str>, cwd: &str) -> crate::app::agent_view::AgentView {
    use crate::app::agent::{AgentId, AgentSession, AgentState};
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let session = AgentSession {
        id: AgentId(0),
        acp_tx: tx,
        session_id: session_id.map(agent_client_protocol::SessionId::new),
        models: crate::acp::model_state::ModelState::default(),
        state: AgentState::Idle,
        tracker: crate::acp::tracker::AcpUpdateTracker::new(),
        cwd: std::path::PathBuf::from(cwd),
        is_worktree: false,
        forked_from: None,
        pending_prompts: std::collections::VecDeque::new(),
        next_queue_id: 0,
        yolo_mode: false,
        auto_mode: false,
        prompt_history: Vec::new(),
        prompt_history_loading: false,
        loading_replay: false,
        restore_degree: None,
        rate_limited: false,
        model_incompatible: false,
        credit_limit_blocked: false,
        free_usage_blocked: false,
        available_commands: Vec::new(),
        available_commands_generation: 0,
        available_tools: None,
        model_switch_pending: false,
        user_model_preference: None,
        deferred_model_switch: None,
        bg_tasks: std::collections::BTreeMap::new(),
        bg_tool_call_to_task: std::collections::HashMap::new(),
        scheduled_tasks: std::collections::HashMap::new(),
        in_flight_prompt: None,
        compact_held_prompt: None,
        current_prompt_id: None,
        created_via_new: false,
    };
    crate::app::agent_view::AgentView::new(
        session,
        crate::scrollback::state::ScrollbackState::new(),
    )
}
/// RAII guard for temporarily overriding an environment variable.
///
/// Captures the original value on construction and restores it on drop.
/// Used by theme and persist tests to redirect `HOME`/`USERPROFILE` to
/// temp directories without affecting the real user config.
pub struct EnvVarGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}
impl EnvVarGuard {
    /// Override `key` to `value` (paths, URLs, flags — anything OsStr-able),
    /// returning a guard that restores the original on drop.
    pub fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let original = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, original }
    }
}
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(value) = &self.original {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}

/// RAII guard pinning the theme cache to GrokNight under the theme test
/// lock, immune to the runner's `NO_COLOR` / `TERM=dumb` environment
/// (unpinned theme resolution otherwise picks degraded palettes and breaks
/// color/glyph assertions in minimal CI shells). Mirrors
/// `app::dispatch::tests::with_theme_test_env` — theme-sensitive tests in
/// every module must hold the same lock so they serialize with the dispatch
/// theme tests that reset the cache on exit.
///
/// Usage: `let _theme = crate::test_util::pin_theme();` at the top of a test.
pub struct PinnedThemeGuard(std::sync::MutexGuard<'static, ()>);

/// Pin the theme cache to GrokNight until the returned guard drops.
pub fn pin_theme() -> PinnedThemeGuard {
    let guard = crate::theme::cache::test_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::theme::cache::reset_for_test();
    crate::theme::cache::seed_auto_theme_defaults_for_test();
    crate::theme::cache::set(crate::theme::ThemeKind::GrokNight);
    crate::theme::system_appearance::clear_mock();
    // `color_support::detect()` is OnceLock-cached env detection — force it
    // per-call so `NO_COLOR`/`TERM=dumb` runners still render full color.
    crate::theme::color_support::force_level_for_test(Some(
        crate::theme::color_support::ColorLevel::TrueColor,
    ));
    PinnedThemeGuard(guard)
}

impl Drop for PinnedThemeGuard {
    fn drop(&mut self) {
        crate::theme::system_appearance::clear_mock();
        crate::theme::cache::reset_for_test();
        crate::theme::color_support::force_level_for_test(None);
    }
}

/// Shared GROK_HOME boundary fixture for the resume-by-title startup and
/// pre-sandbox tests.
///
/// `grok_home()` is OnceLock-cached process-wide, so summaries land under the
/// *resolved* home (possibly the real `~/.grok` when another test pinned the
/// cache first); cwd-encoded dirnames are tempdir-unique, and cleanup runs on
/// drop so it survives assertion panics. Callers must hold
/// `#[serial_test::serial(GROK_HOME)]`.
pub struct GrokHomeFixture {
    _home: tempfile::TempDir,
    cwd: tempfile::TempDir,
    cleanup: Vec<std::path::PathBuf>,
}
impl Drop for GrokHomeFixture {
    fn drop(&mut self) {
        for dir in &self.cleanup {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
impl Default for GrokHomeFixture {
    fn default() -> Self {
        Self::new()
    }
}
impl GrokHomeFixture {
    pub fn new() -> Self {
        let home = tempfile::tempdir().expect("home tempdir");
        unsafe { std::env::set_var("GROK_HOME", home.path()) };
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        Self {
            _home: home,
            cwd,
            cleanup: Vec::new(),
        }
    }
    /// Canonicalized so the summary cwd encoding matches what production
    /// path resolution sees (macOS tempdirs are symlinked). Tests pass this
    /// through the explicit `*_for_cwd` seams; the process cwd is never
    /// mutated.
    pub fn cwd_str(&self) -> String {
        self.cwd
            .path()
            .canonicalize()
            .expect("canonicalize cwd")
            .to_string_lossy()
            .to_string()
    }
    /// Write a minimal valid summary.json (every non-defaulted `Summary`
    /// field) for `id` under `cwd`, merging `extra` fields on top.
    pub fn write_summary(&mut self, cwd: &str, id: &str, extra: serde_json::Value) {
        let sessions_cwd_dir = Self::sessions_cwd_dir(cwd);
        if !self.cleanup.contains(&sessions_cwd_dir) {
            self.cleanup.push(sessions_cwd_dir.clone());
        }
        let dir = sessions_cwd_dir.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let mut v = serde_json::json!({
            "info": { "id": id, "cwd": cwd },
            "session_summary": "auto summary",
            "created_at": "2026-07-01T00:00:00Z",
            "updated_at": "2026-07-01T00:00:00Z",
            "num_messages": 1,
            "current_model_id": "grok-build",
        });
        if let Some(map) = extra.as_object() {
            for (k, val) in map {
                v[k.as_str()] = val.clone();
            }
        }
        std::fs::write(dir.join("summary.json"), serde_json::to_vec(&v).unwrap()).unwrap();
    }
    /// Delete a previously written session dir (concurrent-delete simulation).
    pub fn remove_session(&self, cwd: &str, id: &str) {
        let _ = std::fs::remove_dir_all(Self::sessions_cwd_dir(cwd).join(id));
    }
    fn sessions_cwd_dir(cwd: &str) -> std::path::PathBuf {
        let encoded = xai_grok_shell::util::grok_home::encode_cwd_dirname(cwd);
        xai_grok_shell::util::grok_home::grok_home()
            .join("sessions")
            .join(&encoded)
    }
}
