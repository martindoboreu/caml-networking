//! Wire-format tests: a golden byte fixture pins the exact layout, and the
//! remaining tests cover round-tripping and every rejection path.

#![allow(clippy::unwrap_used)]

use caml_protocol::*;

const GOLDEN_SESSION: u32 = 0x0102_0304;
const GOLDEN_SEND_TIME: u64 = 0x1122_3344_5566_7788;

#[rustfmt::skip]
const GOLDEN_PACKET: [u8; 72] = [
    // ---- packet header (32 bytes) ----
    0x43, 0x4D,                                     // magic "CM"
    0x01,                                           // version
    0x00,                                           // flags
    0x04, 0x03, 0x02, 0x01,                         // session_id
    0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // packet_seq = 5
    0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // send_time_ns
    0x01, 0x00,                                     // msg_count = 1
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00,             // reserved
    // ---- message 0 (40 bytes) ----
    0x28, 0x00,                                     // length = 40
    0x01,                                           // type = Trade
    0x00,                                           // flags
    0x2A, 0x00, 0x00, 0x00,                         // instrument_id = 42
    0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // seq = 9
    0xE8, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // event_time_ns = 1000
    0x00, 0x94, 0x35, 0x77, 0x00, 0x00, 0x00, 0x00, // price = 2.0 (2e9 units)
    0x2C, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // quantity = 300
];

fn golden_event() -> MarketEvent {
    MarketEvent {
        msg_type: MessageType::Trade,
        instrument_id: 42,
        seq: 9,
        event_time_ns: 1000,
        price: Price::from_raw(2 * PRICE_SCALE),
        quantity: 300,
    }
}

fn event(seq: u64) -> MarketEvent {
    MarketEvent {
        msg_type: MessageType::BidUpdate,
        instrument_id: 7,
        seq,
        event_time_ns: 1_700_000_000_000_000_000 + seq,
        price: Price::from_raw(-5 * PRICE_SCALE / 4),
        quantity: u64::MAX - seq,
    }
}

fn encode(session: u32, packet_seq: u64, events: &[MarketEvent]) -> Vec<u8> {
    let mut buf = vec![0u8; packet_len(events.len() as u16)];
    let mut w = PacketWriter::new(&mut buf, session, packet_seq).unwrap();
    for e in events {
        w.push(e).unwrap();
    }
    let len = w.finish(GOLDEN_SEND_TIME);
    buf.truncate(len);
    buf
}

#[test]
fn encoder_produces_golden_bytes() {
    let bytes = encode(GOLDEN_SESSION, 5, &[golden_event()]);
    assert_eq!(bytes, GOLDEN_PACKET);
}

#[test]
fn decoder_reads_golden_bytes() {
    let view = parse_packet(&GOLDEN_PACKET).unwrap();
    assert_eq!(
        *view.header(),
        PacketHeader {
            session_id: GOLDEN_SESSION,
            packet_seq: 5,
            send_time_ns: GOLDEN_SEND_TIME,
            msg_count: 1,
        }
    );
    let msgs: Vec<_> = view.messages().collect();
    assert_eq!(msgs, vec![golden_event()]);
}

#[test]
fn round_trip_full_mtu_packet() {
    let n = max_messages_for_payload(MAX_PAYLOAD_1500_MTU) as u64;
    let events: Vec<_> = (100..100 + n).map(event).collect();
    let bytes = encode(1, u64::MAX, &events);
    assert!(bytes.len() <= MAX_PAYLOAD_1500_MTU);

    let view = parse_packet(&bytes).unwrap();
    assert_eq!(view.header().packet_seq, u64::MAX);
    assert_eq!(view.messages().len(), events.len());
    assert!(view.messages().eq(events.iter().copied()));
}

#[test]
fn heartbeat_packet_has_no_messages() {
    let bytes = encode(1, 2, &[]);
    assert_eq!(bytes.len(), PACKET_HEADER_LEN);
    let view = parse_packet(&bytes).unwrap();
    assert_eq!(view.header().msg_count, 0);
    assert_eq!(view.messages().count(), 0);
}

#[test]
fn writer_rejects_overflowing_buffer() {
    let mut small = [0u8; PACKET_HEADER_LEN - 1];
    assert!(matches!(
        PacketWriter::new(&mut small, 0, 0),
        Err(EncodeError::BufferFull { .. })
    ));

    let mut buf = [0u8; PACKET_HEADER_LEN + MESSAGE_LEN + 10];
    let mut w = PacketWriter::new(&mut buf, 0, 0).unwrap();
    w.push(&event(1)).unwrap();
    assert_eq!(
        w.push(&event(2)),
        Err(EncodeError::BufferFull {
            needed: MESSAGE_LEN,
            available: 10
        })
    );
    assert_eq!(w.msg_count(), 1);
}

fn golden_with(f: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut b = GOLDEN_PACKET.to_vec();
    f(&mut b);
    b
}

#[test]
fn rejects_each_malformation() {
    let cases: Vec<(Vec<u8>, DecodeError)> = vec![
        (vec![], DecodeError::TooShort { len: 0 }),
        (
            GOLDEN_PACKET[..31].to_vec(),
            DecodeError::TooShort { len: 31 },
        ),
        (
            golden_with(|b| b[0] = b'X'),
            DecodeError::BadMagic { found: *b"XM" },
        ),
        (
            golden_with(|b| b[2] = 2),
            DecodeError::UnsupportedVersion { found: 2 },
        ),
        (golden_with(|b| b[3] = 1), DecodeError::ReservedNonZero),
        (golden_with(|b| b[31] = 1), DecodeError::ReservedNonZero),
        (
            golden_with(|b| b.truncate(71)),
            DecodeError::LengthMismatch {
                expected: 72,
                actual: 71,
            },
        ),
        (
            golden_with(|b| b.push(0)),
            DecodeError::LengthMismatch {
                expected: 72,
                actual: 73,
            },
        ),
        (
            golden_with(|b| b[24] = 2),
            DecodeError::LengthMismatch {
                expected: 112,
                actual: 72,
            },
        ),
        (
            golden_with(|b| b[32] = 39),
            DecodeError::BadMessageLength {
                index: 0,
                length: 39,
            },
        ),
        (
            golden_with(|b| b[34] = 0),
            DecodeError::UnknownMessageType {
                index: 0,
                msg_type: 0,
            },
        ),
        (
            golden_with(|b| b[35] = 0x80),
            DecodeError::MessageFlagsNonZero { index: 0 },
        ),
    ];
    for (bytes, expected) in cases {
        assert_eq!(parse_packet(&bytes).unwrap_err(), expected, "{expected}");
    }
}

#[test]
fn bad_later_message_rejects_whole_packet() {
    let mut bytes = encode(1, 1, &[event(1), event(2), event(3)]);
    let third_type = PACKET_HEADER_LEN + 2 * MESSAGE_LEN + 2;
    bytes[third_type] = 99;
    assert_eq!(
        parse_packet(&bytes).unwrap_err(),
        DecodeError::UnknownMessageType {
            index: 2,
            msg_type: 99
        }
    );
}

/// Deterministic SplitMix64 so fuzz-style tests are reproducible.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[test]
fn arbitrary_bytes_never_panic() {
    let mut rng = 0xC0FFEE;
    let valid = encode(9, 9, &[event(1), event(2), event(3), event(4)]);
    for _ in 0..50_000 {
        // Half random garbage, half single/multi-byte corruptions of a valid
        // packet (which exercise the deeper validation paths).
        let bytes = if splitmix(&mut rng).is_multiple_of(2) {
            let len = (splitmix(&mut rng) % 256) as usize;
            (0..len).map(|_| splitmix(&mut rng) as u8).collect()
        } else {
            let mut b = valid.clone();
            for _ in 0..1 + splitmix(&mut rng) % 3 {
                let i = (splitmix(&mut rng) as usize) % b.len();
                b[i] = splitmix(&mut rng) as u8;
            }
            b
        };
        if let Ok(view) = parse_packet(&bytes) {
            assert_eq!(
                view.messages().count(),
                usize::from(view.header().msg_count)
            );
        }
    }
}
