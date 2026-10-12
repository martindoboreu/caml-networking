//! Ethernet/IPv4/UDP framing and pcap output.
//!
//! Raw-packet backends (AF_XDP, DPDK) receive whole Ethernet frames, not UDP
//! payloads. This module wraps synthetic feed packets in correct frames
//! (valid IPv4 and UDP checksums, minimum-size padding, optional 802.1Q tag)
//! and writes them to a nanosecond-resolution pcap file. The file can be
//! replayed with `tcpreplay` onto a `veth`, or fed to DPDK's `net_pcap`
//! virtual device, so the exact same bytes exercise every backend.

use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddrV4};

const ETH_HEADER_LEN: usize = 14;
const VLAN_TAG_LEN: usize = 4;
const IPV4_HEADER_LEN: usize = 20;
const UDP_HEADER_LEN: usize = 8;
/// Ethernet frames shorter than this (without FCS) are padded with zeros.
pub const MIN_FRAME_LEN: usize = 60;
/// Largest UDP payload in one IPv4 datagram.
pub const MAX_UDP_PAYLOAD: usize = 65_507;

/// Addressing for [`build_udp_frame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpFrameSpec {
    /// Source MAC address.
    pub src_mac: [u8; 6],
    /// Destination MAC address.
    pub dst_mac: [u8; 6],
    /// Source IPv4 address and port.
    pub src: SocketAddrV4,
    /// Destination IPv4 address and port.
    pub dst: SocketAddrV4,
    /// 802.1Q VLAN id (12 bits), if tagged.
    pub vlan: Option<u16>,
    /// IPv4 time-to-live.
    pub ttl: u8,
}

impl UdpFrameSpec {
    /// Locally administered MACs, TTL 64, no VLAN. A multicast destination
    /// gets its standard 01:00:5e group MAC.
    pub fn new(src: SocketAddrV4, dst: SocketAddrV4) -> Self {
        let dst_mac = if dst.ip().is_multicast() {
            multicast_mac(*dst.ip())
        } else {
            [0x02, 0, 0, 0, 0, 0x02]
        };
        Self {
            src_mac: [0x02, 0, 0, 0, 0, 0x01],
            dst_mac,
            src,
            dst,
            vlan: None,
            ttl: 64,
        }
    }
}

/// Ethernet MAC for an IPv4 multicast group (RFC 1112): `01:00:5e` followed
/// by the low 23 bits of the group address.
pub fn multicast_mac(group: Ipv4Addr) -> [u8; 6] {
    let o = group.octets();
    [0x01, 0x00, 0x5e, o[1] & 0x7f, o[2], o[3]]
}

/// One's-complement sum of 16-bit big-endian words, unfolded.
fn sum16(bytes: &[u8], mut acc: u32) -> u32 {
    let (words, rest) = bytes.as_chunks::<2>();
    for w in words {
        acc = acc.wrapping_add(u32::from(u16::from_be_bytes(*w)));
    }
    if let [last] = rest {
        acc = acc.wrapping_add(u32::from(*last) << 8);
    }
    acc
}

fn fold(mut acc: u32) -> u16 {
    while acc >> 16 != 0 {
        acc = (acc & 0xFFFF) + (acc >> 16);
    }
    !(acc as u16)
}

/// IPv4 header checksum of `header` (with its checksum field zeroed).
pub fn ipv4_checksum(header: &[u8]) -> u16 {
    fold(sum16(header, 0))
}

/// UDP checksum over the IPv4 pseudo-header and `segment` (UDP header with a
/// zeroed checksum field, plus payload). A computed 0 is sent as `0xFFFF`,
/// because 0 means "no checksum" in IPv4.
pub fn udp_checksum(src: Ipv4Addr, dst: Ipv4Addr, segment: &[u8]) -> u16 {
    let mut acc = sum16(&src.octets(), 0);
    acc = sum16(&dst.octets(), acc);
    acc = acc.wrapping_add(17);
    acc = acc.wrapping_add(segment.len() as u32);
    let c = fold(sum16(segment, acc));
    if c == 0 { 0xFFFF } else { c }
}

/// Error from [`build_udp_frame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameBuildError {
    /// Payload exceeds [`MAX_UDP_PAYLOAD`].
    PayloadTooLarge(usize),
    /// VLAN id exceeds 12 bits.
    BadVlan(u16),
}

impl std::fmt::Display for FrameBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PayloadTooLarge(n) => write!(f, "UDP payload of {n} bytes is too large"),
            Self::BadVlan(v) => write!(f, "VLAN id {v} exceeds 4095"),
        }
    }
}

impl std::error::Error for FrameBuildError {}

/// Builds an Ethernet II frame carrying `payload` in one unfragmented
/// IPv4/UDP datagram, replacing the contents of `out`.
pub fn build_udp_frame(
    spec: &UdpFrameSpec,
    payload: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), FrameBuildError> {
    if payload.len() > MAX_UDP_PAYLOAD {
        return Err(FrameBuildError::PayloadTooLarge(payload.len()));
    }
    if let Some(v) = spec.vlan
        && v > 0x0FFF
    {
        return Err(FrameBuildError::BadVlan(v));
    }
    let udp_len = UDP_HEADER_LEN + payload.len();
    let ip_len = IPV4_HEADER_LEN + udp_len;

    out.clear();
    out.extend_from_slice(&spec.dst_mac);
    out.extend_from_slice(&spec.src_mac);
    if let Some(v) = spec.vlan {
        out.extend_from_slice(&0x8100u16.to_be_bytes());
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.extend_from_slice(&0x0800u16.to_be_bytes());

    let ip_start = out.len();
    out.extend_from_slice(&[0x45, 0x00]);
    out.extend_from_slice(&(ip_len as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0, 0x40, 0x00]); // id 0, DF set, offset 0
    out.extend_from_slice(&[spec.ttl, 17, 0, 0]); // TTL, UDP, checksum placeholder
    out.extend_from_slice(&spec.src.ip().octets());
    out.extend_from_slice(&spec.dst.ip().octets());
    let ip_csum = ipv4_checksum(&out[ip_start..ip_start + IPV4_HEADER_LEN]);
    out[ip_start + 10..ip_start + 12].copy_from_slice(&ip_csum.to_be_bytes());

    let udp_start = out.len();
    out.extend_from_slice(&spec.src.port().to_be_bytes());
    out.extend_from_slice(&spec.dst.port().to_be_bytes());
    out.extend_from_slice(&(udp_len as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(payload);
    let udp_csum = udp_checksum(*spec.src.ip(), *spec.dst.ip(), &out[udp_start..]);
    out[udp_start + 6..udp_start + 8].copy_from_slice(&udp_csum.to_be_bytes());

    if out.len() < MIN_FRAME_LEN {
        out.resize(MIN_FRAME_LEN, 0);
    }
    Ok(())
}

/// Length of the Ethernet (+ VLAN) and IPv4/UDP headers in front of the
/// payload in a frame built by [`build_udp_frame`].
pub fn header_len(spec: &UdpFrameSpec) -> usize {
    ETH_HEADER_LEN
        + if spec.vlan.is_some() { VLAN_TAG_LEN } else { 0 }
        + IPV4_HEADER_LEN
        + UDP_HEADER_LEN
}

/// Writes a classic pcap file with nanosecond timestamps and Ethernet
/// link type.
#[derive(Debug)]
pub struct PcapWriter<W: Write> {
    out: W,
}

/// pcap magic number for nanosecond-resolution timestamps.
pub const PCAP_MAGIC_NS: u32 = 0xA1B2_3C4D;
const LINKTYPE_ETHERNET: u32 = 1;
const SNAPLEN: u32 = 65_535;

impl<W: Write> PcapWriter<W> {
    /// Writes the global header.
    pub fn new(mut out: W) -> io::Result<Self> {
        out.write_all(&PCAP_MAGIC_NS.to_le_bytes())?;
        out.write_all(&2u16.to_le_bytes())?; // version major
        out.write_all(&4u16.to_le_bytes())?; // version minor
        out.write_all(&0i32.to_le_bytes())?; // thiszone
        out.write_all(&0u32.to_le_bytes())?; // sigfigs
        out.write_all(&SNAPLEN.to_le_bytes())?;
        out.write_all(&LINKTYPE_ETHERNET.to_le_bytes())?;
        Ok(Self { out })
    }

    /// Appends one frame captured at `ts_ns` (ns since the Unix epoch).
    pub fn write_frame(&mut self, ts_ns: u64, frame: &[u8]) -> io::Result<()> {
        let len = u32::try_from(frame.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
        let secs = u32::try_from(ts_ns / 1_000_000_000).unwrap_or(u32::MAX);
        let nanos = (ts_ns % 1_000_000_000) as u32;
        self.out.write_all(&secs.to_le_bytes())?;
        self.out.write_all(&nanos.to_le_bytes())?;
        self.out.write_all(&len.to_le_bytes())?;
        self.out.write_all(&len.to_le_bytes())?;
        self.out.write_all(frame)
    }

    /// Flushes and returns the underlying writer.
    pub fn into_inner(mut self) -> io::Result<W> {
        self.out.flush()?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> UdpFrameSpec {
        UdpFrameSpec::new(
            SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 50_000),
            SocketAddrV4::new(Ipv4Addr::new(239, 1, 2, 3), 40_000),
        )
    }

    #[test]
    fn rfc1071_checksum_example() {
        // Header from RFC 1071's worked example style: a known IPv4 header
        // whose checksum is 0xb861 (widely used reference vector).
        let hdr = [
            0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8,
            0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7,
        ];
        assert_eq!(ipv4_checksum(&hdr), 0xb861);
    }

    #[test]
    fn frame_layout_and_checksums_verify() {
        let payload = [0xAB; 100];
        let mut f = Vec::new();
        build_udp_frame(&spec(), &payload, &mut f).unwrap();
        assert_eq!(f.len(), header_len(&spec()) + payload.len());
        assert_eq!(&f[0..6], &[0x01, 0x00, 0x5e, 0x01, 0x02, 0x03]);
        assert_eq!(&f[12..14], &[0x08, 0x00]);
        // A correct checksum makes the one's-complement sum fold to zero.
        assert_eq!(ipv4_checksum(&f[14..34]), 0);
        let mut acc = sum16(&[192, 0, 2, 1, 239, 1, 2, 3], 0);
        acc = acc.wrapping_add(17).wrapping_add(108);
        assert_eq!(fold(sum16(&f[34..], acc)), 0);
        assert_eq!(&f[42..], &payload);
    }

    #[test]
    fn short_frames_are_padded_and_vlan_is_tagged() {
        let mut s = spec();
        s.vlan = Some(42);
        let mut f = Vec::new();
        build_udp_frame(&s, &[1, 2], &mut f).unwrap();
        assert_eq!(f.len(), MIN_FRAME_LEN);
        assert_eq!(&f[12..18], &[0x81, 0x00, 0x00, 42, 0x08, 0x00]);
        // IPv4 total_length excludes the padding.
        assert_eq!(u16::from_be_bytes([f[20], f[21]]), 20 + 8 + 2);

        s.vlan = Some(4096);
        assert_eq!(
            build_udp_frame(&s, &[], &mut f),
            Err(FrameBuildError::BadVlan(4096))
        );
        assert_eq!(
            build_udp_frame(&spec(), &vec![0; MAX_UDP_PAYLOAD + 1], &mut f),
            Err(FrameBuildError::PayloadTooLarge(MAX_UDP_PAYLOAD + 1))
        );
    }

    #[test]
    fn pcap_layout() {
        let mut w = PcapWriter::new(Vec::new()).unwrap();
        w.write_frame(3_000_000_007, &[9; 60]).unwrap();
        let bytes = w.into_inner().unwrap();
        assert_eq!(bytes.len(), 24 + 16 + 60);
        assert_eq!(&bytes[0..4], &PCAP_MAGIC_NS.to_le_bytes());
        assert_eq!(&bytes[20..24], &1u32.to_le_bytes());
        assert_eq!(&bytes[24..28], &3u32.to_le_bytes());
        assert_eq!(&bytes[28..32], &7u32.to_le_bytes());
        assert_eq!(&bytes[32..36], &60u32.to_le_bytes());
    }
}
