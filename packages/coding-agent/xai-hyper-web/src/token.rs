//! Persistent bearer token for the web control plane.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rand::RngCore;

const TOKEN_BYTES: usize = 32;
const TOKEN_HEX_LEN: usize = TOKEN_BYTES * 2;
pub(crate) const TOKEN_FILE_NAME: &str = "web-token";

pub fn token_path(grok_home: &Path) -> PathBuf {
    grok_home.join(TOKEN_FILE_NAME)
}

/// Load an existing token or mint a new one. Unix files are 0600.
pub fn load_or_create(grok_home: &Path) -> Result<String> {
    fs::create_dir_all(grok_home)
        .with_context(|| format!("create grok home {}", grok_home.display()))?;
    let path = token_path(grok_home);
    if path.exists() {
        let existing = read_token(&path)?;
        chmod_owner_only(&path);
        return Ok(existing);
    }
    let token = mint_token();
    write_token(&path, &token)?;
    Ok(token)
}

fn mint_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn read_token(path: &Path) -> Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .with_context(|| format!("read {}", path.display()))?;
    let token = buf.trim().to_string();
    if !is_token_shape(&token) {
        bail!(
            "{} is not a valid Hyper web token (want {TOKEN_HEX_LEN} hex chars)",
            path.display()
        );
    }
    Ok(token)
}

fn write_token(path: &Path, token: &str) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(token.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    file.write_all(b"\n")?;
    Ok(())
}

fn chmod_owner_only(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = fs::set_permissions(path, perms);
        }
    }
    let _ = path;
}

pub fn is_token_shape(token: &str) -> bool {
    token.len() == TOKEN_HEX_LEN && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Constant-time compare for equal-length hex tokens.
pub fn tokens_equal(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mints_and_reuses_token() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create(dir.path()).unwrap();
        assert!(is_token_shape(&first));
        let second = load_or_create(dir.path()).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn rejects_garbage_token_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(token_path(dir.path()), "not-a-token\n").unwrap();
        let err = load_or_create(dir.path()).unwrap_err();
        assert!(err.to_string().contains("not a valid Hyper web token"));
    }

    #[test]
    fn equal_tokens_match() {
        assert!(tokens_equal("aa", "aa"));
        assert!(!tokens_equal("aa", "ab"));
        assert!(!tokens_equal("aa", "aaa"));
    }
}
