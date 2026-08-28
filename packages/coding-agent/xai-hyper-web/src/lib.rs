//! Hyper web control plane: browser remote for a local agent over Tailscale.
//!
//! PR 1 is the listener, bind policy, and token gate. Session chat is later.

mod auth;
mod bind;
mod server;
mod tailscale;
mod token;

pub use bind::check_bind;
pub use server::{DEFAULT_BIND, WebServerConfig, build_router, serve};
pub use tailscale::{ipv4 as tailscale_ipv4, startup_hints as tailscale_startup_hints};
pub use token::{load_or_create as load_or_create_token, token_path};
