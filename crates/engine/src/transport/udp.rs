//! Portable UDP backend: one blocking `recv_from` system call per datagram.
//!
//! This is the baseline every later backend must beat. It works on Windows,
//! Linux, and macOS with no privileges.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};

use super::{PacketSource, RxMeta, TransportStats};

/// Largest possible UDP payload is 65,507 bytes (IPv4). A receive buffer at
/// least this large means a datagram can never be truncated, which matters
/// because Linux truncates silently while Windows reports `WSAEMSGSIZE`.
const RECV_BUFFER_LEN: usize = 64 * 1024;

/// An IPv4 multicast group to join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MulticastGroup {
    /// Group address (224.0.0.0/4).
    pub group: Ipv4Addr,
    /// Local interface address to join on. `127.0.0.1` keeps traffic on the
    /// loopback interface; `0.0.0.0` lets the OS choose.
    pub interface: Ipv4Addr,
}

/// Configuration for [`UdpSource`].
#[derive(Clone, Debug)]
pub struct UdpConfig {
    /// Local address to bind. Binding to `127.0.0.1` keeps traffic on the
    /// loopback interface and does not trigger the Windows firewall. For
    /// multicast on Linux, bind the group address (or `0.0.0.0`) and the
    /// group's port.
    pub bind: SocketAddr,
    /// Requested kernel receive-buffer size (`SO_RCVBUF`). `None` keeps the OS
    /// default. The kernel may grant a different size; see
    /// [`UdpSource::granted_recv_buffer`].
    pub recv_buffer_bytes: Option<usize>,
    /// Maximum time one [`PacketSource::poll`] call blocks. Bounds how quickly
    /// the receive loop notices a shutdown request. Must be non-zero.
    pub poll_timeout: Duration,
    /// Multicast group to join after binding.
    pub multicast: Option<MulticastGroup>,
    /// If non-empty, datagrams from any other source address are dropped and
    /// counted in [`TransportStats::filtered_source`].
    pub allowed_sources: Vec<IpAddr>,
    /// Set `SO_REUSEADDR`, so several receivers can bind the same multicast
    /// port.
    pub reuse_address: bool,
}

impl Default for UdpConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 40_000)),
            recv_buffer_bytes: None,
            poll_timeout: Duration::from_millis(100),
            multicast: None,
            allowed_sources: Vec::new(),
            reuse_address: false,
        }
    }
}

/// Blocking UDP receiver.
#[derive(Debug)]
pub struct UdpSource {
    socket: UdpSocket,
    buf: Box<[u8]>,
    granted_recv_buffer: usize,
    allowed_sources: Box<[IpAddr]>,
    stats: TransportStats,
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// Validates `config`, then creates, configures, and binds the socket
/// (shared by every socket backend). Returns the socket and the receive
/// buffer size the kernel granted.
pub(crate) fn open_socket(config: &UdpConfig) -> io::Result<(Socket, usize)> {
    if config.poll_timeout.is_zero() {
        return Err(invalid("poll_timeout must be non-zero"));
    }
    if let Some(m) = config.multicast {
        if !m.group.is_multicast() {
            return Err(invalid("multicast group must be in 224.0.0.0/4"));
        }
        if !config.bind.is_ipv4() {
            return Err(invalid("IPv4 multicast requires an IPv4 bind address"));
        }
    }

    let socket = Socket::new(
        Domain::for_address(config.bind),
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;
    if config.reuse_address {
        socket.set_reuse_address(true)?;
    }
    if let Some(bytes) = config.recv_buffer_bytes {
        socket.set_recv_buffer_size(bytes)?;
    }
    socket.bind(&config.bind.into())?;
    if let Some(m) = config.multicast {
        socket.join_multicast_v4(&m.group, &m.interface)?;
        // Linux otherwise delivers every group joined by *any* socket on
        // the host to a socket bound to the wildcard address.
        #[cfg(target_os = "linux")]
        socket.set_multicast_all_v4(false)?;
    }
    socket.set_read_timeout(Some(config.poll_timeout))?;
    let granted = socket.recv_buffer_size()?;
    Ok((socket, granted))
}

/// Whether `ip` passes an allowlist (empty = everything passes).
#[inline]
pub(crate) fn source_allowed(allowed: &[IpAddr], ip: IpAddr) -> bool {
    allowed.is_empty() || allowed.contains(&ip)
}

impl UdpSource {
    /// Creates and binds the socket, joins the multicast group if any, and
    /// allocates the receive buffer.
    pub fn bind(config: &UdpConfig) -> io::Result<Self> {
        let (socket, granted_recv_buffer) = open_socket(config)?;
        let socket: UdpSocket = socket.into();

        Ok(Self {
            socket,
            buf: vec![0u8; RECV_BUFFER_LEN].into_boxed_slice(),
            granted_recv_buffer,
            allowed_sources: config.allowed_sources.clone().into_boxed_slice(),
            stats: TransportStats {
                kernel_drops: None,
                ..TransportStats::default()
            },
        })
    }

    /// Address actually bound (useful when binding port 0).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// `SO_RCVBUF` as reported by the kernel after configuration. Linux
    /// reports double the requested value (it reserves half for bookkeeping)
    /// and caps requests at `net.core.rmem_max`.
    pub fn granted_recv_buffer(&self) -> usize {
        self.granted_recv_buffer
    }
}

impl PacketSource for UdpSource {
    #[inline]
    fn poll<F>(&mut self, mut on_packet: F) -> io::Result<usize>
    where
        F: FnMut(&[u8], RxMeta),
    {
        match self.socket.recv_from(&mut self.buf) {
            Ok((len, peer)) => {
                let meta = RxMeta::now();
                self.stats.datagrams = self.stats.datagrams.saturating_add(1);
                if !source_allowed(&self.allowed_sources, peer.ip()) {
                    self.stats.filtered_source = self.stats.filtered_source.saturating_add(1);
                    return Ok(0);
                }
                on_packet(&self.buf[..len], meta);
                Ok(1)
            }
            // Unix reports a read timeout as WouldBlock, Windows as TimedOut.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(0)
            }
            // An ICMP error from an earlier datagram, surfaced on this
            // socket (Windows: WSAECONNRESET). Nothing was lost; keep going.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused
                ) =>
            {
                self.stats.transient_errors = self.stats.transient_errors.saturating_add(1);
                Ok(0)
            }
            Err(e) => Err(e),
        }
    }

    fn stats(&self) -> TransportStats {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let mut s = self.stats;
            s.kernel_drops = super::procfs::socket_drops(self.socket.as_raw_fd());
            s
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.stats
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback_any_port() -> UdpConfig {
        UdpConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            recv_buffer_bytes: Some(1 << 20),
            poll_timeout: Duration::from_millis(20),
            ..UdpConfig::default()
        }
    }

    fn poll_until(src: &mut UdpSource, mut want: impl FnMut(&UdpSource) -> bool) -> Vec<usize> {
        let mut lens = Vec::new();
        for _ in 0..100 {
            src.poll(|b, _| lens.push(b.len())).unwrap();
            if want(src) {
                break;
            }
        }
        lens
    }

    #[test]
    fn rejects_bad_config() {
        let cfg = UdpConfig {
            poll_timeout: Duration::ZERO,
            ..loopback_any_port()
        };
        assert_eq!(
            UdpSource::bind(&cfg).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let cfg = UdpConfig {
            multicast: Some(MulticastGroup {
                group: Ipv4Addr::new(10, 0, 0, 1),
                interface: Ipv4Addr::LOCALHOST,
            }),
            ..loopback_any_port()
        };
        assert_eq!(
            UdpSource::bind(&cfg).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn times_out_when_idle() {
        let mut src = UdpSource::bind(&loopback_any_port()).unwrap();
        assert!(src.granted_recv_buffer() > 0);
        let n = src.poll(|_, _| panic!("no packet expected")).unwrap();
        assert_eq!(n, 0);
        let s = src.stats();
        assert_eq!((s.datagrams, s.dropped()), (0, 0));
        // Kernel drops are observable on Linux only.
        assert_eq!(s.kernel_drops.is_some(), cfg!(target_os = "linux"));
    }

    /// Exact accounting on Linux: every datagram sent is either delivered
    /// or counted in the kernel's drop counter.
    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_drops_are_counted_exactly() {
        let cfg = UdpConfig {
            recv_buffer_bytes: Some(4096),
            ..loopback_any_port()
        };
        let mut src = UdpSource::bind(&cfg).unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let burst = 500u64;
        for _ in 0..burst {
            tx.send_to(&[0xAB; 1000], src.local_addr().unwrap())
                .unwrap();
        }
        let mut delivered = 0u64;
        while src.poll(|_, _| delivered += 1).unwrap() > 0 {}
        let drops = src.stats().kernel_drops.unwrap();
        assert!(drops > 0, "the burst must overflow a 4 KiB buffer");
        assert_eq!(delivered + drops, burst, "every datagram accounted for");
    }

    #[test]
    fn receives_datagram_up_to_max_size() {
        let mut src = UdpSource::bind(&loopback_any_port()).unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let big = vec![0xAB; 65_507];
        tx.send_to(&big, src.local_addr().unwrap()).unwrap();

        let lens = poll_until(&mut src, |s| s.stats().datagrams == 1);
        assert_eq!(lens, [big.len()]);
        assert_eq!(src.stats().datagrams, 1);
    }

    #[test]
    fn source_allowlist_filters_and_counts() {
        let cfg = UdpConfig {
            allowed_sources: vec![IpAddr::from([127, 0, 0, 1])],
            ..loopback_any_port()
        };
        let mut src = UdpSource::bind(&cfg).unwrap();
        let allowed = UdpSocket::bind("127.0.0.1:0").unwrap();
        let other = UdpSocket::bind("127.0.0.2:0").unwrap();
        other.send_to(b"x", src.local_addr().unwrap()).unwrap();
        allowed.send_to(b"yy", src.local_addr().unwrap()).unwrap();

        let lens = poll_until(&mut src, |s| s.stats().datagrams == 2);
        assert_eq!(lens, [2]);
        let s = src.stats();
        assert_eq!((s.datagrams, s.filtered_source, s.dropped()), (2, 1, 1));
    }

    /// Joins a group on the loopback interface. Windows delivers to a socket
    /// bound to 127.0.0.1; Linux needs the group address bound and multicast
    /// enabled on `lo` (`sudo ip link set lo multicast on`), which CI does.
    #[test]
    #[cfg_attr(
        target_os = "linux",
        ignore = "needs `ip link set lo multicast on`; CI enables it"
    )]
    fn joins_multicast_group_on_loopback() {
        let group = Ipv4Addr::new(239, 77, 0, 1);
        let bind_ip = if cfg!(windows) {
            Ipv4Addr::LOCALHOST
        } else {
            group
        };
        let cfg = UdpConfig {
            bind: SocketAddr::from((bind_ip, 0)),
            multicast: Some(MulticastGroup {
                group,
                interface: Ipv4Addr::LOCALHOST,
            }),
            reuse_address: true,
            ..loopback_any_port()
        };
        let mut src = UdpSource::bind(&cfg).unwrap();
        let port = src.local_addr().unwrap().port();

        let tx = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        tx.bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into())
            .unwrap();
        tx.set_multicast_if_v4(&Ipv4Addr::LOCALHOST).unwrap();
        tx.set_multicast_loop_v4(true).unwrap();
        tx.send_to(b"mc", &SocketAddr::from((group, port)).into())
            .unwrap();

        let lens = poll_until(&mut src, |s| s.stats().datagrams == 1);
        assert_eq!(lens, [2]);
    }
}
