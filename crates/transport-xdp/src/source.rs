//! [`XdpSource`]: an AF_XDP [`PacketSource`].
//!
//! Frame ownership, which every `unsafe` block below relies on:
//!
//! * One UMEM per source, created in [`XdpSource::bind`] and never shared.
//! * All [`FRAMES`] frames are handed to the kernel's fill ring at bind.
//! * `poll` takes frames back from the RX ring into `descs`, lends their
//!   payloads to the callback, then returns exactly those frames to the fill
//!   ring before returning. Outside `poll`, the kernel owns every frame.
//!
//! So a descriptor is only ever dereferenced between leaving the RX ring and
//! re-entering the fill ring, which is when `xsk-rs` requires it.

#![allow(unsafe_code)]

use std::error::Error;
use std::io;
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use caml_engine::transport::{PacketSource, RxMeta, TransportStats};
use caml_frame::{FrameCounters, FrameError, FrameFilter};
use xsk_rs::config::{BindFlags, Interface, SocketConfig, UmemConfig, XdpFlags};
use xsk_rs::socket::XdpStatistics;
use xsk_rs::{CompQueue, FillQueue, FrameDesc, RxQueue, Socket, TxQueue, Umem};

/// UMEM frames. Equal to libxdp's default fill-ring size, so the whole UMEM
/// fits in the fill ring and returning frames can never fail for lack of
/// space.
const FRAMES: u32 = 2048;

/// Descriptors taken from the RX ring per `poll`.
const BATCH: usize = 64;

/// How XDP is attached to the interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XdpMode {
    /// Generic (SKB) mode: works on every interface, including `veth`; the
    /// kernel has already allocated an `sk_buff` and copies into UMEM.
    Generic,
    /// Native (driver) mode: XDP runs in the driver before `sk_buff`
    /// allocation. Needs driver support (e.g. `veth`, i40e, ice, mlx5).
    Native,
}

/// Configuration for [`XdpSource`].
#[derive(Clone, Debug)]
pub struct XdpConfig {
    /// Interface to bind (e.g. a `veth` end).
    pub interface: String,
    /// Hardware queue to bind. Traffic on other queues is not seen.
    pub queue_id: u32,
    /// XDP attach mode.
    pub mode: XdpMode,
    /// Request zero-copy (needs a driver that supports it; `veth` does not).
    pub zero_copy: bool,
    /// Which frames to deliver; everything else is counted and recycled.
    pub filter: FrameFilter,
    /// Maximum time one `poll` blocks.
    pub poll_timeout: Duration,
    /// How long `bind` retries while the queue is still busy. The kernel
    /// releases a closed AF_XDP socket's hold on its queue asynchronously,
    /// so binding right after a previous receiver exited briefly fails with
    /// `EBUSY` (measured: well under 1 s).
    pub bind_retry: Duration,
}

impl XdpConfig {
    /// Generic mode, copy mode, queue 0, 100 ms poll timeout, 3 s bind retry.
    pub fn new(interface: impl Into<String>, filter: FrameFilter) -> Self {
        Self {
            interface: interface.into(),
            queue_id: 0,
            mode: XdpMode::Generic,
            zero_copy: false,
            filter,
            poll_timeout: Duration::from_millis(100),
            bind_retry: Duration::from_secs(3),
        }
    }
}

/// AF_XDP socket statistics plus this source's own counters.
#[derive(Clone, Copy, Debug)]
pub struct XdpStats {
    /// Kernel AF_XDP statistics (`XDP_STATISTICS`), if readable.
    pub kernel: Option<XdpStatistics>,
    /// Frame-filter results by reason.
    pub frames: FrameCounters,
    /// Frames that could not be returned to the fill ring. Must stay 0.
    pub lost_frames: u64,
}

/// AF_XDP receiver. See the module docs for the frame-ownership rules.
pub struct XdpSource {
    // Queues before the UMEM, so they drop (and the socket closes) first.
    rx_q: RxQueue,
    fill_q: FillQueue,
    _tx_q: TxQueue,
    _comp_q: CompQueue,
    umem: Umem,
    descs: Box<[FrameDesc]>,
    filter: FrameFilter,
    frames: FrameCounters,
    stats: TransportStats,
    lost_frames: u64,
    poll_timeout_ms: i32,
}

impl std::fmt::Debug for XdpSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XdpSource")
            .field("filter", &self.filter)
            .field("frames", &self.frames)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

/// The OS error underneath an `xsk-rs` error, if there is one.
fn os_cause<'a>(e: &'a (dyn Error + 'static)) -> Option<&'a io::Error> {
    e.source().and_then(|s| s.downcast_ref::<io::Error>())
}

fn is_busy(e: &xsk_rs::socket::SocketCreateError) -> bool {
    os_cause(e).is_some_and(|io| io.kind() == io::ErrorKind::ResourceBusy)
}

/// `xsk-rs` errors display only "non-zero error code returned when creating
/// UMEM" (or "AF_XDP socket"). Keep the OS error's kind and text, so callers
/// (and the fallback's report) can tell a missing interface from missing
/// privileges.
fn with_os_cause<E: Error + Send + Sync + 'static>(e: E) -> io::Error {
    match os_cause(&e) {
        Some(os) => io::Error::new(os.kind(), format!("{e}: {os}")),
        None => io::Error::other(e),
    }
}

impl XdpSource {
    /// Creates the UMEM, loads (or reuses) libxdp's redirect program, binds
    /// the socket, and gives every frame to the kernel. Needs
    /// `CAP_NET_ADMIN`, `CAP_NET_RAW`, and `CAP_BPF` (or root).
    pub fn bind(config: &XdpConfig) -> io::Result<Self> {
        let interface: Interface = config.interface.parse().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "interface name contains NUL")
        })?;
        let frame_count = NonZeroU32::new(FRAMES).ok_or_else(|| io::Error::other("FRAMES is 0"))?;
        let (umem, frames) =
            Umem::new(UmemConfig::default(), frame_count, false).map_err(with_os_cause)?;

        let xdp_flags = match config.mode {
            XdpMode::Generic => XdpFlags::XDP_FLAGS_SKB_MODE,
            XdpMode::Native => XdpFlags::XDP_FLAGS_DRV_MODE,
        };
        let bind_flags = if config.zero_copy {
            BindFlags::XDP_ZEROCOPY
        } else {
            BindFlags::XDP_COPY
        };
        let socket_config = SocketConfig::builder()
            .xdp_flags(xdp_flags)
            .bind_flags(bind_flags)
            .build();

        let deadline = Instant::now() + config.bind_retry;
        let (tx_q, rx_q, fq_and_cq) = loop {
            // SAFETY: `umem` was created above and is not shared with any
            // other socket, so the requirement to set
            // XSK_LIBXDP_FLAGS_INHIBIT_PROG_LOAD (shared UMEM, pair already
            // bound) does not apply.
            match unsafe { Socket::new(socket_config, &umem, &interface, config.queue_id) } {
                Ok(queues) => break queues,
                Err(e) if is_busy(&e) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(with_os_cause(e)),
            }
        };
        let (mut fill_q, comp_q) = fq_and_cq
            .ok_or_else(|| io::Error::other("libxdp returned no fill/completion queue"))?;

        // SAFETY: every descriptor in `frames` comes from `umem` (the UMEM
        // this fill queue belongs to) and none has been handed to the kernel
        // or the TX queue before.
        let produced = unsafe { fill_q.produce(&frames) };
        if produced != frames.len() {
            return Err(io::Error::other(
                "fill ring is smaller than the UMEM frame count",
            ));
        }

        let poll_timeout_ms = i32::try_from(config.poll_timeout.as_millis()).unwrap_or(i32::MAX);
        Ok(Self {
            rx_q,
            fill_q,
            _tx_q: tx_q,
            _comp_q: comp_q,
            umem,
            descs: vec![FrameDesc::default(); BATCH].into_boxed_slice(),
            filter: config.filter,
            frames: FrameCounters::default(),
            stats: TransportStats::default(),
            lost_frames: 0,
            poll_timeout_ms,
        })
    }

    /// AF_XDP kernel statistics and frame-filter counters.
    pub fn xdp_stats(&self) -> XdpStats {
        XdpStats {
            kernel: self.rx_q.fd().xdp_statistics().ok(),
            frames: self.frames,
            lost_frames: self.lost_frames,
        }
    }
}

impl PacketSource for XdpSource {
    fn poll<F>(&mut self, mut on_packet: F) -> io::Result<usize>
    where
        F: FnMut(&[u8], RxMeta),
    {
        let Self {
            rx_q,
            fill_q,
            umem,
            descs,
            filter,
            frames,
            stats,
            lost_frames,
            poll_timeout_ms,
            ..
        } = self;

        // SAFETY: the RX queue writes descriptors of frames from the UMEM
        // its socket was bound with, which is `umem`; `descs` is only
        // scratch storage that gets overwritten.
        let n = match unsafe { rx_q.poll_and_consume(descs, *poll_timeout_ms) } {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(0),
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Ok(0);
        }
        let consumed = descs.get(..n).unwrap_or_default();

        // Every frame in the batch reached user space at this call.
        let meta = RxMeta::now();
        let mut delivered = 0;
        for desc in consumed {
            stats.datagrams = stats.datagrams.saturating_add(1);
            // SAFETY: `desc` was just consumed from the RX ring of a socket
            // bound to `umem`, so it names a frame of `umem` that the kernel
            // has handed back. Nothing else reads or writes it until it is
            // returned to the fill ring below, after this borrow ends.
            let data = unsafe { umem.data(desc) };
            let result = filter.accept(data.contents());
            frames.record(&result);
            match result {
                Ok(datagram) => {
                    on_packet(datagram.payload, meta);
                    delivered += 1;
                }
                Err(FrameError::SourceRejected) => {
                    stats.filtered_source = stats.filtered_source.saturating_add(1);
                }
                Err(e) if e.is_corruption() => {
                    stats.corrupt_frames = stats.corrupt_frames.saturating_add(1);
                }
                Err(_) => stats.filtered_frames = stats.filtered_frames.saturating_add(1),
            }
        }

        // SAFETY: these descriptors came from this UMEM through the RX ring
        // and are no longer borrowed, so handing them back to the kernel
        // cannot race with a reader. They are not on the TX queue.
        let returned = unsafe { fill_q.produce(consumed) };
        if returned != n {
            // Cannot happen while the fill ring holds the whole UMEM, but if
            // it ever did, the frames would be lost to the kernel: count it.
            *lost_frames = lost_frames.saturating_add(n as u64);
        }
        Ok(delivered)
    }

    fn stats(&self) -> TransportStats {
        let mut s = self.stats;
        // Frames the kernel could not deliver to this socket: dropped (e.g.
        // no free fill-ring frame) or RX ring full.
        s.kernel_drops = self
            .rx_q
            .fd()
            .xdp_statistics()
            .ok()
            .map(|x| x.rx_dropped().saturating_add(x.rx_ring_full()));
        s
    }
}
