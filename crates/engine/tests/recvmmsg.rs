//! End-to-end test of the Linux `recvmmsg` backend through the full
//! pipeline. Linux only (runs in CI).

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

mod common;

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use caml_engine::transport::{PacketSource, RecvMmsgSource, UdpConfig};
use caml_engine::{FeedProcessor, RunLimits, StopReason, run};
use caml_protocol::MarketEvent;
use common::{heartbeat, packet};

#[test]
fn full_pipeline_over_recvmmsg() {
    let mut src = RecvMmsgSource::bind(&UdpConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        recv_buffer_bytes: Some(1 << 20),
        poll_timeout: Duration::from_millis(20),
        ..UdpConfig::default()
    })
    .unwrap();
    let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
    tx.connect(src.local_addr().unwrap()).unwrap();
    // Packets carry send_time = T0 - 1 µs (a fixed past time), so the
    // send→rx histogram is meaningless here; kernel→rx is real.
    for k in 0..200u64 {
        tx.send(&packet(7, 1 + k, 1)).unwrap();
    }
    tx.send(&heartbeat(7, 201, true)).unwrap();

    let mut p = FeedProcessor::new(|_: &MarketEvent, _| {});
    let stop = AtomicBool::new(false);
    let limits = RunLimits {
        max_datagrams: Some(201),
        max_duration: Some(Duration::from_secs(10)),
        report_interval: None,
    };
    assert_eq!(
        run(&mut src, &mut p, &stop, limits, |_| {}).unwrap(),
        StopReason::DatagramLimit
    );
    let s = p.snapshot();
    assert_eq!(s.counters.messages_delivered, 200);
    let v = s.integrity();
    assert!(v.is_clean() && v.session_ended, "{v:?}");
    assert_eq!(s.kernel_to_rx_latency.count, 201);
    let t = src.stats();
    assert_eq!((t.datagrams, t.kernel_drops), (201, Some(0)));
}
