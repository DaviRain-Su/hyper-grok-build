//! Shared HTTP circuit breaker.
//!
//! Sliding-window-with-min-samples algorithm: the breaker trips when
//! `sample_count >= min_samples AND error_rate >= error_rate_threshold`
//! over the live window. Server- and client-side consumers run the same
//! state machine and pick a preset via [`BreakerConfig::server`] or
//! [`BreakerConfig::client`].

mod breaker;
mod clock;
mod config;
mod observer;
mod registry;
mod retry_policy;
mod state;
mod window;

pub use breaker::CircuitBreaker;
#[cfg(any(test, feature = "test-hooks"))]
pub use clock::MockClock;
pub use clock::{Clock, SystemClock};
pub use config::{BreakerConfig, default_failure_codes, parse_failure_codes};
pub use observer::{NoopObserver, Observer};
pub use registry::CircuitBreakerRegistry;
pub use retry_policy::{Disposition, RetryPolicy};
pub use state::{BreakerOpen, BreakerState, Outcome};

// The crate's public surface is the re-exports above.
