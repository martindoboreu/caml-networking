//! Proves the steady-state receive path performs zero heap allocations.
//!
//! This binary installs a counting global allocator. Counting is per
//! thread, so the measurement is not disturbed by other tests.

#![allow(clippy::unwrap_used)]

mod common;

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use caml_alloc_count::{CountingAllocator, count_allocations};
use caml_engine::transport::{UdpConfig, UdpSource};
use caml_engine::{Delivery, EventSink, FeedProcessor, RunLimits, StopReason, run};
use caml_protocol::MarketEvent;
use common::{heartbeat, meta_at, packet};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

/// A consumer that does real work without allocating.
#[derive(Default)]
struct Checksum(u64);

impl EventSink for Checksum {
    fn on_event(&mut self, e: &MarketEvent, d: Delivery) {
        self.0 = self
            .0
            .wrapping_mul(31)
            .wrapping_add(e.seq ^ e.quantity ^ d as u64);
    }
}

/// Every path through `process_packet`: in order, gaps, late fills,
/// duplicates, heartbeats, implausible jumps, malformed datagrams, foreign
/// sessions, end of session, and a session change.
fn hostile_stream() -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    let mut seq = 1;
    for k in 0..2_000u64 {
        let p = packet(1, seq, 4);
        seq += 4;
        match k % 10 {
            3 => {} // lost
            5 => {
                v.push(packet(1, seq, 4)); // reordered with its successor
                v.push(p);
                seq += 4;
            }
            7 => {
                v.push(p.clone());
                v.push(p); // duplicate
            }
            _ => v.push(p),
        }
        if k % 50 == 0 {
            v.push(heartbeat(1, seq, false));
            v.push(packet(1, seq + 10_000_000, 1)); // implausible
            v.push(vec![0xFF; 40]); // malformed
            v.push(packet(9, 1, 1)); // foreign session
        }
    }
    v.push(heartbeat(1, seq, true));
    v.push(packet(2, 1, 4)); // session change after end of session
    v
}

#[test]
fn process_packet_never_allocates() {
    let stream = hostile_stream();
    let mut p = FeedProcessor::new(Checksum::default());
    let t = Instant::now();
    // Warm up: the first packet establishes the session.
    p.process_packet(&stream[0], meta_at(t));

    let ((), allocations) = count_allocations(|| {
        for bytes in &stream[1..] {
            p.process_packet(bytes, meta_at(t));
        }
    });
    let s = p.snapshot();
    assert_eq!(allocations, 0, "allocations on the receive path:\n{s}");
    // The stream really did exercise the interesting paths.
    assert!(s.seq.gaps > 0 && s.seq.recovered > 0 && s.seq.duplicates > 0);
    assert!(s.seq.implausible_packets > 0 && s.counters.malformed > 0);
    assert!(s.counters.unconfirmed_session_packets > 0);
    assert_eq!(s.counters.session_changes, 1);
}

#[test]
fn socket_receive_loop_never_allocates() {
    let mut rx = UdpSource::bind(&UdpConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        recv_buffer_bytes: Some(1 << 20),
        poll_timeout: Duration::from_millis(20),
        ..UdpConfig::default()
    })
    .unwrap();
    let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
    tx.connect(rx.local_addr().unwrap()).unwrap();
    let mut p = FeedProcessor::new(Checksum::default());
    let stop = AtomicBool::new(false);

    // Pre-build packets so the measured region contains only the receive
    // loop; the sender (this thread) sends a burst, then the loop drains it.
    let packets: Vec<Vec<u8>> = (0..256u64).map(|k| packet(1, 1 + 4 * k, 4)).collect();
    let mut sent = 0u64;
    let mut total = 0u64;
    for burst in packets.chunks(32) {
        for b in burst {
            tx.send(b).unwrap();
            sent += 1;
        }
        let limits = RunLimits {
            max_datagrams: Some(sent),
            max_duration: Some(Duration::from_secs(10)),
            report_interval: None,
        };
        let (reason, allocations) =
            count_allocations(|| run(&mut rx, &mut p, &stop, limits, |_| {}).unwrap());
        assert_eq!(reason, StopReason::DatagramLimit);
        total += allocations;
    }
    assert_eq!(total, 0, "allocations in the socket receive loop");
    assert_eq!(p.snapshot().counters.messages_delivered, 1024);
}
