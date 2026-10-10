//! CAML market-data wire protocol, version 1.
//!
//! This crate defines the binary format exchanged between the synthetic feed
//! generator and the receiver. It performs no I/O and has no dependencies so
//! that it can be shared by every transport backend, test tool, and (later)
//! FPGA reference model.
//!
//! The normative specification lives in `docs/protocol.md`. In brief:
//!
//! * One UDP datagram carries one [packet](PacketHeader): a 32-byte header
//!   followed by `msg_count` fixed-size 40-byte messages.
//! * All integers are little-endian.
//! * Prices are signed fixed-point integers with [`PRICE_SCALE`] units per 1.0.
//!
//! Decoding is zero-copy: [`parse_packet`] validates the whole datagram up
//! front and returns a [`PacketView`] that borrows the receive buffer.

#![warn(missing_docs)]

mod decode;
mod encode;
mod error;
mod layout;

use std::fmt;

pub use decode::{Messages, PacketView, parse_packet};
pub use encode::PacketWriter;
pub use error::{DecodeError, DecodeErrorKind, EncodeError};

/// First two bytes of every packet: ASCII `"CM"`.
pub const MAGIC: [u8; 2] = *b"CM";

/// Protocol version implemented by this crate.
pub const VERSION: u8 = 1;

/// Size of the packet header in bytes.
pub const PACKET_HEADER_LEN: usize = 32;

/// Size of every version-1 message in bytes.
pub const MESSAGE_LEN: usize = 40;

/// Largest UDP payload that fits in one IPv4 frame on a 1500-byte MTU link
/// without fragmentation (1500 - 20 byte IPv4 header - 8 byte UDP header).
pub const MAX_PAYLOAD_1500_MTU: usize = 1472;

/// Number of fixed-point price units per 1.0 of currency.
pub const PRICE_SCALE: i64 = 1_000_000_000;

/// Maximum number of messages that fit in a packet of `max_payload` bytes.
pub const fn max_messages_for_payload(max_payload: usize) -> usize {
    if max_payload < PACKET_HEADER_LEN {
        0
    } else {
        (max_payload - PACKET_HEADER_LEN) / MESSAGE_LEN
    }
}

/// Total encoded size of a packet carrying `msg_count` messages.
pub const fn packet_len(msg_count: u16) -> usize {
    PACKET_HEADER_LEN + msg_count as usize * MESSAGE_LEN
}

/// Kind of market event carried by a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MessageType {
    /// An executed trade.
    Trade = 1,
    /// New best bid price and size.
    BidUpdate = 2,
    /// New best ask price and size.
    AskUpdate = 3,
}

impl MessageType {
    /// Decodes a wire value, returning `None` for values not defined in v1.
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Trade),
            2 => Some(Self::BidUpdate),
            3 => Some(Self::AskUpdate),
            _ => None,
        }
    }

    /// Wire value of this message type.
    pub const fn to_wire(self) -> u8 {
        self as u8
    }
}

/// Fixed-point price in units of `1 / PRICE_SCALE`.
///
/// Floating point is deliberately avoided: binary floats cannot represent most
/// decimal prices exactly, and comparisons/aggregations must be deterministic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(i64);

impl Price {
    /// Creates a price from raw fixed-point units.
    pub const fn from_raw(units: i64) -> Self {
        Self(units)
    }

    /// Raw fixed-point units.
    pub const fn raw(self) -> i64 {
        self.0
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        let scale = PRICE_SCALE.unsigned_abs();
        write!(f, "{sign}{}.{:09}", abs / scale, abs % scale)
    }
}

/// Decoded packet header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketHeader {
    /// Identifies one run of a feed publisher. A change means sequence numbers
    /// have restarted and receivers must reset their sequence state.
    pub session_id: u32,
    /// Per-session packet sequence number, incremented by one per datagram.
    pub packet_seq: u64,
    /// Publisher wall-clock time just before transmission, in nanoseconds
    /// since the Unix epoch.
    pub send_time_ns: u64,
    /// Number of messages following the header. Zero denotes a heartbeat.
    pub msg_count: u16,
}

/// One decoded market-data message.
///
/// This is a plain 40-byte `Copy` value: decoding into it costs a handful of
/// register loads, so there is no benefit to keeping it as a borrowed view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarketEvent {
    /// Event kind.
    pub msg_type: MessageType,
    /// Publisher-assigned instrument identifier.
    pub instrument_id: u32,
    /// Per-session message sequence number, contiguous across packets.
    pub seq: u64,
    /// Time the event occurred at the source, in nanoseconds since the Unix
    /// epoch.
    pub event_time_ns: u64,
    /// Event price.
    pub price: Price,
    /// Event quantity in integer lots.
    pub quantity: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_display_is_exact() {
        assert_eq!(
            Price::from_raw(123_450_000_000).to_string(),
            "123.450000000"
        );
        assert_eq!(Price::from_raw(-1).to_string(), "-0.000000001");
        assert_eq!(Price::from_raw(0).to_string(), "0.000000000");
        assert_eq!(
            Price::from_raw(i64::MIN).to_string(),
            "-9223372036.854775808"
        );
    }

    #[test]
    fn message_capacity_for_standard_mtu() {
        assert_eq!(max_messages_for_payload(MAX_PAYLOAD_1500_MTU), 36);
        assert!(packet_len(36) <= MAX_PAYLOAD_1500_MTU);
        assert!(packet_len(37) > MAX_PAYLOAD_1500_MTU);
        assert_eq!(max_messages_for_payload(10), 0);
    }

    #[test]
    fn message_type_round_trips() {
        for t in [
            MessageType::Trade,
            MessageType::BidUpdate,
            MessageType::AskUpdate,
        ] {
            assert_eq!(MessageType::from_wire(t.to_wire()), Some(t));
        }
        assert_eq!(MessageType::from_wire(0), None);
        assert_eq!(MessageType::from_wire(4), None);
    }
}
