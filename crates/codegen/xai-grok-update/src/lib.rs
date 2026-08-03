pub mod auto_update;
#[cfg(feature = "community-build")]
mod community;
pub mod version;
mod version_policy;

pub use auto_update::UpdateStatus;
pub use version::{UpdateConfig, channel_label, channel_name, write_version_cache};
pub use version_policy::enforce_version_policy_or_exit;

/// Release-archive contract verification (producer CI + installer tests).
#[cfg(feature = "community-build")]
pub use community::{
    ReleaseArchiveReport, ReleaseArchiveVerifyOptions, run_verify_release_cli,
    verify_release_archive, verify_sha256sums_manifest,
};

/// Test-only install failpoint injection for the community updater.
#[cfg(feature = "community-update-test-hooks")]
#[doc(hidden)]
pub use community::set_install_failpoint;
