use std::fmt;

use caml_protocol::{
    EncodeError, MAX_PAYLOAD_1500_MTU, MarketEvent, MessageType, PRICE_SCALE, PacketWriter, Price,
    max_messages_for_payload,
};

use crate::SplitMix64;

/// Synthetic feed parameters.
#[derive(Clone, Debug)]
pub struct FeedConfig {
    /// Publisher session id written into every packet.
    pub session_id: u32,
    /// Number of instruments, with ids `1..=instruments`.
    pub instruments: u32,
    /// Messages per packet. Zero produces heartbeats only.
    pub msgs_per_packet: u16,
    /// Largest packet the simulator may produce, in bytes.
    pub max_payload: usize,
    /// RNG seed.
    pub seed: u64,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self {
            session_id: 1,
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
/// Packet and message sequence numbers both start at 1 and increase by one.
#[derive(Debug)]
pub struct FeedSimulator {
    config: FeedConfig,
    rng: SplitMix64,
    mids: Vec<i64>,
    next_packet_seq: u64,
    next_msg_seq: u64,
}

impl FeedSimulator {
    /// Validates `config` and creates a simulator.
    pub fn new(config: FeedConfig) -> Result<Self, ConfigError> {
        if config.instruments == 0 {
            return Err(ConfigError("instruments must be at least 1".into()));
        }
        let max_msgs = max_messages_for_payload(config.max_payload);
        if usize::from(config.msgs_per_packet) > max_msgs {
            return Err(ConfigError(format!(
                "msgs_per_packet {} exceeds {} that fit in a {}-byte payload",
                config.msgs_per_packet, max_msgs, config.max_payload
            )));
        }
        Ok(Self {
            rng: SplitMix64::new(config.seed),
            mids: vec![START_MID; config.instruments as usize],
            next_packet_seq: 1,
            next_msg_seq: 1,
            config,
        })
    }

    /// Configuration in use.
    pub fn config(&self) -> &FeedConfig {
        &self.config
    }

    /// Sequence number the next packet will carry.
    pub fn next_packet_seq(&self) -> u64 {
        self.next_packet_seq
    }

    /// Sequence number the next message will carry.
    pub fn next_msg_seq(&self) -> u64 {
        self.next_msg_seq
    }

    /// Encodes the next packet into `buf` and returns its length.
    ///
    /// `now_ns` is used both as the event time and as the send timestamp;
    /// callers should pass a fresh clock reading immediately before sending.
    pub fn next_packet(&mut self, buf: &mut [u8], now_ns: u64) -> Result<usize, EncodeError> {
        let mut w = PacketWriter::new(buf, self.config.session_id, self.next_packet_seq)?;
        for _ in 0..self.config.msgs_per_packet {
            let event = self.next_event(now_ns);
            w.push(&event)?;
            self.next_msg_seq += 1;
        }
        self.next_packet_seq += 1;
        Ok(w.finish(now_ns))
    }

    /// Encodes a heartbeat (a packet with no messages) into `buf` and returns
    /// its length. Heartbeats consume a packet sequence number, so a receiver
    /// can detect loss of the data packet before them.
    pub fn next_heartbeat(&mut self, buf: &mut [u8], now_ns: u64) -> Result<usize, EncodeError> {
        let w = PacketWriter::new(buf, self.config.session_id, self.next_packet_seq)?;
        self.next_packet_seq += 1;
        Ok(w.finish(now_ns))
    }

    fn next_event(&mut self, now_ns: u64) -> MarketEvent {
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
            seq: self.next_msg_seq,
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
        assert!(
            FeedSimulator::new(FeedConfig {
                instruments: 0,
                ..FeedConfig::default()
            })
            .is_err()
        );
        assert!(
            FeedSimulator::new(FeedConfig {
                msgs_per_packet: 37,
                ..FeedConfig::default()
            })
            .is_err()
        );
    }

    #[test]
    fn produces_contiguous_valid_packets() {
        let mut sim = FeedSimulator::new(FeedConfig::default()).unwrap();
        let mut buf = [0u8; MAX_PAYLOAD_1500_MTU];
        let mut expected_msg = 1;
        for expected_pkt in 1..=100u64 {
            let len = sim.next_packet(&mut buf, 42).unwrap();
            let view = parse_packet(&buf[..len]).unwrap();
            assert_eq!(view.header().packet_seq, expected_pkt);
            for ev in view.messages() {
                assert_eq!(ev.seq, expected_msg);
                assert!((1..=8).contains(&ev.instrument_id));
                assert!(ev.price.raw() > 0);
                expected_msg += 1;
            }
        }
    }

    #[test]
    fn heartbeat_consumes_packet_seq_only() {
        let mut sim = FeedSimulator::new(FeedConfig::default()).unwrap();
        let mut buf = [0u8; MAX_PAYLOAD_1500_MTU];
        sim.next_packet(&mut buf, 1).unwrap();
        let len = sim.next_heartbeat(&mut buf, 2).unwrap();
        let view = parse_packet(&buf[..len]).unwrap();
        assert_eq!(view.header().packet_seq, 2);
        assert_eq!(view.header().msg_count, 0);
        assert_eq!(sim.next_packet_seq(), 3);
        assert_eq!(sim.next_msg_seq(), 5);
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
