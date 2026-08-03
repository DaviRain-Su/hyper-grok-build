//! This process's resource gauges.
//!
//! Lives beside `process_scope` so every caller shares one reader.

/// Fields are `None` where the platform offers no cheap equivalent. Threads
/// and open files are Linux-only, matching what the resource soaks bound.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessResources {
    pub rss_bytes: Option<u64>,
    /// Since the process started, not since the last sample.
    pub peak_rss_bytes: Option<u64>,
    pub footprint_bytes: Option<u64>,
    pub threads: Option<u64>,
    pub open_files: Option<u64>,
}

/// One syscall on macOS, one file read plus two directory reads on Linux.
pub fn sample_process_resources() -> ProcessResources {
    imp::sample()
}

/// Memory only, leaving `threads` and `open_files` unset. Skips the two
/// directory scans on Linux, for callers that sample on a timer.
pub fn sample_process_memory() -> ProcessResources {
    imp::sample_memory()
}

#[cfg(target_os = "macos")]
mod imp {
    use super::ProcessResources;

    // Hand-rolled `task_vm_info` prefix through `phys_footprint` (the kernel
    // accepts any count ≤ the current struct revision; passing the prefix
    // count returns exactly these fields). Layout per XNU osfmk/mach/task_info.h.
    #[repr(C)]
    #[derive(Default)]
    struct TaskVmInfoPrefix {
        virtual_size: u64,
        region_count: i32,
        page_size: i32,
        resident_size: u64,
        resident_size_peak: u64,
        device: u64,
        device_peak: u64,
        internal: u64,
        internal_peak: u64,
        external: u64,
        external_peak: u64,
        reusable: u64,
        reusable_peak: u64,
        purgeable_volatile_pmap: u64,
        purgeable_volatile_resident: u64,
        purgeable_volatile_virtual: u64,
        compressed: u64,
        compressed_peak: u64,
        compressed_lifetime: u64,
        phys_footprint: u64,
    }

    const TASK_VM_INFO: u32 = 22;
    // mach natural_t (u32) units.
    const PREFIX_COUNT: u32 = (size_of::<TaskVmInfoPrefix>() / size_of::<u32>()) as u32;

    unsafe extern "C" {
        // libSystem: the calling task's control port and task_info(2).
        static mach_task_self_: u32;
        fn task_info(task: u32, flavor: u32, info: *mut u8, count: *mut u32) -> i32;
    }

    pub(super) fn sample_memory() -> ProcessResources {
        sample()
    }

    pub(super) fn sample() -> ProcessResources {
        let mut info = TaskVmInfoPrefix::default();
        let mut count = PREFIX_COUNT;
        // SAFETY: `info` is a properly sized/aligned out-buffer and `count`
        // tells the kernel its length in natural_t units; TASK_VM_INFO on
        // the caller's own task port cannot fault.
        let kr = unsafe {
            task_info(
                mach_task_self_,
                TASK_VM_INFO,
                (&raw mut info).cast::<u8>(),
                &raw mut count,
            )
        };
        if kr != 0 {
            return ProcessResources::default();
        }
        ProcessResources {
            rss_bytes: Some(info.resident_size),
            peak_rss_bytes: Some(info.resident_size_peak),
            footprint_bytes: Some(info.phys_footprint),
            threads: None,
            open_files: None,
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::ProcessResources;

    pub(super) fn sample() -> ProcessResources {
        ProcessResources {
            threads: count_entries("/proc/self/task"),
            open_files: count_entries("/proc/self/fd"),
            ..sample_memory()
        }
    }

    pub(super) fn sample_memory() -> ProcessResources {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        ProcessResources {
            rss_bytes: field_kb(&status, "VmRSS:").map(|kb| kb * 1024),
            peak_rss_bytes: field_kb(&status, "VmHWM:").map(|kb| kb * 1024),
            footprint_bytes: None,
            threads: None,
            open_files: None,
        }
    }

    /// `/proc/self/status` reports these in kB, so this needs no page size.
    fn field_kb(status: &str, prefix: &str) -> Option<u64> {
        status
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|kb| kb.parse().ok())
    }

    /// The read's own transient descriptor closes with the iterator, so
    /// repeated samples stay comparable.
    fn count_entries(dir: &str) -> Option<u64> {
        Some(std::fs::read_dir(dir).ok()?.count() as u64)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::ProcessResources;

    pub(super) fn sample() -> ProcessResources {
        ProcessResources::default()
    }

    pub(super) fn sample_memory() -> ProcessResources {
        ProcessResources::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{sample_process_memory, sample_process_resources};

    #[test]
    fn a_running_process_reports_its_own_gauges() {
        let usage = sample_process_resources();

        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let rss = usage.rss_bytes.expect("rss");
            assert!(rss > 0);
            assert!(
                usage.peak_rss_bytes.expect("peak") >= rss,
                "peak is a high-water mark, so it can never trail current"
            );
        }

        #[cfg(target_os = "macos")]
        assert!(usage.footprint_bytes.expect("footprint") > 0);

        #[cfg(target_os = "linux")]
        {
            assert!(usage.threads.expect("threads") >= 1);
            assert!(usage.open_files.expect("open files") >= 1);
        }

        #[cfg(not(target_os = "linux"))]
        assert_eq!((usage.threads, usage.open_files), (None, None));
    }

    /// The reason this function exists is the scans it skips, so a rewrite
    /// that delegated to the full sampler would pass everything but this.
    #[test]
    fn the_memory_only_sample_leaves_the_counts_unset() {
        let usage = sample_process_memory();

        assert_eq!((usage.threads, usage.open_files), (None, None));

        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(usage.rss_bytes.expect("rss") > 0);
    }
}
