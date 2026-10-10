//! Packet transports.
//!
//! A transport delivers raw datagrams and nothing else: no parsing, no
//! sequencing. Higher layers are generic over [`PacketSource`], so adding a
//! `recvmmsg` or AF_XDP backend later does not touch protocol or pipeline
//! code.

use std::io;
use std::time::Instant;

use crate::clock::wall_clock_ns;

mod udp;

pub use udp::{UdpConfig, UdpSource};

/// Receive-side metadata captured as soon as a datagram reaches user space.
///
/// Batched backends may capture one `RxMeta` per batch and pass the same
/// value for every datagram in it, since all of them became visible to user
/// space at the same call.
///
/// Marked `#[non_exhaustive]` so fields (e.g. a NIC hardware timestamp) can be
/// added without breaking backends in other crates; construct it with
/// [`RxMeta::now`].
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct RxMeta {
    /// Wall-clock receive time, nanoseconds since the Unix epoch.
    pub rx_wall_ns: u64,
    /// Monotonic receive time, for measuring in-process processing time.
    pub rx_instant: Instant,
}

impl RxMeta {
    /// Captures both receive clocks now.
    #[inline]
    pub fn now() -> Self {
        Self {
            rx_wall_ns: wall_clock_ns(),
            rx_instant: Instant::now(),
        }
    }
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
///   the callback returns, without copying;
/// * DPDK (planned), which can lend each mbuf's UDP payload in wire order and
///   bulk-free the burst after the last callback returns.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rx_meta_now_reads_both_clocks() {
        let before_wall = wall_clock_ns();
        let before = Instant::now();
        let meta = RxMeta::now();
        assert!(meta.rx_wall_ns >= before_wall);
        assert!(meta.rx_instant >= before);
        // 2020-01-01 in ns since the epoch: guards against a zeroed field.
        assert!(meta.rx_wall_ns > 1_577_836_800_000_000_000);
    }
}
