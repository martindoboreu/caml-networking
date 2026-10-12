//! Helpers shared by the engine's integration tests.

#![allow(dead_code, clippy::unwrap_used)]

use std::time::Instant;

use caml_engine::transport::RxMeta;
use caml_engine::{Delivery, EventSink};
use caml_protocol::{
    MarketEvent, MessageType, PACKET_HEADER_LEN, PacketWriter, Price, packet_len, write_heartbeat,
};

/// Receiver wall clock used by byte-level tests (ns since the Unix epoch).
pub const T0: u64 = 1_700_000_000_000_000_000;

pub fn meta_at(rx_instant: Instant) -> RxMeta {
    let mut m = RxMeta::now();
    m.rx_wall_ns = T0;
    m.rx_instant = rx_instant;
    m
}

/// A data packet carrying messages `first..first + count`.
pub fn packet(epoch: u64, first: u64, count: u16) -> Vec<u8> {
    let mut buf = vec![0u8; packet_len(count)];
    let mut w = PacketWriter::new(&mut buf, epoch, first).unwrap();
    for i in 0..u64::from(count) {
        w.push(&MarketEvent {
            msg_type: MessageType::Trade,
            instrument_id: 1,
            seq: first + i,
            event_time_ns: 0,
            price: Price::from_raw(1),
            quantity: 1,
        })
        .unwrap();
    }
    w.finish(T0 - 1_000);
    buf
}

pub fn heartbeat(epoch: u64, next_seq: u64, end_of_session: bool) -> Vec<u8> {
    let mut buf = vec![0u8; PACKET_HEADER_LEN];
    write_heartbeat(&mut buf, epoch, next_seq, end_of_session, T0 - 1_000).unwrap();
    buf
}

/// Records `(seq, delivery)` for every delivered event.
#[derive(Debug, Default)]
pub struct Log(pub Vec<(u64, Delivery)>);

impl EventSink for Log {
    fn on_event(&mut self, e: &MarketEvent, d: Delivery) {
        self.0.push((e.seq, d));
    }
}

impl Log {
    pub fn seqs(&self) -> Vec<u64> {
        self.0.iter().map(|e| e.0).collect()
    }

    pub fn count(&self, seq: u64) -> usize {
        self.0.iter().filter(|e| e.0 == seq).count()
    }
}
