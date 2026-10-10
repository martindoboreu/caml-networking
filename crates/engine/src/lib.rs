//! CAML market-data receive engine.
//!
//! Data flows strictly in one direction:
//!
//! ```text
//! PacketSource ──bytes──▶ FeedProcessor ──MarketEvent──▶ EventSink
//!  (transport)            parse → sequence → metrics       (consumer)
//! ```
//!
//! * [`transport`] owns sockets and receive buffers and knows nothing about
//!   the protocol.
//! * [`FeedProcessor`] owns protocol decoding, sequence validation, and
//!   metrics, and knows nothing about sockets.
//! * [`EventSink`] is the downstream consumer boundary.
//!
//! The first milestone runs all three on one thread; see
//! `docs/architecture.md` for why, and for when that will change.

#![warn(missing_docs)]

pub mod clock;
pub mod histogram;
pub mod metrics;
mod pipeline;
pub mod sequence;
pub mod transport;

pub use pipeline::{Delivery, EventSink, FeedProcessor, RunLimits, StopReason, run};
