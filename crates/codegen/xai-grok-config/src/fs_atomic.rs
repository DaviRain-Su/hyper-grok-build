//! Atomic file writes, shared by the managed-cache marker, the signature
//! sidecar, and downstream identifier caches (e.g. the telemetry agent id).

use std::path::{Path, PathBuf};

/// Resolve a symlink at the final path component so an atomic temp + rename
/// updates the symlink target instead of replacing the link itself.
///
/// Parent-directory symlinks keep their normal filesystem semantics. Only the
/// final component is followed, including relative links and short link chains.
pub fn resolve_write_target(path: &Path) -> std::io::Result<PathBuf> {
    const MAX_SYMLINKS: usize = 40;
    let mut current = path.to_path_buf();
    for _ in 0..MAX_SYMLINKS {
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = std::fs::read_link(&current)?;
                current = if link.is_absolute() {
                    link
                } else {
                    current.parent().unwrap_or_else(|| Path::new(".")).join(link)
                };
            }
            Ok(_) => return Ok(current),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("too many symlinks while resolving {}", path.display()),
    ))
}

/// Atomic temp + rename so a torn write can't leave a half-written file. The temp
/// name is unique per writer (pid + counter) and `create_new`, so concurrent
/// writers don't collide. `mode` (unix only) is applied at temp-file creation, so
/// the final file never exists with looser permissions.
pub fn write_atomically(
    final_path: &Path,
    contents: &str,
    mode: Option<u32>,
) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static WRITE_NONCE: AtomicU64 = AtomicU64::new(0);

    let dir = final_path.parent().unwrap_or_else(|| Path::new("."));
    let name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let nonce = WRITE_NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("{name}.{}.{nonce}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let result = options
        .open(&tmp)
        .and_then(|mut f| f.write_all(contents.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, &final_path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn resolves_relative_final_symlink_chain() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target_dir = root.path().join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        let target = target_dir.join("config.toml");
        std::fs::write(&target, "old").unwrap();
        let middle = root.path().join("middle");
        symlink("target/config.toml", &middle).unwrap();
        let link = root.path().join("config.toml");
        symlink("middle", &link).unwrap();

        assert_eq!(resolve_write_target(&link).unwrap(), target);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomically_keeps_ordinary_target_behavior() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("shared.toml");
        std::fs::write(&target, "old").unwrap();
        let link = root.path().join("config.toml");
        symlink("shared.toml", &link).unwrap();

        let write_target = resolve_write_target(&link).unwrap();
        write_atomically(&write_target, "new", Some(0o600)).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_final_symlink_loops() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        symlink("b", &a).unwrap();
        symlink("a", &b).unwrap();

        let error = resolve_write_target(&a).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}
