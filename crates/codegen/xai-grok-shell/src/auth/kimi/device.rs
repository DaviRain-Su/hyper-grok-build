//! Device identity headers for the Kimi Code OAuth endpoints.
//!
//! Ported from kimi-cli / Kigi-CLI: every OAuth call (and subscription
//! inference) carries:
//! - `X-Msh-Device-Name` — hostname
//! - `X-Msh-Device-Model` — OS/arch string
//! - `X-Msh-Device-Id` — uuid4 hex at `~/.grok/device_id` (owner-only)

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Context as _;

/// Sanitize a header value to ASCII (kimi-cli `_ascii_header_value`).
pub(crate) fn ascii_header_value(value: &str) -> String {
    let sanitized: String = value.chars().filter(char::is_ascii).collect();
    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        "unknown".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Three device-identity headers for OAuth + Kimi Code inference.
pub fn device_headers() -> anyhow::Result<[(&'static str, String); 3]> {
    Ok([
        ("X-Msh-Device-Name", ascii_header_value(&device_name())),
        ("X-Msh-Device-Model", ascii_header_value(device_model())),
        ("X-Msh-Device-Id", ascii_header_value(&device_id()?)),
    ])
}

fn device_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: buf is a valid writable buffer of the passed length.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]).into_owned();
            if !name.trim().is_empty() {
                return name;
            }
        }
        "unknown".to_owned()
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_owned())
    }
    #[cfg(not(any(unix, windows)))]
    {
        "unknown".to_owned()
    }
}

pub(crate) fn device_model() -> &'static str {
    static MODEL: OnceLock<String> = OnceLock::new();
    MODEL.get_or_init(compute_device_model)
}

fn compute_device_model() -> String {
    #[cfg(target_os = "macos")]
    {
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            other => other,
        };
        match macos_product_version() {
            Some(version) => format!("macOS {version} {arch}"),
            None => format!("macOS {arch}"),
        }
    }
    #[cfg(windows)]
    {
        let arch = std::env::consts::ARCH;
        match windows_release() {
            Some(release) => format!("Windows {release} {arch}"),
            None => format!("Windows {arch}"),
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
    }
}

#[cfg(target_os = "macos")]
fn macos_product_version() -> Option<String> {
    let plist = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
    let key_tag = "<key>ProductVersion</key>";
    let after_key = &plist[plist.find(key_tag)? + key_tag.len()..];
    let start = after_key.find("<string>")? + "<string>".len();
    let end = after_key.find("</string>")?;
    (start <= end).then(|| after_key[start..end].trim().to_owned())
}

#[cfg(windows)]
fn windows_release() -> Option<String> {
    let output = std::process::Command::new("cmd")
        .args(["/c", "ver"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.split("Version").nth(1)?.trim();
    let mut parts = version.trim_end_matches(']').split('.');
    let major = parts.next()?.trim().to_owned();
    let _minor = parts.next()?;
    let build: u32 = parts.next()?.trim().parse().ok()?;
    if major == "10" && build >= 22000 {
        Some("11".to_owned())
    } else {
        Some(major)
    }
}

fn device_id_path() -> PathBuf {
    xai_grok_config::grok_home().join("device_id")
}

pub(crate) fn device_id() -> anyhow::Result<String> {
    static DEVICE_ID: OnceLock<String> = OnceLock::new();
    if let Some(id) = DEVICE_ID.get() {
        return Ok(id.clone());
    }
    let id = load_or_create_device_id(&device_id_path())?;
    Ok(DEVICE_ID.get_or_init(|| id).clone())
}

fn load_or_create_device_id(path: &std::path::Path) -> anyhow::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_owned());
        }
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {} for device_id", parent.display()))?;
    }
    std::fs::write(path, format!("{id}\n"))
        .with_context(|| format!("writing device id to {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    tracing::info!(path = %path.display(), "auth: created Kimi device id");
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_header_value_strips_non_ascii() {
        assert_eq!(ascii_header_value("hello"), "hello");
        assert_eq!(ascii_header_value("héllo"), "hllo");
        assert_eq!(ascii_header_value("  "), "unknown");
    }

    #[test]
    fn device_model_is_nonempty_ascii() {
        let model = device_model();
        assert!(!model.is_empty());
        assert!(model.is_ascii(), "{model:?}");
    }
}
