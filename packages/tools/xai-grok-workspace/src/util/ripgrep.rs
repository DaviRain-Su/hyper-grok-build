// Resolution (bundled binary, RG_BIN_PATH, Bazel runfiles, PATH) lives in the
// grok-tools crate; this module only preserves the `crate::util::ripgrep` path.
pub use xai_grok_tools::util::rg_path;
