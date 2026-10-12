//! End-to-end tests over real UDP sockets on 127.0.0.1.
//!
//! Single-threaded and deterministic: the test sends at most [`BURST`]
//! datagrams, then drains them before sending more, so it never relies on
//! the kernel buffering a whole feed. (Stock Linux caps `SO_RCVBUF` at
//! `2 * net.core.rmem_max`, about 416 KiB, and charges each small datagram
//! roughly 1 KiB of buffer.)

#![allow(clippy::unwrap_used)]

use std::net::{SocketAddr, UdpSocket};
use std::num::NonZeroU64;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use caml_engine::clock::wall_clock_ns;
use caml_engine::transport::{PacketSource, UdpConfig, UdpSource};
use caml_engine::{Delivery, EventSink, FeedProcessor, RunLimits, StopReason, run};
use caml_protocol::{DecodeErrorKind, MAX_PAYLOAD_1500_MTU, MarketEvent, patch_send_time};
use caml_sim::{FaultInjector, FaultPlan, FeedConfig, FeedSimulator};

const BURST: u64 = 32;

#[derive(Default)]
struct Collect {
    events: Vec<(u64, Delivery)>,
}

impl EventSink for Collect {
    fn on_event(&mut self, e: &MarketEvent, d: Delivery) {
        self.events.push((e.seq, d));
    }
}

struct Harness {
    rx: UdpSource,
    tx: UdpSocket,
    processor: FeedProcessor<Collect>,
    sent: u64,
}

impl Harness {
    fn new() -> Self {
        let rx = UdpSource::bind(&UdpConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            recv_buffer_bytes: Some(1 << 20),
            poll_timeout: Duration::from_millis(20),
            ..UdpConfig::default()
        })
        .unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.connect(rx.local_addr().unwrap()).unwrap();
        Self {
            rx,
            tx,
            processor: FeedProcessor::new(Collect::default()),
            sent: 0,
        }
    }

    fn send(&mut self, packet: &mut [u8]) {
        patch_send_time(packet, wall_clock_ns()).unwrap();
        self.tx.send(packet).unwrap();
        self.sent += 1;
        if self.sent.is_multiple_of(BURST) {
            self.drain();
        }
    }

    /// Receives until every datagram sent so far has been processed.
    fn drain(&mut self) {
        let stop = AtomicBool::new(false);
        let limits = RunLimits {
            max_datagrams: Some(self.sent),
            max_duration: Some(Duration::from_secs(10)),
            report_interval: None,
        };
        let reason = run(&mut self.rx, &mut self.processor, &stop, limits, |_| {}).unwrap();
        assert_eq!(reason, StopReason::DatagramLimit, "datagrams went missing");
    }

    /// Sends data packets `1..=packets` through `plan`, then the
    /// end-of-session heartbeat, and drains everything.
    fn run_feed(&mut self, packets: u64, msgs_per_packet: u16, plan: FaultPlan) {
        let mut sim = FeedSimulator::new(FeedConfig {
            msgs_per_packet,
            ..FeedConfig::default()
        })
        .unwrap();
        let mut inj = FaultInjector::new(plan, MAX_PAYLOAD_1500_MTU);
        let mut buf = [0u8; MAX_PAYLOAD_1500_MTU];
        for i in 1..=packets {
            let len = sim.next_packet(&mut buf, wall_clock_ns()).unwrap();
            inj.process(i, &mut buf[..len], |p| {
                self.send(p);
                Ok::<(), ()>(())
            })
            .unwrap();
        }
        inj.finish(|p| {
            self.send(p);
            Ok::<(), ()>(())
        })
        .unwrap();
        let len = sim.end_of_session(&mut buf, wall_clock_ns()).unwrap();
        self.send(&mut buf[..len]);
        self.drain();
    }
}

#[test]
fn clean_feed_is_received_completely() {
    let mut h = Harness::new();
    h.run_feed(500, 4, FaultPlan::default());
    assert_eq!(h.sent, 501);

    let s = h.processor.snapshot();
    assert_eq!(s.counters.messages_delivered, 2000);
    assert_eq!(s.seq.gaps, 0);
    assert_eq!(s.counters.malformed, 0);
    assert_eq!(s.counters.sessions_ended, 1);
    assert_eq!(s.send_to_rx_latency.count + s.counters.clock_anomalies, 501);
    let v = s.integrity();
    assert!(v.is_clean() && v.session_ended, "{v:?}");
    let seqs: Vec<u64> = h.processor.sink().events.iter().map(|e| e.0).collect();
    assert_eq!(seqs, (1..=2000).collect::<Vec<_>>());
    assert_eq!(h.rx.stats().datagrams, 501);
}

/// The fault periods are chosen so that no two faults hit the same or
/// adjacent packets, which keeps the expected counts easy to derive by hand:
///
/// * drop      every 25: 25, 50, 75, 100
/// * corrupt   every 41: 41, 82
/// * duplicate every 13: 13, 26, 39, 52, 65, 78, 91
/// * reorder   every 23: 23, 46, 69, 92 (each sent after its successor)
///
/// Packet k carries messages 2k-1 and 2k.
#[test]
fn faults_are_detected_and_accounted() {
    let nz = NonZeroU64::new;
    let plan = FaultPlan {
        drop_every: nz(25),
        corrupt_every: nz(41),
        duplicate_every: nz(13),
        reorder_every: nz(23),
    };
    let mut h = Harness::new();
    h.run_feed(101, 2, plan);
    // 101 data packets - 4 dropped + 7 duplicates + 1 end-of-session.
    assert_eq!(h.sent, 105);

    let s = h.processor.snapshot();
    println!("{s}");
    let c = s.counters;
    assert_eq!(c.datagrams, 105);
    assert_eq!(c.malformed, 2);
    assert_eq!(c.malformed_by_kind[DecodeErrorKind::BadMagic.index()], 2);
    // Every message of a duplicated packet is a duplicate.
    assert_eq!(c.redundant_packets, 7);
    assert_eq!(s.seq.duplicates, 14);

    // 6 packets lost (4 dropped + 2 corrupted) and 4 reordered: each is a
    // two-message gap; the reordered ones are recovered.
    assert_eq!(s.seq.gaps, 10);
    assert_eq!(s.seq.missing_total, 20);
    assert_eq!(s.seq.recovered, 8);
    assert_eq!(s.seq.outstanding, 12);
    assert_eq!(s.seq.evicted, 0);

    assert_eq!(c.messages_delivered, (101 - 6) * 2);
    assert_eq!(c.messages_delivered_late, 8);

    let events = &h.processor.sink().events;
    let late: Vec<u64> = events
        .iter()
        .filter(|e| e.1 == Delivery::Late)
        .map(|e| e.0)
        .collect();
    assert_eq!(late, [45, 46, 91, 92, 137, 138, 183, 184]);
    assert_eq!(
        events.iter().filter(|e| e.1 == Delivery::AfterGap).count(),
        10
    );

    let v = s.integrity();
    assert_eq!((v.lost_messages, v.malformed), (12, 2));
    assert!(!v.is_clean());
}

/// Version 1 could not detect loss of a feed's final packets. The
/// end-of-session heartbeat announces the next sequence number, so it can.
#[test]
fn tail_loss_is_detected_via_end_of_session() {
    let plan = FaultPlan {
        drop_every: NonZeroU64::new(10),
        ..FaultPlan::default()
    };
    let mut h = Harness::new();
    h.run_feed(10, 4, plan);
    let s = h.processor.snapshot();
    assert_eq!(s.counters.messages_delivered, 36);
    assert_eq!(s.seq.outstanding, 4);
    let v = s.integrity();
    assert_eq!(v.lost_messages, 4);
    assert!(v.session_ended && !v.is_clean());
}

#[test]
fn stop_flag_and_duration_limit() {
    let mut h = Harness::new();

    let stop = AtomicBool::new(true);
    let reason = run(
        &mut h.rx,
        &mut h.processor,
        &stop,
        RunLimits::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(reason, StopReason::Signalled);

    let stop = AtomicBool::new(false);
    let limits = RunLimits {
        max_duration: Some(Duration::from_millis(100)),
        report_interval: Some(Duration::from_millis(30)),
        ..RunLimits::default()
    };
    let mut reports = 0;
    let reason = run(&mut h.rx, &mut h.processor, &stop, limits, |_| reports += 1).unwrap();
    assert_eq!(reason, StopReason::DurationLimit);
    // 100 ms at a 30 ms interval is about 3 reports. OS timer granularity
    // can stretch polls, so only the upper bound is strict: missed slots are
    // skipped, never replayed in a burst.
    assert!((1..=4).contains(&reports), "got {reports} reports");
}
