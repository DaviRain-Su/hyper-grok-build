//! `GROK_HOME` override tests in an isolated binary so `grok_home()`'s
//! process-wide `OnceLock` initializes from the overridden env var.

use std::path::PathBuf;

#[test]
fn grok_home_override_path_helpers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let grok_home = tmp.path().to_path_buf();
    unsafe {
        std::env::set_var("GROK_HOME", &grok_home);
    }

    assert_eq!(
        xai_grok_pager::util::pager_toml_path(),
        grok_home.join("pager.toml")
    );
    assert_eq!(
        xai_grok_pager::util::display_grok_home_prefix(),
        "$GROK_HOME"
    );
    assert_eq!(
        xai_grok_pager::util::display_user_grok_path("config.toml"),
        "$GROK_HOME/config.toml"
    );

    let memory_path = grok_home.join("memory/MEMORY.md");
    assert_eq!(
        xai_grok_pager::util::abbreviate_path(&memory_path.display().to_string()),
        "$GROK_HOME/memory/MEMORY.md"
    );

    // Copy-toast paths follow the same abbreviation convention, so a custom
    // $GROK_HOME outside $HOME still displays short.
    assert_eq!(
        xai_grok_pager::clipboard::display_copy_path(&grok_home.join("last-copy.txt")),
        "$GROK_HOME/last-copy.txt"
    );

    assert!(xai_grok_pager::util::is_under_user_grok_home(&memory_path));
    assert!(!xai_grok_pager::util::is_under_user_grok_home(
        PathBuf::from("/tmp/other").as_path()
    ));
}
