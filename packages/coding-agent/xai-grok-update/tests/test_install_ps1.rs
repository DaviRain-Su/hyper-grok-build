//! Windows-native install.ps1 coverage.
//!
//! Runs the real repository-root `install.ps1` against a local HTTP fixture
//! that serves release-shaped zip archives + SHA256SUMS. Non-Windows hosts
//! only compile the marker guard; GitHub Actions `windows-2022` executes the
//! full end-to-end path.

#![cfg(feature = "community-build")]

use std::path::{Path, PathBuf};

fn root_install_ps1() -> PathBuf {
    dunce::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../install.ps1"))
        .expect("repository-root install.ps1 should exist")
}

#[test]
fn install_ps1_keeps_injection_markers_and_no_failpoint_env() {
    let body = std::fs::read_to_string(root_install_ps1()).unwrap();
    assert!(
        body.contains("INJECT_AFTER_STATE_MARKER"),
        "install.ps1 missing INJECT_AFTER_STATE_MARKER"
    );
    assert!(
        body.contains("# INJECT_FAIL_AFTER_STATE"),
        "install.ps1 missing INJECT_FAIL_AFTER_STATE marker"
    );
    assert!(
        !body.contains("HYPER_INSTALL_FAILPOINT"),
        "install.ps1 must not honor HYPER_INSTALL_FAILPOINT"
    );
    assert!(
        body.contains("previous deployment restored if available")
            || body.contains("rollback was incomplete"),
        "install.ps1 must report compensating rollback failures"
    );
}

#[cfg(windows)]
mod windows_live {
    use super::*;
    use std::io::Write as _;
    use std::net::SocketAddr;
    use std::process::Command;

    use serial_test::serial;
    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn sha256(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn write_zip(path: &Path, binary: &[u8], bundle: &[(&str, &[u8])]) {
        use std::collections::HashSet;
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
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
        if !bundle.is_empty() {
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
            for (rel, body) in bundle {
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

    async fn serve_fixture(
        root: PathBuf,
        version: String,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let root = root.clone();
                let version = version.clone();
                let addr = addr;
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let line = req.lines().next().unwrap_or("");
                    let path = line.split_whitespace().nth(1).unwrap_or("/");
                    let (status, ctype, body) = if path == "/latest" || path.starts_with("/tags/") {
                        let asset = format!("hyper-{version}-x86_64-pc-windows-msvc.zip");
                        let json = format!(
                            r#"{{"tag_name":"v{version}","assets":[
                            {{"name":"{asset}","browser_download_url":"http://{addr}/assets/{asset}"}},
                            {{"name":"SHA256SUMS","browser_download_url":"http://{addr}/assets/SHA256SUMS"}}
                        ]}}"#
                        );
                        (200, "application/json", json.into_bytes())
                    } else if let Some(name) = path.strip_prefix("/assets/") {
                        let file = root.join(name);
                        match std::fs::read(&file) {
                            Ok(bytes) => (200, "application/octet-stream", bytes),
                            Err(_) => (404, "text/plain", b"missing".to_vec()),
                        }
                    } else {
                        (404, "text/plain", b"not found".to_vec())
                    };
                    let header = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(header.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });
        (addr, handle)
    }

    fn run_install_ps1(
        install_ps1: &Path,
        hyper_home: &Path,
        grok_home: &Path,
        api_base: &str,
        version: &str,
    ) -> std::process::Output {
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                install_ps1.to_str().unwrap(),
                "-Version",
                version,
            ])
            .env("HYPER_SHARE_DIR", hyper_home)
            .env("GROK_HOME", grok_home)
            .env("HYPER_UPDATE_BASE_URL", api_base)
            .env("USERPROFILE", hyper_home.parent().unwrap_or(hyper_home))
            .output()
            .expect("install.ps1 should spawn")
    }

    fn install_ps1_with_injection(dst: &Path, inject_after_state: bool) {
        let src = std::fs::read_to_string(root_install_ps1()).unwrap();
        assert!(
            !src.contains("HYPER_INSTALL_FAILPOINT"),
            "install.ps1 must not honor production failpoint env vars"
        );
        let body = if inject_after_state {
            // Must throw into the unified transaction catch, which calls
            // Invoke-RollbackAll then Fail — never exit/Fail directly here.
            src.replace(
                "# INJECT_FAIL_AFTER_STATE",
                r#"
        # TEST-ONLY injection (this copy is not the shipped install.ps1).
        throw "injected install failure after state activation"
"#,
            )
        } else {
            src
        };
        assert!(
            body.contains("INJECT_AFTER_STATE_MARKER") || inject_after_state,
            "install.ps1 must keep documented injection markers"
        );
        std::fs::write(dst, body).unwrap();
    }

    fn compile_stub(src: &Path, out: &Path, exits_ok: bool) {
        let code = if exits_ok {
            r#"fn main(){let a:Vec<String>=std::env::args().collect(); if a.get(1).map(|s|s.as_str())==Some("--version"){println!("hyper 0.0.0-test");} std::process::exit(0);}"#
        } else {
            r#"fn main(){std::process::exit(1);}"#
        };
        std::fs::write(src, code).unwrap();
        let status = Command::new("rustc")
            .args([src.to_str().unwrap(), "-O", "-o", out.to_str().unwrap()])
            .status()
            .expect("rustc must be available on windows-2022 CI");
        assert!(
            status.success(),
            "failed to compile stub at {}",
            out.display()
        );
    }

    #[tokio::test]
    #[serial]
    async fn install_ps1_full_binary_only_injected_rollback_and_malicious_zip() {
        let fixture = tempfile::tempdir().unwrap();
        let good_exe = fixture.path().join("good.exe");
        let bad_exe = fixture.path().join("bad.exe");
        compile_stub(&fixture.path().join("good.rs"), &good_exe, true);
        compile_stub(&fixture.path().join("bad.rs"), &bad_exe, false);
        let good_bin = std::fs::read(&good_exe).unwrap();

        let bundle = [
            ("skills/demo/SKILL.md", b"# demo skill\n".as_slice()),
            ("agents/helper.md", b"# helper agent\n".as_slice()),
        ];

        let assets = fixture.path().join("assets");
        std::fs::create_dir_all(&assets).unwrap();

        let hyper_home = tempfile::tempdir().unwrap();
        let grok_home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(grok_home.path().join("bin")).unwrap();
        let official = grok_home.path().join("bin").join("grok");
        std::fs::write(&official, b"official-grok-sentinel\n").unwrap();
        let user_skill = grok_home.path().join("skills").join("user-skill.md");
        std::fs::create_dir_all(user_skill.parent().unwrap()).unwrap();
        std::fs::write(&user_skill, b"# user skill must survive\n").unwrap();
        let old_bundle = grok_home
            .path()
            .join("bundled")
            .join("skills")
            .join("old")
            .join("SKILL.md");
        std::fs::create_dir_all(old_bundle.parent().unwrap()).unwrap();
        std::fs::write(&old_bundle, b"# stale\n").unwrap();

        let clean_ps1 = hyper_home.path().join("install-clean.ps1");
        install_ps1_with_injection(&clean_ps1, false);

        // Full install with bundle.
        let version = "0.2.200";
        let zip_name = format!("hyper-{version}-x86_64-pc-windows-msvc.zip");
        let zip_path = assets.join(&zip_name);
        write_zip(&zip_path, &good_bin, &bundle);
        let digest = sha256(&std::fs::read(&zip_path).unwrap());
        std::fs::write(assets.join("SHA256SUMS"), format!("{digest}  {zip_name}\n")).unwrap();
        let (addr, _srv) = serve_fixture(assets.clone(), version.to_string()).await;
        let api = format!("http://{addr}");
        let out = run_install_ps1(
            &clean_ps1,
            hyper_home.path(),
            grok_home.path(),
            &api,
            version,
        );
        assert!(
            out.status.success(),
            "full install failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(hyper_home.path().join("bin").join("hyper.exe").is_file());
        assert_eq!(
            std::fs::read(
                grok_home
                    .path()
                    .join("bundled")
                    .join("skills")
                    .join("demo")
                    .join("SKILL.md")
            )
            .unwrap(),
            b"# demo skill\n"
        );
        assert_eq!(
            std::fs::read(&official).unwrap(),
            b"official-grok-sentinel\n"
        );
        assert!(!old_bundle.exists());

        // Binary-only package preserves existing bundle.
        let version2 = "0.2.201";
        let zip_name2 = format!("hyper-{version2}-x86_64-pc-windows-msvc.zip");
        write_zip(&assets.join(&zip_name2), &good_bin, &[]);
        let digest2 = sha256(&std::fs::read(assets.join(&zip_name2)).unwrap());
        std::fs::write(
            assets.join("SHA256SUMS"),
            format!("{digest2}  {zip_name2}\n"),
        )
        .unwrap();
        let (addr2, _srv2) = serve_fixture(assets.clone(), version2.to_string()).await;
        let out2 = run_install_ps1(
            &clean_ps1,
            hyper_home.path(),
            grok_home.path(),
            &format!("http://{addr2}"),
            version2,
        );
        assert!(
            out2.status.success(),
            "binary-only install failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out2.stdout),
            String::from_utf8_lossy(&out2.stderr)
        );
        assert_eq!(
            std::fs::read(
                grok_home
                    .path()
                    .join("bundled")
                    .join("skills")
                    .join("demo")
                    .join("SKILL.md")
            )
            .unwrap(),
            b"# demo skill\n",
            "binary-only must preserve existing bundle"
        );

        // Injected throw after state activation: catch → Invoke-RollbackAll.
        // Assert binary + state + bundle all restore.
        let version3 = "0.2.202";
        let zip_name3 = format!("hyper-{version3}-x86_64-pc-windows-msvc.zip");
        write_zip(&assets.join(&zip_name3), &good_bin, &bundle);
        let digest3 = sha256(&std::fs::read(assets.join(&zip_name3)).unwrap());
        std::fs::write(
            assets.join("SHA256SUMS"),
            format!("{digest3}  {zip_name3}\n"),
        )
        .unwrap();
        let (addr3, _srv3) = serve_fixture(assets.clone(), version3.to_string()).await;
        let injected = hyper_home.path().join("install-injected.ps1");
        install_ps1_with_injection(&injected, true);
        let state_before = std::fs::read(hyper_home.path().join("update-state.json")).unwrap();
        let binary_before = std::fs::read(hyper_home.path().join("bin").join("hyper.exe")).unwrap();
        let bundle_before = std::fs::read(
            grok_home
                .path()
                .join("bundled")
                .join("skills")
                .join("demo")
                .join("SKILL.md"),
        )
        .unwrap();
        let failed = run_install_ps1(
            &injected,
            hyper_home.path(),
            grok_home.path(),
            &format!("http://{addr3}"),
            version3,
        );
        assert!(
            !failed.status.success(),
            "injected failure must fail the installer:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&failed.stdout),
            String::from_utf8_lossy(&failed.stderr)
        );
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&failed.stdout),
            String::from_utf8_lossy(&failed.stderr)
        );
        assert!(
            combined.contains("injected"),
            "installer output should mention injected failure: {combined}"
        );
        let state_after = std::fs::read(hyper_home.path().join("update-state.json")).unwrap();
        assert_eq!(
            state_before, state_after,
            "update-state must roll back after injected failure"
        );
        let binary_after = std::fs::read(hyper_home.path().join("bin").join("hyper.exe")).unwrap();
        assert_eq!(
            binary_before, binary_after,
            "binary must roll back after injected failure"
        );
        let bundle_after = std::fs::read(
            grok_home
                .path()
                .join("bundled")
                .join("skills")
                .join("demo")
                .join("SKILL.md"),
        )
        .unwrap();
        assert_eq!(
            bundle_before, bundle_after,
            "bundle must remain the pre-injection tree"
        );
        assert_eq!(
            std::fs::read(&official).unwrap(),
            b"official-grok-sentinel\n"
        );
        assert_eq!(
            std::fs::read(&user_skill).unwrap(),
            b"# user skill must survive\n"
        );

        // Malicious zip with path traversal is rejected before mutation.
        let version4 = "0.2.203";
        let zip_name4 = format!("hyper-{version4}-x86_64-pc-windows-msvc.zip");
        {
            let file = std::fs::File::create(assets.join(&zip_name4)).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("hyper.exe", options).unwrap();
            zip.write_all(&good_bin).unwrap();
            zip.start_file("../evil.txt", options).unwrap();
            zip.write_all(b"pwned\n").unwrap();
            zip.finish().unwrap();
        }
        let digest4 = sha256(&std::fs::read(assets.join(&zip_name4)).unwrap());
        std::fs::write(
            assets.join("SHA256SUMS"),
            format!("{digest4}  {zip_name4}\n"),
        )
        .unwrap();
        let (addr4, _srv4) = serve_fixture(assets.clone(), version4.to_string()).await;
        let evil = run_install_ps1(
            &clean_ps1,
            hyper_home.path(),
            grok_home.path(),
            &format!("http://{addr4}"),
            version4,
        );
        assert!(
            !evil.status.success(),
            "malicious zip must be rejected:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&evil.stdout),
            String::from_utf8_lossy(&evil.stderr)
        );
        let _ = bad_exe;
    }
}
