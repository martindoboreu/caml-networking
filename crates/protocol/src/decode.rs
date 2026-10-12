use crate::layout::{self, get_i64, get_u16, get_u32, get_u64};
use crate::{
    DecodeError, FLAG_END_OF_SESSION, MAGIC, MESSAGE_LEN, MarketEvent, MessageType,
    PACKET_HEADER_LEN, PacketHeader, Price, VERSION, packet_len,
};

/// A validated packet borrowing the datagram it was parsed from.
///
/// The lifetime `'a` ties the view to the receive buffer: the borrow checker
/// guarantees the buffer cannot be overwritten (e.g. by the next `recv`) while
/// any view or message iterator derived from it is alive.
#[derive(Clone, Copy, Debug)]
pub struct PacketView<'a> {
    header: PacketHeader,
    messages: &'a [[u8; MESSAGE_LEN]],
}

impl<'a> PacketView<'a> {
    /// Decoded packet header.
    pub fn header(&self) -> &PacketHeader {
        &self.header
    }

    /// Iterator over the packet's messages, in sequence order.
    pub fn messages(&self) -> Messages<'a> {
        Messages {
            inner: self.messages.iter(),
        }
    }
}

/// Iterator over the messages of a [`PacketView`].
#[derive(Clone, Debug)]
pub struct Messages<'a> {
    inner: std::slice::Iter<'a, [u8; MESSAGE_LEN]>,
}

impl Iterator for Messages<'_> {
    type Item = MarketEvent;

    #[inline]
    fn next(&mut self) -> Option<MarketEvent> {
        let raw = self.inner.next()?;
        let event = read_message(raw);
        // `parse_packet` validated every message, so this cannot be `None`.
        debug_assert!(event.is_some(), "message validated by parse_packet");
        event
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl ExactSizeIterator for Messages<'_> {}

/// Validates an entire datagram and returns a zero-copy view of it.
///
/// Validation is all-or-nothing: the header, the total length, and every
/// message are checked before the view is returned, so downstream code never
/// observes a partially valid packet.
pub fn parse_packet(buf: &[u8]) -> Result<PacketView<'_>, DecodeError> {
    use layout::header as h;

    let Some((hdr, body)) = buf.split_first_chunk::<PACKET_HEADER_LEN>() else {
        return Err(DecodeError::TooShort { len: buf.len() });
    };

    let magic = [hdr[h::MAGIC], hdr[h::MAGIC + 1]];
    if magic != MAGIC {
        return Err(DecodeError::BadMagic { found: magic });
    }
    let version = hdr[h::VERSION];
    if version != VERSION {
        return Err(DecodeError::UnsupportedVersion { found: version });
    }
    let flags = hdr[h::FLAGS];
    if flags & !FLAG_END_OF_SESSION != 0 {
        return Err(DecodeError::InvalidFlags { flags });
    }
    if hdr[h::RESERVED..h::RESERVED + h::RESERVED_LEN]
        .iter()
        .any(|&b| b != 0)
    {
        return Err(DecodeError::ReservedNonZero);
    }

    let msg_count = get_u16(hdr, h::MSG_COUNT);
    let end_of_session = flags & FLAG_END_OF_SESSION != 0;
    if end_of_session && msg_count != 0 {
        return Err(DecodeError::InvalidFlags { flags });
    }
    let session_epoch = get_u64(hdr, h::SESSION_EPOCH);
    if session_epoch == 0 {
        return Err(DecodeError::ZeroSessionEpoch);
    }
    let first_seq = get_u64(hdr, h::FIRST_SEQ);
    if first_seq == 0 || first_seq.checked_add(u64::from(msg_count)).is_none() {
        return Err(DecodeError::InvalidSeqRange {
            first_seq,
            msg_count,
        });
    }

    let header = PacketHeader {
        session_epoch,
        first_seq,
        send_time_ns: get_u64(hdr, h::SEND_TIME_NS),
        msg_count,
        end_of_session,
    };

    let expected = packet_len(msg_count);
    if buf.len() != expected {
        return Err(DecodeError::LengthMismatch {
            expected,
            actual: buf.len(),
        });
    }

    let (messages, remainder) = body.as_chunks::<MESSAGE_LEN>();
    debug_assert!(
        remainder.is_empty(),
        "length check guarantees whole messages"
    );

    // `first_seq + msg_count` was checked above, so `expected_seq` cannot
    // overflow for any index below `msg_count`.
    let mut expected_seq = first_seq;
    for (index, raw) in (0u16..).zip(messages) {
        validate_message(index, raw, expected_seq)?;
        expected_seq = expected_seq.wrapping_add(1);
    }

    Ok(PacketView { header, messages })
}

#[inline]
fn validate_message(
    index: u16,
    raw: &[u8; MESSAGE_LEN],
    expected_seq: u64,
) -> Result<(), DecodeError> {
    use layout::message as m;

    let length = get_u16(raw, m::LENGTH);
    if usize::from(length) != MESSAGE_LEN {
        return Err(DecodeError::BadMessageLength { index, length });
    }
    let type_byte = raw[m::MSG_TYPE];
    if MessageType::from_wire(type_byte).is_none() {
        return Err(DecodeError::UnknownMessageType {
            index,
            msg_type: type_byte,
        });
    }
    if raw[m::FLAGS] != 0 {
        return Err(DecodeError::MessageFlagsNonZero { index });
    }
    let seq = get_u64(raw, m::SEQ);
    if seq != expected_seq {
        return Err(DecodeError::MessageSeqMismatch {
            index,
            expected: expected_seq,
            found: seq,
        });
    }
    Ok(())
}

/// Reads a message that `validate_message` has accepted. Returns `None` only
/// for an unknown type, which validation rules out.
#[inline]
fn read_message(raw: &[u8; MESSAGE_LEN]) -> Option<MarketEvent> {
    use layout::message as m;

    Some(MarketEvent {
        msg_type: MessageType::from_wire(raw[m::MSG_TYPE])?,
        instrument_id: get_u32(raw, m::INSTRUMENT_ID),
        seq: get_u64(raw, m::SEQ),
        event_time_ns: get_u64(raw, m::EVENT_TIME_NS),
        price: Price::from_raw(get_i64(raw, m::PRICE)),
        quantity: get_u64(raw, m::QUANTITY),
    })
}
