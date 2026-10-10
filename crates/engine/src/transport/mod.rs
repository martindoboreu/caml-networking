//! Packet transports.
//!
//! A transport delivers raw datagrams and nothing else: no parsing, no
//! sequencing. Higher layers are generic over [`PacketSource`], so adding a
//! `recvmmsg` or AF_XDP backend later does not touch protocol or pipeline
//! code.

use std::io;
use std::time::Instant;

mod udp;

pub use udp::{UdpConfig, UdpSource};

/// Receive-side metadata captured as soon as a datagram reaches user space.
#[derive(Clone, Copy, Debug)]
pub struct RxMeta {
    /// Wall-clock receive time, nanoseconds since the Unix epoch.
    pub rx_wall_ns: u64,
    /// Monotonic receive time, for measuring in-process processing time.
    pub rx_instant: Instant,
}

/// A source of datagrams.
///
/// Datagrams are *lent* to a callback rather than copied into a caller
/// buffer. The slice passed to `on_packet` borrows transport-owned memory and
/// is only valid for the duration of the call; the compiler rejects any
/// attempt to keep it. This shape works unchanged for:
///
/// * socket backends, which lend their internal receive buffer;
/// * batched backends (`recvmmsg`), which call `on_packet` once per datagram
///   in the batch;
/// * AF_XDP, which can lend a UMEM frame and recycle it to the fill ring after
///   the callback returns, without copying.
pub trait PacketSource {
    /// Waits up to the backend's poll timeout for datagrams and passes each
    /// one to `on_packet`. Returns the number delivered; 0 means the timeout
    /// expired with nothing received.
    ///
    /// Returns `Err` only for errors the backend cannot recover from.
    fn poll<F>(&mut self, on_packet: F) -> io::Result<usize>
    where
        F: FnMut(&[u8], RxMeta);
}
