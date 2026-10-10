use crate::layout::{self, put};
use crate::{EncodeError, MAGIC, MESSAGE_LEN, MarketEvent, PACKET_HEADER_LEN, VERSION};

/// Encodes one packet into a caller-provided buffer without allocating.
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
/// ```
#[derive(Debug)]
pub struct PacketWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
    msg_count: u16,
}

impl<'a> PacketWriter<'a> {
    /// Starts a packet. `buf` must hold at least the packet header.
    pub fn new(buf: &'a mut [u8], session_id: u32, packet_seq: u64) -> Result<Self, EncodeError> {
        if buf.len() < PACKET_HEADER_LEN {
            return Err(EncodeError::BufferFull {
                needed: PACKET_HEADER_LEN,
                available: buf.len(),
            });
        }
        use layout::header as h;
        buf[..PACKET_HEADER_LEN].fill(0);
        put(buf, h::MAGIC, &MAGIC);
        buf[h::VERSION] = VERSION;
        put(buf, h::SESSION_ID, &session_id.to_le_bytes());
        put(buf, h::PACKET_SEQ, &packet_seq.to_le_bytes());
        Ok(Self {
            buf,
            len: PACKET_HEADER_LEN,
            msg_count: 0,
        })
    }

    /// Number of messages written so far.
    pub fn msg_count(&self) -> u16 {
        self.msg_count
    }

    /// Appends one message.
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
    /// The timestamp is taken as late as possible so that it reflects the
    /// moment of transmission rather than the moment encoding started.
    pub fn finish(self, send_time_ns: u64) -> usize {
        use layout::header as h;
        put(self.buf, h::MSG_COUNT, &self.msg_count.to_le_bytes());
        put(self.buf, h::SEND_TIME_NS, &send_time_ns.to_le_bytes());
        self.len
    }
}
