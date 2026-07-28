//! SDK example: on the first Stop, block with feedback; allow on subsequent
//! stop-hook-active rounds (host caps total continuations).
//! Declarative macros only (no proc-macro).
use xai_grok_extension_sdk::prelude::*;

xai_grok_extension_sdk::hyper_extension! {
    stop: || {
        if stop_hook_active() {
            // Already continued once — allow the turn to end.
            allow()
        } else {
            deny(
                "sdk-stop-once: please double-check tests/build before finishing this turn",
            )
        }
    }
}
