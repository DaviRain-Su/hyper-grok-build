//! End-to-end tests for the isolated Hyper community updater.
//!
//! Fixtures match the release producer contract (`tar -C staging .`):
//! root `hyper`, optional licenses, and a full `bundled/**` tree.

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

/// Release-equivalent archive: `tar -C staging -czf … .` layout.
fn release_archive(binary: &[u8], bundle_files: &[(&str, &[u8])]) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);

    // Root directory entry produced by `tar -C staging .`.
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(0o755);
    header.set_cksum();
    builder.append_data(&mut header, ".", &[] as &[u8]).unwrap();

    let mut header = tar::Header::new_gnu();
    header.set_size(binary.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder.append_data(&mut header, "hyper", binary).unwrap();

    for (name, body) in [
        ("LICENSE", b"test license\n".as_slice()),
        ("NOTICE", b"test notice\n".as_slice()),
        ("THIRD-PARTY-NOTICES", b"third party\n".as_slice()),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, body).unwrap();
    }

    if !bundle_files.is_empty() {
        // Directory entries with trailing `/` (GNU tar / install.sh layout from
        // `tar -C staging .`). install.sh skips these and extracts only files.
        for dir in [
            "bundled/",
            "bundled/skills/",
            "bundled/skills/demo/",
            "bundled/agents/",
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, dir, &[] as &[u8]).unwrap();
        }
        for (rel, body) in bundle_files {
            let name = format!("bundled/{rel}");
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *body).unwrap();
        }
    }

    let encoder = builder.into_inner().unwrap();
    encoder.finish().unwrap()
}

fn default_bundle_files() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("skills/demo/SKILL.md", b"# demo skill\n".as_slice()),
        ("agents/helper.md", b"# helper agent\n".as_slice()),
    ]
}

fn binary_only_archive(binary: &[u8]) -> Vec<u8> {
    release_archive(binary, &[])
}

fn full_release_archive(binary: &[u8]) -> Vec<u8> {
    release_archive(binary, &default_bundle_files())
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
        // Wipe managed bundle left by prior tests in this binary's GROK_HOME.
        let _ = std::fs::remove_dir_all(test_home().join("bundled"));
        let _ = std::fs::remove_dir_all(test_home().join("skills"));
        let hyper_home = tempfile::tempdir().unwrap().keep();
        unsafe {
            std::env::set_var("HYPER_SHARE_DIR", &hyper_home);
            std::env::set_var("HYPER_ALLOW_INSECURE_UPDATE_BASE", "1");
        }
        #[cfg(feature = "community-update-test-hooks")]
        xai_grok_update::set_install_failpoint(None);
        Self { hyper_home }
    }

    fn use_server(&self, server: &MockServer) {
        unsafe { std::env::set_var("HYPER_UPDATE_BASE_URL", server.uri()) };
    }

    fn grok_home(&self) -> &Path {
        test_home()
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        #[cfg(feature = "community-update-test-hooks")]
        xai_grok_update::set_install_failpoint(None);
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

fn seed_old_bundle(grok_home: &Path) {
    let skill = grok_home.join("bundled/skills/old/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, b"# stale managed skill\n").unwrap();
    // User-managed skills live beside bundled/, not inside it.
    let user = grok_home.join("skills/user-skill.md");
    std::fs::create_dir_all(user.parent().unwrap()).unwrap();
    std::fs::write(&user, b"# user skill must survive\n").unwrap();
}

fn assert_bundle_contents(grok_home: &Path, expect_new: bool) {
    let bundled = grok_home.join("bundled");
    if expect_new {
        assert_eq!(
            std::fs::read(bundled.join("skills/demo/SKILL.md")).unwrap(),
            b"# demo skill\n"
        );
        assert_eq!(
            std::fs::read(bundled.join("agents/helper.md")).unwrap(),
            b"# helper agent\n"
        );
        assert!(
            !bundled.join("skills/old/SKILL.md").exists(),
            "stale managed bundle files must be replaced"
        );
    }
    assert_eq!(
        std::fs::read(grok_home.join("skills/user-skill.md")).unwrap(),
        b"# user skill must survive\n"
    );
}

#[tokio::test]
#[serial]
async fn community_update_installs_verified_archive_with_bundle() {
    let env = EnvGuard::install();
    set_test_version("0.2.112");
    let (grok, sentinel) = install_official_sentinel();
    install_old_hyper(&env.hyper_home, "0.2.112");
    seed_old_bundle(env.grok_home());

    let archive = full_release_archive(b"#!/bin/sh\nexit 0\n");
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

    assert_bundle_contents(env.grok_home(), true);

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
async fn binary_only_archive_preserves_existing_bundle() {
    let env = EnvGuard::install();
    set_test_version("0.2.112");
    install_old_hyper(&env.hyper_home, "0.2.112");
    seed_old_bundle(env.grok_home());

    let archive = binary_only_archive(b"#!/bin/sh\nexit 0\n");
    let digest = sha256(&archive);
    let (server, _) = mount_release("0.2.113", archive, &digest).await;
    env.use_server(&server);

    let mut config = make_update_config("stable");
    assert_eq!(
        run_update(false, None, None, &mut config)
            .await
            .unwrap()
            .as_deref(),
        Some("0.2.113")
    );

    // Old managed bundle must remain intact.
    assert_eq!(
        std::fs::read(env.grok_home().join("bundled/skills/old/SKILL.md")).unwrap(),
        b"# stale managed skill\n"
    );
    assert_eq!(
        std::fs::read(env.grok_home().join("skills/user-skill.md")).unwrap(),
        b"# user skill must survive\n"
    );
    let active = env.hyper_home.join("bin/hyper");
    let target = std::fs::read_link(&active).unwrap();
    assert!(target.to_string_lossy().contains(&digest));
}

#[tokio::test]
#[serial]
async fn concurrent_updaters_download_and_activate_archive_once() {
    let env = EnvGuard::install();
    set_test_version("0.2.112");
    install_old_hyper(&env.hyper_home, "0.2.112");

    let archive = full_release_archive(b"#!/bin/sh\nexit 0\n");
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
    assert!(
        env.grok_home()
            .join("bundled/skills/demo/SKILL.md")
            .is_file()
    );
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

    let archive = full_release_archive(b"#!/bin/sh\nexit 0\n# republished\n");
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

    let archive_a = full_release_archive(b"#!/bin/sh\nexit 0\n# build a\n");
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

    let archive_b = full_release_archive(b"#!/bin/sh\nexit 0\n# build b\n");
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

    let bad_archive = full_release_archive(b"#!/bin/sh\nexit 1\n");
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
    seed_old_bundle(env.grok_home());

    let archive = full_release_archive(b"#!/bin/sh\nexit 0\n");
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
    // Bundle must be untouched when the download fails before activation.
    assert_eq!(
        std::fs::read(env.grok_home().join("bundled/skills/old/SKILL.md")).unwrap(),
        b"# stale managed skill\n"
    );
}

#[tokio::test]
#[serial]
#[cfg(feature = "community-update-test-hooks")]
async fn state_commit_failpoint_rolls_back_binary_bundle_and_state() {
    let env = EnvGuard::install();
    set_test_version("0.2.112");
    install_old_hyper(&env.hyper_home, "0.2.112");
    seed_old_bundle(env.grok_home());
    let active = env.hyper_home.join("bin/hyper");
    let old_target = std::fs::read_link(&active).unwrap();
    let old_state = serde_json::json!({
        "installed_version": "0.2.112",
        "installed_asset": format!("hyper-0.2.112-{}.tar.gz", platform_triple()),
        "installed_sha256": "a".repeat(64),
        "installed_binary": old_target.file_name().unwrap().to_string_lossy(),
        "checked_at_unix": 1,
    });
    std::fs::write(
        env.hyper_home.join("update-state.json"),
        serde_json::to_vec_pretty(&old_state).unwrap(),
    )
    .unwrap();

    let archive = full_release_archive(b"#!/bin/sh\nexit 0\n# new\n");
    let digest = sha256(&archive);
    let (server, _) = mount_release("0.2.113", archive, &digest).await;
    env.use_server(&server);

    xai_grok_update::set_install_failpoint(Some("before_state_write"));
    let mut config = make_update_config("stable");
    let error = run_update(false, None, None, &mut config)
        .await
        .expect_err("failpoint must abort commit");
    assert!(
        format!("{error:#}").contains("failpoint") || format!("{error:#}").contains("injected"),
        "{error:#}"
    );

    // Full previous deployment restored.
    assert_eq!(std::fs::read_link(&active).unwrap(), old_target);
    assert_eq!(
        std::fs::read(env.grok_home().join("bundled/skills/old/SKILL.md")).unwrap(),
        b"# stale managed skill\n"
    );
    assert!(
        !env.grok_home()
            .join("bundled/skills/demo/SKILL.md")
            .exists()
    );
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(env.hyper_home.join("update-state.json")).unwrap())
            .unwrap();
    assert_eq!(state["installed_version"], "0.2.112");
    assert_eq!(state["installed_sha256"], "a".repeat(64));
    assert_eq!(
        std::fs::read(env.grok_home().join("skills/user-skill.md")).unwrap(),
        b"# user skill must survive\n"
    );
}

#[tokio::test]
#[serial]
#[cfg(feature = "community-update-test-hooks")]
async fn bundle_activation_failpoint_rolls_back_before_binary_swap() {
    let env = EnvGuard::install();
    set_test_version("0.2.112");
    install_old_hyper(&env.hyper_home, "0.2.112");
    seed_old_bundle(env.grok_home());
    let active = env.hyper_home.join("bin/hyper");
    let old_target = std::fs::read_link(&active).unwrap();
    let old_state = serde_json::json!({
        "installed_version": "0.2.112",
        "installed_asset": "old-asset",
        "installed_sha256": "b".repeat(64),
        "installed_binary": old_target.file_name().unwrap().to_string_lossy(),
        "checked_at_unix": 2,
    });
    std::fs::write(
        env.hyper_home.join("update-state.json"),
        serde_json::to_vec_pretty(&old_state).unwrap(),
    )
    .unwrap();

    let archive = full_release_archive(b"#!/bin/sh\nexit 0\n# new\n");
    let digest = sha256(&archive);
    let (server, _) = mount_release("0.2.113", archive, &digest).await;
    env.use_server(&server);

    xai_grok_update::set_install_failpoint(Some("after_bundle_activation"));
    let mut config = make_update_config("stable");
    let error = run_update(false, None, None, &mut config)
        .await
        .expect_err("failpoint after bundle activation must abort");
    assert!(
        format!("{error:#}").contains("failpoint") || format!("{error:#}").contains("injected")
    );

    assert_eq!(std::fs::read_link(&active).unwrap(), old_target);
    assert_eq!(
        std::fs::read(env.grok_home().join("bundled/skills/old/SKILL.md")).unwrap(),
        b"# stale managed skill\n"
    );
    assert!(
        !env.grok_home()
            .join("bundled/skills/demo/SKILL.md")
            .exists(),
        "new bundle must not remain after rollback"
    );
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(env.hyper_home.join("update-state.json")).unwrap())
            .unwrap();
    assert_eq!(state["installed_sha256"], "b".repeat(64));
}

#[tokio::test]
#[serial]
async fn state_symlink_preflight_fails_before_activation() {
    let env = EnvGuard::install();
    set_test_version("0.2.112");
    install_old_hyper(&env.hyper_home, "0.2.112");
    seed_old_bundle(env.grok_home());
    let active = env.hyper_home.join("bin/hyper");
    let old_target = std::fs::read_link(&active).unwrap();

    // Make update-state.json a symlink — preflight must reject before mutate.
    let real = env.hyper_home.join("real-state.json");
    std::fs::write(&real, b"{\"installed_version\":\"0.2.112\"}\n").unwrap();
    let state_path = env.hyper_home.join("update-state.json");
    let _ = std::fs::remove_file(&state_path);
    std::os::unix::fs::symlink(&real, &state_path).unwrap();

    let archive = full_release_archive(b"#!/bin/sh\nexit 0\n");
    let digest = sha256(&archive);
    let (server, _) = mount_release("0.2.113", archive, &digest).await;
    env.use_server(&server);

    let mut config = make_update_config("stable");
    let error = run_update(false, None, None, &mut config)
        .await
        .expect_err("symlinked update state must fail closed");
    let msg = format!("{error:#}");
    assert!(
        msg.contains("symlink") || msg.contains("update state"),
        "{msg}"
    );

    assert_eq!(std::fs::read_link(&active).unwrap(), old_target);
    assert_eq!(
        std::fs::read(env.grok_home().join("bundled/skills/old/SKILL.md")).unwrap(),
        b"# stale managed skill\n"
    );
    assert!(
        !env.grok_home()
            .join("bundled/skills/demo/SKILL.md")
            .exists(),
        "bundle must not activate when state preflight fails"
    );
    // Symlink shape preserved.
    assert!(state_path.is_symlink());
    assert_eq!(
        std::fs::read(&real).unwrap(),
        b"{\"installed_version\":\"0.2.112\"}\n"
    );
}

#[tokio::test]
#[serial]
async fn state_directory_preflight_fails_before_activation() {
    let env = EnvGuard::install();
    set_test_version("0.2.112");
    install_old_hyper(&env.hyper_home, "0.2.112");
    seed_old_bundle(env.grok_home());
    let active = env.hyper_home.join("bin/hyper");
    let old_target = std::fs::read_link(&active).unwrap();

    let state_path = env.hyper_home.join("update-state.json");
    let _ = std::fs::remove_file(&state_path);
    std::fs::create_dir(&state_path).unwrap();

    let archive = full_release_archive(b"#!/bin/sh\nexit 0\n");
    let digest = sha256(&archive);
    let (server, _) = mount_release("0.2.113", archive, &digest).await;
    env.use_server(&server);

    let mut config = make_update_config("stable");
    let error = run_update(false, None, None, &mut config)
        .await
        .expect_err("directory update state must fail closed");
    let msg = format!("{error:#}");
    assert!(
        msg.contains("regular file") || msg.contains("update state"),
        "{msg}"
    );

    assert_eq!(std::fs::read_link(&active).unwrap(), old_target);
    assert_eq!(
        std::fs::read(env.grok_home().join("bundled/skills/old/SKILL.md")).unwrap(),
        b"# stale managed skill\n"
    );
    assert!(state_path.is_dir());
}

/// Build a release archive with the system `tar -C staging -czf archive .`
/// producer (same as GitHub Release workflows). Must not silently skip.
fn system_tar_release_archive(staging: &Path, archive: &Path) {
    // Populate a workflow-equivalent staging tree.
    std::fs::write(staging.join("hyper"), b"#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            staging.join("hyper"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    std::fs::write(staging.join("LICENSE"), b"test license\n").unwrap();
    std::fs::write(staging.join("NOTICE"), b"test notice\n").unwrap();
    std::fs::write(staging.join("THIRD-PARTY-NOTICES"), b"third party\n").unwrap();
    let skill = staging.join("bundled/skills/demo/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, b"# demo skill\n").unwrap();
    let agent = staging.join("bundled/agents/helper.md");
    std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
    std::fs::write(&agent, b"# helper agent\n").unwrap();

    let status = Command::new("tar")
        .args([
            "-C",
            staging.to_str().expect("utf-8 staging path"),
            "-czf",
            archive.to_str().expect("utf-8 archive path"),
            ".",
        ])
        .status()
        .expect("system tar must be available on release-supported Unix platforms");
    assert!(
        status.success(),
        "system tar -C staging -czf failed with {status}"
    );
    assert!(
        archive.is_file() && std::fs::metadata(archive).unwrap().len() > 0,
        "system tar produced an empty archive"
    );
}

#[tokio::test]
#[serial]
async fn system_tar_producer_archive_installs_successfully() {
    let env = EnvGuard::install();
    set_test_version("0.2.112");
    install_old_hyper(&env.hyper_home, "0.2.112");
    seed_old_bundle(env.grok_home());

    let staging = tempfile::tempdir().unwrap();
    let archive_path = env.hyper_home.join("system-release.tar.gz");
    system_tar_release_archive(staging.path(), &archive_path);
    let archive = std::fs::read(&archive_path).unwrap();
    let digest = sha256(&archive);
    let (server, asset) = mount_release("0.2.113", archive, &digest).await;
    env.use_server(&server);

    let mut config = make_update_config("stable");
    let installed = run_update(false, None, None, &mut config)
        .await
        .expect("system-tar archive must install");
    assert_eq!(installed.as_deref(), Some("0.2.113"));

    let active = env.hyper_home.join("bin/hyper");
    let target = std::fs::read_link(&active).unwrap();
    assert!(target.to_string_lossy().contains(&digest));
    assert_bundle_contents(env.grok_home(), true);
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(env.hyper_home.join("update-state.json")).unwrap())
            .unwrap();
    assert_eq!(state["installed_sha256"], digest);
    assert_eq!(state["installed_asset"], asset);
}

// ── install.sh compensating-transaction + release contract coverage ─────────

fn write_zip_release_archive(path: &Path, binary: &[u8], bundle_files: &[(&str, &[u8])]) {
    use std::collections::HashSet;
    use std::io::Write as _;
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("hyper.exe", options).unwrap();
    zip.write_all(binary).unwrap();
    for (name, body) in [
        ("LICENSE", b"test license\n".as_slice()),
        ("NOTICE", b"test notice\n".as_slice()),
        ("THIRD-PARTY-NOTICES", b"third party\n".as_slice()),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(body).unwrap();
    }
    if !bundle_files.is_empty() {
        let mut dirs = HashSet::new();
        let mut ensure_dir = |zip: &mut zip::ZipWriter<std::fs::File>, dir: &str| {
            let key = if dir.ends_with('/') {
                dir.to_string()
            } else {
                format!("{dir}/")
            };
            if dirs.insert(key.clone()) {
                zip.add_directory(key, options).unwrap();
            }
        };
        ensure_dir(&mut zip, "bundled/");
        for (rel, body) in bundle_files {
            let name = format!("bundled/{rel}");
            if let Some(parent) = Path::new(&name).parent() {
                let mut prefix = String::new();
                for component in parent.components() {
                    if !prefix.is_empty() {
                        prefix.push('/');
                    }
                    prefix.push_str(component.as_os_str().to_str().unwrap());
                    ensure_dir(&mut zip, &prefix);
                }
            }
            zip.start_file(name, options).unwrap();
            zip.write_all(body).unwrap();
        }
    }
    zip.finish().unwrap();
}

#[test]
fn release_archive_contract_accepts_system_tar_and_zip() {
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let tar_path = root.path().join("good.tar.gz");
    system_tar_release_archive(&staging, &tar_path);
    let digest = sha256(&std::fs::read(&tar_path).unwrap());

    let report = xai_grok_update::verify_release_archive(
        &tar_path,
        xai_grok_update::ReleaseArchiveVerifyOptions {
            binary_entry: Some("hyper"),
            expected_sha256: Some(&digest),
            expected_bundle_root: None,
            require_bundle: true,
        },
    )
    .expect("system tar release archive must satisfy contract");
    assert_eq!(report.binary_entry, "hyper");
    assert!(report.bundle_file_count >= 2, "{report:?}");

    let zip_path = root.path().join("good.zip");
    write_zip_release_archive(
        &zip_path,
        b"MZ-fake-windows-binary",
        &default_bundle_files(),
    );
    let zip_digest = sha256(&std::fs::read(&zip_path).unwrap());
    let zip_report = xai_grok_update::verify_release_archive(
        &zip_path,
        xai_grok_update::ReleaseArchiveVerifyOptions {
            binary_entry: Some("hyper.exe"),
            expected_sha256: Some(&zip_digest),
            expected_bundle_root: None,
            require_bundle: true,
        },
    )
    .expect("zip release archive must satisfy contract");
    assert_eq!(zip_report.binary_entry, "hyper.exe");
    assert_eq!(zip_report.bundle_file_count, 2);

    // SHA256SUMS multi-archive verification (publish-job contract).
    let sums = root.path().join("SHA256SUMS");
    std::fs::write(
        &sums,
        format!("{digest}  good.tar.gz\n{zip_digest}  good.zip\n"),
    )
    .unwrap();
    xai_grok_update::verify_sha256sums_manifest(
        &sums,
        &[
            ("good.tar.gz".into(), tar_path.clone()),
            ("good.zip".into(), zip_path.clone()),
        ],
        true,
    )
    .expect("SHA256SUMS must validate both archives");
}

#[test]
fn release_archive_contract_rejects_extra_missing_and_content_diff() {
    let root = tempfile::tempdir().unwrap();
    let expected_bundle = root.path().join("expected-bundled");
    std::fs::create_dir_all(expected_bundle.join("skills/demo")).unwrap();
    std::fs::write(
        expected_bundle.join("skills/demo/SKILL.md"),
        b"# demo skill\n",
    )
    .unwrap();
    std::fs::create_dir_all(expected_bundle.join("agents")).unwrap();
    std::fs::write(
        expected_bundle.join("agents/helper.md"),
        b"# helper agent\n",
    )
    .unwrap();

    // Good archive matches expected.
    let good = full_release_archive(b"#!/bin/sh\nexit 0\n");
    let good_path = root.path().join("good.tar.gz");
    std::fs::write(&good_path, &good).unwrap();
    xai_grok_update::verify_release_archive(
        &good_path,
        xai_grok_update::ReleaseArchiveVerifyOptions {
            binary_entry: Some("hyper"),
            expected_sha256: None,
            expected_bundle_root: Some(&expected_bundle),
            require_bundle: true,
        },
    )
    .expect("matching bundle content must pass");

    // Content differs.
    let mut different_files = default_bundle_files();
    different_files[0] = ("skills/demo/SKILL.md", b"# wrong content\n".as_slice());
    let diff = release_archive(b"#!/bin/sh\nexit 0\n", &different_files);
    let diff_path = root.path().join("diff.tar.gz");
    std::fs::write(&diff_path, &diff).unwrap();
    let err = xai_grok_update::verify_release_archive(
        &diff_path,
        xai_grok_update::ReleaseArchiveVerifyOptions {
            binary_entry: Some("hyper"),
            expected_sha256: None,
            expected_bundle_root: Some(&expected_bundle),
            require_bundle: true,
        },
    )
    .expect_err("content mismatch must fail");
    assert!(
        format!("{err:#}").contains("content differs") || format!("{err:#}").contains("SKILL"),
        "{err:#}"
    );

    // Extra file in archive.
    let mut extra_files = default_bundle_files();
    extra_files.push(("skills/extra/SKILL.md", b"# extra\n".as_slice()));
    let extra = release_archive(b"#!/bin/sh\nexit 0\n", &extra_files);
    let extra_path = root.path().join("extra.tar.gz");
    std::fs::write(&extra_path, &extra).unwrap();
    let err = xai_grok_update::verify_release_archive(
        &extra_path,
        xai_grok_update::ReleaseArchiveVerifyOptions {
            binary_entry: Some("hyper"),
            expected_sha256: None,
            expected_bundle_root: Some(&expected_bundle),
            require_bundle: true,
        },
    )
    .expect_err("extra bundle file must fail");
    assert!(format!("{err:#}").contains("extra"), "{err:#}");

    // Missing file in archive.
    let missing = release_archive(
        b"#!/bin/sh\nexit 0\n",
        &[("skills/demo/SKILL.md", b"# demo skill\n".as_slice())],
    );
    let missing_path = root.path().join("missing.tar.gz");
    std::fs::write(&missing_path, &missing).unwrap();
    let err = xai_grok_update::verify_release_archive(
        &missing_path,
        xai_grok_update::ReleaseArchiveVerifyOptions {
            binary_entry: Some("hyper"),
            expected_sha256: None,
            expected_bundle_root: Some(&expected_bundle),
            require_bundle: true,
        },
    )
    .expect_err("missing bundle file must fail");
    assert!(format!("{err:#}").contains("missing"), "{err:#}");

    // Manifest case collision / duplicate archive args.
    let sums = root.path().join("SHA256SUMS");
    let d = sha256(&good);
    std::fs::write(&sums, format!("{d}  good.tar.gz\n{d}  GOOD.tar.gz\n")).unwrap();
    let err = xai_grok_update::verify_sha256sums_manifest(
        &sums,
        &[
            ("good.tar.gz".into(), good_path.clone()),
            ("GOOD.tar.gz".into(), good_path.clone()),
        ],
        true,
    )
    .expect_err("case-colliding manifest names must fail");
    assert!(
        format!("{err:#}").contains("case-colliding") || format!("{err:#}").contains("duplicate"),
        "{err:#}"
    );
}

#[test]
fn release_archive_contract_rejects_unexpected_root_and_bad_checksum() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("bad.tar.gz");
    // Unexpected root README must fail closed.
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (name, body) in [
        ("hyper", b"#!/bin/sh\nexit 0\n".as_slice()),
        ("README", b"nope\n".as_slice()),
        ("bundled/skills/x/SKILL.md", b"# x\n".as_slice()),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, body).unwrap();
    }
    let bytes = builder.into_inner().unwrap().finish().unwrap();
    std::fs::write(&archive, &bytes).unwrap();
    let err = xai_grok_update::verify_release_archive(
        &archive,
        xai_grok_update::ReleaseArchiveVerifyOptions {
            binary_entry: Some("hyper"),
            expected_sha256: None,
            expected_bundle_root: None,
            require_bundle: true,
        },
    )
    .expect_err("unexpected root entry must fail");
    assert!(
        format!("{err:#}").contains("unexpected") || format!("{err:#}").contains("README"),
        "{err:#}"
    );

    let good = full_release_archive(b"#!/bin/sh\nexit 0\n");
    let good_path = dir.path().join("good.tar.gz");
    std::fs::write(&good_path, &good).unwrap();
    let err = xai_grok_update::verify_release_archive(
        &good_path,
        xai_grok_update::ReleaseArchiveVerifyOptions {
            binary_entry: Some("hyper"),
            expected_sha256: Some(&"0".repeat(64)),
            expected_bundle_root: None,
            require_bundle: true,
        },
    )
    .expect_err("checksum mismatch must fail");
    assert!(format!("{err:#}").contains("SHA-256"), "{err:#}");
}

/// Copy `install.sh` and optionally inject a hard failure at a documented
/// marker. Production scripts never read failpoint environment variables.
fn install_sh_for_test(user_home: &Path, inject_after_state: bool) -> PathBuf {
    let src = std::fs::read_to_string(root_installer_path()).unwrap();
    // Production must not expose a controllable failpoint env var.
    assert!(
        !src.contains("HYPER_INSTALL_FAILPOINT"),
        "install.sh must not honor production-controllable failpoint env vars"
    );
    let body = if inject_after_state {
        src.replace(
            "# INJECT_FAIL_AFTER_STATE",
            r#"
# TEST-ONLY injection (this copy is not the shipped install.sh).
# Must roll back activated binary+state before exiting.
rollback_all
fail_with_rollback "injected install failure after state activation"
"#,
        )
    } else {
        src
    };
    assert!(
        body.contains("INJECT_AFTER_STATE_MARKER") || inject_after_state,
        "install.sh must keep documented injection markers for tests"
    );
    let path = user_home.join(if inject_after_state {
        "install-injected.sh"
    } else {
        "install-copy.sh"
    });
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn run_root_installer_with_grok(
    hyper_home: &Path,
    user_home: &Path,
    grok_home: &Path,
    server: &MockServer,
    install_sh: &Path,
) -> Output {
    let tmp = user_home.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    Command::new("/bin/sh")
        .arg(install_sh)
        .env("HOME", user_home)
        .env("TMPDIR", tmp)
        .env("PATH", inherited_path)
        .env("SHELL", "/bin/sh")
        .env("HYPER_SHARE_DIR", hyper_home)
        .env("GROK_HOME", grok_home)
        .env("HYPER_UPDATE_BASE_URL", server.uri())
        .env("GITHUB_TOKEN", "must-not-leak-to-custom-update-host")
        .output()
        .expect("root Hyper installer should run")
}

#[tokio::test]
#[serial]
async fn root_installer_bundle_transaction_rolls_back_binary_and_state() {
    // Real install.sh: full archive → binary-only preserve → failpoint after
    // state commit restores previous binary+state+bundle as one transaction.
    let env = EnvGuard::install();
    let user_home = tempfile::tempdir().unwrap();
    let grok_home = user_home.path().join(".grok");
    std::fs::create_dir_all(grok_home.join("bin")).unwrap();
    let official = grok_home.join("bin/grok");
    std::fs::write(&official, b"official-grok-sentinel\n").unwrap();

    // Seed previous managed bundle + user skill.
    let old_skill = grok_home.join("bundled/skills/old/SKILL.md");
    std::fs::create_dir_all(old_skill.parent().unwrap()).unwrap();
    std::fs::write(&old_skill, b"# stale managed skill\n").unwrap();
    let user_skill = grok_home.join("skills/user-skill.md");
    std::fs::create_dir_all(user_skill.parent().unwrap()).unwrap();
    std::fs::write(&user_skill, b"# user skill must survive\n").unwrap();

    let clean_sh = install_sh_for_test(user_home.path(), false);
    let injected_sh = install_sh_for_test(user_home.path(), true);

    // First install succeeds with full bundle.
    let archive_a = full_release_archive(b"#!/bin/sh\nexit 0\n# build a\n");
    let digest_a = sha256(&archive_a);
    let (server_a, _) = mount_release("0.2.113", archive_a, &digest_a).await;
    let first = run_root_installer_with_grok(
        &env.hyper_home,
        user_home.path(),
        &grok_home,
        &server_a,
        &clean_sh,
    );
    assert!(
        first.status.success(),
        "first install failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    let active = env.hyper_home.join("bin/hyper");
    let target_a = active_target(&active);
    assert!(target_a.to_string_lossy().contains(&digest_a));
    assert_eq!(
        std::fs::read(grok_home.join("bundled/skills/demo/SKILL.md")).unwrap(),
        b"# demo skill\n"
    );
    assert!(!grok_home.join("bundled/skills/old/SKILL.md").exists());

    // Binary-only package: must keep the existing bundle and only flip binary/state.
    let archive_bin = binary_only_archive(b"#!/bin/sh\nexit 0\n# binary only\n");
    let digest_bin = sha256(&archive_bin);
    let (server_bin, _) = mount_release("0.2.114", archive_bin, &digest_bin).await;
    let binary_only = run_root_installer_with_grok(
        &env.hyper_home,
        user_home.path(),
        &grok_home,
        &server_bin,
        &clean_sh,
    );
    assert!(
        binary_only.status.success(),
        "binary-only install failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&binary_only.stdout),
        String::from_utf8_lossy(&binary_only.stderr)
    );
    let target_bin = active_target(&active);
    assert!(target_bin.to_string_lossy().contains(&digest_bin));
    assert_eq!(
        std::fs::read(grok_home.join("bundled/skills/demo/SKILL.md")).unwrap(),
        b"# demo skill\n",
        "binary-only package must preserve existing bundle"
    );
    assert_eq!(
        std::fs::read(&official).unwrap(),
        b"official-grok-sentinel\n",
        "installer must never overwrite ~/.grok/bin/grok"
    );
    assert_eq!(
        std::fs::read(&user_skill).unwrap(),
        b"# user skill must survive\n"
    );
    let state_bin: serde_json::Value =
        serde_json::from_slice(&std::fs::read(env.hyper_home.join("update-state.json")).unwrap())
            .unwrap();
    assert_eq!(state_bin["installed_sha256"], digest_bin);

    // Injected failure after state write (script copy only): binary+state
    // activated, bundle not yet — rollback must restore previous deployment.
    let archive_b = full_release_archive(b"#!/bin/sh\nexit 0\n# build b should roll back\n");
    let digest_b = sha256(&archive_b);
    let (server_b, _) = mount_release("0.2.115", archive_b, &digest_b).await;
    let failed = run_root_installer_with_grok(
        &env.hyper_home,
        user_home.path(),
        &grok_home,
        &server_b,
        &injected_sh,
    );
    assert!(
        !failed.status.success(),
        "injected failure must abort the installer:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&failed.stdout),
        String::from_utf8_lossy(&failed.stderr)
    );
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("injected"),
        "stderr should mention injected failure: {stderr}"
    );

    // Binary + state restored to the binary-only install identity.
    assert_eq!(
        active_target(&active),
        target_bin,
        "binary must roll back when post-state injection fires"
    );
    let state_after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(env.hyper_home.join("update-state.json")).unwrap())
            .unwrap();
    assert_eq!(
        state_after["installed_sha256"], digest_bin,
        "update-state must roll back with the binary"
    );
    // Prior bundle still active (never replaced).
    assert_eq!(
        std::fs::read(grok_home.join("bundled/skills/demo/SKILL.md")).unwrap(),
        b"# demo skill\n"
    );
    assert_eq!(
        std::fs::read(&official).unwrap(),
        b"official-grok-sentinel\n"
    );
    assert_eq!(
        std::fs::read(&user_skill).unwrap(),
        b"# user skill must survive\n"
    );
    assert_no_authorization_header(&server_a).await;
    assert_no_authorization_header(&server_bin).await;
    assert_no_authorization_header(&server_b).await;
}

#[test]
fn install_ps1_static_syntax_and_contract_markers() {
    // Full Windows execution lives in test_install_ps1 (windows-2022 CI).
    // Linux still guards shipped-script markers and optional pwsh parse.
    let ps1 =
        dunce::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../install.ps1"))
            .expect("install.ps1 must exist at repo root");
    let body = std::fs::read_to_string(&ps1).unwrap();

    for needle in [
        "rollback was incomplete",
        "Invoke-RollbackAll",
        "bundled.install.",
        "update-state.json",
        "hyper.exe",
        "SHA256SUMS",
        "installed_sha256",
        "INJECT_AFTER_STATE_MARKER",
        "# INJECT_FAIL_AFTER_STATE",
        "MovedBinaryAside",
        "MovedStateAside",
        "MovedBundleAside",
    ] {
        assert!(
            body.contains(needle),
            "install.ps1 missing expected marker {needle:?}"
        );
    }
    assert!(
        !body.contains("HYPER_INSTALL_FAILPOINT"),
        "install.ps1 must not honor production failpoint env vars"
    );
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if (lower.contains(".grok\\bin\\grok") || lower.contains(".grok/bin/grok"))
            && (lower.contains("copy-item")
                || lower.contains("move-item")
                || lower.contains("writeall")
                || lower.contains("set-content")
                || lower.contains("out-file")
                || lower.contains("= join-path")
                || lower.contains("destination"))
        {
            panic!("install.ps1 must not write ~/.grok/bin/grok: {trimmed}");
        }
    }

    // PowerShell requires [ref] targets to be *existing variables* — `[ref]$null`
    // and an undeclared `$errs` raise InvalidOperation on modern pwsh and fail
    // the static parse gate even when install.ps1 itself is fine.
    if let Ok(status) = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "$tokens = $null; $errs = $null; $null = [System.Management.Automation.Language.Parser]::ParseFile('{}', [ref]$tokens, [ref]$errs); if ($errs) {{ $errs | ForEach-Object {{ $_.ToString() }}; exit 1 }}",
                ps1.display().to_string().replace('\'', "''")
            ),
        ])
        .status()
    {
        assert!(status.success(), "install.ps1 failed PowerShell parse check");
    } else {
        eprintln!("pwsh not available; skipped live PowerShell parse of install.ps1");
    }
}

#[test]
fn install_sh_keeps_injection_markers_and_no_failpoint_env() {
    let body = std::fs::read_to_string(root_installer_path()).unwrap();
    assert!(body.contains("INJECT_AFTER_STATE_MARKER"));
    assert!(body.contains("# INJECT_FAIL_AFTER_STATE"));
    assert!(!body.contains("HYPER_INSTALL_FAILPOINT"));
    assert!(body.contains("MOVED_BINARY_ASIDE"));
    assert!(body.contains("MOVED_STATE_ASIDE"));
    assert!(body.contains("MOVED_BUNDLE_ASIDE"));
    assert!(
        body.contains("TAR_FLAVOR=gnu") && body.contains("TAR_FLAVOR=bsd"),
        "install.sh must branch on GNU tar vs bsdtar"
    );
    assert!(
        body.contains("*//*)"),
        "install.sh must explicitly reject mid-path double-slash"
    );
}

/// Exercise install.sh's strict SHA256SUMS whole-manifest parser against a
/// local release fixture. Malformed lines must fail before any live mutation.
#[tokio::test]
#[serial]
async fn install_sh_rejects_malformed_sha256sums_before_mutation() {
    let env = EnvGuard::install();
    let user_home = tempfile::tempdir().unwrap();
    let grok_home = user_home.path().join(".grok");
    std::fs::create_dir_all(grok_home.join("bin")).unwrap();
    let official = grok_home.join("bin/grok");
    std::fs::write(&official, b"official-grok-sentinel\n").unwrap();

    let archive = full_release_archive(b"#!/bin/sh\nexit 0\n");
    let digest = sha256(&archive);
    let asset = format!("hyper-0.2.150-{}.tar.gz", platform_triple());

    // Case-colliding names in the manifest.
    let bad_sums = format!(
        "{digest}  {asset}\n{digest}  {}\n",
        asset.to_ascii_uppercase()
    );
    let server = MockServer::start().await;
    let base = server.uri();
    let metadata = serde_json::json!({
        "tag_name": "v0.2.150",
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
        .respond_with(ResponseTemplate::new(200).set_body_string(bad_sums))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/assets/{asset}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(archive))
        .mount(&server)
        .await;

    let clean_sh = install_sh_for_test(user_home.path(), false);
    let out = run_root_installer_with_grok(
        &env.hyper_home,
        user_home.path(),
        &grok_home,
        &server,
        &clean_sh,
    );
    assert!(
        !out.status.success(),
        "case-colliding SHA256SUMS must fail:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("case-colliding")
            || stderr.contains("duplicate")
            || stderr.contains("SHA256SUMS"),
        "stderr should mention SUMS rejection: {stderr}"
    );
    // No live mutation of official grok or hyper home binary.
    assert_eq!(
        std::fs::read(&official).unwrap(),
        b"official-grok-sentinel\n"
    );
    assert!(!env.hyper_home.join("bin/hyper").exists());
}

#[test]
fn install_sh_normalize_member_rejects_double_slash_and_dotdot() {
    // GNU/bsdtar listings often normalize `//` away, so exercise the explicit
    // normalize_member rules from the shipped script in isolation.
    let install_sh = std::fs::read_to_string(root_installer_path()).unwrap();
    let start = install_sh
        .find("normalize_member() {")
        .expect("normalize_member function");
    let rest = &install_sh[start..];
    let end = rest.find("\n}\n").expect("end of normalize_member");
    let func = &rest[..=end + 1];
    let harness = format!(
        r#"{func}
fail=0
normalize_member 'bundled//evil' >/dev/null 2>&1 && fail=1
normalize_member 'bundled/../escape' >/dev/null 2>&1 && fail=1
normalize_member 'bundled/skills/x.md' >/dev/null 2>&1 || fail=1
normalize_member './hyper' >/dev/null 2>&1 || fail=1
normalize_member 'bundled/' >/dev/null 2>&1 || fail=1
exit $fail
"#
    );
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(harness)
        .status()
        .expect("normalize harness");
    assert!(
        status.success(),
        "normalize_member must reject // and .. while accepting safe paths"
    );
}

#[tokio::test]
#[serial]
async fn install_sh_rejects_symlink_members_before_mutation() {
    let env = EnvGuard::install();
    let user_home = tempfile::tempdir().unwrap();
    let grok_home = user_home.path().join(".grok");
    std::fs::create_dir_all(grok_home.join("bin")).unwrap();
    let official = grok_home.join("bin/grok");
    std::fs::write(&official, b"official-grok-sentinel\n").unwrap();

    // Tar with a symlink entry — pre-scan must fail closed before extract.
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_link_name("outside").unwrap();
    header.set_mode(0o777);
    header.set_cksum();
    builder
        .append_data(&mut header, "hyper", &[] as &[u8])
        .unwrap();
    let archive = builder.into_inner().unwrap().finish().unwrap();
    let digest = sha256(&archive);
    let (server, _) = mount_release("0.2.152", archive, &digest).await;

    let clean_sh = install_sh_for_test(user_home.path(), false);
    let out = run_root_installer_with_grok(
        &env.hyper_home,
        user_home.path(),
        &grok_home,
        &server,
        &clean_sh,
    );
    assert!(
        !out.status.success(),
        "symlink member must fail before mutation:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("symlink") || stderr.contains("unsupported"),
        "stderr should mention symlink rejection: {stderr}"
    );
    assert_eq!(
        std::fs::read(&official).unwrap(),
        b"official-grok-sentinel\n"
    );
    assert!(!env.hyper_home.join("bin/hyper").exists());
}
