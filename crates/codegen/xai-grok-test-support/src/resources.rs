//! Generic OS resource snapshots for soak tests. No shell types: `rss_bytes`
//! reads `/proc` (Linux) or shells out to `ps` (macOS); the task/fd counters
//! are Linux-only and return `None` elsewhere.

/// RSS (bytes), live threads, and open fds sampled together. `None` marks a
/// metric the platform can't report.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceSnapshot {
    pub rss: Option<usize>,
    pub threads: Option<usize>,
    pub fds: Option<usize>,
}

/// Saturating per-field growth of one [`ResourceSnapshot`] over an earlier
/// baseline. A distinct type from a snapshot so a delta can't be mistaken for
/// an absolute sample. `None` marks a field either side couldn't report.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceGrowth {
    pub rss: Option<usize>,
    pub threads: Option<usize>,
    pub fds: Option<usize>,
}

impl ResourceSnapshot {
    pub fn capture() -> Self {
        Self {
            rss: rss_bytes(),
            threads: thread_count(),
            fds: fd_count(),
        }
    }

    /// RSS only, skipping the thread and fd probes. For hot sampling loops that
    /// use just `rss`: on Linux this avoids the per-tick `/proc/self/{task,fd}`
    /// directory scans. The RSS read itself still shells out to `ps` on macOS.
    pub fn capture_rss() -> Option<usize> {
        rss_bytes()
    }

    /// Growth of `self` (after) over `baseline` (before); see [`ResourceGrowth`].
    pub fn growth_from(&self, baseline: &ResourceSnapshot) -> ResourceGrowth {
        let delta = |after: Option<usize>, before: Option<usize>| {
            before.zip(after).map(|(b, a)| a.saturating_sub(b))
        };
        ResourceGrowth {
            rss: delta(self.rss, baseline.rss),
            threads: delta(self.threads, baseline.threads),
            fds: delta(self.fds, baseline.fds),
        }
    }
}

fn rss_bytes() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(val) = line.strip_prefix("VmRSS:") {
                let kb: usize = val.trim().trim_end_matches(" kB").trim().parse().ok()?;
                return Some(kb * 1024);
            }
        }
        None
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let output = Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        let kb: usize = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .ok()?;
        Some(kb * 1024)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

fn thread_count() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        Some(std::fs::read_dir("/proc/self/task").ok()?.count())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The read's own transient fd closes with the iterator, so before and after
/// samples stay symmetric.
fn fd_count() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        Some(std::fs::read_dir("/proc/self/fd").ok()?.count())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growth_from_saturates_and_propagates_none() {
        let before = ResourceSnapshot {
            rss: Some(100),
            threads: Some(5),
            fds: None,
        };
        let after = ResourceSnapshot {
            rss: Some(30),
            threads: Some(9),
            fds: Some(3),
        };
        let growth = after.growth_from(&before);
        assert_eq!(growth.rss, Some(0), "a shrink saturates to zero");
        assert_eq!(growth.threads, Some(4), "growth is the delta");
        assert_eq!(
            growth.fds, None,
            "a missing baseline sample propagates None"
        );
    }
}
