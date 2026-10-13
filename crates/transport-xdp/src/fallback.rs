//! AF_XDP with automatic fallback to the kernel's batched socket backend.
//!
//! [`FallbackSource::bind`] tries AF_XDP first. If it cannot start (missing
//! privileges, no such interface, unsupported driver, queue still busy after
//! the retry window), it binds a [`RecvMmsgSource`] with the given socket
//! configuration instead. The choice is made once, at bind, and is always
//! reported ([`FallbackSource::fallback_reason`]); it is never silent.
//!
//! Deliberately out of scope: switching backends mid-run if AF_XDP fails
//! after starting. That error ends the run instead, so a failure is visible.

use std::io;
use std::net::SocketAddr;

use caml_engine::transport::{PacketSource, RecvMmsgSource, RxMeta, TransportStats, UdpConfig};

use crate::{XdpConfig, XdpSource, XdpStats};

/// Which backend a [`FallbackSource`] is using.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// AF_XDP ([`XdpSource`]).
    AfXdp,
    /// The kernel socket fallback ([`RecvMmsgSource`]).
    Recvmmsg,
}

enum Inner {
    // Boxed: an `XdpSource` is several times larger than a socket source.
    Xdp(Box<XdpSource>),
    Socket(RecvMmsgSource),
}

/// AF_XDP if it can start, otherwise `recvmmsg`. See the module docs.
pub struct FallbackSource {
    inner: Inner,
    fallback_reason: Option<String>,
}

impl std::fmt::Debug for FallbackSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FallbackSource")
            .field("backend", &self.backend())
            .field("fallback_reason", &self.fallback_reason)
            .finish_non_exhaustive()
    }
}

impl FallbackSource {
    /// Binds AF_XDP with `xdp`, or, if that fails, `recvmmsg` with `socket`.
    /// Fails only if both fail; the error names both causes.
    pub fn bind(xdp: &XdpConfig, socket: &UdpConfig) -> io::Result<Self> {
        let xdp_err = match XdpSource::bind(xdp) {
            Ok(source) => {
                return Ok(Self {
                    inner: Inner::Xdp(Box::new(source)),
                    fallback_reason: None,
                });
            }
            Err(e) => e,
        };
        match RecvMmsgSource::bind(socket) {
            Ok(source) => Ok(Self {
                inner: Inner::Socket(source),
                fallback_reason: Some(format!("AF_XDP on {}: {xdp_err}", xdp.interface)),
            }),
            Err(socket_err) => Err(io::Error::new(
                socket_err.kind(),
                format!(
                    "AF_XDP on {} failed ({xdp_err}), and the recvmmsg fallback failed too ({socket_err})",
                    xdp.interface
                ),
            )),
        }
    }

    /// The backend in use.
    pub fn backend(&self) -> Backend {
        match self.inner {
            Inner::Xdp(_) => Backend::AfXdp,
            Inner::Socket(_) => Backend::Recvmmsg,
        }
    }

    /// Why AF_XDP could not start, if the fallback is in use.
    pub fn fallback_reason(&self) -> Option<&str> {
        self.fallback_reason.as_deref()
    }

    /// Bound socket address, when running on the socket fallback.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        match &self.inner {
            Inner::Xdp(_) => None,
            Inner::Socket(s) => s.local_addr().ok(),
        }
    }

    /// `SO_RCVBUF` granted by the kernel, when running on the socket
    /// fallback.
    pub fn granted_recv_buffer(&self) -> Option<usize> {
        match &self.inner {
            Inner::Xdp(_) => None,
            Inner::Socket(s) => Some(s.granted_recv_buffer()),
        }
    }

    /// AF_XDP statistics, when running on AF_XDP.
    pub fn xdp_stats(&self) -> Option<XdpStats> {
        match &self.inner {
            Inner::Xdp(s) => Some(s.xdp_stats()),
            Inner::Socket(_) => None,
        }
    }
}

impl PacketSource for FallbackSource {
    #[inline]
    fn poll<F>(&mut self, on_packet: F) -> io::Result<usize>
    where
        F: FnMut(&[u8], RxMeta),
    {
        match &mut self.inner {
            Inner::Xdp(s) => s.poll(on_packet),
            Inner::Socket(s) => s.poll(on_packet),
        }
    }

    fn stats(&self) -> TransportStats {
        match &self.inner {
            Inner::Xdp(s) => s.stats(),
            Inner::Socket(s) => s.stats(),
        }
    }
}
