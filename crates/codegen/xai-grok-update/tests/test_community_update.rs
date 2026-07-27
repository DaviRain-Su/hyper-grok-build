//! End-to-end tests for the isolated Hyper community updater.

#![cfg(all(unix, feature = "community-build"))]

mod common;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flate2::Compression;
use flate2::write::GzEncoder;
use serial_test::serial;
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{make_update_config, reset_home, set_test_version, test_home};
use xai_grok_update::auto_update::{check_update_status, run_update};

#[allow(unreachable_code)]
fn platform_triple() -> &'static str {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return "x86_64-unknown-linux-gnu";
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return "aarch64-unknown-linux-gnu";
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return "x86_64-apple-darwin";
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return "aarch64-apple-darwin";
    panic!("unsupported community updater test platform")
}

fn local_platform() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    (os, arch)
}

fn release_archive(binary: &[u8]) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);

    let mut header = tar::Header::new_gnu();
    header.set_size(binary.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder.append_data(&mut header, "hyper", binary).unwrap();

    let license = b"test license\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(license.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, "LICENSE", &license[..])
        .unwrap();

    let encoder = builder.into_inner().unwrap();
    encoder.finish().unwrap()
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn root_installer_path() -> PathBuf {
    dunce::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../install.sh"))
        .expect("repository-root install.sh should exist")
}

fn run_root_installer(hyper_home: &Path, user_home: &Path, server: &MockServer) -> Output {
    let tmp = user_home.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(hyper_home.join("bin")).chain(std::env::split_paths(&inherited_path)),
    )
    .unwrap();

    Command::new("/bin/sh")
        .arg(root_installer_path())
        .env("HOME", user_home)
        .env("TMPDIR", tmp)
        .env("PATH", path)
        .env("SHELL", "/bin/sh")
        .env("HYPER_SHARE_DIR", hyper_home)
        .env("HYPER_UPDATE_BASE_URL", server.uri())
        .env("GITHUB_TOKEN", "must-not-leak-to-custom-update-host")
        .output()
        .expect("root Hyper installer should run")
}

fn active_target(active: &Path) -> PathBuf {
    let target = std::fs::read_link(active).unwrap();
    dunce::canonicalize(active.parent().unwrap().join(target)).unwrap()
}

async fn assert_no_authorization_header(server: &MockServer) {
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|request| {
        request
            .headers
            .iter()
            .all(|(name, _)| !name.as_str().eq_ignore_ascii_case("authorization"))
    }));
}

async fn mount_release(
    version: &str,
    archive: Vec<u8>,
    manifest_hash: &str,
) -> (MockServer, String) {
    let server = MockServer::start().await;
    let asset = format!("hyper-{version}-{}.tar.gz", platform_triple());
    let base = server.uri();
    let metadata = serde_json::json!({
        "tag_name": format!("v{version}"),
        "draft": false,
        "prerelease": false,
        "assets": [
            {
                "name": asset,
                "browser_download_url": format!("{base}/assets/{asset}")
            },
            {
                "name": "SHA256SUMS",
                "browser_download_url": format!("{base}/assets/SHA256SUMS")
            }
        ]
    });

    Mock::given(method("GET"))
        .and(path("/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/assets/SHA256SUMS"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("{manifest_hash}  {asset}\n")),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/assets/{asset}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(archive))
        .mount(&server)
        .await;

    (server, asset)
}

struct EnvGuard {
    hyper_home: PathBuf,
}

impl EnvGuard {
    fn install() -> Self {
        let _ = test_home();
        reset_home();
        let hyper_home = tempfile::tempdir().unwrap().keep();
        unsafe {
            std::env::set_var("HYPER_SHARE_DIR", &hyper_home);
            std::env::set_var("HYPER_ALLOW_INSECURE_UPDATE_BASE", "1");
        }
        Self { hyper_home }
    }

    fn use_server(&self, server: &MockServer) {
        unsafe { std::env::set_var("HYPER_UPDATE_BASE_URL", server.uri()) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            std::env::remove_var("HYPER_SHARE_DIR");
            std::env::remove_var("HYPER_ALLOW_INSECURE_UPDATE_BASE");
            std::env::remove_var("HYPER_UPDATE_BASE_URL");
        }
        let _ = std::fs::remove_dir_all(&self.hyper_home);
    }
}

fn install_official_sentinel() -> (PathBuf, Vec<u8>) {
    let grok = test_home().join("bin/grok");
    std::fs::create_dir_all(grok.parent().unwrap()).unwrap();
    let bytes = b"official-grok-must-not-change\n".to_vec();
    std::fs::write(&grok, &bytes).unwrap();
    (grok, bytes)
}

fn install_old_hyper(home: &Path, version: &str) -> PathBuf {
    let (os, arch) = local_platform();
    let downloads = home.join("downloads");
    let bin = home.join("bin");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let old = downloads.join(format!("hyper-{version}-{os}-{arch}"));
    let mut file = std::fs::File::create(&old).unwrap();
    file.write_all(b"#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(
            Path::new("..")
                .join("downloads")
                .join(old.file_name().unwrap()),
            bin.join("hyper"),
        )
        .unwrap();
    }
    old
}

#[tokio::test]
#[serial]
async fn community_update_installs_verified_archive_without_touching_official_grok() {
    let env = EnvGuard::install();
    set_test_version("0.2.112");
    let (grok, sentinel) = install_official_sentinel();
    install_old_hyper(&env.hyper_home, "0.2.112");

    let archive = release_archive(b"#!/bin/sh\nexit 0\n");
    let digest = sha256(&archive);
    let (server, asset) = mount_release("0.2.113", archive, &digest).await;
    env.use_server(&server);

    let mut config = make_update_config("stable");
    let installed = run_update(false, None, None, &mut config)
        .await
        .expect("community update should install");
    assert_eq!(installed.as_deref(), Some("0.2.113"));
    assert_eq!(std::fs::read(&grok).unwrap(), sentinel);

    let active = env.hyper_home.join("bin/hyper");
    assert!(active.is_symlink());
    let target = std::fs::read_link(&active).unwrap();
    let target_name = target.file_name().unwrap().to_string_lossy();
    assert!(target_name.contains("hyper-0.2.113-"), "{target_name}");
    assert!(target_name.contains(&digest), "{target_name}");
    assert!(std::fs::metadata(&active).unwrap().len() > 0);

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(env.hyper_home.join("update-state.json")).unwrap())
            .unwrap();
    assert_eq!(state["installed_version"], "0.2.113");
    assert_eq!(state["installed_asset"], asset);
    assert_eq!(state["installed_sha256"], digest);

    let status = check_update_status(&config).await;
    assert_eq!(status.installer.as_deref(), Some("community-github"));
    assert!(!status.update_available);
    assert!(status.error.is_none(), "{:?}", status.error);

    let requests = server.received_requests().await.unwrap();
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|request| matches!(
        request.url.path(),
        "/latest" | "/assets/SHA256SUMS"
    ) || request.url.path().starts_with("/assets/hyper-")));
}

#[tokio::test]
#[serial]
async fn concurrent_updaters_download_and_activate_archive_once() {
    let env = EnvGuard::install();
    set_test_version("0.2.112");
    install_old_hyper(&env.hyper_home, "0.2.112");

    let archive = release_archive(b"#!/bin/sh\nexit 0\n");
    let digest = sha256(&archive);
    let (server, asset) = mount_release("0.2.113", archive, &digest).await;
    env.use_server(&server);

    let updates = (0..10).map(|_| async {
        let mut config = make_update_config("stable");
        run_update(false, None, None, &mut config).await
    });
    for result in futures::future::join_all(updates).await {
        assert_eq!(result.unwrap().as_deref(), Some("0.2.113"));
    }

    let requests = server.received_requests().await.unwrap();
    let archive_path = format!("/assets/{asset}");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == archive_path)
            .count(),
        1,
        "the cross-process install lock must suppress duplicate archive downloads"
    );
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(env.hyper_home.join("update-state.json")).unwrap())
            .unwrap();
    assert_eq!(state["installed_sha256"], digest);
    assert!(std::fs::metadata(env.hyper_home.join("bin/hyper")).is_ok());
}

#[tokio::test]
#[serial]
async fn same_semver_digest_change_updates_once_then_converges() {
    let env = EnvGuard::install();
    set_test_version("0.2.113");
    let old = install_old_hyper(&env.hyper_home, "0.2.113");
    let old_name = old.file_name().unwrap().to_string_lossy().to_string();
    std::fs::write(
        env.hyper_home.join("update-state.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "installed_version": "0.2.113",
            "installed_asset": format!("hyper-0.2.113-{}.tar.gz", platform_triple()),
            "installed_sha256": "1".repeat(64),
            "installed_binary": old_name,
            "checked_at_unix": 0,
        }))
        .unwrap(),
    )
    .unwrap();

    let archive = release_archive(b"#!/bin/sh\nexit 0\n# republished\n");
    let digest = sha256(&archive);
    let (server, asset) = mount_release("0.2.113", archive, &digest).await;
    env.use_server(&server);

    for _ in 0..2 {
        let mut config = make_update_config("stable");
        assert_eq!(
            run_update(false, None, None, &mut config)
                .await
                .unwrap()
                .as_deref(),
            Some("0.2.113")
        );
    }

    let requests = server.received_requests().await.unwrap();
    let archive_path = format!("/assets/{asset}");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == archive_path)
            .count(),
        1,
        "same tag + new digest installs once; the matching digest then converges"
    );
    let target = std::fs::read_link(env.hyper_home.join("bin/hyper")).unwrap();
    assert!(target.to_string_lossy().contains(&digest));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn root_installer_same_semver_republish_is_atomic_and_isolated() {
    let env = EnvGuard::install();
    let user_home = tempfile::tempdir().unwrap();
    let official = user_home.path().join(".grok/bin/grok");
    std::fs::create_dir_all(official.parent().unwrap()).unwrap();
    std::fs::write(&official, b"official-grok-sentinel\n").unwrap();

    let archive_a = release_archive(b"#!/bin/sh\nexit 0\n# build a\n");
    let digest_a = sha256(&archive_a);
    let (server_a, _) = mount_release("0.2.113", archive_a, &digest_a).await;
    let first = run_root_installer(&env.hyper_home, user_home.path(), &server_a);
    assert!(
        first.status.success(),
        "first install failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    let active = env.hyper_home.join("bin/hyper");
    let target_a = active_target(&active);
    assert!(target_a.to_string_lossy().contains(&digest_a));
    assert!(target_a.exists());

    let archive_b = release_archive(b"#!/bin/sh\nexit 0\n# build b\n");
    let digest_b = sha256(&archive_b);
    let (server_b, _) = mount_release("0.2.113", archive_b, &digest_b).await;
    let second = run_root_installer(&env.hyper_home, user_home.path(), &server_b);
    assert!(
        second.status.success(),
        "republished install failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );

    let target_b = active_target(&active);
    assert_ne!(target_a, target_b);
    assert!(
        target_a.exists(),
        "republish must not overwrite the prior target"
    );
    assert!(target_b.to_string_lossy().contains(&digest_b));

    let bad_archive = release_archive(b"#!/bin/sh\nexit 1\n");
    let bad_digest = sha256(&bad_archive);
    let (bad_server, _) = mount_release("0.2.113", bad_archive, &bad_digest).await;
    let failed = run_root_installer(&env.hyper_home, user_home.path(), &bad_server);
    assert!(
        !failed.status.success(),
        "bad binary must fail its smoke test"
    );
    assert_eq!(active_target(&active), target_b);

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(env.hyper_home.join("update-state.json")).unwrap())
            .unwrap();
    assert_eq!(state["installed_sha256"], digest_b);
    assert_eq!(
        std::fs::read(&official).unwrap(),
        b"official-grok-sentinel\n"
    );

    assert_no_authorization_header(&server_a).await;
    assert_no_authorization_header(&server_b).await;
    assert_no_authorization_header(&bad_server).await;
}

#[tokio::test]
#[serial]
async fn checksum_failure_preserves_both_active_hyper_and_official_grok() {
    let env = EnvGuard::install();
    set_test_version("0.2.112");
    let (grok, sentinel) = install_official_sentinel();
    let old = install_old_hyper(&env.hyper_home, "0.2.112");
    let active = env.hyper_home.join("bin/hyper");
    let old_target = std::fs::read_link(&active).unwrap();

    let archive = release_archive(b"#!/bin/sh\nexit 0\n");
    let (server, _) = mount_release("0.2.113", archive, &"0".repeat(64)).await;
    env.use_server(&server);

    let mut config = make_update_config("stable");
    let error = run_update(false, None, None, &mut config)
        .await
        .expect_err("bad checksum must fail closed");
    assert!(format!("{error:#}").contains("SHA-256 mismatch"));
    assert_eq!(std::fs::read_link(&active).unwrap(), old_target);
    assert_eq!(std::fs::read(&old).unwrap(), b"#!/bin/sh\nexit 0\n");
    assert_eq!(std::fs::read(&grok).unwrap(), sentinel);
    assert!(!env.hyper_home.join("update-state.json").exists());
}
