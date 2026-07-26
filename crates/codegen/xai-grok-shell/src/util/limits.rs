//! Startup logging of effective OS resource limits, so EMFILE/EAGAIN/OOM
//! crash reports carry the ceilings that were in effect.

/// Emit one `startup.effective_limits` entry to the unified log.
pub fn log_effective_limits() {
    xai_grok_telemetry::unified_log::info("startup.effective_limits", None, Some(gather()));
}

fn gather() -> serde_json::Value {
    serde_json::json!({
        "nofile": rlimit_pair(RlimitKind::Nofile),
        "nproc": rlimit_pair(RlimitKind::Nproc),
        "available_parallelism": std::thread::available_parallelism().map(usize::from).ok(),
        "cgroup": cgroup_v2_limits(),
    })
}

enum RlimitKind {
    Nofile,
    Nproc,
}

/// `[soft, hard]` for the given rlimit; `RLIM_INFINITY` maps to JSON null.
#[cfg(unix)]
fn rlimit_pair(kind: RlimitKind) -> Option<serde_json::Value> {
    let resource = match kind {
        RlimitKind::Nofile => libc::RLIMIT_NOFILE,
        RlimitKind::Nproc => libc::RLIMIT_NPROC,
    };
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes only into local `lim`.
    if unsafe { libc::getrlimit(resource, &mut lim) } != 0 {
        return None;
    }
    let val = |v: libc::rlim_t| (v != libc::RLIM_INFINITY).then_some(v);
    Some(serde_json::json!([val(lim.rlim_cur), val(lim.rlim_max)]))
}

#[cfg(not(unix))]
fn rlimit_pair(_kind: RlimitKind) -> Option<serde_json::Value> {
    None
}

/// Best-effort cgroup v2 pids/memory ceilings — the limits behind EAGAIN
/// thread-spawn failures and memcg OOM kills on shared hosts. `None` on any
/// read error or non-cgroup-v2 environment.
#[cfg(target_os = "linux")]
fn cgroup_v2_limits() -> Option<serde_json::Value> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    // cgroup v2 unified hierarchy line: "0::<path>".
    let path = cgroup.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    let read = |f: &str| {
        std::fs::read_to_string(format!("/sys/fs/cgroup{path}/{f}"))
            .ok()
            .map(|s| s.trim().to_owned())
    };
    Some(serde_json::json!({
        "pids_current": read("pids.current"),
        "pids_max": read("pids.max"),
        "memory_current": read("memory.current"),
        "memory_max": read("memory.max"),
    }))
}

#[cfg(not(target_os = "linux"))]
fn cgroup_v2_limits() -> Option<serde_json::Value> {
    None
}

#[cfg(test)]
mod tests {
    use super::gather;

    #[test]
    #[cfg(unix)]
    fn gather_reports_rlimits_and_parallelism() {
        let v = gather();
        assert!(v["nofile"].is_array(), "nofile missing: {v}");
        assert!(v["nproc"].is_array(), "nproc missing: {v}");
        assert!(v["available_parallelism"].is_u64(), "parallelism: {v}");
    }
}
