use std::fmt;

use caml_protocol::{
    EncodeError, MAX_PAYLOAD_1500_MTU, MarketEvent, MessageType, PRICE_SCALE, PacketWriter, Price,
    max_messages_for_payload, packet_len, write_heartbeat,
};

use crate::SplitMix64;

/// Synthetic feed parameters.
#[derive(Clone, Debug)]
pub struct FeedConfig {
    /// Publisher session epoch written into every packet. Must be non-zero;
    /// a real publisher uses its start time in ns since the Unix epoch.
    pub session_epoch: u64,
    /// Number of instruments, with ids `1..=instruments`.
    pub instruments: u32,
    /// Messages per data packet (at least 1).
    pub msgs_per_packet: u16,
    /// Largest packet the simulator may produce, in bytes.
    pub max_payload: usize,
    /// RNG seed.
    pub seed: u64,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self {
            session_epoch: 1,
            instruments: 8,
            msgs_per_packet: 4,
            max_payload: MAX_PAYLOAD_1500_MTU,
            seed: 0xCA41,
        }
    }
}

/// Invalid [`FeedConfig`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Tick size used for the random walk: 0.01.
const TICK: i64 = PRICE_SCALE / 100;
/// Starting mid price for every instrument: 100.00.
const START_MID: i64 = 100 * PRICE_SCALE;

/// Generates a random-walk market and encodes it into packets.
///
/// Message sequence numbers start at 1 and increase by one per message;
/// heartbeats announce the next one.
#[derive(Debug)]
pub struct FeedSimulator {
    config: FeedConfig,
    rng: SplitMix64,
    mids: Vec<i64>,
    next_seq: u64,
    data_packets: u64,
}

impl FeedSimulator {
    /// Validates `config` and creates a simulator.
    pub fn new(config: FeedConfig) -> Result<Self, ConfigError> {
        if config.session_epoch == 0 {
            return Err(ConfigError("session_epoch must be non-zero".into()));
        }
        if config.instruments == 0 {
            return Err(ConfigError("instruments must be at least 1".into()));
        }
        let max_msgs = max_messages_for_payload(config.max_payload);
        if config.msgs_per_packet == 0 || usize::from(config.msgs_per_packet) > max_msgs {
            return Err(ConfigError(format!(
                "msgs_per_packet must be 1..={max_msgs} for a {}-byte payload, got {}",
                config.max_payload, config.msgs_per_packet
            )));
        }
        Ok(Self {
            rng: SplitMix64::new(config.seed),
            mids: vec![START_MID; config.instruments as usize],
            next_seq: 1,
            data_packets: 0,
            config,
        })
    }

    /// Configuration in use.
    pub fn config(&self) -> &FeedConfig {
        &self.config
    }

    /// Sequence number the next message will carry.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Data packets generated so far.
    pub fn data_packets(&self) -> u64 {
        self.data_packets
    }

    /// Encodes the next data packet into `buf` and returns its length.
    ///
    /// `now_ns` is used as the event time and as the initial send timestamp.
    /// On error nothing is consumed: the next call produces the same
    /// sequence numbers.
    pub fn next_packet(&mut self, buf: &mut [u8], now_ns: u64) -> Result<usize, EncodeError> {
        let needed = packet_len(self.config.msgs_per_packet);
        if buf.len() < needed {
            return Err(EncodeError::BufferFull {
                needed,
                available: buf.len(),
            });
        }
        let mut w = PacketWriter::new(buf, self.config.session_epoch, self.next_seq)?;
        for _ in 0..self.config.msgs_per_packet {
            let event = self.next_event(now_ns, w.next_seq());
            w.push(&event)?;
        }
        self.next_seq = w.next_seq();
        self.data_packets += 1;
        Ok(w.finish(now_ns))
    }

    /// Encodes a heartbeat announcing the next sequence number.
    pub fn next_heartbeat(&self, buf: &mut [u8], now_ns: u64) -> Result<usize, EncodeError> {
        write_heartbeat(buf, self.config.session_epoch, self.next_seq, false, now_ns)
    }

    /// Encodes the session's final, end-of-session heartbeat.
    pub fn end_of_session(&self, buf: &mut [u8], now_ns: u64) -> Result<usize, EncodeError> {
        write_heartbeat(buf, self.config.session_epoch, self.next_seq, true, now_ns)
    }

    fn next_event(&mut self, now_ns: u64, seq: u64) -> MarketEvent {
        let idx = self.rng.below(self.mids.len() as u64) as usize;
        let step = self.rng.below(3) as i64 - 1;
        let mid = (self.mids[idx] + step * TICK).max(TICK);
        self.mids[idx] = mid;

        let (msg_type, price) = match self.rng.below(3) {
            0 => (MessageType::Trade, mid),
            1 => (MessageType::BidUpdate, mid - TICK),
            _ => (MessageType::AskUpdate, mid + TICK),
        };
        MarketEvent {
            msg_type,
            instrument_id: idx as u32 + 1,
            seq,
            event_time_ns: now_ns,
            price: Price::from_raw(price),
            quantity: 1 + self.rng.below(1000),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caml_protocol::parse_packet;

    #[test]
    fn rejects_bad_config() {
        for bad in [
            FeedConfig {
                instruments: 0,
                ..FeedConfig::default()
            },
            FeedConfig {
                msgs_per_packet: 37,
                ..FeedConfig::default()
            },
            FeedConfig {
                msgs_per_packet: 0,
                ..FeedConfig::default()
            },
            FeedConfig {
                session_epoch: 0,
                ..FeedConfig::default()
            },
        ] {
            assert!(FeedSimulator::new(bad).is_err());
        }
    }

    #[test]
    fn produces_contiguous_valid_packets() {
        let mut sim = FeedSimulator::new(FeedConfig::default()).unwrap();
        let mut buf = [0u8; MAX_PAYLOAD_1500_MTU];
        let mut expected = 1;
        for _ in 0..100 {
            let len = sim.next_packet(&mut buf, 42).unwrap();
            let view = parse_packet(&buf[..len]).unwrap();
            assert_eq!(view.header().first_seq, expected);
            for ev in view.messages() {
                assert_eq!(ev.seq, expected);
                assert!((1..=8).contains(&ev.instrument_id));
                assert!(ev.price.raw() > 0);
                expected += 1;
            }
        }
        assert_eq!(sim.data_packets(), 100);
    }

    #[test]
    fn heartbeats_announce_next_seq() {
        let mut sim = FeedSimulator::new(FeedConfig::default()).unwrap();
        let mut buf = [0u8; MAX_PAYLOAD_1500_MTU];
        sim.next_packet(&mut buf, 1).unwrap();
        let len = sim.next_heartbeat(&mut buf, 2).unwrap();
        let h = *parse_packet(&buf[..len]).unwrap().header();
        assert_eq!((h.msg_count, h.first_seq, h.end_of_session), (0, 5, false));
        let len = sim.end_of_session(&mut buf, 3).unwrap();
        let h = *parse_packet(&buf[..len]).unwrap().header();
        assert_eq!((h.first_seq, h.end_of_session), (5, true));
        assert_eq!(sim.next_seq(), 5);
    }

    #[test]
    fn short_buffer_consumes_nothing() {
        let mut sim = FeedSimulator::new(FeedConfig::default()).unwrap();
        let mut small = [0u8; 100];
        assert!(sim.next_packet(&mut small, 1).is_err());
        assert_eq!(sim.next_seq(), 1);
        assert_eq!(sim.data_packets(), 0);
    }

    #[test]
    fn same_seed_same_bytes() {
        let run = || {
            let mut sim = FeedSimulator::new(FeedConfig::default()).unwrap();
            let mut out = Vec::new();
            let mut buf = [0u8; MAX_PAYLOAD_1500_MTU];
            for _ in 0..10 {
                let len = sim.next_packet(&mut buf, 7).unwrap();
                out.extend_from_slice(&buf[..len]);
            }
            out
        };
        assert_eq!(run(), run());
    }
}
