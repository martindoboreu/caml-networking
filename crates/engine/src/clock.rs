//! Timestamp sources.
//!
//! Two clocks are used, for different purposes:
//!
//! * **Wall clock** ([`wall_clock_ns`]): comparable across processes on the
//!   same host, so it is used for publisher→receiver latency. It can jump if
//!   the OS adjusts the time (NTP), so negative or absurd deltas must be
//!   counted rather than trusted.
//! * **Monotonic clock** ([`std::time::Instant`]): never goes backwards, used
//!   for in-process durations such as processing time.
//!
//! Resolution is platform-dependent. On Windows both are backed by 100 ns
//! ticks, so sub-microsecond latencies are quantised to 100 ns.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time in nanoseconds since the Unix epoch.
///
/// Returns 0 if the system clock is set before 1970, and saturates at
/// `u64::MAX` (year 2554).
#[inline]
pub fn wall_clock_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}
