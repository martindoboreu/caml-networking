//! Randomized property tests for the receive pipeline.
//!
//! Deterministic (seeded SplitMix64), so any failure is reproducible from the
//! printed seed. Each case feeds the processor a hostile mix of valid
//! packets from several sessions, duplicates, reordering, huge sequence
//! jumps, heartbeats, end-of-session markers, corrupted bytes, and garbage,
//! and checks invariants after every datagram:
//!
//! 1. nothing panics (debug builds run with overflow checks);
//! 2. `missing_total == outstanding + recovered + evicted + abandoned`;
//! 3. within one session adoption, no message sequence number is delivered
//!    twice;
//! 4. every delivered message is counted, and vice versa.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use caml_engine::{EventSink, FeedProcessor, ProcessorConfig};
use caml_protocol::MarketEvent;
use caml_sim::SplitMix64;
use common::{heartbeat, meta_at, packet};

/// Collects the sequence numbers delivered while processing one datagram.
#[derive(Default)]
struct Seen {
    pending: Vec<u64>,
    count: u64,
}

impl EventSink for Seen {
    fn on_event(&mut self, e: &MarketEvent, _: caml_engine::Delivery) {
        self.count += 1;
        self.pending.push(e.seq);
    }
}

fn random_datagram(rng: &mut SplitMix64, epochs: &[u64], cursor: &mut [u64]) -> Vec<u8> {
    let which = rng.below(epochs.len() as u64) as usize;
    let epoch = epochs[which];
    let next = &mut cursor[which];
    // Keep every generated range representable (first + count <= u64::MAX).
    if *next > u64::MAX - 1_024 {
        *next = 1 + rng.below(1_000);
    }
    match rng.below(100) {
        // Mostly: the next packet of this session's stream.
        0..=59 => {
            let count = 1 + rng.below(8) as u16;
            let p = packet(epoch, *next, count);
            *next += u64::from(count);
            p
        }
        // Replay or reorder: an earlier range.
        60..=69 => {
            let back = rng.below(64).min(*next - 1);
            packet(epoch, (*next - back).max(1), 1 + rng.below(4) as u16)
        }
        // Skip ahead (loss), sometimes absurdly far.
        70..=76 => {
            let jump = match rng.below(4) {
                0 => rng.below(10),
                1 => rng.below(5_000),
                2 => rng.below(1 << 30),
                _ => rng.next_u64() >> 2,
            };
            *next = next.saturating_add(jump).min(u64::MAX - 64);
            packet(epoch, *next, 1)
        }
        77..=84 => heartbeat(epoch, *next, rng.below(10) == 0),
        // Extreme sequence values near the top of the range.
        85..=87 => packet(epoch, u64::MAX - 1 - rng.below(8), 1),
        // A corrupted valid packet.
        88..=94 => {
            let mut p = packet(epoch, *next, 2);
            let i = rng.below(p.len() as u64) as usize;
            p[i] ^= 1 << rng.below(8);
            p
        }
        // Garbage.
        _ => (0..rng.below(120)).map(|_| rng.next_u64() as u8).collect(),
    }
}

/// Runs one case; returns its final counters for coverage checks.
fn run_case(
    seed: u64,
    datagrams: usize,
) -> (
    caml_engine::metrics::Counters,
    caml_engine::sequence::SeqStats,
) {
    let mut rng = SplitMix64::new(seed);
    let mut config = ProcessorConfig::default();
    config.seq.max_tracked_gaps = 1 + rng.below(64) as usize;
    config.seq.resync_after = 1 + rng.below(4) as u32;
    config.session_confirm_packets = 1 + rng.below(4) as u32;
    let mut p = FeedProcessor::with_config(Seen::default(), config);

    let epochs: Vec<u64> = (0..1 + rng.below(3)).map(|i| 100 + i).collect();
    let mut cursor = vec![1u64; epochs.len()];
    let start = Instant::now();
    let mut clock = Duration::ZERO;
    // (adoption period, seq) of every delivered message.
    let mut delivered: HashSet<(u64, u64)> = HashSet::new();

    for _ in 0..datagrams {
        let bytes = random_datagram(&mut rng, &epochs, &mut cursor);
        clock += Duration::from_micros(rng.below(400_000));
        p.process_packet(&bytes, meta_at(start + clock));

        let counters = *p.counters();
        let stats = p.seq_stats();
        // A packet's deliveries happen after its session gate decided, so
        // the post-packet period is the right tag for all of them.
        let period = counters.session_changes;
        for seq in p.sink_mut().pending.drain(..) {
            assert!(
                delivered.insert((period, seq)),
                "seed {seed}: seq {seq} delivered twice in session period {period}"
            );
        }

        let saturated = [
            stats.missing_total,
            stats.evicted,
            stats.abandoned,
            stats.outstanding,
        ]
        .iter()
        .any(|&v| v > u64::MAX / 4);
        if !saturated {
            assert_eq!(
                stats.missing_total,
                stats.outstanding + stats.recovered + stats.evicted + stats.abandoned,
                "seed {seed}: accounting invariant"
            );
        }
        assert_eq!(
            counters.messages_delivered,
            p.sink().count,
            "seed {seed}: delivery count"
        );
    }
    (*p.counters(), p.seq_stats())
}

#[test]
fn hostile_streams_preserve_invariants() {
    let cases = if cfg!(debug_assertions) { 60 } else { 600 };
    let mut total = caml_engine::metrics::Counters::default();
    let mut seq = caml_engine::sequence::SeqStats::default();
    for seed in 0..cases {
        let (c, s) = run_case(0xA11CE ^ seed, 1_500);
        total.datagrams = total.datagrams.saturating_add(c.datagrams);
        total.messages_delivered = total
            .messages_delivered
            .saturating_add(c.messages_delivered);
        total.messages_delivered_late = total
            .messages_delivered_late
            .saturating_add(c.messages_delivered_late);
        total.malformed = total.malformed.saturating_add(c.malformed);
        total.session_changes = total.session_changes.saturating_add(c.session_changes);
        total.sessions_ended = total.sessions_ended.saturating_add(c.sessions_ended);
        total.stale_session_packets = total
            .stale_session_packets
            .saturating_add(c.stale_session_packets);
        total.unconfirmed_session_packets = total
            .unconfirmed_session_packets
            .saturating_add(c.unconfirmed_session_packets);
        seq.evicted = seq.evicted.saturating_add(s.evicted);
        seq.abandoned = seq.abandoned.saturating_add(s.abandoned);
        seq.duplicates = seq.duplicates.saturating_add(s.duplicates);
        seq.implausible_packets = seq
            .implausible_packets
            .saturating_add(s.implausible_packets);
        seq.resyncs = seq.resyncs.saturating_add(s.resyncs);
        seq.before_baseline = seq.before_baseline.saturating_add(s.before_baseline);
        seq.missed_before_join = seq.missed_before_join.saturating_add(s.missed_before_join);
    }
    println!(
        "{total:?}
{seq:?}"
    );
    // The generator must actually reach every interesting path; otherwise
    // the invariants above were checked against an easy workload.
    assert_eq!(total.datagrams, cases * 1_500);
    for (name, n) in [
        ("late deliveries", total.messages_delivered_late),
        ("malformed", total.malformed),
        ("session changes", total.session_changes),
        ("sessions ended", total.sessions_ended),
        ("stale session pkts", total.stale_session_packets),
        (
            "unconfirmed session pkts",
            total.unconfirmed_session_packets,
        ),
        ("evicted", seq.evicted),
        ("abandoned", seq.abandoned),
        ("duplicates", seq.duplicates),
        ("implausible", seq.implausible_packets),
        ("resyncs", seq.resyncs),
    ] {
        assert!(n > 0, "property workload never produced {name}");
    }
}
