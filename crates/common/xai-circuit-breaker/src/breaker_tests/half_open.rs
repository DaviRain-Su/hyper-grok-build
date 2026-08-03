//! Half-open probe limiting, `half_open_max_probes = 0` clamping,
//! abandoned-probe lease reclaim, and CAS-loss recovery on the
//! Open → HalfOpen transition.

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use super::super::*;
use super::support::{breaker_with_mock, fast_config};

#[test]
fn half_open_limits_concurrent_probes() {
    let (cb, clock) = breaker_with_mock(fast_config(|c| {
        c.min_samples = 1;
        c.open_duration = Duration::from_millis(50);
        c.half_open_max_probes = 2;
    }));

    cb.record(Outcome::Failure);
    clock.advance(Duration::from_millis(70));

    assert!(cb.check().is_ok());
    assert_eq!(cb.state(), BreakerState::HalfOpen);

    assert!(cb.check().is_ok());

    // Third exceeds max_probes
    assert!(cb.check().is_err());
}

#[test]
fn max_probes_clamped_to_at_least_one() {
    let (cb, clock) = breaker_with_mock(BreakerConfig {
        half_open_max_probes: 0,
        min_samples: 1,
        open_duration: Duration::from_millis(50),
        ..Default::default()
    });

    cb.record(Outcome::Failure);
    clock.advance(Duration::from_millis(70));

    // Even with max_probes=0 in config, clamped to 1 so one probe gets through
    assert!(cb.check().is_ok());
    assert_eq!(cb.state(), BreakerState::HalfOpen);
    // Second is rejected
    assert!(cb.check().is_err());
}

#[test]
fn breaker_half_open_serialises_concurrent_probes() {
    let (cb, clock) = breaker_with_mock(BreakerConfig {
        half_open_max_probes: 1,
        ..BreakerConfig::client()
    });
    for _ in 0..5 {
        cb.record(Outcome::Failure);
    }
    assert_eq!(cb.state(), BreakerState::Open);

    clock.advance(Duration::from_secs(61));

    // First check claims the only probe slot.
    assert!(cb.check().is_ok());
    // Subsequent checks must short-circuit until the probe
    // resolves and the breaker transitions.
    for _ in 0..10 {
        assert!(cb.check().is_err());
    }
}

/// A probe whose owner never records (its future was dropped on caller
/// cancellation) must not strand the breaker in `HalfOpen` forever:
/// once the claim is older than `open_duration`, one caller reclaims
/// the slot and recovery proceeds.
#[test]
fn abandoned_probe_slot_reclaimed_after_lease_expiry() {
    let (cb, clock) = breaker_with_mock(fast_config(|c| {
        c.min_samples = 1;
        c.open_duration = Duration::from_millis(50);
        c.half_open_max_probes = 1;
    }));

    cb.record(Outcome::Failure);
    clock.advance(Duration::from_millis(70));

    // Claim the only probe slot, then abandon it: no record() ever fires.
    assert!(cb.check().is_ok());
    assert_eq!(cb.state(), BreakerState::HalfOpen);
    // While the lease is live, the slot stays claimed.
    assert!(cb.check().is_err());

    // Once the lease (open_duration) expires, the claim is treated as
    // abandoned: exactly one caller takes the slot over.
    clock.advance(Duration::from_millis(50));
    assert!(
        cb.check().is_ok(),
        "expired probe lease must be reclaimable"
    );
    assert!(cb.check().is_err(), "only one takeover per expired lease");

    // The takeover probe's outcome drives the state machine as usual.
    cb.record(Outcome::Success);
    assert_eq!(cb.state(), BreakerState::Closed);
}

/// The reclaim path must also handle repeated abandonment: each expired
/// lease admits exactly one replacement probe.
#[test]
fn repeatedly_abandoned_probes_keep_recovery_alive() {
    let (cb, clock) = breaker_with_mock(fast_config(|c| {
        c.min_samples = 1;
        c.open_duration = Duration::from_millis(50);
        c.half_open_max_probes = 1;
    }));

    cb.record(Outcome::Failure);
    clock.advance(Duration::from_millis(70));

    for round in 0..3 {
        assert!(cb.check().is_ok(), "round {round}: probe must be admitted");
        assert!(cb.check().is_err(), "round {round}: second probe rejected");
        // Abandon the probe and let its lease expire.
        clock.advance(Duration::from_millis(50));
    }

    // A probe that finally records still closes the breaker.
    assert!(cb.check().is_ok());
    cb.record(Outcome::Success);
    assert_eq!(cb.state(), BreakerState::Closed);
}

/// Probe reservation holds the generation lock, so trip cannot clear accounting
/// mid-admission. After the paused reservation completes and trip lands, the
/// next Open→HalfOpen generation must admit exactly `half_open_max_probes`.
#[test]
fn paused_reservation_does_not_contaminate_next_generation() {
    let (cb, clock) = breaker_with_mock(fast_config(|c| {
        c.min_samples = 1;
        c.open_duration = Duration::from_millis(50);
        c.half_open_max_probes = 1;
    }));
    cb.force_half_open();

    let cb = Arc::new(cb);
    let (reserved_tx, reserved_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let reserver = {
        let cb = cb.clone();
        thread::spawn(move || {
            cb.try_half_open_probe_after_reservation(|| {
                reserved_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .is_ok()
        })
    };

    reserved_rx.recv().unwrap();
    // Trip blocks until the in-flight reservation finishes under the probe lock.
    let tripper = {
        let cb = cb.clone();
        thread::spawn(move || {
            cb.record(Outcome::Failure);
            cb.state()
        })
    };

    // Give the tripper a moment to block on the probe lock.
    thread::sleep(Duration::from_millis(20));
    assert_eq!(
        cb.state(),
        BreakerState::HalfOpen,
        "trip must wait for the in-flight reservation"
    );

    release_tx.send(()).unwrap();
    assert!(
        reserver.join().unwrap(),
        "paused reservation must still admit"
    );
    assert_eq!(tripper.join().unwrap(), BreakerState::Open);

    clock.advance(Duration::from_millis(50));
    assert!(
        cb.check().is_ok(),
        "next generation must admit exactly one fresh probe"
    );
    assert!(
        cb.check().is_err(),
        "next generation must not inherit stale probe slots"
    );
}

#[test]
fn paused_reservation_respects_max_probes_two_on_next_generation() {
    let (cb, clock) = breaker_with_mock(fast_config(|c| {
        c.min_samples = 1;
        c.open_duration = Duration::from_millis(50);
        c.half_open_max_probes = 2;
    }));
    cb.force_half_open();

    let cb = Arc::new(cb);
    let (reserved_tx, reserved_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let reserver = {
        let cb = cb.clone();
        thread::spawn(move || {
            cb.try_half_open_probe_after_reservation(|| {
                reserved_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .is_ok()
        })
    };

    reserved_rx.recv().unwrap();
    let tripper = {
        let cb = cb.clone();
        thread::spawn(move || {
            cb.record(Outcome::Failure);
            cb.state()
        })
    };

    thread::sleep(Duration::from_millis(20));
    release_tx.send(()).unwrap();
    assert!(reserver.join().unwrap());
    assert_eq!(tripper.join().unwrap(), BreakerState::Open);

    clock.advance(Duration::from_millis(50));
    assert!(cb.check().is_ok());
    assert!(cb.check().is_ok());
    assert!(cb.check().is_err());
}

#[test]
fn zero_elapsed_probe_claim_reclaims_only_after_lease() {
    let (cb, clock) = breaker_with_mock(fast_config(|c| {
        c.min_samples = 1;
        c.open_duration = Duration::from_millis(50);
        c.half_open_max_probes = 1;
    }));
    cb.force_half_open();

    assert!(cb.check().is_ok());
    assert!(cb.check().is_err());
    clock.advance(Duration::from_millis(49));
    assert!(cb.check().is_err());
    clock.advance(Duration::from_millis(1));
    assert!(
        cb.check().is_ok(),
        "elapsed time zero is a valid published claim and must expire normally"
    );
}

/// Race many threads attempting the Open → HalfOpen CAS. Only one
/// should win the CAS; the losers must observe `HalfOpen` and
/// take the same probe-counting path so the half_open_probes
/// counter is consistent.
#[test]
fn cas_loss_recovery_with_mock_clock() {
    let (cb, clock) = breaker_with_mock(BreakerConfig {
        half_open_max_probes: 1,
        ..fast_config(|c| {
            c.min_samples = 1;
            c.open_duration = Duration::from_millis(50);
        })
    });
    cb.record(Outcome::Failure);
    assert_eq!(cb.state(), BreakerState::Open);

    clock.advance(Duration::from_millis(70));

    // Spawn many threads simultaneously. Only one probe slot;
    // exactly one Ok overall.
    let cb_arc = Arc::new(cb);
    let barrier = Arc::new(std::sync::Barrier::new(16));
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let cb = cb_arc.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                cb.check().is_ok()
            })
        })
        .collect();
    let oks: usize = handles
        .into_iter()
        .map(|h| h.join().unwrap() as usize)
        .sum();
    assert_eq!(oks, 1, "exactly one thread should claim the probe slot");
    assert_eq!(cb_arc.state(), BreakerState::HalfOpen);
}
