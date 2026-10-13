//! AF_XDP receive tests on `veth` pairs. They need root (CAP_NET_ADMIN,
//! CAP_BPF) and create network interfaces, so they are ignored by default:
//! `scripts/xdp-veth.sh` builds them as you and runs them under sudo.

#![cfg(all(target_os = "linux", feature = "xdp"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::SocketAddrV4;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use caml_engine::clock::wall_clock_ns;
use caml_engine::transport::PacketSource;
use caml_engine::{FeedProcessor, RunLimits, StopReason, run};
use caml_protocol::{MAX_PAYLOAD_1500_MTU, MarketEvent};
use caml_sim::{FeedConfig, FeedSimulator};
use caml_transport_xdp::{FrameError, FrameFilter};
use common::{GROUP, PORT, Veth, drain};

/// The whole receive path over AF_XDP: a synthetic feed is multicast across
/// the veth pair, received zero-copy from UMEM, filtered, decoded, and
/// sequenced. Every datagram must be accounted for.
#[test]
#[ignore = "needs root and creates veth pairs: run scripts/xdp-veth.sh"]
fn delivers_a_multicast_feed_exactly() {
    let veth = Veth::new("cxdp1", 1);
    let mut src = veth.source(veth.filter());
    let tx = veth.sender();
    let dst = SocketAddrV4::new(GROUP, PORT);

    let mut sim = FeedSimulator::new(FeedConfig {
        session_epoch: wall_clock_ns(),
        ..FeedConfig::default()
    })
    .unwrap();
    let mut buf = [0u8; MAX_PAYLOAD_1500_MTU];
    for _ in 0..500 {
        let len = sim.next_packet(&mut buf, wall_clock_ns()).unwrap();
        tx.send_to(&buf[..len], dst).unwrap();
    }
    let len = sim.end_of_session(&mut buf, wall_clock_ns()).unwrap();
    tx.send_to(&buf[..len], dst).unwrap();

    let mut p = FeedProcessor::new(|_: &MarketEvent, _| {});
    let stop = AtomicBool::new(false);
    let limits = RunLimits {
        max_datagrams: Some(501),
        max_duration: Some(Duration::from_secs(10)),
        report_interval: None,
    };
    let reason = run(&mut src, &mut p, &stop, limits, |_| {}).unwrap();
    assert_eq!(reason, StopReason::DatagramLimit, "{:?}", src.xdp_stats());

    let s = p.snapshot();
    assert_eq!(s.counters.messages_delivered, 2000);
    let v = s.integrity();
    assert!(v.is_clean() && v.session_ended, "{v:?}");

    let t = src.stats();
    assert_eq!(t.datagrams, 501, "{t:?}");
    assert_eq!(t.kernel_drops, Some(0), "{t:?}");
    assert_eq!((t.dropped(), t.filtered_frames), (0, 0), "{t:?}");
    let x = src.xdp_stats();
    assert_eq!((x.frames.accepted, x.lost_frames), (501, 0), "{x:?}");
}

/// Frames for another port are filtered and counted, never delivered; a
/// rebind right after the previous socket closed succeeds (the source
/// retries while the kernel still holds the queue).
#[test]
#[ignore = "needs root and creates veth pairs: run scripts/xdp-veth.sh"]
fn filters_and_counts_frames_not_for_us() {
    let veth = Veth::new("cxdp2", 2);
    let tx = veth.sender();

    let mut src = veth.source(veth.filter());
    for port in [PORT + 1, PORT] {
        for _ in 0..10 {
            tx.send_to(b"payload", SocketAddrV4::new(GROUP, port))
                .unwrap();
        }
    }
    let got = drain(&mut src, 10, |b, _| assert_eq!(b, b"payload"));
    assert_eq!(got, 10, "{:?}", src.xdp_stats());
    let x = src.xdp_stats();
    assert_eq!(x.frames.rejected[FrameError::DstPortMismatch.index()], 10);
    let t = src.stats();
    assert_eq!((t.filtered_frames, t.dropped()), (10, 0), "{t:?}");
    drop(src);

    // Immediately rebind with a filter that expects a different source.
    let other = FrameFilter {
        src_ip: Some([10, 77, 2, 99].into()),
        ..veth.filter()
    };
    let mut src = veth.source(other);
    for _ in 0..5 {
        tx.send_to(b"x", SocketAddrV4::new(GROUP, PORT)).unwrap();
    }
    // Nothing is delivered; wait for the frames to be seen and counted.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while src.stats().filtered_source < 5 && std::time::Instant::now() < deadline {
        assert_eq!(src.poll(|_, _| panic!("must not deliver")).unwrap(), 0);
    }
    assert_eq!(src.stats().filtered_source, 5, "{:?}", src.xdp_stats());
}
