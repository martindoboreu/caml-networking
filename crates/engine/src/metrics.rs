//! Receiver counters, integrity verdict, and reporting.
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
    /// Valid heartbeat packets of the current session.
    pub heartbeats: u64,
    /// Sessions that announced their end with an end-of-session heartbeat.
    pub sessions_ended: u64,
    /// Sessions replaced without having announced their end. Loss of their
    /// final packets cannot be detected.
    pub unterminated_sessions: u64,
    /// Data packets none of whose messages were new (duplicates or below the
    /// baseline).
    pub redundant_packets: u64,
    /// Times the receiver adopted a different session epoch.
    pub session_changes: u64,
    /// Packets from an older session while the current one is active.
    pub stale_session_packets: u64,
    /// Packets from a newer session not (yet) confirmed.
    pub unconfirmed_session_packets: u64,
    /// Packets whose session epoch is implausibly far in the future.
    pub implausible_epoch_packets: u64,
    /// Messages passed to the sink.
    pub messages_delivered: u64,
    /// Messages passed to the sink that filled an earlier gap.
    pub messages_delivered_late: u64,
    /// Packets whose send (or kernel) timestamp was after the receive
    /// timestamp. These are excluded from the latency histograms.
    pub clock_anomalies: u64,
}

/// The session the receiver is currently following.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    /// Session epoch.
    pub epoch: u64,
    /// Whether its end-of-session heartbeat has been received.
    pub ended: bool,
}

/// Point-in-time copy of all receiver statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Packet/message counters.
    pub counters: Counters,
    /// Message-sequence statistics, cumulative over all sessions.
    pub seq: SeqStats,
    /// Current session, if any packet has been accepted.
    pub session: Option<SessionInfo>,
    /// Publisher `send_time_ns` → receiver user-space receive, nanoseconds.
    /// Includes the sender's syscall, the network path (or kernel loopback),
    /// socket-buffer queueing, and receiver wakeup. Valid only when both
    /// clocks are synchronized (same host, or PTP).
    pub send_to_rx_latency: LatencySummary,
    /// Kernel (or NIC) arrival stamp → receiver user-space receive,
    /// nanoseconds: socket-buffer queueing plus wakeup. Only recorded by
    /// backends that provide [`RxMeta::rx_kernel_ns`](crate::transport::RxMeta::rx_kernel_ns).
    pub kernel_to_rx_latency: LatencySummary,
    /// Receiver user-space receive → sink returned, nanoseconds (sampled).
    pub processing_latency: LatencySummary,
}

/// Feed-integrity verdict derived from a [`Snapshot`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Integrity {
    /// Messages known to be lost (outstanding, evicted, or abandoned gaps).
    pub lost_messages: u64,
    /// Datagrams rejected by the decoder.
    pub malformed: u64,
    /// Packets dropped as implausible (sequence jump or session epoch).
    pub implausible_packets: u64,
    /// Packets dropped by the session gate (stale or unconfirmed epoch).
    pub rejected_session_packets: u64,
    /// Messages below the baseline after a late join (never delivered).
    pub before_baseline: u64,
    /// Messages sent before this receiver joined. Informational: not loss.
    pub missed_before_join: u64,
    /// Session changes observed.
    pub session_changes: u64,
    /// Earlier sessions that ended without an end-of-session heartbeat.
    /// Not proven loss, but their tail loss would be invisible.
    pub unterminated_sessions: u64,
    /// Whether the current session announced its end. If not, loss after the
    /// last received packet cannot be detected.
    pub session_ended: bool,
}

impl Integrity {
    /// True when nothing was lost, rejected, or malformed.
    pub fn is_clean(&self) -> bool {
        self.lost_messages == 0
            && self.malformed == 0
            && self.implausible_packets == 0
            && self.rejected_session_packets == 0
            && self.before_baseline == 0
    }
}

impl Snapshot {
    /// Computes the integrity verdict.
    pub fn integrity(&self) -> Integrity {
        let c = &self.counters;
        Integrity {
            lost_messages: self.seq.lost(),
            malformed: c.malformed,
            implausible_packets: self
                .seq
                .implausible_packets
                .saturating_add(c.implausible_epoch_packets),
            rejected_session_packets: c
                .stale_session_packets
                .saturating_add(c.unconfirmed_session_packets),
            before_baseline: self.seq.before_baseline,
            missed_before_join: self.seq.missed_before_join,
            session_changes: c.session_changes,
            unterminated_sessions: c.unterminated_sessions,
            session_ended: self.session.is_some_and(|s| s.ended),
        }
    }
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

impl fmt::Display for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = &self.counters;
        writeln!(
            f,
            "  datagrams   n={} bytes={} malformed={} heartbeats={} redundant_pkts={}",
            c.datagrams, c.bytes, c.malformed, c.heartbeats, c.redundant_packets
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
            "  sessions    current={} ended={} unterminated={} changes={} stale_pkts={} unconfirmed_pkts={} implausible_epoch_pkts={}",
            self.session
                .map_or_else(|| "none".to_string(), |s| s.epoch.to_string()),
            c.sessions_ended,
            c.unterminated_sessions,
            c.session_changes,
            c.stale_session_packets,
            c.unconfirmed_session_packets,
            c.implausible_epoch_packets
        )?;
        writeln!(
            f,
            "  messages    delivered={} delivered_late={} clock_anomalies={}",
            c.messages_delivered, c.messages_delivered_late, c.clock_anomalies
        )?;
        let s = &self.seq;
        writeln!(
            f,
            "  sequence    in_order={} gaps={} missing={} recovered={} outstanding={} evicted={} abandoned={} duplicates={}",
            s.in_order,
            s.gaps,
            s.missing_total,
            s.recovered,
            s.outstanding,
            s.evicted,
            s.abandoned,
            s.duplicates
        )?;
        writeln!(
            f,
            "              before_baseline={} missed_before_join={} implausible_pkts={} resyncs={}",
            s.before_baseline, s.missed_before_join, s.implausible_packets, s.resyncs
        )?;
        fmt_latency(f, "send_to_rx", &self.send_to_rx_latency)?;
        if self.kernel_to_rx_latency.count > 0 {
            fmt_latency(f, "kernel_to_rx", &self.kernel_to_rx_latency)?;
        }
        fmt_latency(f, "processing", &self.processing_latency)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_flags_every_problem_class() {
        assert!(Snapshot::default().integrity().is_clean());
        let dirty = [
            |s: &mut Snapshot| s.seq.abandoned = 1,
            |s: &mut Snapshot| s.seq.evicted = 1,
            |s: &mut Snapshot| s.seq.outstanding = 1,
            |s: &mut Snapshot| s.counters.malformed = 1,
            |s: &mut Snapshot| s.seq.implausible_packets = 1,
            |s: &mut Snapshot| s.counters.implausible_epoch_packets = 1,
            |s: &mut Snapshot| s.counters.stale_session_packets = 1,
            |s: &mut Snapshot| s.counters.unconfirmed_session_packets = 1,
            |s: &mut Snapshot| s.seq.before_baseline = 1,
        ];
        for make_dirty in dirty {
            let mut s = Snapshot::default();
            make_dirty(&mut s);
            assert!(!s.integrity().is_clean(), "{s:?}");
        }
        // A late join is reported but is not a failure.
        let mut s = Snapshot::default();
        s.seq.missed_before_join = 10;
        assert!(s.integrity().is_clean());
    }

    #[test]
    fn display_renders_every_section() {
        let text = Snapshot::default().to_string();
        for section in [
            "datagrams",
            "sessions",
            "messages",
            "sequence",
            "send_to_rx",
        ] {
            assert!(text.contains(section), "{section} missing from:\n{text}");
        }
    }
}
