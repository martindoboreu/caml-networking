//! Packet transports.
//!
//! A transport delivers raw datagrams and nothing else: no parsing, no
//! sequencing. Higher layers are generic over [`PacketSource`], so adding a
//! `recvmmsg` or AF_XDP backend later does not touch protocol or pipeline
//! code.

use std::io;
use std::time::Instant;

use crate::clock::wall_clock_ns;

#[cfg(target_os = "linux")]
mod procfs;
#[cfg(target_os = "linux")]
mod recvmmsg;
mod udp;

#[cfg(target_os = "linux")]
pub use recvmmsg::{BATCH, RecvMmsgSource, SLOT_LEN};
pub use udp::{MulticastGroup, UdpConfig, UdpSource};

/// Receive-side metadata captured as soon as a datagram reaches user space.
///
/// Batched backends may capture one `RxMeta` per batch and pass the same
/// value for every datagram in it, since all of them became visible to user
/// space at the same call.
///
/// Marked `#[non_exhaustive]` so fields can be added without breaking
/// backends in other crates; construct it with [`RxMeta::now`].
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct RxMeta {
    /// Wall-clock receive time, nanoseconds since the Unix epoch.
    pub rx_wall_ns: u64,
    /// Monotonic receive time, for measuring in-process processing time.
    pub rx_instant: Instant,
    /// Time the kernel (or NIC) stamped the datagram on arrival, in
    /// nanoseconds since the Unix epoch, if the backend provides it (Linux
    /// `SO_TIMESTAMPNS`, later `SO_TIMESTAMPING` or an AF_XDP/DPDK hardware
    /// stamp). The gap to `rx_wall_ns` is socket queueing plus wakeup.
    pub rx_kernel_ns: Option<u64>,
}

impl RxMeta {
    /// Captures both receive clocks now.
    #[inline]
    pub fn now() -> Self {
        Self {
            rx_wall_ns: wall_clock_ns(),
            rx_instant: Instant::now(),
            rx_kernel_ns: None,
        }
    }
}

/// Counters a transport keeps about datagrams it did **not** deliver, or
/// could not see. Every drop below the protocol layer is counted here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TransportStats {
    /// Datagrams received from the OS (or NIC), including filtered ones.
    pub datagrams: u64,
    /// Datagrams dropped because their source is not allowed.
    pub filtered_source: u64,
    /// Datagrams dropped because they exceeded the receive buffer.
    pub truncated: u64,
    /// Transient receive errors that were counted and survived (e.g. an ICMP
    /// error reported on a UDP socket).
    pub transient_errors: u64,
    /// Datagrams the kernel dropped for this socket before we could read
    /// them (mostly receive-buffer overflow), if the platform exposes it.
    /// Linux socket backends read the kernel's `sk_drops` counter from
    /// `/proc/net/udp`. `None` means "not observable here", not zero.
    pub kernel_drops: Option<u64>,
}

impl TransportStats {
    /// Datagrams the transport knows it did not deliver.
    pub fn dropped(&self) -> u64 {
        self.filtered_source
            .saturating_add(self.truncated)
            .saturating_add(self.kernel_drops.unwrap_or(0))
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
    /// one to `on_packet`. Returns the number delivered to `on_packet`; 0
    /// means none (the timeout expired, or everything received was dropped
    /// and counted in [`stats`](Self::stats)).
    ///
    /// Returns `Err` only for errors the backend cannot recover from.
    fn poll<F>(&mut self, on_packet: F) -> io::Result<usize>
    where
        F: FnMut(&[u8], RxMeta);

    /// Transport-level counters.
    ///
    /// May read the OS (e.g. `/proc` on Linux) and allocate: call it from
    /// reporting paths, not per packet.
    fn stats(&self) -> TransportStats;
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

    #[test]
    fn dropped_sums_known_drops() {
        let mut s = TransportStats {
            filtered_source: 2,
            truncated: 3,
            ..TransportStats::default()
        };
        assert_eq!(s.dropped(), 5);
        s.kernel_drops = Some(10);
        assert_eq!(s.dropped(), 15);
    }
}
