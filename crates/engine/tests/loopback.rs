//! End-to-end tests over real UDP sockets on 127.0.0.1.
//!
//! The receiver socket is bound before anything is sent, so every datagram is
//! queued in the kernel receive buffer and the test can run single-threaded
//! and deterministically.

#![allow(clippy::unwrap_used)]

use std::net::{SocketAddr, UdpSocket};
use std::num::NonZeroU64;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use caml_engine::clock::wall_clock_ns;
use caml_engine::transport::{UdpConfig, UdpSource};
use caml_engine::{Delivery, FeedProcessor, RunLimits, StopReason, run};
use caml_protocol::{DecodeErrorKind, MAX_PAYLOAD_1500_MTU, MarketEvent};
use caml_sim::{FaultInjector, FaultPlan, FeedConfig, FeedSimulator};

fn bind_receiver() -> UdpSource {
    UdpSource::bind(&UdpConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        recv_buffer_bytes: Some(4 << 20),
        poll_timeout: Duration::from_millis(20),
    })
    .unwrap()
}

#[derive(Default)]
struct Collect {
    events: Vec<(u64, Delivery)>,
}

impl caml_engine::EventSink for Collect {
    fn on_event(&mut self, e: &MarketEvent, d: Delivery) {
        self.events.push((e.seq, d));
    }
}

/// Sends packets `1..=packets` through the fault injector; returns datagrams sent.
fn send_feed(target: SocketAddr, packets: u64, msgs_per_packet: u16, plan: FaultPlan) -> u64 {
    let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
    tx.connect(target).unwrap();
    let mut sim = FeedSimulator::new(FeedConfig {
        msgs_per_packet,
        ..FeedConfig::default()
    })
    .unwrap();
    let mut inj = FaultInjector::new(plan, MAX_PAYLOAD_1500_MTU);
    let mut buf = [0u8; MAX_PAYLOAD_1500_MTU];
    let mut sent = 0u64;
    let mut send = |p: &[u8]| {
        sent += 1;
        tx.send(p).map(|_| ())
    };
    for i in 1..=packets {
        let len = sim.next_packet(&mut buf, wall_clock_ns()).unwrap();
        inj.process(i, &mut buf[..len], &mut send).unwrap();
    }
    inj.finish(&mut send).unwrap();
    sent
}

#[test]
fn clean_feed_is_received_completely() {
    let mut rx = bind_receiver();
    let sent = send_feed(rx.local_addr().unwrap(), 500, 4, FaultPlan::default());
    assert_eq!(sent, 500);

    let mut p = FeedProcessor::new(Collect::default());
    let stop = AtomicBool::new(false);
    let limits = RunLimits {
        max_datagrams: Some(sent),
        max_duration: Some(Duration::from_secs(10)),
        report_interval: None,
    };
    let reason = run(&mut rx, &mut p, &stop, limits, |_| {}).unwrap();
    assert_eq!(reason, StopReason::DatagramLimit);

    let s = p.snapshot();
    assert_eq!(s.counters.messages_delivered, 2000);
    assert_eq!(s.packet_seq.gaps, 0);
    assert_eq!(s.message_seq.gaps, 0);
    assert_eq!(s.counters.malformed, 0);
    assert_eq!(s.wire_latency.count + s.counters.clock_anomalies, 500);
    let seqs: Vec<u64> = p.sink().events.iter().map(|e| e.0).collect();
    assert_eq!(seqs, (1..=2000).collect::<Vec<_>>());
}

/// The fault periods are chosen so that no two faults hit the same or
/// adjacent packets, which keeps the expected counts easy to derive by hand:
///
/// * drop      every 25: 25, 50, 75, 100
/// * corrupt   every 41: 41, 82
/// * duplicate every 13: 13, 26, 39, 52, 65, 78, 91
/// * reorder   every 23: 23, 46, 69, 92 (each sent after its successor)
#[test]
fn faults_are_detected_and_accounted() {
    let nz = NonZeroU64::new;
    let plan = FaultPlan {
        drop_every: nz(25),
        corrupt_every: nz(41),
        duplicate_every: nz(13),
        reorder_every: nz(23),
    };
    let mut rx = bind_receiver();
    let sent = send_feed(rx.local_addr().unwrap(), 101, 2, plan);
    assert_eq!(sent, 101 - 4 + 7);

    let mut p = FeedProcessor::new(Collect::default());
    let stop = AtomicBool::new(false);
    let limits = RunLimits {
        max_datagrams: Some(sent),
        max_duration: Some(Duration::from_secs(10)),
        report_interval: None,
    };
    let reason = run(&mut rx, &mut p, &stop, limits, |_| {}).unwrap();
    assert_eq!(reason, StopReason::DatagramLimit);
    let s = p.snapshot();
    println!("{s}");

    let c = s.counters;
    assert_eq!(c.datagrams, 104);
    assert_eq!(c.malformed, 2);
    assert_eq!(c.malformed_by_kind[DecodeErrorKind::BadMagic.index()], 2);
    assert_eq!(c.duplicate_packets, 7);

    // Packet level: 6 lost (4 dropped + 2 corrupt) and 4 reordered, each a
    // one-packet gap; the reordered ones are later recovered.
    let ps = s.packet_seq;
    assert_eq!(ps.gaps, 10);
    assert_eq!(ps.missing_total, 10);
    assert_eq!(ps.recovered, 4);
    assert_eq!(ps.outstanding, 6);
    assert_eq!(ps.duplicates, 7);
    assert_eq!(ps.evicted, 0);

    // Message level: same events, two messages per packet. Duplicate packets
    // are filtered before message sequencing, so no message duplicates.
    let ms = s.message_seq;
    assert_eq!(ms.gaps, 10);
    assert_eq!(ms.missing_total, 20);
    assert_eq!(ms.recovered, 8);
    assert_eq!(ms.outstanding, 12);
    assert_eq!(ms.duplicates, 0);

    assert_eq!(c.messages_delivered, (101 - 6) * 2);
    assert_eq!(c.messages_delivered_late, 8);

    let events = &p.sink().events;
    let late: Vec<u64> = events
        .iter()
        .filter(|e| e.1 == Delivery::Late)
        .map(|e| e.0)
        .collect();
    // Packet k carries messages 2k-1 and 2k.
    assert_eq!(late, [45, 46, 91, 92, 137, 138, 183, 184]);
    assert_eq!(
        events.iter().filter(|e| e.1 == Delivery::AfterGap).count(),
        10
    );
}

#[test]
fn stop_flag_and_duration_limit() {
    let mut rx = bind_receiver();
    let mut p = FeedProcessor::new(Collect::default());

    let stop = AtomicBool::new(true);
    let reason = run(&mut rx, &mut p, &stop, RunLimits::default(), |_| {}).unwrap();
    assert_eq!(reason, StopReason::Signalled);

    let stop = AtomicBool::new(false);
    let limits = RunLimits {
        max_duration: Some(Duration::from_millis(100)),
        report_interval: Some(Duration::from_millis(30)),
        ..RunLimits::default()
    };
    let mut reports = 0;
    let reason = run(&mut rx, &mut p, &stop, limits, |_| reports += 1).unwrap();
    assert_eq!(reason, StopReason::DurationLimit);
    assert!(reports >= 1, "expected periodic reports, got {reports}");
}
