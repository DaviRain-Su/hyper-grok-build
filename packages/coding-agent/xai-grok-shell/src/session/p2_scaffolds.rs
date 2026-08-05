//! P2 scaffolds (OMP-inspired, not full ports).
//!
//! | Surface | Status |
//! |---------|--------|
//! | `conflict://` | **MVP shipped** via `xai_grok_tools::internal_urls` |
//! | collab relay / web | Config + docs only (no remote mesh in this tree) |
//! | DAP debugger tool | Stub tool description; no adapter process yet |
//! | eval kernel | Stub module for future isolated code-eval sessions |

/// Config keys reserved for future collab (mirror OMP `collab.*`).
#[derive(Debug, Clone, Default)]
pub(crate) struct CollabConfig {
    /// When true, `/collab` and collab UI entry points advertise themselves.
    pub enabled: bool,
    /// Optional websocket relay URL (`wss://…`). Empty = disabled.
    pub relay_url: Option<String>,
    /// Optional browser UI origin; empty derives from relay when possible.
    pub web_url: Option<String>,
    pub display_name: Option<String>,
}

/// Config keys for a future DAP (`debug` tool) integration.
#[derive(Debug, Clone, Default)]
pub(crate) struct DapConfig {
    pub enabled: bool,
    /// Adapter command template (e.g. `lldb-dap`, `js-debug`).
    pub adapter_command: Option<String>,
}

/// Config for a future eval kernel (sandboxed code execution agent).
#[derive(Debug, Clone, Default)]
pub(crate) struct EvalKernelConfig {
    pub enabled: bool,
    /// Max concurrent eval sessions.
    pub max_sessions: u32,
}

impl EvalKernelConfig {
    pub(crate) fn default_disabled() -> Self {
        Self {
            enabled: false,
            max_sessions: 1,
        }
    }
}

/// Human-readable status for docs / `/status`-style dumps.
pub(crate) fn p2_status_lines(
    collab: &CollabConfig,
    dap: &DapConfig,
    eval: &EvalKernelConfig,
) -> Vec<String> {
    vec![
        format!(
            "conflict://: available (read/write via read_file + search_replace)"
        ),
        format!(
            "collab: {}{}",
            if collab.enabled { "enabled" } else { "disabled" },
            collab
                .relay_url
                .as_ref()
                .map(|u| format!(" relay={u}"))
                .unwrap_or_default()
        ),
        format!(
            "dap: {}{}",
            if dap.enabled { "enabled (stub)" } else { "disabled" },
            dap.adapter_command
                .as_ref()
                .map(|c| format!(" adapter={c}"))
                .unwrap_or_default()
        ),
        format!(
            "eval_kernel: {}",
            if eval.enabled {
                "enabled (stub)"
            } else {
                "disabled"
            }
        ),
    ]
}
