//! Regression tests for the defects found by the 2026-10-09 audit (P1-P7).
//!
//! Each test reproduces the original failure scenario and asserts the
//! correct behavior. They must pass in both debug (overflow checks on) and
//! release builds.

#![allow(clippy::unwrap_used)]

mod common;

use std::time::{Duration, Instant};

use caml_engine::{Delivery, FeedProcessor, ProcessorConfig};
use common::{Log, heartbeat, meta_at, packet};

fn processor() -> FeedProcessor<Log> {
    FeedProcessor::new(Log::default())
}

/// P1: an interleaved packet from another session must not reset state and
/// cause a duplicate to be re-delivered.
#[test]
fn p1_interleaved_session_does_not_redeliver_duplicates() {
    for other_epoch in [5, 20] {
        let mut p = processor();
        let t = Instant::now();
        let a1 = packet(10, 1, 1);
        p.process_packet(&a1, meta_at(t));
        p.process_packet(&packet(other_epoch, 1, 1), meta_at(t));
        p.process_packet(&a1, meta_at(t));

        assert_eq!(p.sink().count(1), 1, "message 1 delivered exactly once");
        let s = p.snapshot();
        assert_eq!(s.counters.session_changes, 0);
        assert_eq!(s.session.map(|x| x.epoch), Some(10));
        assert_eq!(
            s.counters.stale_session_packets + s.counters.unconfirmed_session_packets,
            1
        );
        assert_eq!(s.seq.duplicates, 1);
    }
}

/// P2: loss before a session change must still be reported as loss.
#[test]
fn p2_loss_before_session_change_is_reported() {
    let mut p = processor();
    let t = Instant::now();
    p.process_packet(&packet(1, 1, 1), meta_at(t));
    p.process_packet(&packet(1, 3, 1), meta_at(t)); // message 2 lost
    // Publisher crashes and restarts; the receiver adopts it after idling.
    p.process_packet(&packet(2, 1, 1), meta_at(t + Duration::from_secs(2)));

    let v = p.snapshot().integrity();
    assert_eq!(v.session_changes, 1);
    assert_eq!(v.lost_messages, 1);
    assert!(!v.is_clean());
}

/// P3: wire-controlled sequence numbers and epochs must never panic (this
/// test runs with overflow checks in debug builds) or break accounting.
#[test]
fn p3_extreme_sequence_numbers_never_panic() {
    let mut p = processor();
    let t = Instant::now();
    let packets = [
        packet(1, 1, 1),
        packet(1, u64::MAX - 1, 1),
        packet(1, u64::MAX - 1, 1),
        packet(1, u64::MAX - 1, 1),
        heartbeat(1, u64::MAX, false),
        packet(2, u64::MAX - 1, 1),
        heartbeat(2, u64::MAX, true),
        packet(3, 1, 1),
        packet(3, 1 << 63, 1),
        packet(3, (1 << 63) + 1, 1),
        packet(3, (1 << 63) + 2, 1),
        heartbeat(3, u64::MAX, false),
    ];
    for (i, b) in packets.iter().enumerate() {
        p.process_packet(b, meta_at(t + Duration::from_secs(i as u64)));
    }
    let s = p.snapshot().seq;
    assert_eq!(
        s.missing_total,
        s.outstanding + s.recovered + s.evicted + s.abandoned
    );
}

/// P4: packets reordered at session start must be delivered late, not
/// dropped as duplicates.
#[test]
fn p4_reorder_at_session_start_is_recovered() {
    let mut p = processor();
    let t = Instant::now();
    p.process_packet(&packet(1, 3, 2), meta_at(t));
    p.process_packet(&packet(1, 1, 2), meta_at(t));
    p.process_packet(&packet(1, 5, 2), meta_at(t));

    assert_eq!(p.sink().seqs(), [3, 4, 1, 2, 5, 6]);
    assert_eq!(p.sink().0[0].1, Delivery::AfterGap);
    assert_eq!(p.sink().0[2].1, Delivery::Late);
    let s = p.snapshot();
    assert_eq!((s.seq.missing_total, s.seq.recovered), (2, 2));
    assert!(s.integrity().is_clean());
}

/// P5: one far-future packet must not poison the session.
#[test]
fn p5_far_future_packet_does_not_poison_session() {
    let mut p = processor();
    let t = Instant::now();
    p.process_packet(&packet(1, 1, 1), meta_at(t));
    p.process_packet(&packet(1, 1_000_000_000, 1), meta_at(t));
    for k in 2..=10 {
        p.process_packet(&packet(1, k, 1), meta_at(t));
    }
    assert_eq!(p.sink().seqs(), (1..=10).collect::<Vec<_>>());
    assert!(p.sink().0.iter().all(|e| e.1 == Delivery::InOrder));
    let s = p.snapshot();
    assert_eq!(s.seq.implausible_packets, 1);
    assert_eq!(s.seq.missing_total, 0);
}

/// P6: a delayed data packet must be delivered even after many heartbeats
/// and a small gap table (heartbeats no longer consume gap capacity).
#[test]
fn p6_heartbeats_do_not_evict_recoverable_data() {
    let mut config = ProcessorConfig::default();
    config.seq.max_tracked_gaps = 2;
    let mut p = FeedProcessor::with_config(Log::default(), config);
    let t = Instant::now();
    p.process_packet(&packet(1, 1, 1), meta_at(t));
    p.process_packet(&packet(1, 3, 1), meta_at(t)); // 2 delayed
    for _ in 0..10 {
        p.process_packet(&heartbeat(1, 4, false), meta_at(t));
    }
    p.process_packet(&packet(1, 2, 1), meta_at(t));

    assert_eq!(p.sink().count(2), 1);
    assert_eq!(p.sink().0.last(), Some(&(2, Delivery::Late)));
    assert!(p.snapshot().integrity().is_clean());
}

/// P7: the receiver's loss verdict counts evicted and abandoned loss, not
/// just outstanding gaps (the old `caml-recv` check missed both).
#[test]
fn p7_verdict_includes_evicted_and_abandoned_loss() {
    let mut config = ProcessorConfig::default();
    config.seq.max_tracked_gaps = 1;
    let mut p = FeedProcessor::with_config(Log::default(), config);
    let t = Instant::now();
    p.process_packet(&packet(1, 1, 1), meta_at(t));
    p.process_packet(&packet(1, 3, 1), meta_at(t)); // gap {2}
    p.process_packet(&packet(1, 5, 1), meta_at(t)); // gap {4}; {2} evicted
    p.process_packet(&heartbeat(1, 6, true), meta_at(t));
    p.process_packet(&packet(2, 1, 1), meta_at(t)); // new session: {4} abandoned

    let s = p.snapshot();
    assert_eq!(
        (s.seq.evicted, s.seq.abandoned, s.seq.outstanding),
        (1, 1, 0)
    );
    assert_eq!(s.integrity().lost_messages, 2);
    assert!(!s.integrity().is_clean());
}
