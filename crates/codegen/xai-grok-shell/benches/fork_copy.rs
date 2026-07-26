//! Fork-path benchmark and profiling workbench.
//!
//! Synthesizes a session whose `updates.jsonl` matches a configurable target
//! size (realistic mixed update shapes: user/agent chunks, tool calls with
//! large results), then measures `StorageAdapter::copy_session_data` — the
//! path that materializes the whole file and produced multi-GB RSS spikes on
//! large production sessions. Also the substrate for allocation/CPU profiling
//! (`cargo flamegraph --bench fork_copy`, dhat) and future peak-RSS bounds.
//!
//! Run: `cargo bench -p xai-grok-shell --bench fork_copy`
//! Size override: `FORK_BENCH_MB=64 cargo bench ...` (default 16 MB).

use std::hint::black_box;
use std::time::Duration;

use acp::{ContentBlock, ContentChunk, TextContent};
use agent_client_protocol as acp;
use criterion::{
    BenchmarkId, Criterion, SamplingMode, Throughput, criterion_group, criterion_main,
};
use tempfile::TempDir;
use xai_grok_shell::session::info::Info;
use xai_grok_shell::session::storage::{
    CopySessionOptions, JsonlStorageAdapter, SessionUpdate, StorageAdapter,
};

/// One synthetic "turn": a user chunk, agent chunks, and a bulky tool result,
/// so line-size distribution and parse cost resemble production sessions.
fn turn_updates(info: &Info, turn: usize) -> Vec<SessionUpdate> {
    let text = |s: String| ContentChunk::new(ContentBlock::Text(TextContent::new(s)));
    let notify =
        |u| SessionUpdate::Acp(Box::new(acp::SessionNotification::new(info.id.clone(), u)));
    let mut updates = vec![notify(acp::SessionUpdate::UserMessageChunk(text(format!(
        "prompt {turn}: check the build and summarize failures"
    ))))];
    for i in 0..8 {
        updates.push(notify(acp::SessionUpdate::AgentMessageChunk(text(format!(
            "agent chunk {turn}/{i}: analyzing module {i} for regressions and drafting a fix plan"
        )))));
    }
    // ~4 KB tool-result payload: the dominant byte source in real sessions.
    updates.push(notify(acp::SessionUpdate::AgentMessageChunk(text(
        format!("tool result {turn}: {}", "x".repeat(4096)),
    ))));
    updates
}

/// Build a session dir whose `updates.jsonl` is at least `target_bytes`.
fn synthesize_session(root: &TempDir, target_bytes: u64) -> Info {
    let adapter = JsonlStorageAdapter::with_root(root.path().to_path_buf());
    let info = Info {
        id: acp::SessionId::new("fork-bench-src"),
        cwd: "/bench/workspace".to_string(),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    rt.block_on(async {
        adapter
            .init_session(&info, acp::ModelId::new("bench-model"))
            .await
            .expect("init session");
        let updates_path = adapter.updates_file_path(&info).expect("updates path");
        let mut turn = 0usize;
        loop {
            for update in turn_updates(&info, turn) {
                adapter.append_update(&info, &update).await.expect("append");
            }
            turn += 1;
            // Stat every 32 turns; sizes only grow.
            if turn % 32 == 0
                && std::fs::metadata(&updates_path)
                    .map(|m| m.len())
                    .unwrap_or(0)
                    >= target_bytes
            {
                break;
            }
        }
    });
    info
}

fn bench_fork_copy(c: &mut Criterion) {
    let target_mb: u64 = std::env::var("FORK_BENCH_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    let root = TempDir::new().expect("tempdir");
    let source = synthesize_session(&root, target_mb * 1024 * 1024);
    let adapter = JsonlStorageAdapter::with_root(root.path().to_path_buf());
    let updates_len = std::fs::metadata(adapter.updates_file_path(&source).expect("updates path"))
        .expect("updates.jsonl")
        .len();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");

    let mut group = c.benchmark_group("fork_copy");
    group
        .sampling_mode(SamplingMode::Flat)
        .sample_size(10)
        .measurement_time(Duration::from_secs(30))
        .throughput(Throughput::Bytes(updates_len));
    group.bench_function(
        BenchmarkId::new("copy_session_data", format!("{target_mb}MB")),
        |b| {
            let mut n = 0usize;
            b.iter(|| {
                n += 1;
                let target = Info {
                    id: acp::SessionId::new(format!("fork-bench-dst-{n}")),
                    cwd: "/bench/workspace-fork".to_string(),
                };
                let result = rt
                    .block_on(adapter.copy_session_data(
                        &source,
                        &target,
                        CopySessionOptions::default(),
                    ))
                    .expect("fork copy");
                // Keep each iteration's output dir from accumulating.
                if let Some(dir) = adapter
                    .updates_file_path(&target)
                    .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
                {
                    std::fs::remove_dir_all(&dir).ok();
                }
                black_box(result)
            });
        },
    );
    group.finish();
}

criterion_group!(benches, bench_fork_copy);
criterion_main!(benches);
