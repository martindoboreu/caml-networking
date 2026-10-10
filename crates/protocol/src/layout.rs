//! Byte offsets of every field. See `docs/protocol.md` for the normative table.

pub(crate) mod header {
    pub(crate) const MAGIC: usize = 0;
    pub(crate) const VERSION: usize = 2;
    pub(crate) const FLAGS: usize = 3;
    pub(crate) const SESSION_ID: usize = 4;
    pub(crate) const PACKET_SEQ: usize = 8;
    pub(crate) const SEND_TIME_NS: usize = 16;
    pub(crate) const MSG_COUNT: usize = 24;
    pub(crate) const RESERVED: usize = 26;
}

pub(crate) mod message {
    pub(crate) const LENGTH: usize = 0;
    pub(crate) const MSG_TYPE: usize = 2;
    pub(crate) const FLAGS: usize = 3;
    pub(crate) const INSTRUMENT_ID: usize = 4;
    pub(crate) const SEQ: usize = 8;
    pub(crate) const EVENT_TIME_NS: usize = 16;
    pub(crate) const PRICE: usize = 24;
    pub(crate) const QUANTITY: usize = 32;
}

/// Reads `N` bytes starting at `at`. Panics if out of bounds, so callers must
/// only use it on buffers whose length has already been validated.
#[inline(always)]
fn array<const N: usize>(buf: &[u8], at: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&buf[at..at + N]);
    out
}

#[inline(always)]
pub(crate) fn get_u16(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(array(buf, at))
}

#[inline(always)]
pub(crate) fn get_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(array(buf, at))
}

#[inline(always)]
pub(crate) fn get_u64(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(array(buf, at))
}

#[inline(always)]
pub(crate) fn get_i64(buf: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(array(buf, at))
}

#[inline(always)]
pub(crate) fn put(buf: &mut [u8], at: usize, bytes: &[u8]) {
    buf[at..at + bytes.len()].copy_from_slice(bytes);
}
