//! Portable UDP backend: one blocking `recv_from` system call per datagram.
//!
//! This is the baseline every later backend must beat. It works on Windows,
//! Linux, and macOS with no privileges.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};

use super::{PacketSource, RxMeta};

/// Largest possible UDP payload is 65,507 bytes (IPv4). A receive buffer at
/// least this large means a datagram can never be truncated, which matters
/// because Linux truncates silently while Windows reports `WSAEMSGSIZE`.
const RECV_BUFFER_LEN: usize = 64 * 1024;

/// Configuration for [`UdpSource`].
#[derive(Clone, Debug)]
pub struct UdpConfig {
    /// Local address to bind. Binding to `127.0.0.1` keeps traffic on the
    /// loopback interface and does not trigger the Windows firewall.
    pub bind: SocketAddr,
    /// Requested kernel receive-buffer size (`SO_RCVBUF`). `None` keeps the OS
    /// default. The kernel may grant a different size; see
    /// [`UdpSource::granted_recv_buffer`].
    pub recv_buffer_bytes: Option<usize>,
    /// Maximum time one [`PacketSource::poll`] call blocks. Bounds how quickly
    /// the receive loop notices a shutdown request. Must be non-zero.
    pub poll_timeout: Duration,
}

impl Default for UdpConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 40_000)),
            recv_buffer_bytes: None,
            poll_timeout: Duration::from_millis(100),
        }
    }
}

/// Blocking UDP receiver.
#[derive(Debug)]
pub struct UdpSource {
    socket: UdpSocket,
    buf: Box<[u8]>,
    granted_recv_buffer: usize,
}

impl UdpSource {
    /// Creates and binds the socket and allocates the receive buffer.
    pub fn bind(config: &UdpConfig) -> io::Result<Self> {
        if config.poll_timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "poll_timeout must be non-zero",
            ));
        }

        let socket = Socket::new(
            Domain::for_address(config.bind),
            Type::DGRAM,
            Some(Protocol::UDP),
        )?;
        if let Some(bytes) = config.recv_buffer_bytes {
            socket.set_recv_buffer_size(bytes)?;
        }
        socket.bind(&config.bind.into())?;
        let granted_recv_buffer = socket.recv_buffer_size()?;

        let socket: UdpSocket = socket.into();
        socket.set_read_timeout(Some(config.poll_timeout))?;

        Ok(Self {
            socket,
            buf: vec![0u8; RECV_BUFFER_LEN].into_boxed_slice(),
            granted_recv_buffer,
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
            Ok((len, _peer)) => {
                on_packet(&self.buf[..len], RxMeta::now());
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
            Err(e) => Err(e),
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
        }
    }

    #[test]
    fn rejects_zero_timeout() {
        let cfg = UdpConfig {
            poll_timeout: Duration::ZERO,
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
    }

    #[test]
    fn receives_datagram_up_to_max_size() {
        let mut src = UdpSource::bind(&loopback_any_port()).unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let big = vec![0xAB; 65_507];
        tx.send_to(&big, src.local_addr().unwrap()).unwrap();

        let sent_at = crate::clock::wall_clock_ns();
        let mut got = None;
        for _ in 0..50 {
            if src.poll(|b, m| got = Some((b.len(), m))).unwrap() == 1 {
                break;
            }
        }
        let (len, meta) = got.expect("datagram not received");
        assert_eq!(len, big.len());
        assert!(meta.rx_wall_ns >= sent_at);
    }
}
