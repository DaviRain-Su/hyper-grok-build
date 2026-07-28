//! SDK example: block obviously destructive commands in tool JSON.
use xai_grok_extension_sdk::prelude::*;

xai_grok_extension_sdk::extension_boilerplate!();

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

#[unsafe(no_mangle)]
pub extern "C" fn hyper_ext_on_pre_tool_use() -> i32 {
    for pat in BLOCK {
        if input_contains(pat) {
            return deny(&format!("sdk-path-guard: blocked pattern `{pat}`"));
        }
    }
    // Broad rm -rf anywhere in the payload
    if input_contains("rm -rf") {
        return deny("sdk-path-guard: blocked `rm -rf` in tool input");
    }
    allow()
}
