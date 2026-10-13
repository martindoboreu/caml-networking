//! Automatic fallback from AF_XDP to `recvmmsg`.
//!
//! The fallback tests make AF_XDP fail on purpose (an interface that does not
//! exist; without root it also fails for lack of privileges), so they need
//! no special rights. The last test needs root and a veth pair.

#![cfg(all(target_os = "linux", feature = "xdp"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use caml_engine::transport::{PacketSource, UdpConfig};
use caml_transport_xdp::{Backend, FallbackSource, FrameFilter, XdpConfig};
use common::{GROUP, PORT, Veth, drain_any};

fn missing_interface() -> XdpConfig {
    XdpConfig {
        bind_retry: Duration::ZERO,
        ..XdpConfig::new("cxnosuchdev0", FrameFilter::port(PORT))
    }
}

fn loopback_socket() -> UdpConfig {
    UdpConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        poll_timeout: Duration::from_millis(20),
        ..UdpConfig::default()
    }
}

#[test]
fn falls_back_to_recvmmsg_when_af_xdp_cannot_start() {
    let mut src = FallbackSource::bind(&missing_interface(), &loopback_socket()).unwrap();
    assert_eq!(src.backend(), Backend::Recvmmsg);
    let reason = src.fallback_reason().expect("the reason is reported");
    assert!(reason.contains("cxnosuchdev0"), "{reason}");
    // The OS cause survives: no such device as root, no permission otherwise.
    assert!(reason.contains("os error"), "{reason}");
    assert!(src.xdp_stats().is_none());

    let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
    tx.send_to(b"hello", src.local_addr().unwrap()).unwrap();
    let mut got = Vec::new();
    assert_eq!(drain_any(&mut src, 1, |b, _| got.push(b.to_vec())), 1);
    assert_eq!(got, [b"hello".to_vec()]);
    assert_eq!(src.stats().datagrams, 1);
}

#[test]
fn reports_both_causes_when_both_backends_fail() {
    let broken_socket = UdpConfig {
        poll_timeout: Duration::ZERO, // rejected by the socket backend
        ..loopback_socket()
    };
    let err = FallbackSource::bind(&missing_interface(), &broken_socket).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("AF_XDP on cxnosuchdev0 failed"), "{msg}");
    assert!(msg.contains("recvmmsg fallback failed too"), "{msg}");
}

#[test]
#[ignore = "needs root and creates veth pairs: run scripts/xdp-veth.sh"]
fn prefers_af_xdp_when_it_can_start() {
    let veth = Veth::new("cxdp3", 3);
    let xdp = XdpConfig::new(&veth.rx, veth.filter());
    let mut src = FallbackSource::bind(&xdp, &loopback_socket()).unwrap();
    assert_eq!(src.backend(), Backend::AfXdp);
    assert!(src.fallback_reason().is_none());
    assert!(src.local_addr().is_none());

    let tx = veth.sender();
    for _ in 0..10 {
        tx.send_to(b"over-xdp", SocketAddrV4::new(GROUP, PORT))
            .unwrap();
    }
    let got = drain_any(&mut src, 10, |b, _| assert_eq!(b, b"over-xdp"));
    assert_eq!(got, 10, "{:?}", src.xdp_stats());
    assert_eq!(src.xdp_stats().unwrap().frames.accepted, 10);
}
