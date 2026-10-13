//! Helpers for the privileged AF_XDP tests: a `veth` pair that removes
//! itself, and a UDP multicast sender on its far end.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::process::Command;
use std::time::{Duration, Instant};

use caml_engine::transport::{PacketSource, RxMeta};
use caml_transport_xdp::{FrameFilter, XdpConfig, XdpSource};
use socket2::{Domain, Protocol, Socket, Type};

/// Multicast group the tests send to.
pub const GROUP: Ipv4Addr = Ipv4Addr::new(239, 77, 0, 9);
/// UDP port the receiver filters on.
pub const PORT: u16 = 40_000;

fn ip(args: &[&str]) {
    let status = Command::new("ip").args(args).status().expect("run `ip`");
    assert!(status.success(), "`ip {}` failed", args.join(" "));
}

/// A `veth` pair: `<tag>t` (sender side, with an IPv4 address) and `<tag>r`
/// (receiver side, no address). Deleted when dropped.
pub struct Veth {
    pub tx: String,
    pub rx: String,
    pub tx_ip: Ipv4Addr,
}

impl Veth {
    /// `tag` must be at most 14 characters (Linux interface names are ≤ 15).
    pub fn new(tag: &str, subnet: u8) -> Self {
        let tx = format!("{tag}t");
        let rx = format!("{tag}r");
        // A previous aborted run may have left the pair behind.
        let _ = Command::new("ip").args(["link", "del", &tx]).status();
        ip(&["link", "add", &tx, "type", "veth", "peer", "name", &rx]);
        for dev in [&tx, &rx] {
            // No IPv6 autoconfiguration chatter, so frame counts are exact.
            let _ = Command::new("sysctl")
                .args(["-qw", &format!("net.ipv6.conf.{dev}.disable_ipv6=1")])
                .status();
            ip(&["link", "set", dev, "up"]);
        }
        let tx_ip = Ipv4Addr::new(10, 77, subnet, 1);
        ip(&["addr", "add", &format!("{tx_ip}/24"), "dev", &tx]);
        // veth advertises TX checksum offload, so the sending kernel leaves
        // the UDP checksum partial for "hardware" to finish, and the
        // receiver's software check (correctly) rejects the frame. A real
        // NIC puts a complete checksum on the wire; make veth do the same.
        let status = Command::new("ethtool")
            .args(["-K", &tx, "tx", "off"])
            .stdout(std::process::Stdio::null())
            .status()
            .expect("run `ethtool` (apt install ethtool)");
        assert!(status.success(), "ethtool -K {tx} tx off failed");
        Self { tx, rx, tx_ip }
    }

    /// A UDP socket that sends multicast out of the `tx` end, so frames
    /// arrive at the `rx` end exactly as a multicast feed would.
    pub fn sender(&self) -> UdpSocket {
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        s.bind(&SocketAddrV4::new(self.tx_ip, 0).into()).unwrap();
        s.set_multicast_if_v4(&self.tx_ip).unwrap();
        s.set_multicast_ttl_v4(1).unwrap();
        s.set_multicast_loop_v4(false).unwrap();
        s.into()
    }

    /// The filter a receiver of this pair uses: group, port, and source.
    pub fn filter(&self) -> FrameFilter {
        FrameFilter {
            dst_ip: Some(GROUP),
            src_ip: Some(self.tx_ip),
            ..FrameFilter::port(PORT)
        }
    }

    /// Binds an AF_XDP source on the `rx` end.
    pub fn source(&self, filter: FrameFilter) -> XdpSource {
        XdpSource::bind(&XdpConfig::new(&self.rx, filter)).expect("bind AF_XDP (needs root)")
    }
}

impl Drop for Veth {
    fn drop(&mut self) {
        let _ = Command::new("ip").args(["link", "del", &self.tx]).status();
    }
}

/// Polls until `want` payloads were delivered or 5 s pass; returns how many.
pub fn drain(src: &mut XdpSource, want: u64, mut f: impl FnMut(&[u8], RxMeta)) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = 0;
    while got < want && Instant::now() < deadline {
        got += src.poll(&mut f).unwrap() as u64;
    }
    got
}
