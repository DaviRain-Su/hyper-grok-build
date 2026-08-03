//! Build script for bundling ripgrep for the grok-shell crate.
//!
//! - If `GROK_SHELL_BUNDLE_RG_PATH` is set, always bundle it
//! - Otherwise, only bundle in release builds
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const RG_VER: &str = "15.0.0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Only bundle in release builds to avoid slowing down cargo check.
    println!("cargo:rerun-if-env-changed=GROK_SHELL_BUNDLE_RG_PATH");
    println!("cargo:rerun-if-env-changed=GROK_SHELL_RG_DOWNLOAD_BASE");
    // Declare our custom cfg to the compiler so cfg(bundle_rg) is recognized by lints
    println!("cargo:rustc-check-cfg=cfg(bundle_rg)");

    // Decide whether to bundle: path override OR release build. Bail before
    // touching the filesystem so debug `cargo check` needs no environment.
    let path_override = env::var("GROK_SHELL_BUNDLE_RG_PATH").ok();
    let is_release = env::var("PROFILE").as_deref() == Ok("release");
    if path_override.is_none() && !is_release {
        return Ok(());
    }

    // In Bazel builds, write into OUT_DIR (which is writable) rather than
    // XAI_ROOT/target/tmp (which is read-only inside the sandbox). Outside
    // Bazel, prefer XAI_ROOT's shared cache dir (monorepo behavior) and fall
    // back to OUT_DIR for standalone checkouts where XAI_ROOT is not a thing.
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let in_bazel = is_bazel_build(&manifest_dir);
    let gen_dir = if in_bazel {
        // OUT_DIR is always set by Cargo/Bazel for build scripts.
        PathBuf::from(env::var("OUT_DIR")?)
    } else if let Ok(xai_root) = env::var("XAI_ROOT") {
        PathBuf::from(xai_root).join("target/tmp/grok-shell-bundle-rg")
    } else {
        PathBuf::from(env::var("OUT_DIR")?)
    };
    fs::create_dir_all(&gen_dir)?;

    // Skip auto-bundling on Windows: ripgrep ships .zip there (not .tar.gz)
    // and we do not yet have a zip-extraction path. Returning here BEFORE
    // emitting `cargo:rustc-cfg=bundle_rg` keeps the include_bytes! macros
    // gated on cfg(bundle_rg) compiled-out, so the runtime falls back to
    // `rg` on PATH (see src/util/ripgrep.rs::rg_path). Users install via
    // `winget install BurntSushi.ripgrep.MSVC` or `scoop install ripgrep`.
    // An explicit GROK_SHELL_BUNDLE_RG_PATH still bundles on Windows (the
    // override path below copies any binary regardless of target).
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" && path_override.is_none() {
        return Ok(());
    }

    // Expose cfg so the crate can include the bundled bytes.
    println!("cargo:rustc-cfg=bundle_rg");
    println!("cargo:rustc-env=GROK_SHELL_RG_VER={}", RG_VER);
    println!(
        "cargo:rustc-env=GROK_SHELL_RG_GEN_DIR={}",
        gen_dir.display()
    );

    // If a local rg binary is provided, copy it directly (skips target check).
    if let Some(path) = path_override {
        let dest = gen_dir.join(format!("rg-{}-override.bin", RG_VER));
        println!("cargo:rustc-env=GROK_SHELL_RG_TARGET=override");
        let _ = fs::remove_file(&dest);
        fs::copy(PathBuf::from(path.clone()), &dest).map_err(|e| {
            format!(
                "Failed copying GROK_SHELL_BUNDLE_RG_PATH: {e} from path {path} to dest {}",
                dest.display()
            )
        })?;
        return Ok(());
    }

    // Determine supported ripgrep asset triple for auto-download.
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    let asset_triple = match (target_os.as_str(), target_arch.as_str()) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        _ => {
            return Err(format!(
                "Unsupported target for ripgrep bundling: {os}-{arch}. Set GROK_SHELL_BUNDLE_RG_PATH to a local rg binary for offline or unsupported builds.",
                os = target_os,
                arch = target_arch
            ).into());
        }
    };

    println!("cargo:rustc-env=GROK_SHELL_RG_TARGET={}", asset_triple);
    let dest = gen_dir.join(format!("rg-{}-{}.bin", RG_VER, asset_triple));
    let _ = fs::remove_file(&dest);

    // Download base is overridable so sandboxed/offline CI can point at an
    // internal mirror (e.g. GROK_SHELL_RG_DOWNLOAD_BASE=http://<mirror>/github/
    // BurntSushi/ripgrep/releases/download). Defaults to the public GitHub
    // releases URL.
    let download_base = env::var("GROK_SHELL_RG_DOWNLOAD_BASE")
        .unwrap_or_else(|_| "https://github.com/BurntSushi/ripgrep/releases/download".to_string());
    let url = format!(
        "{base}/{v}/ripgrep-{v}-{t}.tar.gz",
        base = download_base.trim_end_matches('/'),
        v = RG_VER,
        t = asset_triple
    );

    let bytes: Vec<u8> = {
        let resp = reqwest::blocking::get(&url).map_err(|e| {
            format!(
                "Failed to download ripgrep: {}\nSet GROK_SHELL_BUNDLE_RG_PATH to a local rg for offline builds.",
                e
            )
        })?;
        if !resp.status().is_success() {
            return Err(format!(
                "HTTP {} downloading ripgrep. Set GROK_SHELL_BUNDLE_RG_PATH for offline builds.",
                resp.status()
            )
            .into());
        }
        resp.bytes()?.to_vec()
    };

    let gz = flate2::read::GzDecoder::new(&bytes[..]);
    let mut ar = tar::Archive::new(gz);
    let mut found = false;
    for entry in ar.entries()? {
        let mut e = entry?;
        let p = e.path()?;
        if p.file_name().is_some_and(|n| n == "rg") {
            let data: Vec<u8> = {
                let mut v = Vec::new();
                io::copy(&mut e, &mut v)?;
                v
            };
            fs::write(&dest, &data)?;
            found = true;
            break;
        }
    }

    if !found {
        return Err(format!(
            "Could not find 'rg' in ripgrep archive {}. Set GROK_SHELL_BUNDLE_RG_PATH for offline builds.",
            url
        )
        .into());
    }

    Ok(())
}

fn is_bazel_build(manifest_dir: &Path) -> bool {
    let manifest_dir_str = manifest_dir.to_string_lossy();
    env::var_os("BAZEL_WORKSPACE").is_some()
        || env::var_os("BUILD_WORKSPACE_DIRECTORY").is_some()
        || env::var_os("BAZEL_EXECUTION_ROOT").is_some()
        || env::var_os("BAZEL_OUTPUT_BASE").is_some()
        || manifest_dir_str.contains("/execroot/")
        || manifest_dir_str.contains("/bazel-out/")
}
