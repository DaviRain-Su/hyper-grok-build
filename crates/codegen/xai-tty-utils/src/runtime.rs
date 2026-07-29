//! Worker-thread policy for multi-thread tokio runtimes.
//!
//! Tokio defaults to one worker per core. On many-core shared hosts (100+-core
//! HPC login nodes) that pins 100+ thread slots per grok process against
//! per-user ceilings — systemd user-slice `pids.max` (commonly 1000) or
//! `RLIMIT_NPROC` — and later thread spawns die with EAGAIN. Grok's runtimes
//! are I/O-bound, so throughput does not scale with workers past a small
//! count.
//!
//! This is the single home for the cap policy; every multi-thread runtime in
//! the workspace (the `grok` binary, the `workspace_server` daemon) derives
//! its worker count from here so the policy cannot drift across crates.

use std::num::NonZeroUsize;

/// Maximum runtime worker threads for any grok process.
pub const MAX_WORKER_THREADS: NonZeroUsize = NonZeroUsize::new(8).unwrap();

/// Pure, testable: `min(cores, MAX_WORKER_THREADS)`.
pub fn cap_worker_threads(cores: NonZeroUsize) -> NonZeroUsize {
    cores.min(MAX_WORKER_THREADS)
}

/// Reads the host: `min(available_parallelism, MAX_WORKER_THREADS)`.
pub fn capped_worker_threads() -> NonZeroUsize {
    cap_worker_threads(std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn cap_is_identity_at_or_below_max() {
        assert_eq!(cap_worker_threads(nz(1)), nz(1));
        assert_eq!(cap_worker_threads(nz(4)), nz(4));
        assert_eq!(cap_worker_threads(nz(8)), nz(8));
    }

    #[test]
    fn cap_clamps_many_core_hosts() {
        assert_eq!(cap_worker_threads(nz(9)), nz(8));
        assert_eq!(cap_worker_threads(nz(360)), nz(8));
    }

    #[test]
    fn capped_worker_threads_stays_in_bounds() {
        let n = capped_worker_threads();
        assert!(n <= MAX_WORKER_THREADS, "got {n}");
    }
}
