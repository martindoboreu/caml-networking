//! Linux btched UDP backend: one `recvmmsg` system call returns up to
//! [`BATCH`] datagrams.
//!
//! Compared with [`UdpSource`](super::UdpSource) it adds two things the
//! portable backend cannot observe:
//!
//! * **Kernel drops.** `SO_RXQ_OVFL` makes the kernel attach the socket's
//!   cumulative drop counter (datagrams discarded because the receive buffer
//!   was full) to received datagrams, reported as
//!   [`TransportStats::kernel_drops`]. The counter only travels with a
//!   delivered datagram, so drops after the last one received are reported
//!   when the next one arrives.
//! * **Kernel receive timestamps.** `SO_TIMESTAMPNS` stamps each datagram
//!   when the kernel received it, reported as [`RxMeta::rx_kernel_ns`]. The
//!   gap to the user-space stamp is socket queueing plus wakeup.
//!
//! Uses `nix`'s safe wrappers; this module contains no `unsafe`. All buffers
//! and message headers are allocated once in [`RecvMmsgSource::bind`]; `poll`
//! does not allocate.

use std::io::{self, IoSliceMut};
use std::net::{IpAddr, SocketAddr};
use std::os::fd::AsRawFd;

use nix::errno::Errno;
use nix::sys::socket::{
    self, ControlMessageOwned, MsgFlags, MultiHeaders, SockaddrStorage, setsockopt, sockopt,
};
use nix::sys::time::TimeSpec;
use socket2::Socket;

use super::udp::{open_socket, source_allowed};
use super::{PacketSource, RxMeta, TransportStats, UdpConfig};

/// Datagrams requested per `recvmmsg` call.
pub const BATCH: usize = 32;

/// Bytes per receive slot. Larger datagrams are dropped and counted as
/// [`TransportStats::truncated`]; market-data packets fit a 1500-byte MTU.
pub const SLOT_LEN: usize = 2048;

/// Batched receiver using `recvmmsg(2)`.
#[derive(Debug)]
pub struct RecvMmsgSource {
    socket: Socket,
    slots: Box<[[u8; SLOT_LEN]]>,
    headers: MultiHeaders<SockaddrStorage>,
    allowed_sources: Box<[IpAddr]>,
    granted_recv_buffer: usize,
    stats: TransportStats,
}

fn timespec_ns(ts: &TimeSpec) -> Option<u64> {
    let secs = u64::try_from(ts.tv_sec()).ok()?;
    let nanos = u64::try_from(ts.tv_nsec()).ok()?;
    secs.checked_mul(1_000_000_000)?.checked_add(nanos)
}

fn source_ip(addr: &SockaddrStorage) -> Option<IpAddr> {
    if let Some(v4) = addr.as_sockaddr_in() {
        return Some(IpAddr::V4(v4.ip()));
    }
    addr.as_sockaddr_in6().map(|v6| IpAddr::V6(v6.ip()))
}

impl RecvMmsgSource {
    /// Creates and binds the socket (same configuration as
    /// [`UdpSource`](super::UdpSource)), enables drop reporting and kernel
    /// timestamps, and allocates every buffer.
    pub fn bind(config: &UdpConfig) -> io::Result<Self> {
        let (socket, granted_recv_buffer) = open_socket(config)?;
        setsockopt(&socket, sockopt::RxqOvfl, &1)?;
        setsockopt(&socket, sockopt::ReceiveTimestampns, &true)?;
        Ok(Self {
            socket,
            slots: vec![[0u8; SLOT_LEN]; BATCH].into_boxed_slice(),
            headers: MultiHeaders::preallocate(BATCH, Some(nix::cmsg_space!(TimeSpec, u32))),
            allowed_sources: config.allowed_sources.clone().into_boxed_slice(),
            granted_recv_buffer,
            stats: TransportStats {
                kernel_drops: Some(0),
                ..TransportStats::default()
            },
        })
    }

    /// Address actually bound.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket
            .local_addr()?
            .as_socket()
            .ok_or_else(|| io::Error::other("bound address is not an IP socket address"))
    }

    /// `SO_RCVBUF` as reported by the kernel after configuration.
    pub fn granted_recv_buffer(&self) -> usize {
        self.granted_recv_buffer
    }
}

impl PacketSource for RecvMmsgSource {
    fn poll<F>(&mut self, mut on_packet: F) -> io::Result<usize>
    where
        F: FnMut(&[u8], RxMeta),
    {
        let Self {
            socket,
            slots,
            headers,
            allowed_sources,
            stats,
            ..
        } = self;

        // One iovec per slot, built on the stack: no allocation.
        let mut free = slots.iter_mut();
        let mut iovs: [[IoSliceMut<'_>; 1]; BATCH] = std::array::from_fn(|_| {
            [IoSliceMut::new(
                free.next()
                    .map_or_else(Default::default, |s| s.as_mut_slice()),
            )]
        });

        // MSG_WAITFORONE: block (up to SO_RCVTIMEO) for the first datagram,
        // then take whatever else is already queued without waiting.
        let results = match socket::recvmmsg(
            socket.as_raw_fd(),
            headers,
            iovs.iter_mut(),
            MsgFlags::MSG_WAITFORONE,
            None,
        ) {
            Ok(r) => r,
            Err(Errno::EAGAIN | Errno::EINTR) => return Ok(0),
            Err(Errno::ECONNREFUSED) => {
                stats.transient_errors = stats.transient_errors.saturating_add(1);
                return Ok(0);
            }
            Err(e) => return Err(e.into()),
        };

        // Every datagram in the batch reached user space at this call.
        let batch_meta = RxMeta::now();
        let mut delivered = 0;
        for msg in results {
            stats.datagrams = stats.datagrams.saturating_add(1);
            let mut meta = batch_meta;
            if let Ok(cmsgs) = msg.cmsgs() {
                for cmsg in cmsgs {
                    match cmsg {
                        ControlMessageOwned::ScmTimestampns(ts) => {
                            meta.rx_kernel_ns = timespec_ns(&ts);
                        }
                        ControlMessageOwned::RxqOvfl(dropped) => {
                            let dropped = u64::from(dropped);
                            stats.kernel_drops =
                                Some(stats.kernel_drops.map_or(dropped, |k| k.max(dropped)));
                        }
                        _ => {}
                    }
                }
            }
            if msg.flags.contains(MsgFlags::MSG_TRUNC) {
                stats.truncated = stats.truncated.saturating_add(1);
                continue;
            }
            if !allowed_sources.is_empty() {
                let ok = msg
                    .address
                    .as_ref()
                    .and_then(source_ip)
                    .is_some_and(|ip| source_allowed(allowed_sources, ip));
                if !ok {
                    stats.filtered_source = stats.filtered_source.saturating_add(1);
                    continue;
                }
            }
            let payload = msg.iovs().next().unwrap_or_default();
            on_packet(payload, meta);
            delivered += 1;
        }
        Ok(delivered)
    }

    fn stats(&self) -> TransportStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::time::Duration;

    fn config() -> UdpConfig {
        UdpConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            recv_buffer_bytes: Some(1 << 20),
            poll_timeout: Duration::from_millis(20),
            ..UdpConfig::default()
        }
    }

    fn drain(src: &mut RecvMmsgSource, want: u64) -> Vec<(usize, RxMeta)> {
        let mut got = Vec::new();
        for _ in 0..200 {
            src.poll(|b, m| got.push((b.len(), m))).unwrap();
            if src.stats().datagrams >= want {
                break;
            }
        }
        got
    }

    #[test]
    fn receives_batches_with_kernel_timestamps() {
        let mut src = RecvMmsgSource::bind(&config()).unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        for i in 0..100u8 {
            tx.send_to(&[i; 64], src.local_addr().unwrap()).unwrap();
        }
        let got = drain(&mut src, 100);
        assert_eq!(got.len(), 100);
        for (len, meta) in &got {
            assert_eq!(*len, 64);
            let kernel = meta.rx_kernel_ns.expect("SO_TIMESTAMPNS stamp");
            assert!(kernel <= meta.rx_wall_ns);
        }
        assert_eq!(src.stats().kernel_drops, Some(0));
    }

    #[test]
    fn kernel_drops_are_counted_exactly() {
        // A tiny receive buffer that cannot hold the burst.
        let cfg = UdpConfig {
            recv_buffer_bytes: Some(4096),
            ..config()
        };
        let mut src = RecvMmsgSource::bind(&cfg).unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let target = src.local_addr().unwrap();
        let burst = 500u64;
        for _ in 0..burst {
            tx.send_to(&[0xAB; 1000], target).unwrap();
        }
        let mut delivered = drain(&mut src, burst).len() as u64;
        // The drop counter travels with the next delivered datagram.
        tx.send_to(&[0xCD; 10], target).unwrap();
        let before = src.stats().datagrams;
        delivered += drain(&mut src, before + 1).len() as u64;
        let drops = src.stats().kernel_drops.unwrap();
        assert!(drops > 0, "the burst must overflow a 4 KiB buffer");
        assert_eq!(delivered + drops, burst + 1, "every datagram accounted for");
    }

    #[test]
    fn oversized_datagrams_are_counted_as_truncated() {
        let mut src = RecvMmsgSource::bind(&config()).unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.send_to(&[0; SLOT_LEN + 1], src.local_addr().unwrap())
            .unwrap();
        tx.send_to(&[1; 8], src.local_addr().unwrap()).unwrap();
        let got = drain(&mut src, 2);
        assert_eq!(got.len(), 1);
        assert_eq!(src.stats().truncated, 1);
    }

    #[test]
    fn source_allowlist_filters_and_counts() {
        let cfg = UdpConfig {
            allowed_sources: vec![IpAddr::from([127, 0, 0, 1])],
            ..config()
        };
        let mut src = RecvMmsgSource::bind(&cfg).unwrap();
        let other = UdpSocket::bind("127.0.0.2:0").unwrap();
        let allowed = UdpSocket::bind("127.0.0.1:0").unwrap();
        other.send_to(b"x", src.local_addr().unwrap()).unwrap();
        allowed.send_to(b"yy", src.local_addr().unwrap()).unwrap();
        let got = drain(&mut src, 2);
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), [2]);
        assert_eq!(src.stats().filtered_source, 1);
    }
}
