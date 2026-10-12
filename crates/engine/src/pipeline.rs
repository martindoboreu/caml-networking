#![deny(clippy::arithmetic_side_effects)]

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use caml_protocol::{MarketEvent, PacketHeader, parse_packet};

use crate::histogram::LatencyHistogram;
use crate::metrics::{Counters, SessionInfo, Snapshot};
use crate::sequence::{Admission, SeqConfig, SeqOutcome, SequenceTracker};
use crate::transport::{PacketSource, RxMeta};

/// Ordering context attached to every delivered event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Next expected message.
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
/// Within a session, every message sequence number is delivered at most
/// once, tagged with its [`Delivery`] status, so the consumer, not the
/// engine, decides how to treat out-of-order data. Messages that are never
/// delivered are always counted (see [`Snapshot::integrity`]).
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

/// Tunables for a [`FeedProcessor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessorConfig {
    /// Message sequencing.
    pub seq: SeqConfig,
    /// A different session epoch is adopted after this many consecutive
    /// packets from it (no current-session packet in between). Values below 1
    /// act as 1.
    pub session_confirm_packets: u32,
    /// If the current session has been silent this long, a *newer* epoch is
    /// adopted immediately, and an older one after confirmation. Must exceed
    /// the publisher's heartbeat interval.
    pub session_idle_switch: Duration,
    /// Packets whose `session_epoch` is further than this ahead of the
    /// receiver's wall clock are rejected as implausible.
    pub max_epoch_future_skew: Duration,
    /// Record processing latency for one packet in every N (each sample
    /// costs a clock read). 0 disables it.
    pub processing_sample_every: u32,
}

impl Default for ProcessorConfig {
    fn default() -> Self {
        Self {
            seq: SeqConfig::default(),
            session_confirm_packets: 3,
            session_idle_switch: Duration::from_millis(500),
            max_epoch_future_skew: Duration::from_secs(300),
            processing_sample_every: 1,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Session {
    epoch: u64,
    last_rx: Instant,
    ended: bool,
}

/// Decodes, validates, and accounts for datagrams; forwards events to a sink.
///
/// Transport-agnostic: it accepts byte slices and metadata from any
/// [`PacketSource`].
#[derive(Debug)]
pub struct FeedProcessor<K> {
    sink: K,
    config: ProcessorConfig,
    session: Option<Session>,
    /// Epoch competing with the current session, and its consecutive packets.
    candidate: Option<(u64, u32)>,
    seq: SequenceTracker,
    counters: Counters,
    send_to_rx_latency: LatencyHistogram,
    kernel_to_rx_latency: LatencyHistogram,
    processing_latency: LatencyHistogram,
    sample_countdown: u32,
    last_rx: Option<Instant>,
}

#[inline]
fn bump(counter: &mut u64) {
    *counter = counter.saturating_add(1);
}

impl<K: EventSink> FeedProcessor<K> {
    /// Creates a processor with the default configuration.
    pub fn new(sink: K) -> Self {
        Self::with_config(sink, ProcessorConfig::default())
    }

    /// Creates a processor. All memory is allocated here.
    pub fn with_config(sink: K, config: ProcessorConfig) -> Self {
        Self {
            sink,
            config,
            session: None,
            candidate: None,
            seq: SequenceTracker::new(config.seq),
            counters: Counters::default(),
            send_to_rx_latency: LatencyHistogram::new(),
            kernel_to_rx_latency: LatencyHistogram::new(),
            processing_latency: LatencyHistogram::new(),
            sample_countdown: 1,
            last_rx: None,
        }
    }

    /// Configuration in use.
    pub fn config(&self) -> &ProcessorConfig {
        &self.config
    }

    /// Processes one datagram. Never panics on malformed input and never
    /// allocates.
    pub fn process_packet(&mut self, bytes: &[u8], meta: RxMeta) {
        bump(&mut self.counters.datagrams);
        self.counters.bytes = self.counters.bytes.saturating_add(bytes.len() as u64);
        self.last_rx = Some(meta.rx_instant);

        let packet = match parse_packet(bytes) {
            Ok(p) => p,
            Err(e) => {
                bump(&mut self.counters.malformed);
                if let Some(c) = self.counters.malformed_by_kind.get_mut(e.kind().index()) {
                    bump(c);
                }
                return;
            }
        };
        let header = *packet.header();

        if !self.admit_session(&header, &meta) {
            return;
        }

        match meta.rx_wall_ns.checked_sub(header.send_time_ns) {
            Some(latency) => self.send_to_rx_latency.record(latency),
            None => bump(&mut self.counters.clock_anomalies),
        }
        if let Some(kernel_ns) = meta.rx_kernel_ns {
            match meta.rx_wall_ns.checked_sub(kernel_ns) {
                Some(queued) => self.kernel_to_rx_latency.record(queued),
                None => bump(&mut self.counters.clock_anomalies),
            }
        }

        if self.seq.admit(header.first_seq, header.end_seq()) == Admission::Implausible {
            return;
        }

        if header.is_heartbeat() {
            bump(&mut self.counters.heartbeats);
            self.seq.observe_next_seq(header.first_seq);
            if header.end_of_session
                && let Some(s) = self.session.as_mut()
                && !s.ended
            {
                s.ended = true;
                bump(&mut self.counters.sessions_ended);
            }
        } else {
            let mut delivered_any = false;
            for event in packet.messages() {
                let delivery = match self.seq.observe(event.seq) {
                    SeqOutcome::InOrder => Delivery::InOrder,
                    SeqOutcome::Gap { .. } => Delivery::AfterGap,
                    SeqOutcome::Late => Delivery::Late,
                    // Counted by the tracker; never delivered.
                    SeqOutcome::Duplicate | SeqOutcome::BeforeBaseline => continue,
                };
                delivered_any = true;
                bump(&mut self.counters.messages_delivered);
                if delivery == Delivery::Late {
                    bump(&mut self.counters.messages_delivered_late);
                }
                self.sink.on_event(&event, delivery);
            }
            if !delivered_any {
                bump(&mut self.counters.redundant_packets);
            }
        }

        self.sample_processing(meta.rx_instant);
    }

    #[inline]
    fn sample_processing(&mut self, rx_instant: Instant) {
        let every = self.config.processing_sample_every;
        if every == 0 {
            return;
        }
        if self.sample_countdown <= 1 {
            self.sample_countdown = every;
            let ns = u64::try_from(rx_instant.elapsed().as_nanos()).unwrap_or(u64::MAX);
            self.processing_latency.record(ns);
        } else {
            self.sample_countdown = self.sample_countdown.saturating_sub(1);
        }
    }

    /// Session gate. Returns whether the packet belongs to the (possibly
    /// newly adopted) current session.
    fn admit_session(&mut self, header: &PacketHeader, meta: &RxMeta) -> bool {
        let epoch = header.session_epoch;
        let skew_ns =
            u64::try_from(self.config.max_epoch_future_skew.as_nanos()).unwrap_or(u64::MAX);
        if epoch > meta.rx_wall_ns.saturating_add(skew_ns) {
            bump(&mut self.counters.implausible_epoch_packets);
            return false;
        }

        let Some(current) = self.session else {
            self.start_session(epoch, meta.rx_instant);
            return true;
        };
        if current.epoch == epoch {
            if let Some(s) = self.session.as_mut() {
                s.last_rx = meta.rx_instant;
            }
            self.candidate = None;
            return true;
        }

        let count = match self.candidate {
            Some((e, n)) if e == epoch => n.saturating_add(1),
            _ => 1,
        };
        self.candidate = Some((epoch, count));
        let confirmed = count >= self.config.session_confirm_packets.max(1);
        let idle = meta.rx_instant.saturating_duration_since(current.last_rx)
            >= self.config.session_idle_switch;
        let newer = epoch > current.epoch;
        // A newer epoch wins at once if the current session said goodbye or
        // went quiet; otherwise it must prove itself. An older epoch is only
        // adopted when the current one is quiet *and* the old one is
        // demonstrably alive, which un-sticks a receiver that latched onto a
        // stray packet without letting in-flight stale packets flap state.
        let switch = if newer {
            current.ended || idle || confirmed
        } else {
            idle && confirmed
        };
        if switch {
            bump(&mut self.counters.session_changes);
            if !current.ended {
                // The previous publisher never said goodbye (crash, kill, or
                // lost end-of-session): loss of its final packets would be
                // undetectable, so say so.
                bump(&mut self.counters.unterminated_sessions);
            }
            self.seq.reset();
            self.start_session(epoch, meta.rx_instant);
            true
        } else {
            if newer {
                bump(&mut self.counters.unconfirmed_session_packets);
            } else {
                bump(&mut self.counters.stale_session_packets);
            }
            false
        }
    }

    fn start_session(&mut self, epoch: u64, now: Instant) {
        self.session = Some(Session {
            epoch,
            last_rx: now,
            ended: false,
        });
        self.candidate = None;
    }

    /// The session currently followed, if any.
    pub fn session(&self) -> Option<SessionInfo> {
        self.session.map(|s| SessionInfo {
            epoch: s.epoch,
            ended: s.ended,
        })
    }

    /// Copies all statistics.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            counters: self.counters,
            seq: self.seq.stats(),
            session: self.session(),
            send_to_rx_latency: self.send_to_rx_latency.summary(),
            kernel_to_rx_latency: self.kernel_to_rx_latency.summary(),
            processing_latency: self.processing_latency.summary(),
        }
    }

    /// Number of datagrams processed so far (valid or not).
    pub fn datagrams(&self) -> u64 {
        self.counters.datagrams
    }

    /// Live counters (cheap; [`snapshot`](Self::snapshot) also summarizes the
    /// latency histograms).
    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// Message-sequence statistics (cheap).
    pub fn seq_stats(&self) -> crate::sequence::SeqStats {
        self.seq.stats()
    }

    /// Receive time of the most recent datagram, if any.
    pub fn last_rx(&self) -> Option<Instant> {
        self.last_rx
    }

    /// Clears the latency histograms, e.g. after a warmup period. Counters
    /// and sequence state are unaffected.
    pub fn reset_latency(&mut self) {
        self.send_to_rx_latency.reset();
        self.kernel_to_rx_latency.reset();
        self.processing_latency.reset();
    }

    /// The sink.
    pub fn sink(&self) -> &K {
        &self.sink
    }

    /// The sink, mutably (e.g. to drain what it collected).
    pub fn sink_mut(&mut self) -> &mut K {
        &mut self.sink
    }

    /// Consumes the processor and returns the sink.
    pub fn into_sink(self) -> K {
        self.sink
    }
}

/// Conditions under which [`run`] returns on its own.
#[derive(Clone, Copy, Debug, Default)]
pub struct RunLimits {
    /// Stop once this many datagrams (valid or not) have been processed.
    /// Checked between polls, so a batched backend may overshoot by up to one
    /// batch.
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
///
/// The loop reads the clock only when a poll delivered nothing; otherwise it
/// reuses the receive timestamp the transport already took, so it adds no
/// per-packet clock reads of its own.
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
    let deadline = limits.max_duration.and_then(|d| start.checked_add(d));
    let mut next_report = limits.report_interval.and_then(|i| start.checked_add(i));
    let mut now = start;

    loop {
        // Relaxed is sufficient: the flag carries no data that must become
        // visible together with it; we only need to see it eventually.
        if stop.load(Ordering::Relaxed) {
            return Ok(StopReason::Signalled);
        }
        if limits
            .max_datagrams
            .is_some_and(|max| processor.datagrams() >= max)
        {
            return Ok(StopReason::DatagramLimit);
        }
        if deadline.is_some_and(|d| now >= d) {
            return Ok(StopReason::DurationLimit);
        }
        if let (Some(due), Some(interval)) = (next_report, limits.report_interval)
            && now >= due
        {
            on_report(processor);
            // Skip missed slots instead of firing a burst of catch-up reports.
            let mut next = due;
            while next <= now {
                match next.checked_add(interval) {
                    Some(n) if n > next => next = n,
                    _ => break,
                }
            }
            next_report = Some(next);
        }

        let delivered = source.poll(|bytes, meta| processor.process_packet(bytes, meta))?;
        now = match processor.last_rx() {
            Some(t) if delivered > 0 => t,
            _ => Instant::now(),
        };
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use caml_protocol::{MessageType, PacketWriter, Price, packet_len, write_heartbeat};

    const T0: u64 = 1_000_000_000;

    fn meta_at(rx_instant: Instant) -> RxMeta {
        let mut m = RxMeta::now();
        m.rx_wall_ns = T0;
        m.rx_instant = rx_instant;
        m
    }

    fn meta() -> RxMeta {
        meta_at(Instant::now())
    }

    fn packet(epoch: u64, seqs: std::ops::RangeInclusive<u64>) -> Vec<u8> {
        let seqs: Vec<u64> = seqs.collect();
        let mut buf = vec![0u8; packet_len(seqs.len() as u16)];
        let mut w = PacketWriter::new(&mut buf, epoch, seqs[0]).unwrap();
        for &seq in &seqs {
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
        w.finish(T0 - 1_000);
        buf
    }

    fn heartbeat(epoch: u64, next_seq: u64, eos: bool) -> Vec<u8> {
        let mut buf = vec![0u8; 32];
        write_heartbeat(&mut buf, epoch, next_seq, eos, T0 - 1_000).unwrap();
        buf
    }

    /// Records `(seq, delivery)` for every delivered event.
    #[derive(Debug, Default)]
    struct Log(Vec<(u64, Delivery)>);

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
        p.process_packet(&packet(1, 1..=2), meta());
        p.process_packet(&packet(1, 3..=3), meta());
        assert_eq!(
            p.sink().0,
            [
                (1, Delivery::InOrder),
                (2, Delivery::InOrder),
                (3, Delivery::InOrder)
            ]
        );
        let s = p.snapshot();
        assert_eq!(s.counters.messages_delivered, 3);
        assert_eq!(s.send_to_rx_latency.count, 2);
        assert_eq!(s.send_to_rx_latency.max, 1_000);
        assert!(s.integrity().is_clean());
    }

    #[test]
    fn malformed_is_counted_not_delivered() {
        let mut p = processor();
        let mut bad = packet(1, 1..=1);
        bad[0] = 0;
        p.process_packet(&bad, meta());
        p.process_packet(&[], meta());
        let s = p.snapshot();
        let c = s.counters;
        assert_eq!((c.datagrams, c.malformed, c.messages_delivered), (2, 2, 0));
        assert_eq!(
            c.malformed_by_kind[caml_protocol::DecodeErrorKind::BadMagic.index()],
            1
        );
        assert_eq!(
            c.malformed_by_kind[caml_protocol::DecodeErrorKind::TooShort.index()],
            1
        );
        assert!(!s.integrity().is_clean());
    }

    #[test]
    fn duplicate_packet_is_dropped_once() {
        let mut p = processor();
        let pkt = packet(1, 1..=2);
        p.process_packet(&pkt, meta());
        p.process_packet(&pkt, meta());
        let s = p.snapshot();
        assert_eq!(s.counters.redundant_packets, 1);
        assert_eq!(s.counters.messages_delivered, 2);
        assert_eq!(s.seq.duplicates, 2);
    }

    #[test]
    fn reordered_packets_are_tagged() {
        let mut p = processor();
        p.process_packet(&packet(1, 1..=1), meta());
        p.process_packet(&packet(1, 3..=3), meta());
        p.process_packet(&packet(1, 2..=2), meta());
        assert_eq!(
            p.sink().0,
            [
                (1, Delivery::InOrder),
                (3, Delivery::AfterGap),
                (2, Delivery::Late)
            ]
        );
        let s = p.snapshot();
        assert_eq!(s.seq.recovered, 1);
        assert_eq!(s.seq.outstanding, 0);
        assert_eq!(s.counters.messages_delivered_late, 1);
        assert!(s.integrity().is_clean());
    }

    #[test]
    fn heartbeat_reveals_tail_loss_and_eos_ends_session() {
        let mut p = processor();
        p.process_packet(&packet(1, 1..=2), meta());
        // Messages 3..=4 lost; the heartbeat says the next one is 5.
        p.process_packet(&heartbeat(1, 5, false), meta());
        p.process_packet(&heartbeat(1, 5, true), meta());
        let s = p.snapshot();
        assert_eq!(s.counters.heartbeats, 2);
        assert_eq!(s.counters.sessions_ended, 1);
        assert_eq!(s.seq.outstanding, 2);
        let v = s.integrity();
        assert_eq!(v.lost_messages, 2);
        assert!(v.session_ended && !v.is_clean());
    }

    #[test]
    fn newer_epoch_after_end_of_session_switches_at_once() {
        let mut p = processor();
        p.process_packet(&packet(1, 1..=1), meta());
        p.process_packet(&heartbeat(1, 2, true), meta());
        p.process_packet(&packet(2, 1..=1), meta());
        let s = p.snapshot();
        assert_eq!(s.counters.session_changes, 1);
        assert_eq!(s.session.map(|x| x.epoch), Some(2));
        assert_eq!(s.counters.messages_delivered, 2);
        assert_eq!(s.counters.unterminated_sessions, 0);
        assert!(s.integrity().is_clean());
    }

    #[test]
    fn stray_newer_epoch_is_ignored_while_session_is_active() {
        let mut p = processor();
        let t = Instant::now();
        p.process_packet(&packet(1, 1..=1), meta_at(t));
        p.process_packet(&packet(9, 1..=1), meta_at(t));
        p.process_packet(&packet(1, 2..=2), meta_at(t));
        let s = p.snapshot();
        assert_eq!(s.counters.session_changes, 0);
        assert_eq!(s.counters.unconfirmed_session_packets, 1);
        assert_eq!(p.sink().0, [(1, Delivery::InOrder), (2, Delivery::InOrder)]);
    }

    #[test]
    fn newer_epoch_after_idle_period_switches_at_once() {
        let mut p = processor();
        let t = Instant::now();
        p.process_packet(&packet(1, 1..=1), meta_at(t));
        p.process_packet(&packet(2, 1..=1), meta_at(t + Duration::from_secs(1)));
        assert_eq!(p.snapshot().counters.session_changes, 1);
    }

    #[test]
    fn crash_restart_is_adopted_after_confirmation() {
        let mut p = processor();
        let t = Instant::now();
        p.process_packet(&packet(1, 1..=1), meta_at(t));
        for k in 1..=3 {
            p.process_packet(&packet(2, k..=k), meta_at(t));
        }
        let s = p.snapshot();
        assert_eq!(s.counters.session_changes, 1);
        assert_eq!(s.counters.unconfirmed_session_packets, 2);
        // The two unconfirmed packets are visible as loss in the new session.
        assert_eq!(s.seq.outstanding, 2);
        // The crashed session never said goodbye.
        assert_eq!(s.counters.unterminated_sessions, 1);
        assert_eq!(s.integrity().unterminated_sessions, 1);
    }

    #[test]
    fn epoch_far_in_the_future_is_rejected() {
        let mut p = processor();
        p.process_packet(&packet(u64::MAX, 1..=1), meta());
        let s = p.snapshot();
        assert_eq!(s.counters.implausible_epoch_packets, 1);
        assert!(s.session.is_none());
    }

    #[test]
    fn future_send_time_is_a_clock_anomaly() {
        let mut p = processor();
        let mut m = meta();
        m.rx_wall_ns = 1;
        p.process_packet(&packet(1, 1..=1), m);
        let s = p.snapshot();
        assert_eq!(s.counters.clock_anomalies, 1);
        assert_eq!(s.send_to_rx_latency.count, 0);
        assert_eq!(s.counters.messages_delivered, 1);
    }

    #[test]
    fn processing_latency_sampling() {
        let config = ProcessorConfig {
            processing_sample_every: 4,
            ..ProcessorConfig::default()
        };
        let mut p = FeedProcessor::with_config(Log::default(), config);
        for k in 1..=8 {
            p.process_packet(&packet(1, k..=k), meta());
        }
        assert_eq!(p.snapshot().processing_latency.count, 2);

        let config = ProcessorConfig {
            processing_sample_every: 0,
            ..ProcessorConfig::default()
        };
        let mut p = FeedProcessor::with_config(Log::default(), config);
        p.process_packet(&packet(1, 1..=1), meta());
        assert_eq!(p.snapshot().processing_latency.count, 0);
    }
}
