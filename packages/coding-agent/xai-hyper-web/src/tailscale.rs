//! Best-effort Tailscale discovery. Missing `tailscale` is not an error.

use std::process::Command;

/// First IPv4 that `tailscale ip -4` prints, if the CLI is installed and up.
pub fn ipv4() -> Option<String> {
    let output = Command::new("tailscale").args(["ip", "-4"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let ip = stdout.lines().next()?.trim();
    if ip.is_empty() {
        None
    } else {
        Some(ip.to_owned())
    }
}

/// Human-readable startup notes for Tailscale. Never includes the token.
pub fn startup_hints(local_url: &str, tailscale_ipv4: Option<&str>) -> String {
    let mut lines = Vec::new();
    lines.push("Tailscale (stay on the tailnet; do not enable Funnel):".to_string());
    lines.push(format!("  tailscale serve --bg {local_url}"));
    lines.push(
        "Then open the HTTPS MagicDNS URL Tailscale prints and add the token query parameter."
            .to_string(),
    );
    if let Some(ip) = tailscale_ipv4 {
        lines.push(format!("This machine's Tailscale IPv4: {ip}"));
    } else {
        lines.push(
            "tailscale CLI not found or not logged in; install Tailscale if you need remote access."
                .to_string(),
        );
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_mention_serve_and_no_funnel() {
        let text = startup_hints("http://127.0.0.1:9100", Some("100.64.0.1"));
        assert!(text.contains("tailscale serve --bg http://127.0.0.1:9100"));
        assert!(text.contains("100.64.0.1"));
        assert!(text.contains("Funnel"));
        assert!(!text.contains("?token="));
    }
}
