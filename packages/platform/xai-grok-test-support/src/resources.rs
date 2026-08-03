//! OS resource snapshots for soak tests, read through `xai_tty_utils` so the
//! soaks and production measure the same way.

/// RSS in bytes, live threads, and open files, sampled together. `None` marks a
/// metric the platform can't report.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceSnapshot {
    pub rss: Option<usize>,
    pub threads: Option<usize>,
    pub open_files: Option<usize>,
}

/// Saturating per-field growth of one [`ResourceSnapshot`] over an earlier
/// baseline. A distinct type from a snapshot so a delta can't be mistaken for
/// an absolute sample. `None` marks a field either side couldn't report.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceGrowth {
    pub rss: Option<usize>,
    pub threads: Option<usize>,
    pub open_files: Option<usize>,
}

impl ResourceSnapshot {
    pub fn capture() -> Self {
        let usage = xai_tty_utils::sample_process_resources();
        let widen = |value: Option<u64>| value.map(|n| n as usize);
        Self {
            rss: widen(usage.rss_bytes),
            threads: widen(usage.threads),
            open_files: widen(usage.open_files),
        }
    }

    /// RSS only, skipping the thread and descriptor scans. For sampling loops
    /// that read just `rss`.
    pub fn capture_rss() -> Option<usize> {
        xai_tty_utils::sample_process_memory()
            .rss_bytes
            .map(|n| n as usize)
    }

    /// Growth of `self` (after) over `baseline` (before); see [`ResourceGrowth`].
    pub fn growth_from(&self, baseline: &ResourceSnapshot) -> ResourceGrowth {
        let delta = |after: Option<usize>, before: Option<usize>| {
            before.zip(after).map(|(b, a)| a.saturating_sub(b))
        };
        ResourceGrowth {
            rss: delta(self.rss, baseline.rss),
            threads: delta(self.threads, baseline.threads),
            open_files: delta(self.open_files, baseline.open_files),
        }
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
            open_files: None,
        };
        let after = ResourceSnapshot {
            rss: Some(30),
            threads: Some(9),
            open_files: Some(3),
        };
        let growth = after.growth_from(&before);
        assert_eq!(growth.rss, Some(0), "a shrink saturates to zero");
        assert_eq!(growth.threads, Some(4), "growth is the delta");
        assert_eq!(
            growth.open_files, None,
            "a missing baseline sample propagates None"
        );
    }
}
