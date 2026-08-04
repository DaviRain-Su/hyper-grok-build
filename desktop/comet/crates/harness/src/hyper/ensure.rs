//! Ensure a Hyper CLI binary is available for the desktop local-link path.
//!
//! Order: resolve existing install → download the matching platform archive
//! from GitHub Releases into `~/.hyper/desktop/bin/hyper` (override with
//! `COMET_DATA_DIR` / `HYPER_DESKTOP_DATA_DIR`).

use std::path::{Path, PathBuf};

use crate::HarnessError;
use super::rpc::{default_desktop_bin_dir, host_asset_triple, resolve_hyper_bin_existing};

const REPO: &str = "DaviRain-Su/hyper-grok-build";
const API_LATEST: &str = "https://api.github.com/repos/DaviRain-Su/hyper-grok-build/releases/latest";

/// Resolve or download Hyper. Sets `HYPER_AGENT_BIN` for this process when a
/// binary is found or installed.
pub async fn ensure_hyper_bin() -> Result<PathBuf, HarnessError> {
    if let Ok(p) = resolve_hyper_bin_existing() {
        // SAFETY: process-local env for the desktop engine; set before agent spawn.
        unsafe {
            std::env::set_var("HYPER_AGENT_BIN", &p);
        }
        return Ok(p);
    }
    tracing::info!("hyper CLI not found; downloading from GitHub Releases");
    let path = download_hyper_cli().await?;
    unsafe {
        std::env::set_var("HYPER_AGENT_BIN", &path);
    }
    Ok(path)
}

/// Blocking wrapper for sync call sites.
pub fn ensure_hyper_bin_blocking() -> Result<PathBuf, HarnessError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| HarnessError::Io(std::io::Error::other(e)))?;
    rt.block_on(ensure_hyper_bin())
}

async fn download_hyper_cli() -> Result<PathBuf, HarnessError> {
    let triple = host_asset_triple().ok_or_else(|| {
        HarnessError::NotInstalled(
            "cannot detect platform for Hyper CLI download (macOS arm64 / Linux only)"
                .into(),
        )
    })?;

    let client = reqwest::Client::builder()
        .user_agent(format!("hyper-desktop/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| HarnessError::Protocol(format!("http client: {e}")))?;

    let release: serde_json::Value = client
        .get(API_LATEST)
        .send()
        .await
        .map_err(|e| HarnessError::Protocol(format!("fetch release: {e}")))?
        .error_for_status()
        .map_err(|e| HarnessError::Protocol(format!("release status: {e}")))?
        .json()
        .await
        .map_err(|e| HarnessError::Protocol(format!("release json: {e}")))?;

    let tag = release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::Protocol("release missing tag_name".into()))?;
    let version = tag.trim_start_matches('v');
    let asset_name = format!("hyper-{version}-{triple}.tar.gz");

    let assets = release
        .get("assets")
        .and_then(|v| v.as_array())
        .ok_or_else(|| HarnessError::Protocol("release missing assets".into()))?;

    let mut archive_url = None;
    let mut sums_url = None;
    for a in assets {
        let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let url = a
            .get("browser_download_url")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if name == asset_name {
            archive_url = Some(url.to_string());
        }
        if name == "SHA256SUMS" {
            sums_url = Some(url.to_string());
        }
    }

    let archive_url = archive_url.unwrap_or_else(|| {
        format!("https://github.com/{REPO}/releases/download/{tag}/{asset_name}")
    });
    let sums_url = sums_url.unwrap_or_else(|| {
        format!("https://github.com/{REPO}/releases/download/{tag}/SHA256SUMS")
    });

    tracing::info!(%archive_url, "downloading Hyper CLI");
    let bytes = client
        .get(&archive_url)
        .send()
        .await
        .map_err(|e| HarnessError::Protocol(format!("download archive: {e}")))?
        .error_for_status()
        .map_err(|e| HarnessError::Protocol(format!("archive status: {e}")))?
        .bytes()
        .await
        .map_err(|e| HarnessError::Protocol(format!("archive body: {e}")))?;

    if let Ok(sums_text) = client.get(&sums_url).send().await {
        if let Ok(sums_text) = sums_text.error_for_status() {
            if let Ok(text) = sums_text.text().await {
                if let Some(expected) = parse_sum_line(&text, &asset_name) {
                    let actual = sha256_hex(&bytes);
                    if actual != expected {
                        return Err(HarnessError::Protocol(format!(
                            "Hyper CLI checksum mismatch for {asset_name}: expected {expected}, got {actual}"
                        )));
                    }
                }
            }
        }
    }

    let dest_dir = default_desktop_bin_dir();
    std::fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join("hyper");

    let tmp = dest_dir.join(format!(".hyper-download-{}.tar.gz", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    extract_hyper_from_tar(&tmp, &dest)?;
    let _ = std::fs::remove_file(&tmp);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms)?;
    }

    if !dest.is_file() {
        return Err(HarnessError::Protocol(
            "download succeeded but hyper binary missing after extract".into(),
        ));
    }
    tracing::info!(path = %dest.display(), "installed Hyper CLI");
    Ok(dest.canonicalize().unwrap_or(dest))
}

fn parse_sum_line(sums: &str, asset: &str) -> Option<String> {
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let digest = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        if name == asset {
            return Some(digest.to_ascii_lowercase());
        }
    }
    None
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

fn extract_hyper_from_tar(archive: &Path, dest: &Path) -> Result<(), HarnessError> {
    // Prefer system tar (always present on macOS/Linux runners and desktops).
    let extract_dir = dest
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".hyper-extract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&extract_dir);
    std::fs::create_dir_all(&extract_dir)?;

    let status = std::process::Command::new("tar")
        .args(["-xzf"])
        .arg(archive)
        .arg("-C")
        .arg(&extract_dir)
        .status()
        .map_err(|e| HarnessError::Protocol(format!("tar extract failed: {e}")))?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&extract_dir);
        return Err(HarnessError::Protocol(format!(
            "tar exited {status} extracting Hyper archive"
        )));
    }

    // Release layout: root `hyper` or nested.
    let candidate = if extract_dir.join("hyper").is_file() {
        extract_dir.join("hyper")
    } else {
        walk_find_hyper(&extract_dir).ok_or_else(|| {
            HarnessError::Protocol("archive did not contain a hyper binary".into())
        })?
    };
    // Atomic-ish replace
    let tmp_dest = dest.with_extension("new");
    std::fs::copy(&candidate, &tmp_dest)?;
    std::fs::rename(&tmp_dest, dest)?;
    let _ = std::fs::remove_dir_all(&extract_dir);
    Ok(())
}

fn walk_find_hyper(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).ok()?;
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().and_then(|n| n.to_str()) == Some("hyper") {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sum_line_finds_asset() {
        let sums = "\
abc123  hyper-0.2.119-r1-x86_64-unknown-linux-gnu.tar.gz
def456  other.tar.gz
";
        assert_eq!(
            parse_sum_line(sums, "hyper-0.2.119-r1-x86_64-unknown-linux-gnu.tar.gz").as_deref(),
            Some("abc123")
        );
    }
}
