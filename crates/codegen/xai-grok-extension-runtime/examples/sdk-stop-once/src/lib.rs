//! SDK example: on the first Stop, block with feedback; allow on subsequent
//! stop-hook-active rounds (host caps total continuations).
use xai_grok_extension_sdk::prelude::*;

xai_grok_extension_sdk::extension_boilerplate!();

#[unsafe(no_mangle)]
pub extern "C" fn hyper_ext_on_stop() -> i32 {
    if xai_grok_extension_sdk::host::stop_hook_active() {
        // Already continued once — allow the turn to end.
        allow()
    } else {
        deny(
            "sdk-stop-once: please double-check tests/build before finishing this turn",
        )
    }
}
