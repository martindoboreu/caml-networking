//! Receiver counters and reporting.
//!
//! Counters are plain `u64` fields, not atomics: in the current single-thread
//! design the only writer and the only reader are the same thread, so atomics
//! would add cost (and imply sharing) without adding correctness. When a
//! separate telemetry thread is introduced, it will receive copies of
//! [`Snapshot`] rather than reading live counters.

use std::fmt;

use caml_protocol::DecodeErrorKind;

use crate::histogram::LatencySummary;
use crate::sequence::SeqStats;

/// Packet- and message-level counters maintained by the processor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Datagrams handed to the processor, valid or not.
    pub datagrams: u64,
    /// Total bytes of those datagrams.
    pub bytes: u64,
    /// Datagrams rejected by the decoder, all reasons.
    pub malformed: u64,
    /// Rejections by reason, indexed by [`DecodeErrorKind::index`].
    pub malformed_by_kind: [u64; DecodeErrorKind::COUNT],
    /// Valid packets dropped because their packet sequence was a duplicate.
    pub duplicate_packets: u64,
    /// Valid packets with zero messages.
    pub heartbeats: u64,
    /// Times the publisher session id changed after the first packet.
    pub session_changes: u64,
    /// Messages passed to the sink.
    pub messages_delivered: u64,
    /// Messages passed to the sink that arrived after a later message.
    pub messages_delivered_late: u64,
    /// Packets whose send timestamp was after the receive timestamp. These
    /// are excluded from the wire-latency histogram.
    pub clock_anomalies: u64,
}

/// Point-in-time copy of all receiver statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Packet/message counters.
    pub counters: Counters,
    /// Packet-sequence tracker statistics.
    pub packet_seq: SeqStats,
    /// Message-sequence tracker statistics.
    pub message_seq: SeqStats,
    /// Publisher send → receiver user-space receive, nanoseconds.
    pub wire_latency: LatencySummary,
    /// Receiver user-space receive → sink returned, nanoseconds.
    pub processing_latency: LatencySummary,
}

fn fmt_latency(f: &mut fmt::Formatter<'_>, name: &str, s: &LatencySummary) -> fmt::Result {
    if s.count == 0 {
        return writeln!(f, "  {name:<11} no samples");
    }
    let us = |ns: u64| ns as f64 / 1_000.0;
    writeln!(
        f,
        "  {name:<11} n={} min={:.1} p50={:.1} p95={:.1} p99={:.1} p99.9={:.1} max={:.1} mean={:.1} (us)",
        s.count,
        us(s.min),
        us(s.p50),
        us(s.p95),
        us(s.p99),
        us(s.p999),
        us(s.max),
        us(s.mean)
    )
}

fn fmt_seq(f: &mut fmt::Formatter<'_>, name: &str, s: &SeqStats) -> fmt::Result {
    writeln!(
        f,
        "  {name:<11} in_order={} gaps={} missing={} recovered={} outstanding={} evicted={} duplicates={}",
        s.in_order, s.gaps, s.missing_total, s.recovered, s.outstanding, s.evicted, s.duplicates
    )
}

impl fmt::Display for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = &self.counters;
        writeln!(
            f,
            "  datagrams   n={} bytes={} malformed={} duplicate_pkts={} heartbeats={} session_changes={}",
            c.datagrams, c.bytes, c.malformed, c.duplicate_packets, c.heartbeats, c.session_changes
        )?;
        if c.malformed > 0 {
            write!(f, "  malformed  ")?;
            for kind in DecodeErrorKind::ALL {
                let n = c.malformed_by_kind[kind.index()];
                if n > 0 {
                    write!(f, " {}={n}", kind.name())?;
                }
            }
            writeln!(f)?;
        }
        writeln!(
            f,
            "  messages    delivered={} delivered_late={} clock_anomalies={}",
            c.messages_delivered, c.messages_delivered_late, c.clock_anomalies
        )?;
        fmt_seq(f, "packet_seq", &self.packet_seq)?;
        fmt_seq(f, "message_seq", &self.message_seq)?;
        fmt_latency(f, "wire", &self.wire_latency)?;
        fmt_latency(f, "processing", &self.processing_latency)
    }
}
