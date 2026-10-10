//! Deterministic synthetic market-data feed and fault injection.
//!
//! Everything here is seeded and reproducible: the same configuration always
//! produces byte-identical packets, which makes test failures replayable.

#![warn(missing_docs)]

mod fault;
mod feed;
mod rng;

pub use fault::{Fault, FaultCounts, FaultInjector, FaultPlan};
pub use feed::{FeedConfig, FeedSimulator};
pub use rng::SplitMix64;
