//! CAML market-data receive engine.
//!
//! Data flows strictly in one direction:
//!
//! ```text
//! PacketSource ──bytes, RxMeta──▶ FeedProcessor ──MarketEvent──▶ EventSink
//!  (transport)        parse → session gate → sequence → metrics   (consumer)
//! ```
//!
//! * [`transport`] owns sockets and receive buffers, counts every drop it
//!   makes or observes ([`transport::TransportStats`]), and knows nothing
//!   about the protocol.
//! * [`FeedProcessor`] owns protocol decoding, the session gate, sequence
//!   validation, and metrics, and knows nothing about sockets. Its
//!   [`metrics::Snapshot::integrity`] verdict accounts for every message that
//!   was not delivered.
//! * [`EventSink`] is the downstream consumer boundary.
//!
//! All three run on one thread; see `docs/architecture.md` for why, and for
//! when that will change.

#![warn(missing_docs)]

pub mod clock;
pub mod histogram;
pub mod metrics;
mod pipeline;
pub mod sequence;
pub mod transport;

pub use pipeline::{
    Delivery, EventSink, FeedProcessor, ProcessorConfig, RunLimits, StopReason, run,
};
