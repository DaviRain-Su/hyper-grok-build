//! SDK example: block obviously destructive commands in tool JSON.
//! The hook is an ordinary function wrapped by the `#[hyper_plugin]` proc macro.
use xai_grok_extension_sdk::prelude::*;

const BLOCK: &[&str] = &[
    "rm -rf /",
    "rm -rf /*",
    "mkfs.",
    "dd if=/dev/zero of=/dev/",
    ":(){ :|:& };:",
    "curl | sh",
    "wget | sh",
    "chmod -R 777 /",
];

#[hyper_plugin]
mod plugin {
    use super::*;

    #[hyper_hook(pre_tool_use)]
    fn block_destructive_paths() -> i32 {
        for pat in BLOCK {
            if input_contains(pat) {
                log_warn(&format!("sdk-path-guard: blocked pattern `{pat}`"));
                return deny(&format!("sdk-path-guard: blocked pattern `{pat}`"));
            }
        }
        if input_contains("rm -rf") {
            log_warn("sdk-path-guard: blocked `rm -rf` in tool input");
            return deny("sdk-path-guard: blocked `rm -rf` in tool input");
        }
        allow()
    }
}
