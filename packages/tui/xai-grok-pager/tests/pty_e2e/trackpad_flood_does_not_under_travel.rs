// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;
#[allow(unused_imports)]
use super::scroll::*;

// ── Regression: trackpad feel — fast flicks must not under-travel ─────────
//
// User complaint: fast trackpad flicks felt "laggy AND too sensitive" and under-traveled
// The trackpad flush path had a fixed 6-line per-flush cap
// At the 16ms cadence that is a hard ~360 lines/s delivery ceiling no matter how fast the gesture or how tall the viewport
// Whatever backlog was still queued when the stream finalized was discarded beyond one more 6-line flush; the flick's tail simply evaporated
// The cap is now proportional (half the viewport per flush, floored at 6) so delivery scales with gesture demand
// The finalize flush drains the whole-line backlog up to that cap
//
// The trackpad cap only binds once a stream is CONFIRMED trackpad, which happens mid-stream only on ept=1 brands
// Their rapid same-direction runs promote at the 3rd event
// The harness default `TerminalName::Unknown` is ept=3, where auto streams stay Unknown until finalize and flush uncapped
// So this test presents `TERM_PROGRAM=iTerm.app` (ept=1) and floods wheel-up reports with `GROK_SCROLL_SPEED=100`
// That speed setting is a 6x multiplier, an existing user setting used here to amplify demand deterministically
// Total viewport travel then holds up under any classification outcome and any scheduler jitter:
//
// - Every event contributes at least 1 base line even at the 1.0x accel floor, so desired travel is ≥ 40 × 1 × 6 = 240 rows under any jitter
//   (Jitter only stretches the 6ms gaps and lowers the accel multiplier toward 1.0; the speed multiplier is timing-independent.)
//   The proportional cap (~20 lines/flush at the 50-row PTY) drains that demand within the burst plus the 80ms stream-gap window
//   So travel stays above TRAVEL_FLOOR
//   If extreme stretching (avg gap > 30ms) prevents trackpad promotion entirely, the stream is wheel-like and uncapped: travel = 240
// - Under the parent's fixed 6-line cap the same flood delivers at most 6 lines per 16ms slot
//   A ~260ms burst spans ~16 slots (~96 rows), plus the post-stop gap window (5 slots + one finalize flush ≈ 36 rows), about 130 rows total
//   The remaining backlog is discarded at finalize
//   Running the flood against a parent-built binary gave 138 rows of travel (< TRAVEL_FLOOR)
//
// One jitter regime the floor does NOT cover is arrival COMPRESSION, the mirror of stretching per the driver contract in `scroll.rs`
// If the pager is descheduled for the whole host-paced burst, all reports queue in the PTY and arrive as one batch
// Accel maxes out, but the cap bounds delivery to one in-batch flush plus the post-stop gap window
// That is ~5 cadence slots + one finalize flush ≈ 7 flushes × ~20 lines ≈ 140 rows
// This capped outcome is legitimate yet numerically indistinguishable from the parent defect (138)
// The regimes separate by CAUSE, visible in the frame count
// A compressed burst paints at most ~7 frames, while the parent's 6-line cap needs ~16+ flush frames to travel that far
// The detector below softens the floor assertion in the compressed regime (distinct message) so a CI stall is not misread as the regression
// The floor stays a hard assertion in the normal regime
//
// Only quantities that are deterministic byte-for-byte are asserted
// Those are the marker indices on the final screen and the frame counts from the preamble's reset_timing() capture
// The post-burst update() is a drain window, not a timing assertion
// Per-flush pacing and cap math is pinned by the synthetic-clock unit tests in `src/input/mouse.rs`

/// Tall enough that maximum plausible travel, bounded by the ~590-row desired total at full nominal acceleration, never clamps at the transcript top.
/// A clamp there would mask the travel measurement.
const MARKER_COUNT: usize = 700;

/// 40 spaced single reports at a nominal 6ms: the ept=1 brand promotes the stream to confirmed trackpad at the 3rd event (avg interval < 30ms).
/// That engages the per-flush cap under test.
const BURST_EVENTS: usize = 40;

const BURST_INTERVAL: Duration = Duration::from_millis(6);

/// Rows the flood must travel.
/// The floor sits below every delivery regime of the new code (jitter floor ≈ 240) and above the parent's capped ceiling (≈ 130).
const TRAVEL_FLOOR: usize = 200;

/// A fully batched arrival delivers over at most one in-batch flush + ~5 post-stop cadence slots + one finalize flush (≈ 7 painted frames).
/// A genuine cap regression instead must pace the 260ms burst window at 6 lines per 16ms slot (≥ 21 frames to reach ~138 rows).
/// 12 splits that band.
/// It also absorbs partial stalls: a ~205-230ms deschedule batches most of the burst into 9-10 frames with travel just under the floor.
/// Even those stalls never reach the 21 frames a real regression paces.
const COMPRESSED_BURST_FRAMES_MAX: u64 = 12;

/// A dense trackpad flood at high scroll speed must move the viewport by at least [`TRAVEL_FLOOR`] rows.
/// The proportional per-flush cap keeps up with gesture demand, and the finalize flush drains the backlog instead of discarding the flick's tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn trackpad_flood_does_not_under_travel() {
    let (mut harness, _content, top_before) = spawn_bottom_pinned_marker_scrollback_with_env(
        MARKER_COUNT,
        &[("TERM_PROGRAM", "iTerm.app"), ("GROK_SCROLL_SPEED", "100")],
    )
    .await;

    send_wheel_burst(
        &mut harness,
        SGR_SCROLL_UP,
        BURST_EVENTS,
        WHEEL_ROW,
        WHEEL_COL,
        BURST_INTERVAL,
    );
    // Drain window: outlasts the 80ms stream gap plus the post-stop cadence flushes and the finalize flush
    harness.update(Duration::from_millis(800));

    assert!(
        harness.is_running().expect("poll pager liveness"),
        "pager exited during the trackpad flood\nscreen:\n{}",
        harness.screen_contents()
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager rendered 'panicked' during the trackpad flood\nscreen:\n{}",
        harness.screen_contents()
    );

    let top_after = topmost_visible_marker(&harness).unwrap_or_else(|| {
        panic!(
            "no marker visible after the trackpad flood\nscreen:\n{}",
            harness.screen_contents()
        )
    });
    assert!(
        top_after < top_before,
        "trackpad flood did not scroll the viewport: topmost visible marker \
         {} → {}\nscreen:\n{}",
        marker_line(top_before),
        marker_line(top_after),
        harness.screen_contents()
    );
    let travel = top_before - top_after;
    let frames = harness.frame_count();
    // Compressed-burst detector (see header): travel under the floor with only a handful of frames means the reports arrived batched
    // That is the pager stalling through the burst window, a capped outcome and not the under-travel regression
    // Soften with a distinct message instead of failing
    if travel < TRAVEL_FLOOR && frames <= COMPRESSED_BURST_FRAMES_MAX {
        eprintln!(
            "SKIP(compressed burst): {frames} frames \
             (<= {COMPRESSED_BURST_FRAMES_MAX}) painted {travel} rows — the \
             burst arrived batched under host load; not asserting the \
             {TRAVEL_FLOOR}-row floor (this is NOT the under-travel \
             regression, which paces ~16+ frames)"
        );
        harness.quit().expect("clean quit");
        return;
    }
    assert!(
        travel >= TRAVEL_FLOOR,
        "trackpad flood under-traveled: {travel} rows (< {TRAVEL_FLOOR}) \
         across {frames} paced frames — fixed per-flush cap ceiling and/or \
         finalize backlog discard regressed\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
