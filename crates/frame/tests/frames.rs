//! Frame-parser tests: round trips through `caml-sim`'s frame builder, every
//! rejection reason, checksum policy, filtering, a no-panic fuzz, and a
//! differential comparison with the independent `etherparse` crate.

#![allow(clippy::unwrap_used)]

use std::net::{Ipv4Addr, SocketAddrV4};

use caml_frame::{ChecksumPolicy, FrameError, FrameFilter, parse_udp};
use caml_sim::SplitMix64;
use caml_sim::pcap::{UdpFrameSpec, build_udp_frame};

fn spec() -> UdpFrameSpec {
    UdpFrameSpec::new(
        SocketAddrV4::new(Ipv4Addr::new(10, 1, 2, 3), 50_000),
        SocketAddrV4::new(Ipv4Addr::new(239, 7, 7, 7), 40_000),
    )
}

fn frame(spec: &UdpFrameSpec, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::new();
    build_udp_frame(spec, payload, &mut f).unwrap();
    f
}

#[test]
fn round_trip_untagged_tagged_and_padded() {
    for vlan in [None, Some(0), Some(100), Some(4095)] {
        for len in [0usize, 1, 5, 17, 18, 100, 1472, 9000] {
            let mut s = spec();
            s.vlan = vlan;
            let payload: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let f = frame(&s, &payload);
            let d = parse_udp(&f, ChecksumPolicy::Verify).unwrap();
            assert_eq!(d.payload, payload.as_slice(), "vlan={vlan:?} len={len}");
            assert_eq!(d.src, s.src);
            assert_eq!(d.dst, s.dst);
            assert_eq!(d.vlan, vlan);
        }
    }
}

#[test]
fn ethernet_padding_is_not_payload() {
    let f = frame(&spec(), &[1, 2, 3]);
    assert_eq!(f.len(), 60, "builder pads to the Ethernet minimum");
    let d = parse_udp(&f, ChecksumPolicy::Verify).unwrap();
    assert_eq!(d.payload, &[1, 2, 3]);
}

fn with(f: &[u8], edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut v = f.to_vec();
    edit(&mut v);
    v
}

#[test]
fn every_rejection_reason() {
    let f = frame(&spec(), &[0xAA; 40]);
    let ip = 14;
    let udp = 34;
    let cases: Vec<(Vec<u8>, FrameError)> = vec![
        (f[..13].to_vec(), FrameError::Truncated),
        (f[..30].to_vec(), FrameError::Truncated),
        (
            with(&f, |b| b[12..14].copy_from_slice(&[0x86, 0xDD])),
            FrameError::NotIpv4,
        ),
        (
            with(&f, |b| b[12..14].copy_from_slice(&[0x88, 0xA8])),
            FrameError::UnsupportedVlan,
        ),
        (with(&f, |b| b[ip] = 0x65), FrameError::BadIpHeader), // version 6
        (with(&f, |b| b[ip] = 0x44), FrameError::BadIpHeader), // IHL 16 bytes
        (
            with(&f, |b| b[ip + 2..ip + 4].copy_from_slice(&[0, 10])),
            FrameError::BadIpHeader,
        ),
        (
            with(&f, |b| b[ip + 2..ip + 4].copy_from_slice(&[0x05, 0xDC])),
            FrameError::Truncated,
        ),
        (with(&f, |b| b[ip + 6] = 0x20), FrameError::Fragmented), // MF
        (with(&f, |b| b[ip + 7] = 0x01), FrameError::Fragmented), // offset
        (with(&f, |b| b[ip + 9] = 6), FrameError::NotUdp),
        (
            with(&f, |b| b[udp + 4..udp + 6].copy_from_slice(&[0, 7])),
            FrameError::BadUdpLength,
        ),
        (
            with(&f, |b| b[udp + 4..udp + 6].copy_from_slice(&[0xFF, 0xFF])),
            FrameError::BadUdpLength,
        ),
        (with(&f, |b| b[ip + 8] ^= 1), FrameError::BadIpChecksum), // TTL changed
        (with(&f, |b| b[udp + 8] ^= 1), FrameError::BadUdpChecksum), // payload changed
    ];
    for (bytes, expected) in cases {
        assert_eq!(
            parse_udp(&bytes, ChecksumPolicy::Verify),
            Err(expected),
            "{expected}"
        );
    }

    // QinQ inside an 802.1Q tag.
    let mut s = spec();
    s.vlan = Some(5);
    let tagged = frame(&s, &[1]);
    let stacked = with(&tagged, |b| b[16..18].copy_from_slice(&[0x81, 0x00]));
    assert_eq!(
        parse_udp(&stacked, ChecksumPolicy::Skip),
        Err(FrameError::UnsupportedVlan)
    );
}

#[test]
fn checksum_policy() {
    let f = frame(&spec(), &[0xAA; 40]);
    let corrupted = with(&f, |b| b[42] ^= 0xFF);
    assert_eq!(
        parse_udp(&corrupted, ChecksumPolicy::Verify),
        Err(FrameError::BadUdpChecksum)
    );
    assert!(parse_udp(&corrupted, ChecksumPolicy::Skip).is_ok());
    // A UDP checksum of 0 means "absent" in IPv4 and is accepted.
    let absent = with(&corrupted, |b| b[40..42].fill(0));
    assert!(parse_udp(&absent, ChecksumPolicy::Verify).is_ok());
}

#[test]
fn filter_checks_each_field() {
    let mut s = spec();
    s.vlan = Some(7);
    let f = frame(&s, &[1, 2, 3]);
    let base = FrameFilter {
        dst_ip: Some(*s.dst.ip()),
        src_ip: Some(*s.src.ip()),
        vlan: Some(7),
        ..FrameFilter::port(s.dst.port())
    };
    assert_eq!(base.accept(&f).unwrap().payload, &[1, 2, 3]);
    let cases = [
        (
            FrameFilter {
                vlan: Some(8),
                ..base
            },
            FrameError::VlanMismatch,
        ),
        (
            FrameFilter {
                dst_ip: Some(Ipv4Addr::new(239, 0, 0, 1)),
                ..base
            },
            FrameError::DstIpMismatch,
        ),
        (
            FrameFilter {
                dst_port: 1,
                ..base
            },
            FrameError::DstPortMismatch,
        ),
        (
            FrameFilter {
                src_ip: Some(Ipv4Addr::new(10, 9, 9, 9)),
                ..base
            },
            FrameError::SourceRejected,
        ),
    ];
    for (filter, expected) in cases {
        assert_eq!(filter.accept(&f), Err(expected));
    }
}

fn random_frame(rng: &mut SplitMix64) -> Vec<u8> {
    let mut s = UdpFrameSpec::new(
        SocketAddrV4::new(Ipv4Addr::from(rng.next_u64() as u32), rng.next_u64() as u16),
        SocketAddrV4::new(Ipv4Addr::from(rng.next_u64() as u32), rng.next_u64() as u16),
    );
    if rng.below(2) == 0 {
        s.vlan = Some(rng.below(4096) as u16);
    }
    let len = rng.below(200) as usize;
    let payload: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
    let mut f = frame(&s, &payload);
    // Mutate 0-3 bytes in the headers, sometimes truncate or extend.
    for _ in 0..rng.below(4) {
        let i = rng.below(f.len().min(64) as u64) as usize;
        f[i] = rng.next_u64() as u8;
    }
    match rng.below(10) {
        0 => {
            let keep = rng.below(f.len() as u64 + 1) as usize;
            f.truncate(keep);
        }
        1 => f.extend((0..rng.below(16)).map(|_| rng.next_u64() as u8)),
        _ => {}
    }
    f
}

#[test]
fn arbitrary_frames_never_panic() {
    let mut rng = SplitMix64::new(0xF4A3E);
    for _ in 0..100_000 {
        let f = if rng.below(4) == 0 {
            (0..rng.below(128)).map(|_| rng.next_u64() as u8).collect()
        } else {
            random_frame(&mut rng)
        };
        for policy in [ChecksumPolicy::Verify, ChecksumPolicy::Skip] {
            if let Ok(d) = parse_udp(&f, policy) {
                // The payload is a subslice of the frame.
                let start = d.payload.as_ptr() as usize - f.as_ptr() as usize;
                assert!(start + d.payload.len() <= f.len());
            }
        }
    }
}

/// What etherparse says the frame contains, restricted to the cases our
/// parser is meant to accept: IPv4, not fragmented, UDP, at most one VLAN.
fn etherparse_udp(f: &[u8]) -> Option<(SocketAddrV4, SocketAddrV4, Option<u16>, &[u8])> {
    use etherparse::{NetSlice, SlicedPacket, TransportSlice};
    // Deliberate difference: we accept only the 802.1Q TPID (0x8100) and
    // reject 802.1ad / legacy QinQ TPIDs even as a single tag; etherparse
    // accepts them.
    let outer_tpid = u16::from_be_bytes([*f.get(12)?, *f.get(13)?]);
    if matches!(outer_tpid, 0x88A8 | 0x9100 | 0x9200) {
        return None;
    }
    let s = SlicedPacket::from_ethernet(f).ok()?;
    let vlans = s.vlan_ids();
    if vlans.len() > 1 {
        return None;
    }
    let Some(NetSlice::Ipv4(ip)) = &s.net else {
        return None;
    };
    if ip.header().is_fragmenting_payload() {
        return None;
    }
    let Some(TransportSlice::Udp(udp)) = &s.transport else {
        return None;
    };
    // Deliberate difference: a UDP length of 0 is only meaningful for IPv6
    // jumbograms (RFC 2675); over IPv4 we reject it, etherparse is lax.
    if udp.length() == 0 {
        return None;
    }
    Some((
        SocketAddrV4::new(ip.header().source_addr(), udp.source_port()),
        SocketAddrV4::new(ip.header().destination_addr(), udp.destination_port()),
        vlans.first().map(|v| v.value()),
        udp.payload(),
    ))
}

#[test]
fn differential_against_etherparse() {
    let mut rng = SplitMix64::new(0xD1FF);
    let mut agree_accept = 0;
    let mut agree_reject = 0;
    for _ in 0..100_000 {
        let f = random_frame(&mut rng);
        // etherparse does not verify checksums while slicing, so compare
        // structure only.
        let ours = parse_udp(&f, ChecksumPolicy::Skip)
            .ok()
            .map(|d| (d.src, d.dst, d.vlan, d.payload));
        let theirs = etherparse_udp(&f);
        match (&ours, &theirs) {
            (Some(_), Some(_)) => {
                assert_eq!(ours, theirs, "frame {f:02x?}");
                agree_accept += 1;
            }
            (None, None) => agree_reject += 1,
            _ => panic!(
                "parsers disagree on frame {f:02x?}\n ours:   {:?}\n theirs: {theirs:?}",
                parse_udp(&f, ChecksumPolicy::Skip)
            ),
        }
    }
    // Both outcomes were exercised substantially.
    assert!(agree_accept > 10_000, "accepted {agree_accept}");
    assert!(agree_reject > 10_000, "rejected {agree_reject}");
}
