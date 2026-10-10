use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use caml_protocol::{MarketEvent, parse_packet};

use crate::histogram::LatencyHistogram;
use crate::metrics::{Counters, Snapshot};
use crate::sequence::{SeqOutcome, SequenceTracker};
use crate::transport::{PacketSource, RxMeta};

/// Ordering context attached to every delivered event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Next expected message (or the first message of a session).
    InOrder,
    /// First message after a detected sequence gap; earlier messages are
    /// missing and may or may not arrive later.
    AfterGap,
    /// A message that fills an earlier gap. Consumers that maintain state from
    /// incremental updates (e.g. a book) usually must not apply it blindly.
    Late,
}

/// Downstream consumer of decoded market events.
///
/// Duplicates are never delivered. Every other valid message is delivered
/// exactly once, tagged with its [`Delivery`] status, so the consumer, not
/// the engine, decides how to treat out-of-order data.
pub trait EventSink {
    /// Handles one event. Runs on the receive thread: time spent here is
    /// time the socket is not being drained.
    fn on_event(&mut self, event: &MarketEvent, delivery: Delivery);
}

impl<F: FnMut(&MarketEvent, Delivery)> EventSink for F {
    fn on_event(&mut self, event: &MarketEvent, delivery: Delivery) {
        self(event, delivery)
    }
}

/// Default gap-table capacity for each sequence tracker.
const DEFAULT_MAX_TRACKED_GAPS: usize = 1024;

/// Decodes, validates, and accounts for datagrams; forwards events to a sink.
///
/// Transport-agnostic: it accepts byte slices and metadata from any
/// [`PacketSource`].
#[derive(Debug)]
pub struct FeedProcessor<K> {
    sink: K,
    session_id: Option<u32>,
    packet_seq: SequenceTracker,
    message_seq: SequenceTracker,
    counters: Counters,
    wire_latency: LatencyHistogram,
    processing_latency: LatencyHistogram,
}

impl<K: EventSink> FeedProcessor<K> {
    /// Creates a processor with default gap-table capacity.
    pub fn new(sink: K) -> Self {
        Self::with_gap_capacity(sink, DEFAULT_MAX_TRACKED_GAPS)
    }

    /// Creates a processor whose sequence trackers each remember up to
    /// `max_tracked_gaps` gaps.
    pub fn with_gap_capacity(sink: K, max_tracked_gaps: usize) -> Self {
        Self {
            sink,
            session_id: None,
            packet_seq: SequenceTracker::new(max_tracked_gaps),
            message_seq: SequenceTracker::new(max_tracked_gaps),
            counters: Counters::default(),
            wire_latency: LatencyHistogram::new(),
            processing_latency: LatencyHistogram::new(),
        }
    }

    /// Processes one datagram. Never panics on malformed input and never
    /// allocates.
    pub fn process_packet(&mut self, bytes: &[u8], meta: RxMeta) {
        self.counters.datagrams += 1;
        self.counters.bytes += bytes.len() as u64;

        let packet = match parse_packet(bytes) {
            Ok(p) => p,
            Err(e) => {
                self.counters.malformed += 1;
                self.counters.malformed_by_kind[e.kind().index()] += 1;
                return;
            }
        };
        let header = packet.header();

        if self.session_id != Some(header.session_id) {
            if self.session_id.is_some() {
                self.counters.session_changes += 1;
                self.packet_seq.reset();
                self.message_seq.reset();
            }
            self.session_id = Some(header.session_id);
        }

        if header.send_time_ns <= meta.rx_wall_ns {
            self.wire_latency
                .record(meta.rx_wall_ns - header.send_time_ns);
        } else {
            self.counters.clock_anomalies += 1;
        }

        if self.packet_seq.observe(header.packet_seq) == SeqOutcome::Duplicate {
            self.counters.duplicate_packets += 1;
            return;
        }
        if header.msg_count == 0 {
            self.counters.heartbeats += 1;
        }

        for event in packet.messages() {
            let delivery = match self.message_seq.observe(event.seq) {
                SeqOutcome::First | SeqOutcome::InOrder => Delivery::InOrder,
                SeqOutcome::Gap { .. } => Delivery::AfterGap,
                SeqOutcome::Late => Delivery::Late,
                // Packet-level dedup already removed retransmitted packets, so
                // this only fires for a publisher reusing message numbers.
                SeqOutcome::Duplicate => continue,
            };
            self.counters.messages_delivered += 1;
            if delivery == Delivery::Late {
                self.counters.messages_delivered_late += 1;
            }
            self.sink.on_event(&event, delivery);
        }

        let elapsed = meta.rx_instant.elapsed();
        self.processing_latency
            .record(u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX));
    }

    /// Copies all statistics.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            counters: self.counters,
            packet_seq: self.packet_seq.stats(),
            message_seq: self.message_seq.stats(),
            wire_latency: self.wire_latency.summary(),
            processing_latency: self.processing_latency.summary(),
        }
    }

    /// The sink.
    pub fn sink(&self) -> &K {
        &self.sink
    }

    /// Consumes the processor and returns the sink.
    pub fn into_sink(self) -> K {
        self.sink
    }
}

/// Conditions under which [`run`] returns on its own.
#[derive(Clone, Copy, Debug, Default)]
pub struct RunLimits {
    /// Stop after this many datagrams (valid or not).
    pub max_datagrams: Option<u64>,
    /// Stop after this much wall time.
    pub max_duration: Option<Duration>,
    /// Invoke the report callback at this interval.
    pub report_interval: Option<Duration>,
}

/// Why [`run`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The stop flag was set.
    Signalled,
    /// `max_datagrams` was reached.
    DatagramLimit,
    /// `max_duration` elapsed.
    DurationLimit,
}

/// Receive loop: polls `source` and feeds every datagram to `processor` until
/// a limit is reached or `stop` is set.
///
/// `stop` is checked between polls, so shutdown latency is bounded by the
/// source's poll timeout. `on_report` runs on this thread between polls.
pub fn run<S, K, R>(
    source: &mut S,
    processor: &mut FeedProcessor<K>,
    stop: &AtomicBool,
    limits: RunLimits,
    mut on_report: R,
) -> io::Result<StopReason>
where
    S: PacketSource,
    K: EventSink,
    R: FnMut(&FeedProcessor<K>),
{
    let start = Instant::now();
    let deadline = limits.max_duration.map(|d| start + d);
    let mut next_report = limits.report_interval.map(|i| start + i);

    loop {
        // Relaxed is sufficient: the flag carries no data that must become
        // visible together with it; we only need to see it eventually.
        if stop.load(Ordering::Relaxed) {
            return Ok(StopReason::Signalled);
        }
        if limits
            .max_datagrams
            .is_some_and(|max| processor.counters.datagrams >= max)
        {
            return Ok(StopReason::DatagramLimit);
        }
        let now = Instant::now();
        if deadline.is_some_and(|d| now >= d) {
            return Ok(StopReason::DurationLimit);
        }
        if let (Some(due), Some(interval)) = (next_report, limits.report_interval)
            && now >= due
        {
            on_report(processor);
            next_report = Some(due + interval);
        }

        source.poll(|bytes, meta| processor.process_packet(bytes, meta))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caml_protocol::{MessageType, PacketWriter, Price, packet_len};

    fn meta() -> RxMeta {
        RxMeta {
            rx_wall_ns: 1_000_000,
            rx_instant: Instant::now(),
        }
    }

    fn packet(session: u32, packet_seq: u64, msg_seqs: &[u64]) -> Vec<u8> {
        let mut buf = vec![0u8; packet_len(msg_seqs.len() as u16)];
        let mut w = PacketWriter::new(&mut buf, session, packet_seq).unwrap();
        for &seq in msg_seqs {
            w.push(&MarketEvent {
                msg_type: MessageType::Trade,
                instrument_id: 1,
                seq,
                event_time_ns: 0,
                price: Price::from_raw(1),
                quantity: 1,
            })
            .unwrap();
        }
        w.finish(999_000);
        buf
    }

    /// Records `(seq, delivery)` for every delivered event.
    #[derive(Debug, Default)]
    struct Log(Vec<(u64, Delivery)>);

    impl Log {
        fn log(&self) -> &[(u64, Delivery)] {
            &self.0
        }
    }

    impl EventSink for Log {
        fn on_event(&mut self, e: &MarketEvent, d: Delivery) {
            self.0.push((e.seq, d));
        }
    }

    fn processor() -> FeedProcessor<Log> {
        FeedProcessor::new(Log::default())
    }

    #[test]
    fn delivers_in_order_messages() {
        let mut p = processor();
        p.process_packet(&packet(1, 1, &[1, 2]), meta());
        p.process_packet(&packet(1, 2, &[3]), meta());
        assert_eq!(
            p.sink().log(),
            &[
                (1, Delivery::InOrder),
                (2, Delivery::InOrder),
                (3, Delivery::InOrder)
            ]
        );
        let s = p.snapshot();
        assert_eq!(s.counters.messages_delivered, 3);
        assert_eq!(s.wire_latency.count, 2);
        assert_eq!(s.wire_latency.max, 1_000);
    }

    #[test]
    fn malformed_is_counted_not_delivered() {
        let mut p = processor();
        let mut bad = packet(1, 1, &[1]);
        bad[0] = 0;
        p.process_packet(&bad, meta());
        p.process_packet(&[], meta());
        let c = p.snapshot().counters;
        assert_eq!((c.datagrams, c.malformed, c.messages_delivered), (2, 2, 0));
        assert_eq!(
            c.malformed_by_kind[caml_protocol::DecodeErrorKind::BadMagic.index()],
            1
        );
        assert_eq!(
            c.malformed_by_kind[caml_protocol::DecodeErrorKind::TooShort.index()],
            1
        );
    }

    #[test]
    fn duplicate_packet_is_dropped_once() {
        let mut p = processor();
        let pkt = packet(1, 1, &[1, 2]);
        p.process_packet(&pkt, meta());
        p.process_packet(&pkt, meta());
        let s = p.snapshot();
        assert_eq!(s.counters.duplicate_packets, 1);
        assert_eq!(s.counters.messages_delivered, 2);
        assert_eq!(s.message_seq.duplicates, 0);
    }

    #[test]
    fn reordered_packets_are_tagged() {
        let mut p = processor();
        p.process_packet(&packet(1, 1, &[1]), meta());
        p.process_packet(&packet(1, 3, &[3]), meta());
        p.process_packet(&packet(1, 2, &[2]), meta());
        assert_eq!(
            p.sink().log(),
            &[
                (1, Delivery::InOrder),
                (3, Delivery::AfterGap),
                (2, Delivery::Late)
            ]
        );
        let s = p.snapshot();
        assert_eq!(s.packet_seq.recovered, 1);
        assert_eq!(s.message_seq.outstanding, 0);
        assert_eq!(s.counters.messages_delivered_late, 1);
    }

    #[test]
    fn session_change_resets_sequencing() {
        let mut p = processor();
        p.process_packet(&packet(1, 1, &[1]), meta());
        p.process_packet(&packet(1, 2, &[2]), meta());
        p.process_packet(&packet(2, 1, &[1]), meta());
        let s = p.snapshot();
        assert_eq!(s.counters.session_changes, 1);
        assert_eq!(s.counters.duplicate_packets, 0);
        assert_eq!(s.counters.messages_delivered, 3);
    }

    #[test]
    fn heartbeat_advances_packet_sequence() {
        let mut p = processor();
        p.process_packet(&packet(1, 1, &[1]), meta());
        p.process_packet(&packet(1, 2, &[]), meta());
        p.process_packet(&packet(1, 3, &[2]), meta());
        let s = p.snapshot();
        assert_eq!(s.counters.heartbeats, 1);
        assert_eq!(s.packet_seq.gaps, 0);
        assert_eq!(s.message_seq.gaps, 0);
    }

    #[test]
    fn future_send_time_is_a_clock_anomaly() {
        let mut p = processor();
        let m = RxMeta {
            rx_wall_ns: 1,
            rx_instant: Instant::now(),
        };
        p.process_packet(&packet(1, 1, &[1]), m);
        let s = p.snapshot();
        assert_eq!(s.counters.clock_anomalies, 1);
        assert_eq!(s.wire_latency.count, 0);
        assert_eq!(s.counters.messages_delivered, 1);
    }
}
