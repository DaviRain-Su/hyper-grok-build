//! Hot-reloadable appearance configuration.
//!
//! Two unrelated concerns live under this module name:
//!
//! - **`config` + `watcher`**: dev-only `~/.grok/pager.toml` RenderConfig
//!   (200+ fields for terminal rendering tuning). Hot-reloaded in dev mode,
//!   static defaults in prod.
//! - **`cache`**: thread-local in-memory caches for the user-facing UI bool
//!   settings (`compact_mode`, `show_timestamps`, `simple_mode`). Disk
//!   writes happen in `xai_grok_shell::util::config::set_<field>()` via
//!   `Effect::PersistSetting`, NOT here — this is a read-cache only.
//! - **`permission_cursor`**: the `default_selected_permission` value type
//!   plus the caches and resolution logic for which row a permission prompt
//!   preselects.

pub mod cache;
mod config;
pub mod permission_cursor;
pub mod render_mermaid;
pub mod scroll_mode;
pub mod text_selection;
mod watcher;

pub use config::{
    AnimationConfig, AppearanceConfig, BlockBackground, BlocksConfig, EditBlockConfig,
    ExecuteHeaderStyle, FollowIndicator, LayoutConfig, PromptConfig, PromptViewConfig,
    RawAltScreenMode, RawAppearanceConfig, RawTerminalConfig, ScrollConfig, ScrollbackConfig,
    ScrollbarConfig, TodoBadgeFormat, TodoConfig, ToolBullet, ToolConfig,
    persist_respect_manual_folds,
};
pub use render_mermaid::RenderMermaid;
pub use scroll_mode::ScrollMode;
pub use text_selection::TextSelection;
pub use watcher::ConfigWatcher;

// -- Global tab_width --------------------------------------------------------
//
// Stored as an atomic so MarkdownContent can read the current value
// without needing the AppearanceConfig threaded through its API.
// Updated by the event loop whenever pager.toml is (re)loaded.

use std::sync::atomic::{AtomicU8, Ordering};

static TAB_WIDTH: AtomicU8 = AtomicU8::new(4);

/// Current tab expansion width (number of spaces per `\t`).
pub fn tab_width() -> u8 {
    TAB_WIDTH.load(Ordering::Relaxed)
}

/// Update the global tab width (called when config is loaded/reloaded).
pub fn set_tab_width(w: u8) {
    TAB_WIDTH.store(w, Ordering::Relaxed);
}
