//! Ethernet / 802.1Q / IPv4 / UDP frame parsing for raw-packet backends.
//!
//! Kernel sockets hand the receiver a UDP payload. Kernel-bypass backends
//! (AF_XDP, DPDK) hand it a whole Ethernet frame, so the header validation
//! the kernel used to do becomes ours. This crate does exactly that and
//! nothing more:
//!
//! * Ethernet II, optionally with **one** 802.1Q (`0x8100`) tag; stacked or
//!   802.1ad/legacy-QinQ tags (`0x88A8`, `0x9100`, `0x9200`) are rejected;
//! * IPv4 only, with options allowed, fragments rejected (market-data feeds
//!   never fragment, and reassembly is not worth its cost or risk);
//! * the IPv4 `total_length`, not the frame length, bounds the datagram,
//!   because Ethernet pads short frames to 60 bytes;
//! * optional IPv4 header and UDP checksum verification (a UDP checksum of 0
//!   means "absent" in IPv4);
//! * filtering by VLAN, destination address/port, and source address.
//!
//! Every rejection has a reason ([`FrameError`]) that can be counted with
//! [`FrameCounters`]. The parser never panics, never allocates, and has no
//! dependencies; all arithmetic is checked (enforced by a lint).

#![warn(missing_docs)]
#![deny(clippy::arithmetic_side_effects)]

use std::net::{Ipv4Addr, SocketAddrV4};

const ETH_HEADER_LEN: usize = 14;
const VLAN_TAG_LEN: usize = 4;
const IPV4_MIN_HEADER_LEN: usize = 20;
const UDP_HEADER_LEN: usize = 8;
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_VLAN: u16 = 0x8100;
const IP_PROTO_UDP: u8 = 17;

/// Tag protocol identifiers used for stacked VLANs: 802.1ad (`0x88A8`) and
/// the legacy pre-standard QinQ values (`0x9100`, `0x9200`).
const fn is_stacking_tpid(ethertype: u16) -> bool {
    matches!(ethertype, 0x88A8 | 0x9100 | 0x9200)
}

/// Whether to verify checksums in software.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChecksumPolicy {
    /// Verify the IPv4 header checksum and, when present, the UDP checksum.
    Verify,
    /// Skip verification: use when the NIC already verified them (check the
    /// backend's offload flags first) or for benchmarking.
    Skip,
}

/// A UDP datagram located inside an Ethernet frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpDatagram<'a> {
    /// Source address and port.
    pub src: SocketAddrV4,
    /// Destination address and port.
    pub dst: SocketAddrV4,
    /// 802.1Q VLAN id, if the frame was tagged.
    pub vlan: Option<u16>,
    /// UDP payload, borrowed from the frame.
    pub payload: &'a [u8],
}

/// Why a frame was not delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FrameError {
    /// Shorter than its headers (or IPv4 `total_length`) require.
    Truncated,
    /// Not IPv4.
    NotIpv4,
    /// Stacked VLAN tags (QinQ).
    UnsupportedVlan,
    /// IPv4 version, header length, or total length is invalid.
    BadIpHeader,
    /// An IPv4 fragment.
    Fragmented,
    /// Not UDP.
    NotUdp,
    /// UDP length field is invalid.
    BadUdpLength,
    /// IPv4 header checksum mismatch.
    BadIpChecksum,
    /// UDP checksum mismatch.
    BadUdpChecksum,
    /// VLAN does not match the filter.
    VlanMismatch,
    /// Destination address does not match the filter.
    DstIpMismatch,
    /// Destination port does not match the filter.
    DstPortMismatch,
    /// Source address is not allowed by the filter.
    SourceRejected,
}

impl FrameError {
    /// Number of variants.
    pub const COUNT: usize = 13;

    /// All variants, in index order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Truncated,
        Self::NotIpv4,
        Self::UnsupportedVlan,
        Self::BadIpHeader,
        Self::Fragmented,
        Self::NotUdp,
        Self::BadUdpLength,
        Self::BadIpChecksum,
        Self::BadUdpChecksum,
        Self::VlanMismatch,
        Self::DstIpMismatch,
        Self::DstPortMismatch,
        Self::SourceRejected,
    ];

    /// Dense index in `0..COUNT`.
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Whether the frame was damaged or undeliverable (counted as loss), as
    /// opposed to valid traffic for some other receiver.
    pub const fn is_corruption(self) -> bool {
        match self {
            Self::Truncated
            | Self::BadIpHeader
            | Self::Fragmented
            | Self::BadUdpLength
            | Self::BadIpChecksum
            | Self::BadUdpChecksum => true,
            Self::NotIpv4
            | Self::UnsupportedVlan
            | Self::NotUdp
            | Self::VlanMismatch
            | Self::DstIpMismatch
            | Self::DstPortMismatch
            | Self::SourceRejected => false,
        }
    }

    /// Stable snake_case name for logs and reports.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Truncated => "truncated",
            Self::NotIpv4 => "not_ipv4",
            Self::UnsupportedVlan => "unsupported_vlan",
            Self::BadIpHeader => "bad_ip_header",
            Self::Fragmented => "fragmented",
            Self::NotUdp => "not_udp",
            Self::BadUdpLength => "bad_udp_length",
            Self::BadIpChecksum => "bad_ip_checksum",
            Self::BadUdpChecksum => "bad_udp_checksum",
            Self::VlanMismatch => "vlan_mismatch",
            Self::DstIpMismatch => "dst_ip_mismatch",
            Self::DstPortMismatch => "dst_port_mismatch",
            Self::SourceRejected => "source_rejected",
        }
    }
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl std::error::Error for FrameError {}

#[inline]
fn be16(b: &[u8], at: usize) -> Option<u16> {
    let end = at.checked_add(2)?;
    let s = b.get(at..end)?;
    Some(u16::from_be_bytes([s[0], s[1]]))
}

#[inline]
fn ipv4(b: &[u8], at: usize) -> Option<Ipv4Addr> {
    let end = at.checked_add(4)?;
    let s = b.get(at..end)?;
    Some(Ipv4Addr::new(s[0], s[1], s[2], s[3]))
}

/// One's-complement sum of big-endian 16-bit words.
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

/// Folds a one's-complement sum; a valid checksummed region folds to 0xFFFF.
fn folds_to_all_ones(mut acc: u32) -> bool {
    while acc >> 16 != 0 {
        acc = (acc & 0xFFFF).wrapping_add(acc >> 16);
    }
    acc == 0xFFFF
}

/// Parses an Ethernet frame down to its UDP payload.
pub fn parse_udp(frame: &[u8], checksum: ChecksumPolicy) -> Result<UdpDatagram<'_>, FrameError> {
    // ---- Ethernet / 802.1Q ----
    let mut ethertype = be16(frame, 12).ok_or(FrameError::Truncated)?;
    let mut l3 = ETH_HEADER_LEN;
    let mut vlan = None;
    if is_stacking_tpid(ethertype) {
        return Err(FrameError::UnsupportedVlan);
    }
    if ethertype == ETHERTYPE_VLAN {
        let tci = be16(frame, 14).ok_or(FrameError::Truncated)?;
        vlan = Some(tci & 0x0FFF);
        ethertype = be16(frame, 16).ok_or(FrameError::Truncated)?;
        if ethertype == ETHERTYPE_VLAN || is_stacking_tpid(ethertype) {
            return Err(FrameError::UnsupportedVlan);
        }
        l3 = ETH_HEADER_LEN + VLAN_TAG_LEN;
    }
    if ethertype != ETHERTYPE_IPV4 {
        return Err(FrameError::NotIpv4);
    }

    // ---- IPv4 ----
    let ip_avail = frame.get(l3..).ok_or(FrameError::Truncated)?;
    let &vihl = ip_avail.first().ok_or(FrameError::Truncated)?;
    if ip_avail.len() < IPV4_MIN_HEADER_LEN {
        return Err(FrameError::Truncated);
    }
    let ihl = usize::from(vihl & 0x0F).wrapping_mul(4);
    if vihl >> 4 != 4 || ihl < IPV4_MIN_HEADER_LEN {
        return Err(FrameError::BadIpHeader);
    }
    let total_len = usize::from(be16(ip_avail, 2).ok_or(FrameError::Truncated)?);
    if total_len < ihl {
        return Err(FrameError::BadIpHeader);
    }
    // `total_length` (not the frame length, which may include padding)
    // bounds the datagram.
    let ip = ip_avail.get(..total_len).ok_or(FrameError::Truncated)?;
    let frag = be16(ip, 6).ok_or(FrameError::Truncated)?;
    if frag & 0x3FFF != 0 {
        // MF flag set or non-zero fragment offset.
        return Err(FrameError::Fragmented);
    }
    let &proto = ip.get(9).ok_or(FrameError::Truncated)?;
    if proto != IP_PROTO_UDP {
        return Err(FrameError::NotUdp);
    }
    let ip_header = ip.get(..ihl).ok_or(FrameError::Truncated)?;
    if checksum == ChecksumPolicy::Verify && !folds_to_all_ones(sum16(ip_header, 0)) {
        return Err(FrameError::BadIpChecksum);
    }
    let src_ip = ipv4(ip, 12).ok_or(FrameError::Truncated)?;
    let dst_ip = ipv4(ip, 16).ok_or(FrameError::Truncated)?;

    // ---- UDP ----
    let udp_avail = ip.get(ihl..).ok_or(FrameError::Truncated)?;
    if udp_avail.len() < UDP_HEADER_LEN {
        return Err(FrameError::Truncated);
    }
    let udp_len = usize::from(be16(udp_avail, 4).ok_or(FrameError::Truncated)?);
    if udp_len < UDP_HEADER_LEN || udp_len > udp_avail.len() {
        return Err(FrameError::BadUdpLength);
    }
    let udp = udp_avail.get(..udp_len).ok_or(FrameError::Truncated)?;
    let src_port = be16(udp, 0).ok_or(FrameError::Truncated)?;
    let dst_port = be16(udp, 2).ok_or(FrameError::Truncated)?;
    let udp_csum = be16(udp, 6).ok_or(FrameError::Truncated)?;
    if checksum == ChecksumPolicy::Verify && udp_csum != 0 {
        let mut acc = sum16(&src_ip.octets(), 0);
        acc = sum16(&dst_ip.octets(), acc);
        acc = acc.wrapping_add(u32::from(IP_PROTO_UDP));
        acc = acc.wrapping_add(udp_len as u32);
        if !folds_to_all_ones(sum16(udp, acc)) {
            return Err(FrameError::BadUdpChecksum);
        }
    }
    let payload = udp.get(UDP_HEADER_LEN..).ok_or(FrameError::Truncated)?;

    Ok(UdpDatagram {
        src: SocketAddrV4::new(src_ip, src_port),
        dst: SocketAddrV4::new(dst_ip, dst_port),
        vlan,
        payload,
    })
}

/// Which frames a backend should deliver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameFilter {
    /// Required destination UDP port.
    pub dst_port: u16,
    /// Required destination address (a unicast address or multicast group).
    pub dst_ip: Option<Ipv4Addr>,
    /// Required source address (e.g. the feed's publisher).
    pub src_ip: Option<Ipv4Addr>,
    /// Required VLAN id. `None` accepts untagged and single-tagged frames.
    pub vlan: Option<u16>,
    /// Checksum verification.
    pub checksum: ChecksumPolicy,
}

impl FrameFilter {
    /// Accepts any source and destination address on `dst_port`, verifying
    /// checksums.
    pub fn port(dst_port: u16) -> Self {
        Self {
            dst_port,
            dst_ip: None,
            src_ip: None,
            vlan: None,
            checksum: ChecksumPolicy::Verify,
        }
    }

    /// Parses `frame` and applies the filter.
    pub fn accept<'a>(&self, frame: &'a [u8]) -> Result<UdpDatagram<'a>, FrameError> {
        let d = parse_udp(frame, self.checksum)?;
        if self.vlan.is_some() && d.vlan != self.vlan {
            return Err(FrameError::VlanMismatch);
        }
        if self.dst_ip.is_some_and(|ip| ip != *d.dst.ip()) {
            return Err(FrameError::DstIpMismatch);
        }
        if d.dst.port() != self.dst_port {
            return Err(FrameError::DstPortMismatch);
        }
        if self.src_ip.is_some_and(|ip| ip != *d.src.ip()) {
            return Err(FrameError::SourceRejected);
        }
        Ok(d)
    }
}

/// Per-reason counts of filtered frames, so no drop goes uncounted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameCounters {
    /// Frames delivered.
    pub accepted: u64,
    /// Rejections, indexed by [`FrameError::index`].
    pub rejected: [u64; FrameError::COUNT],
}

impl FrameCounters {
    /// Counts one filter result.
    #[inline]
    pub fn record<T>(&mut self, result: &Result<T, FrameError>) {
        let slot = match result {
            Ok(_) => &mut self.accepted,
            Err(e) => match self.rejected.get_mut(e.index()) {
                Some(slot) => slot,
                None => return,
            },
        };
        *slot = slot.saturating_add(1);
    }

    /// Total rejections.
    pub fn total_rejected(&self) -> u64 {
        self.rejected
            .iter()
            .fold(0u64, |acc, &n| acc.saturating_add(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_indices_are_dense_and_names_unique() {
        for (i, e) in FrameError::ALL.iter().enumerate() {
            assert_eq!(e.index(), i);
            for other in &FrameError::ALL[i + 1..] {
                assert_ne!(e.name(), other.name());
            }
        }
    }

    #[test]
    fn corruption_classification() {
        let corrupt: Vec<_> = FrameError::ALL
            .iter()
            .filter(|e| e.is_corruption())
            .map(|e| e.name())
            .collect();
        assert_eq!(
            corrupt,
            [
                "truncated",
                "bad_ip_header",
                "fragmented",
                "bad_udp_length",
                "bad_ip_checksum",
                "bad_udp_checksum"
            ]
        );
    }

    #[test]
    fn counters_record_by_reason() {
        let mut c = FrameCounters::default();
        c.record(&Ok::<(), FrameError>(()));
        c.record(&Err::<(), _>(FrameError::Fragmented));
        c.record(&Err::<(), _>(FrameError::Fragmented));
        assert_eq!(c.accepted, 1);
        assert_eq!(c.rejected[FrameError::Fragmented.index()], 2);
        assert_eq!(c.total_rejected(), 2);
    }

    #[test]
    fn empty_and_tiny_frames_are_truncated() {
        for len in 0..14 {
            assert_eq!(
                parse_udp(&vec![0; len], ChecksumPolicy::Skip),
                Err(FrameError::Truncated)
            );
        }
    }
}
