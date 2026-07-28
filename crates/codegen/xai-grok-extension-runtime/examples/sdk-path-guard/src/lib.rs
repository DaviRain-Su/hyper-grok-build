//! SDK example: block obviously destructive commands in tool JSON.
//! Declarative macros only (no proc-macro).
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

xai_grok_extension_sdk::hyper_extension! {
    pre_tool_use: || {
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
