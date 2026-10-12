//! Deterministic synthetic market-data feed, fault injection, and pcap
//! output.
//!
//! Everything here is seeded and reproducible: the same configuration always
//! produces byte-identical packets, which makes test failures replayable.

#![warn(missing_docs)]

mod fault;
mod feed;
pub mod pcap;
mod rng;

pub use fault::{Fault, FaultCounts, FaultInjector, FaultPlan};
pub use feed::{ConfigError, FeedConfig, FeedSimulator};
pub use rng::SplitMix64;
