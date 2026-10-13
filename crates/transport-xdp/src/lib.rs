//! AF_XDP receive backend for `caml-engine`.
//!
//! `XdpSource` (with the `xdp` feature) receives Ethernet frames through an
//! AF_XDP socket bound to one queue of a network interface, filters them
//! with [`caml_frame::FrameFilter`], and lends each matching UDP payload to
//! the pipeline straight out of UMEM (no copy), returning the frame to the
//! kernel's fill ring after the callback.
//!
//! The kernel side is libxdp's standard redirect program (loaded through
//! [`xsk-rs`](https://crates.io/crates/xsk-rs)): it redirects packets on the
//! bound queue to the socket and passes everything else (and everything, if
//! no socket is bound) to the normal kernel stack. See `docs/decisions.md`
//! (D15) for the attach and crash behavior this implies.
//!
//! Linux only, behind the `xdp` feature; without it this crate is empty.
//! Development and CI use `veth` pairs (`scripts/xdp-veth.sh`); never attach
//! to a physical interface without authorization (AGENTS.md).

#![warn(missing_docs)]

#[cfg(all(target_os = "linux", feature = "xdp"))]
mod source;

#[cfg(all(target_os = "linux", feature = "xdp"))]
pub use source::{XdpConfig, XdpMode, XdpSource, XdpStats};

/// Re-exported so users configure filtering without depending on
/// `caml-frame` directly.
pub use caml_frame::{ChecksumPolicy, FrameCounters, FrameError, FrameFilter};
