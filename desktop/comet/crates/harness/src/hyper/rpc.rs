//! Hyper ACP client helpers: binary discovery + the `x.ai/interject` steer
//! request builder. The actual `ClientSideConnection` lives in `mod.rs`
//! (it is `!Send`, so it runs on a dedicated `LocalSet` thread).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol as acp;
use crate::HarnessError;

/// Resolve the `hyper` (or `grok`) CLI binary.
///
/// Search order:
/// 1. `HYPER_AGENT_BIN` (absolute or relative path)
/// 2. Beside this process executable (`comet` and `hyper` co-installed)
/// 3. Desktop managed install (`~/.hyper/desktop/bin/hyper`, see
///    [`default_desktop_bin_dir`])
/// 4. `PATH` (`hyper` then `grok`)
/// 5. Common install locations (`~/.local/bin`, `~/.grok/bin`, …)
/// 6. CWD-relative `target/{debug,release}/{hyper,grok}`
/// 7. Walk up from CWD (and from the exe path) looking for a Hyper monorepo
///    root (`Cargo.toml` + `packages/coding-agent` or legacy `crates/codegen`)
///    and use that tree's `target/{release,debug}/hyper`
///
/// Does **not** download. Use [`super::ensure::ensure_hyper_bin`] when a
/// missing binary should be fetched from GitHub Releases.
pub fn resolve_hyper_bin() -> Result<PathBuf, HarnessError> {
    resolve_hyper_bin_existing()
}

/// Same as [`resolve_hyper_bin`] — existing install only (no network).
pub fn resolve_hyper_bin_existing() -> Result<PathBuf, HarnessError> {
    if let Ok(p) = std::env::var("HYPER_AGENT_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Ok(pb);
        }
        // Allow relative paths that will work once the agent is spawned from cwd.
        if pb.as_os_str().len() > 0 {
            return Ok(pb);
        }
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        if let Some(p) = first_agent_in_dir(dir) {
            return Ok(p);
        }
        // cargo-run layout: .../desktop/comet/target/debug/comet
        if let Some(p) = monorepo_hyper_from_path(dir) {
            return Ok(p);
        }
    }

    // Managed desktop install path (auto-download target).
    {
        let managed = default_desktop_bin_dir().join("hyper");
        if managed.is_file() {
            return Ok(canonicalize_if_possible(managed));
        }
    }

    for name in ["hyper", "grok"] {
        if let Some(p) = which(name) {
            return Ok(p);
        }
    }

    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for rel in [
            ".local/bin/hyper",
            ".local/bin/grok",
            ".grok/bin/hyper",
            ".grok/bin/grok",
            ".cargo/bin/hyper",
            ".cargo/bin/grok",
        ] {
            let cand = home.join(rel);
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }

    for rel in [
        "target/release/hyper",
        "target/debug/hyper",
        "target/release/grok",
        "target/debug/grok",
    ] {
        let p = PathBuf::from(rel);
        if p.is_file() {
            return Ok(canonicalize_if_possible(p));
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        if let Some(p) = monorepo_hyper_from_path(&cwd) {
            return Ok(p);
        }
    }

    Err(HarnessError::NotInstalled(
        "hyper (or grok) not found. Desktop will download it on first use, or set \
         HYPER_AGENT_BIN / build the monorepo agent \
         (`cargo build -p xai-grok-pager-bin --release`)."
            .into(),
    ))
}

/// Default directory for the desktop-managed Hyper CLI binary.
///
/// `COMET_DATA_DIR` / `HYPER_DESKTOP_DATA_DIR` / `~/.hyper/desktop`, then `bin/`.
pub fn default_desktop_bin_dir() -> PathBuf {
    let data = std::env::var_os("COMET_DATA_DIR")
        .or_else(|| std::env::var_os("HYPER_DESKTOP_DATA_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".hyper").join("desktop")
        });
    data.join("bin")
}

/// GitHub release asset triple for this host (CLI archives only).
///
/// Desktop + CLI releases currently ship: `aarch64-apple-darwin`,
/// `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`.
pub fn host_asset_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

fn first_agent_in_dir(dir: &Path) -> Option<PathBuf> {
    for name in ["hyper", "grok"] {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(canonicalize_if_possible(cand));
        }
    }
    None
}

/// Walk `start` and its ancestors for a Hyper monorepo root, then pick a built
/// `hyper`/`grok` under that root's `target/`.
fn monorepo_hyper_from_path(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors().take(12) {
        if !is_hyper_monorepo_root(dir) {
            continue;
        }
        for rel in [
            "target/release/hyper",
            "target/debug/hyper",
            "target/release/grok",
            "target/debug/grok",
            // Nested cargo target when CARGO_TARGET_DIR is unset under desktop/comet
            "desktop/comet/target/release/hyper",
            "desktop/comet/target/debug/hyper",
        ] {
            let cand = dir.join(rel);
            if cand.is_file() {
                return Some(canonicalize_if_possible(cand));
            }
        }
        // Custom CARGO_TARGET_DIR is unknown; still useful when building in-tree.
        break;
    }
    None
}

fn is_hyper_monorepo_root(dir: &Path) -> bool {
    if !dir.join("Cargo.toml").is_file() {
        return false;
    }
    // packages/* layout (current Hyper) or legacy crates/codegen
    dir.join("packages/coding-agent").is_dir()
        || dir.join("packages/tui").is_dir()
        || dir.join("crates/codegen").is_dir()
        || dir.join("VERSION").is_file() && dir.join("install.sh").is_file()
}

fn canonicalize_if_possible(p: PathBuf) -> PathBuf {
    p.canonicalize().unwrap_or(p)
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(canonicalize_if_possible(cand));
        }
    }
    None
}

/// Build the `x.ai/interject` ACP `ExtRequest` for a steer.
///
/// Wire method becomes `_x.ai/interject` (the ACP lib prefixes `_`); the grok
/// agent strips the `_` and dispatches to its registered `x.ai/interject`
/// handler, which sends `SessionCommand::Interject` → the turn loop drains it
/// at the next safe point. See monorepo `docs/design-acp-steer.md`.
pub fn interject_request(
    session_id: &acp::SessionId,
    text: &str,
    interjection_id: Option<&str>,
) -> Result<acp::ExtRequest, HarnessError> {
    let mut params = serde_json::json!({
        "sessionId": session_id.0.as_ref(),
        "text": text,
    });
    if let Some(id) = interjection_id {
        params["interjectionId"] = serde_json::json!(id);
    }
    // ExtRequest::new takes Arc<RawValue>; build from the JSON value string.
    let raw = acp::RawValue::from_string(params.to_string())
        .map_err(|e| HarnessError::Protocol(format!("interject params: {e}")))?;
    Ok(acp::ExtRequest::new("x.ai/interject", Arc::from(raw)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monorepo_root_detects_packages_layout() {
        // This test file lives under desktop/comet/crates/harness/...
        let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // .../desktop/comet/crates/harness → monorepo root is 4 levels up
        let root = here
            .ancestors()
            .nth(4)
            .expect("harness crate is nested under monorepo");
        assert!(
            is_hyper_monorepo_root(root),
            "expected monorepo root at {}",
            root.display()
        );
    }
}
