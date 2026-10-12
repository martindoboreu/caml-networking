# CAML Market-Data Wire Protocol — Version 2

Status: **implemented, tested** (`crates/protocol`). The golden byte fixtures
in `crates/protocol/tests/wire.rs` are the executable form of this document;
if the two disagree, that is a bug.

Version 2 replaced version 1 on 2026-10-09 after an audit found that v1's
sequencing could not detect tail loss, order publisher restarts, or tell
reordering at session start from duplication (see `docs/decisions.md`, D3).
v1 packets are rejected as `UnsupportedVersion` and counted.

## Transport framing

* One UDP datagram carries exactly one packet.
* A packet is a 32-byte header followed by `msg_count` messages of 40 bytes.
* Packet length must be exactly `32 + 40 * msg_count`. Anything else is
  rejected.
* Senders must keep packets at or below 1472 bytes (one unfragmented IPv4
  datagram on a 1500-byte MTU), which allows at most **36 messages per
  packet**. Receivers accept any length up to the UDP maximum.

## Byte order

All multi-byte integers are **little-endian**.

Rationale: every host we target (x86-64, AArch64) is little-endian, so decoding
is a plain load with no byte swap. CME's MDP 3.0 / SBE feeds make the same
choice. (NASDAQ ITCH/MoldUDP64 use big-endian; both choices are common. A byte
swap costs about one cycle, so this is a clarity decision, not a performance
one.)

## Packet header (32 bytes)

| Offset | Size | Type  | Field           | Rules |
|-------:|-----:|-------|-----------------|-------|
| 0      | 2    | bytes | `magic`         | ASCII `"CM"` (`0x43 0x4D`) |
| 2      | 1    | u8    | `version`       | `2` |
| 3      | 1    | u8    | `flags`         | bit 0 `END_OF_SESSION`; all other bits must be 0. `END_OF_SESSION` is only valid when `msg_count == 0` |
| 4      | 2    | u16   | `msg_count`     | `0` = heartbeat |
| 6      | 2    | bytes | reserved        | Must be `0` |
| 8      | 8    | u64   | `session_epoch` | Non-zero. Publisher start time, ns since the Unix epoch. A restarted publisher must use a **strictly larger** value |
| 16     | 8    | u64   | `first_seq`     | Non-zero. Data packet: `seq` of its first message. Heartbeat: the `seq` the publisher's next message will carry. `first_seq + msg_count` must not overflow `u64` |
| 24     | 8    | u64   | `send_time_ns`  | Publisher wall clock, ns since the Unix epoch, taken immediately before the send call |

Every 8-byte field is naturally aligned relative to the start of the
datagram.

## Message (40 bytes)

| Offset | Size | Type | Field           | Rules |
|-------:|-----:|------|-----------------|-------|
| 0      | 2    | u16  | `length`        | Must be `40` |
| 2      | 1    | u8   | `msg_type`      | `1` Trade, `2` BidUpdate, `3` AskUpdate |
| 3      | 1    | u8   | `flags`         | Must be `0` (reserved) |
| 4      | 4    | u32  | `instrument_id` | Publisher-assigned |
| 8      | 8    | u64  | `seq`           | Must equal `first_seq + index` (messages in a packet are contiguous) |
| 16     | 8    | u64  | `event_time_ns` | Source event time, ns since Unix epoch |
| 24     | 8    | i64  | `price`         | Fixed point, 1e-9 units: `100.25` → `100_250_000_000` |
| 32     | 8    | u64  | `quantity`      | Integer lots |

Header and message sizes are multiples of 8, so every 8-byte field is
naturally aligned relative to the start of the datagram. The decoder does not
rely on this (it uses byte-wise loads), but it keeps the layout friendly to
hardware parsers.

### Price representation

`i64` with scale 10⁹ covers ±9.2 × 10⁹ currency units at nanounit resolution.
Floating point is not used: binary floats cannot represent most decimal prices
exactly, and arithmetic on them is not associative.

## Validation and error handling

`parse_packet` validates the **entire** packet before returning anything:
magic, version, flags (including `END_OF_SESSION` only on heartbeats),
reserved bytes, non-zero `session_epoch`, a valid `first_seq` range, exact
length, and each message's length, type, flags, and sequence number. If any
check fails, the whole datagram is rejected with a specific `DecodeError`
(12 kinds), and the receiver counts it by kind. No message from a rejected
packet is delivered.

v2 receivers **reject unknown message types**. The `length` field exists so
that a future version can define skip-unknown semantics, but v2 prefers loud
failure to silently ignoring data.

## Sequencing semantics (MoldUDP64 model)

* One sequence space per session: message `seq`, starting at **1**.
* A data packet carries the contiguous range `[first_seq, first_seq + msg_count)`.
* A heartbeat carries no messages; its `first_seq` is the next `seq` the
  publisher will use. A receiver that has seen less than that knows the
  difference was lost. **This is how loss of the final packets before an idle
  period is detected.** Publishers send a heartbeat after every idle interval
  (`caml-feed --heartbeat-ms`, default 100 ms).
* The last packet of a session is a heartbeat with `END_OF_SESSION`. It tells
  the receiver the session is complete (no undetectable tail) and that a newer
  session may be adopted immediately.
* `session_epoch` orders publisher runs. Receivers follow one session at a
  time and only move to a different epoch under the rules in
  `docs/architecture.md` §6 (session gate). A receiver rejects epochs more than
  a configurable skew (default 300 s) ahead of its own clock.
* There is no retransmission or snapshot recovery in v2. Loss is detected and
  reported, not repaired.

## Not in v2 (planned or deliberately omitted)

* **No checksum.** The UDP checksum covers corruption in transit on the
  kernel path. Raw-packet backends verify it in software (`caml-frame`,
  `ChecksumPolicy::Verify`) or trust NIC offload flags. An application-level
  CRC would only matter for storage/replay.
* No retransmission/gap-fill requests, no snapshots.
* No channel ids. A/B line arbitration works on `(session_epoch, seq)`: both
  lines carry identical packets and the receiver delivers whichever arrives
  first (planned; see `docs/hardware-plan.md`).
* No order-level (L3) messages.
