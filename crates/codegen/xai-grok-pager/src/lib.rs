//! xai-grok-pager — Grok Build TUI.
//!
//! A clean-room implementation built on the v3 pager rendering engine.

pub mod acp;
pub mod actions;
pub mod app;
pub mod client_identity;
pub mod codex;
mod codex_app_server;
pub mod completions_cmd;
mod config_toml_edit;
pub mod diagnostics;
pub mod diff;
pub mod docs;
pub mod export_cmd;
pub mod git_info;
pub mod headless;
pub mod hyperlink_route;
pub mod inline_media_ffmpeg;
pub mod input;
pub mod input_log;
pub mod mcp_cmd;
pub mod memory_cmd;
pub mod memory_release;
pub mod memory_trace;
// ── Minimal (scrollback-native) mode seam ────────────────────────────────────
// The *only* minimal-specific surface in this (the "full pager") crate. Both
// modules are grouped under `src/minimal/` so a full-pager contributor sees one
// folder to ignore, not files scattered through the module list. All the actual
// minimal rendering lives in the sibling `xai-grok-pager-minimal` crate; these
// are just the two narrow seams it connects through:
//   - `minimal_hook` — pager → minimal dispatch (fn-pointer IoC seam).
//   - `minimal_api`  — minimal → pager read surface (facade over `pub(crate)`s).
// Module names are kept flat (via `#[path]`) so existing references and
// every `crate::minimal_{api,hook}` call site stay valid.
#[path = "minimal/api.rs"]
pub mod minimal_api;
#[path = "minimal/hook.rs"]
pub mod minimal_hook;
pub mod models;
pub mod notifications;
#[allow(unused_imports, unused_macros)]
pub mod obf;
pub mod plugin_cmd;
pub mod project_picker;
pub mod pty_wrap;
pub mod scrollback;
pub mod search;
pub mod sessions_cmd;
pub mod settings;
pub mod share_cmd;
pub mod slash;
pub mod startup;
pub mod tips;
pub mod wrap_clipboard_image;
pub mod wrap_cmd;
pub(crate) mod wrap_filter;
pub(crate) mod wrap_restore;

pub mod tool_usage;

// Presentation-primitives layer extracted into the sibling crate
// `xai-grok-pager-render`. Re-exported at the crate root so existing
// `crate::<module>::...` references throughout the pager keep resolving.
pub use xai_grok_pager_render::{
    appearance, clipboard, gboom, glyphs, host, link_opener, modal_window_state, prompt_images,
    render, syntax, terminal, theme, util,
};
pub mod trace_cmd;
pub mod tracing;
pub mod unified_log;
pub mod views;
pub mod voice;
pub mod worktree_cmd;

#[cfg(test)]
pub mod test_util;
