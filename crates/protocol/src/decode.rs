use crate::layout::{self, get_i64, get_u16, get_u32, get_u64};
use crate::{
    DecodeError, MAGIC, MESSAGE_LEN, MarketEvent, MessageType, PACKET_HEADER_LEN, PacketHeader,
    Price, VERSION, packet_len,
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

    /// Iterator over the packet's messages.
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
        // Every message was validated by `parse_packet`, so `decode_message`
        // cannot fail here; `.ok()` only exists to avoid a panic path.
        self.inner
            .next()
            .and_then(|raw| decode_message(0, raw).ok())
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
    let Some((hdr, body)) = buf.split_first_chunk::<PACKET_HEADER_LEN>() else {
        return Err(DecodeError::TooShort { len: buf.len() });
    };

    let magic = [hdr[layout::header::MAGIC], hdr[layout::header::MAGIC + 1]];
    if magic != MAGIC {
        return Err(DecodeError::BadMagic { found: magic });
    }
    let version = hdr[layout::header::VERSION];
    if version != VERSION {
        return Err(DecodeError::UnsupportedVersion { found: version });
    }
    if hdr[layout::header::FLAGS] != 0 || hdr[layout::header::RESERVED..].iter().any(|&b| b != 0) {
        return Err(DecodeError::ReservedNonZero);
    }

    let header = PacketHeader {
        session_id: get_u32(hdr, layout::header::SESSION_ID),
        packet_seq: get_u64(hdr, layout::header::PACKET_SEQ),
        send_time_ns: get_u64(hdr, layout::header::SEND_TIME_NS),
        msg_count: get_u16(hdr, layout::header::MSG_COUNT),
    };

    let expected = packet_len(header.msg_count);
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

    for (index, raw) in (0u16..).zip(messages) {
        decode_message(index, raw)?;
    }

    Ok(PacketView { header, messages })
}

#[inline]
fn decode_message(index: u16, raw: &[u8; MESSAGE_LEN]) -> Result<MarketEvent, DecodeError> {
    use layout::message as m;

    let length = get_u16(raw, m::LENGTH);
    if usize::from(length) != MESSAGE_LEN {
        return Err(DecodeError::BadMessageLength { index, length });
    }
    let type_byte = raw[m::MSG_TYPE];
    let Some(msg_type) = MessageType::from_wire(type_byte) else {
        return Err(DecodeError::UnknownMessageType {
            index,
            msg_type: type_byte,
        });
    };
    if raw[m::FLAGS] != 0 {
        return Err(DecodeError::MessageFlagsNonZero { index });
    }

    Ok(MarketEvent {
        msg_type,
        instrument_id: get_u32(raw, m::INSTRUMENT_ID),
        seq: get_u64(raw, m::SEQ),
        event_time_ns: get_u64(raw, m::EVENT_TIME_NS),
        price: Price::from_raw(get_i64(raw, m::PRICE)),
        quantity: get_u64(raw, m::QUANTITY),
    })
}
