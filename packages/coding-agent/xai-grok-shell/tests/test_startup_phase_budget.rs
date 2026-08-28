#![cfg(unix)]

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use xai_grok_telemetry::startup::STARTUP_COMPLETE_MSG;
use xai_grok_test_support::headless::{run_headless_in_sandbox_borrowed, stderr_tail};
use xai_grok_test_support::mock_server::MockInferenceServer;
use xai_grok_test_support::sandbox::TestSandbox;

fn fetch_delay() -> Duration {
    xai_grok_test_support::scaled(Duration::from_millis(750))
        .min(xai_grok_shell::http::STARTUP_FETCH_TIMEOUT - Duration::from_secs(1))
}

const DEFAULT_PHASE_BUDGET: Duration = Duration::from_secs(10);

const DEFAULT_TOTAL_BUDGET: Duration = Duration::from_secs(20);

fn budget_ms(name: &str, raw: Option<&str>, default_ms: u64) -> u64 {
    match raw {
        None => default_ms,
        Some(value) => value
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be an integer of milliseconds, got {value:?}")),
    }
}

fn env_budget_ms(name: &str, default: Duration) -> (u64, &'static str) {
    let raw = std::env::var(name).ok();
    let note = if raw.is_some() {
        ""
    } else {
        " (default scaled by GROK_TEST_TIMEOUT_SCALE)"
    };
    let default_ms = xai_grok_test_support::scaled(default).as_millis() as u64;
    (budget_ms(name, raw.as_deref(), default_ms), note)
}

fn parse_phase_summary(summary: &str) -> Vec<(String, u64)> {
    summary
        .split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            let (name, value) = entry.split_once(">=").or_else(|| entry.split_once('='))?;
            let ms = if let Some(v) = value.strip_suffix("ms") {
                v.parse::<f64>().ok()?
            } else if let Some(v) = value.strip_suffix('s') {
                v.parse::<f64>().ok()? * 1000.0
            } else {
                return None;
            };
            Some((name.trim().to_string(), ms as u64))
        })
        .collect()
}

fn parse_attribution(log: &Path) -> Option<(u64, Vec<(String, u64)>)> {
    let text = std::fs::read_to_string(log).ok()?;
    let done = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .rfind(|event| event["msg"] == STARTUP_COMPLETE_MSG)?;
    let phases = parse_phase_summary(done["ctx"]["phases"].as_str()?);
    Some((done["ctx"]["total_ms"].as_u64()?, phases))
}

async fn boot_and_attribute(server: &MockInferenceServer) -> (u64, Vec<(String, u64)>) {
    let sandbox = TestSandbox::builder().mock_url(server.url()).git().build();
    let mut cmd = tokio::process::Command::new(xai_grok_test_support::env::grok_binary());
    cmd.args([
        "-p",
        "startup phase probe",
        "--no-auto-update",
        "--yolo",
        "--output-format",
        "json",
    ])
    .arg("--cwd")
    .arg(sandbox.workspace())
    .current_dir(sandbox.workspace());

    let result = run_headless_in_sandbox_borrowed(cmd, &sandbox).await;
    assert!(
        !result.timed_out,
        "headless boot timed out\nstderr tail:\n{}",
        stderr_tail(&result.stderr, 1200)
    );

    let log = sandbox.grok_home().join("logs").join("unified.jsonl");
    parse_attribution(&log).unwrap_or_else(|| {
        panic!(
            "no parseable 'startup complete' in {}\nstderr tail:\n{}",
            log.display(),
            stderr_tail(&result.stderr, 1200)
        )
    })
}

#[tokio::test]
#[ignore] // needs the built binary
async fn every_startup_phase_and_the_boot_total_fit_their_budgets() {
    let server = MockInferenceServer::start()
        .await
        .expect("start mock server");

    let (total_ms, phases) = boot_and_attribute(&server).await;
    assert!(!phases.is_empty(), "boot reported no startup phases");

    let (phase_budget_ms, phase_note) =
        env_budget_ms("GROK_PERF_STARTUP_PHASE_BUDGET_MS", DEFAULT_PHASE_BUDGET);
    let over: Vec<&(String, u64)> = phases
        .iter()
        .filter(|(_, ms)| *ms > phase_budget_ms)
        .collect();
    assert!(
        over.is_empty(),
        "startup phase(s) over the {phase_budget_ms}ms budget \
         (GROK_PERF_STARTUP_PHASE_BUDGET_MS overrides{phase_note}) in a {total_ms}ms boot: \
         {over:?}\n\
         all phases: {phases:?}",
    );

    let (total_budget_ms, total_note) =
        env_budget_ms("GROK_PERF_STARTUP_TOTAL_BUDGET_MS", DEFAULT_TOTAL_BUDGET);
    let attributed: u64 = phases.iter().map(|(_, ms)| *ms).sum();
    assert!(
        total_ms <= total_budget_ms,
        "startup total {total_ms}ms is over the {total_budget_ms}ms budget \
         (GROK_PERF_STARTUP_TOTAL_BUDGET_MS overrides{total_note}; {attributed}ms inside phases): \
         {phases:?}",
    );
}

#[tokio::test]
#[ignore] // needs the built binary
async fn injected_prefetch_latency_is_charged_to_a_startup_phase() {
    let server = MockInferenceServer::start()
        .await
        .expect("start mock server");
    let delay = fetch_delay();
    server.set_startup_fetch_delay(delay);

    let (total_ms, phases) = boot_and_attribute(&server).await;

    assert!(
        server.startup_stalls_served() > 0,
        "no stalled /v1/models or /v1/settings response was served to completion, \
         so the injected stall was not exercised",
    );

    let delay_ms = delay.as_millis() as u64;
    assert!(
        total_ms >= delay_ms,
        "the boot did not wait through the injected {delay_ms}ms stall (total {total_ms}ms), \
         so attribution has nothing to prove",
    );
    let attributed: u64 = phases.iter().map(|(_, ms)| *ms).sum();
    let unattributed_ms = total_ms.saturating_sub(attributed);
    assert!(
        unattributed_ms < delay_ms,
        "the {delay_ms}ms prefetch stall escaped phase attribution: {unattributed_ms}ms of a \
         {total_ms}ms boot is outside every phase scope. phases={phases:?}",
    );
}

#[test]
fn a_missing_or_valid_budget_override_resolves() {
    assert_eq!(budget_ms("X", None, 10_000), 10_000);
    assert_eq!(budget_ms("X", Some("2500"), 10_000), 2_500);
}

#[test]
#[should_panic(expected = "GROK_PERF_STARTUP_PHASE_BUDGET_MS must be an integer")]
fn an_unparseable_budget_override_panics_naming_the_variable() {
    budget_ms("GROK_PERF_STARTUP_PHASE_BUDGET_MS", Some("fast"), 10_000);
}

#[test]
fn parse_phase_summary_reads_the_frozen_format() {
    assert_eq!(
        parse_phase_summary("config_load=12ms, bootstrap=1.5s, session_create>=3ms"),
        [
            ("config_load".to_owned(), 12),
            ("bootstrap".to_owned(), 1500),
            ("session_create".to_owned(), 3),
        ],
    );
}

#[test]
fn parse_phase_summary_skips_garbage_entries() {
    assert_eq!(
        parse_phase_summary("nonsense, broken=xyz, stuck>=oops, config_load=12ms,"),
        [("config_load".to_owned(), 12)],
    );
}
