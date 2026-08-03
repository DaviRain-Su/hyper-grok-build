//! SDK example: on the first Stop, block with feedback; allow on subsequent
//! stop-hook-active rounds (host caps total continuations).
//! The hook is an ordinary function wrapped by the `#[hyper_plugin]` proc macro.
use xai_grok_extension_sdk::prelude::*;

#[hyper_plugin]
mod plugin {
    use super::*;

    #[hyper_hook(stop)]
    fn require_one_more_round() -> i32 {
        if stop_hook_active() {
            // Already continued once — allow the turn to end.
            allow()
        } else {
            deny("sdk-stop-once: please double-check tests/build before finishing this turn")
        }
    }
}
