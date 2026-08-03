//! Process-isolated: oversize GROK_EXTRA_CA_BUNDLE → ignored; client still builds.

use std::io::Write;

#[test]
fn oversized_bundle_ignored_clients_build() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("oversized.pem");
    {
        let mut f = std::fs::File::create(&path).expect("create");
        let chunk = vec![b'X'; 64 * 1024];
        let mut written = 0u64;
        let target = xai_grok_extra_ca::MAX_EXTRA_CA_BUNDLE_BYTES + 1;
        while written < target {
            let n = ((target - written) as usize).min(chunk.len());
            f.write_all(&chunk[..n]).expect("write");
            written += n as u64;
        }
    }

    // Safety: sole test in this binary; set before any OnceLock resolve.
    unsafe {
        std::env::set_var(
            xai_grok_extra_ca::ENV_GROK_EXTRA_CA_BUNDLE,
            path.as_os_str(),
        );
    }

    assert!(xai_grok_extra_ca::extra_root_ders().is_empty());

    xai_grok_extra_ca::with_extra_root_certificates(reqwest::Client::builder())
        .build()
        .expect("client builds after oversized reject");
}
