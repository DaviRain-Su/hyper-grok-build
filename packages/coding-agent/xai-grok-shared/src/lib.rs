//! Shared utilities used by both `xai-grok-shell` and its downstream clients
//! (e.g. `xai-grok-pager-render`). This crate sits upstream of `xai-grok-shell`
//! so it must never depend on it.

pub mod clipboard;
pub mod placeholder_images;
pub mod session;
pub mod stderr;
pub mod ui_config;
