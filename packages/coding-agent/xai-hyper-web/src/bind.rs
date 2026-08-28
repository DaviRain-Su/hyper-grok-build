//! Bind-address policy: loopback by default; Tailscale bind is opt-in.

use std::net::IpAddr;

use anyhow::{Result, bail};

/// Refuse a non-loopback bind unless the operator passed `--allow-remote`.
///
/// The intended remote path is `tailscale serve` in front of loopback, not a
/// naked `0.0.0.0` listener.
pub fn check_bind(ip: IpAddr, allow_remote: bool) -> Result<()> {
    if ip.is_loopback() {
        return Ok(());
    }
    if !allow_remote {
        bail!(
            "hyper web refuses to bind {ip} without --allow-remote\n\
             Prefer keeping the server on 127.0.0.1 and running:\n\
               tailscale serve --bg http://127.0.0.1:<port>\n\
             --allow-remote is for binding a Tailscale 100.x address on this machine.\n\
             Do not use Tailscale Funnel or a public bind."
        );
    }
    if ip.is_unspecified() {
        bail!(
            "hyper web will not bind {ip} even with --allow-remote\n\
             Bind a loopback or Tailscale address instead of 0.0.0.0 / ::"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn loopback_is_ok() {
        check_bind(IpAddr::V4(Ipv4Addr::LOCALHOST), false).unwrap();
        check_bind(IpAddr::V6(Ipv6Addr::LOCALHOST), false).unwrap();
    }

    #[test]
    fn remote_without_flag_is_rejected() {
        let err = check_bind(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--allow-remote"));
        assert!(msg.contains("tailscale serve"));
    }

    #[test]
    fn unspecified_is_always_rejected() {
        let err = check_bind(IpAddr::V4(Ipv4Addr::UNSPECIFIED), true).unwrap_err();
        assert!(err.to_string().contains("0.0.0.0"));
    }

    #[test]
    fn tailscale_cg_nat_ok_with_flag() {
        check_bind(IpAddr::V4(Ipv4Addr::new(100, 64, 1, 2)), true).unwrap();
    }
}
