use crate::layout::{self, put};
use crate::{
    EncodeError, FLAG_END_OF_SESSION, MAGIC, MESSAGE_LEN, MarketEvent, PACKET_HEADER_LEN, VERSION,
};

/// Encodes one data packet into a caller-provided buffer without allocating.
///
/// Messages must be pushed in sequence order starting at `first_seq`; the
/// writer rejects any event whose `seq` does not follow.
///
/// ```
/// use caml_protocol::*;
///
/// let mut buf = [0u8; MAX_PAYLOAD_1500_MTU];
/// let mut w = PacketWriter::new(&mut buf, 7, 1).unwrap();
/// w.push(&MarketEvent {
///     msg_type: MessageType::Trade,
///     instrument_id: 42,
///     seq: 1,
///     event_time_ns: 0,
///     price: Price::from_raw(100 * PRICE_SCALE),
///     quantity: 10,
/// })
/// .unwrap();
/// let len = w.finish(123);
///
/// let view = parse_packet(&buf[..len]).unwrap();
/// assert_eq!(view.header().msg_count, 1);
/// assert_eq!(view.header().end_seq(), 2);
/// ```
#[derive(Debug)]
pub struct PacketWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
    msg_count: u16,
    first_seq: u64,
}

fn write_header(buf: &mut [u8], session_epoch: u64, first_seq: u64) -> Result<(), EncodeError> {
    if buf.len() < PACKET_HEADER_LEN {
        return Err(EncodeError::BufferFull {
            needed: PACKET_HEADER_LEN,
            available: buf.len(),
        });
    }
    if session_epoch == 0 {
        return Err(EncodeError::ZeroSessionEpoch);
    }
    if first_seq == 0 {
        return Err(EncodeError::InvalidSeq);
    }
    use layout::header as h;
    buf[..PACKET_HEADER_LEN].fill(0);
    put(buf, h::MAGIC, &MAGIC);
    buf[h::VERSION] = VERSION;
    put(buf, h::SESSION_EPOCH, &session_epoch.to_le_bytes());
    put(buf, h::FIRST_SEQ, &first_seq.to_le_bytes());
    Ok(())
}

impl<'a> PacketWriter<'a> {
    /// Starts a data packet whose first message will carry `first_seq`.
    /// `buf` must hold at least the packet header.
    pub fn new(buf: &'a mut [u8], session_epoch: u64, first_seq: u64) -> Result<Self, EncodeError> {
        write_header(buf, session_epoch, first_seq)?;
        Ok(Self {
            buf,
            len: PACKET_HEADER_LEN,
            msg_count: 0,
            first_seq,
        })
    }

    /// Number of messages written so far.
    pub fn msg_count(&self) -> u16 {
        self.msg_count
    }

    /// Sequence number the next pushed event must carry.
    pub fn next_seq(&self) -> u64 {
        self.first_seq.saturating_add(u64::from(self.msg_count))
    }

    /// Appends one message. Its `seq` must equal [`next_seq`](Self::next_seq).
    pub fn push(&mut self, event: &MarketEvent) -> Result<(), EncodeError> {
        let available = self.buf.len() - self.len;
        if available < MESSAGE_LEN {
            return Err(EncodeError::BufferFull {
                needed: MESSAGE_LEN,
                available,
            });
        }
        let Some(next_count) = self.msg_count.checked_add(1) else {
            return Err(EncodeError::TooManyMessages);
        };
        // The packet's end sequence (`first_seq + count`) must fit in u64.
        if self.first_seq.checked_add(u64::from(next_count)).is_none() {
            return Err(EncodeError::InvalidSeq);
        }
        let expected = self.next_seq();
        if event.seq != expected {
            return Err(EncodeError::SeqMismatch {
                expected,
                found: event.seq,
            });
        }

        use layout::message as m;
        let msg = &mut self.buf[self.len..self.len + MESSAGE_LEN];
        put(msg, m::LENGTH, &(MESSAGE_LEN as u16).to_le_bytes());
        msg[m::MSG_TYPE] = event.msg_type.to_wire();
        msg[m::FLAGS] = 0;
        put(msg, m::INSTRUMENT_ID, &event.instrument_id.to_le_bytes());
        put(msg, m::SEQ, &event.seq.to_le_bytes());
        put(msg, m::EVENT_TIME_NS, &event.event_time_ns.to_le_bytes());
        put(msg, m::PRICE, &event.price.raw().to_le_bytes());
        put(msg, m::QUANTITY, &event.quantity.to_le_bytes());

        self.len += MESSAGE_LEN;
        self.msg_count = next_count;
        Ok(())
    }

    /// Writes the message count and send timestamp; returns the packet length.
    ///
    /// Publishers that want the timestamp to reflect the moment of
    /// transmission should overwrite it with [`patch_send_time`] immediately
    /// before sending.
    pub fn finish(self, send_time_ns: u64) -> usize {
        use layout::header as h;
        put(self.buf, h::MSG_COUNT, &self.msg_count.to_le_bytes());
        put(self.buf, h::SEND_TIME_NS, &send_time_ns.to_le_bytes());
        self.len
    }
}

/// Encodes a heartbeat (a packet with no messages) and returns its length.
///
/// `next_seq` is the sequence number the publisher's next message will carry,
/// which lets a receiver detect loss of the final messages before an idle
/// period. Set `end_of_session` on the last packet a publisher sends.
pub fn write_heartbeat(
    buf: &mut [u8],
    session_epoch: u64,
    next_seq: u64,
    end_of_session: bool,
    send_time_ns: u64,
) -> Result<usize, EncodeError> {
    write_header(buf, session_epoch, next_seq)?;
    use layout::header as h;
    if end_of_session {
        buf[h::FLAGS] = FLAG_END_OF_SESSION;
    }
    put(buf, h::SEND_TIME_NS, &send_time_ns.to_le_bytes());
    Ok(PACKET_HEADER_LEN)
}

/// Overwrites the `send_time_ns` field of an already encoded packet.
///
/// Only the header length is checked; the rest of `packet` is untouched, so
/// this is safe to call on a packet that fault injection has corrupted.
pub fn patch_send_time(packet: &mut [u8], send_time_ns: u64) -> Result<(), EncodeError> {
    if packet.len() < PACKET_HEADER_LEN {
        return Err(EncodeError::BufferFull {
            needed: PACKET_HEADER_LEN,
            available: packet.len(),
        });
    }
    put(
        packet,
        layout::header::SEND_TIME_NS,
        &send_time_ns.to_le_bytes(),
    );
    Ok(())
}
