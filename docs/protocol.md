# CAML Market-Data Wire Protocol — Version 1

Status: **implemented and tested** (`crates/protocol`). The golden byte fixture
in `crates/protocol/tests/wire.rs` is the executable form of this document; if
the two disagree, that is a bug.

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

| Offset | Size | Type  | Field          | Rules |
|-------:|-----:|-------|----------------|-------|
| 0      | 2    | bytes | `magic`        | ASCII `"CM"` (`0x43 0x4D`) |
| 2      | 1    | u8    | `version`      | `1` |
| 3      | 1    | u8    | `flags`        | Must be `0` (reserved) |
| 4      | 4    | u32   | `session_id`   | Identifies one publisher run |
| 8      | 8    | u64   | `packet_seq`   | +1 per packet within a session |
| 16     | 8    | u64   | `send_time_ns` | Publisher wall clock, ns since Unix epoch, taken just before send |
| 24     | 2    | u16   | `msg_count`    | `0` = heartbeat |
| 26     | 6    | bytes | reserved       | Must be `0` |

## Message (40 bytes)

| Offset | Size | Type | Field           | Rules |
|-------:|-----:|------|-----------------|-------|
| 0      | 2    | u16  | `length`        | Must be `40` |
| 2      | 1    | u8   | `msg_type`      | `1` Trade, `2` BidUpdate, `3` AskUpdate |
| 3      | 1    | u8   | `flags`         | Must be `0` (reserved) |
| 4      | 4    | u32  | `instrument_id` | Publisher-assigned |
| 8      | 8    | u64  | `seq`           | +1 per message within a session, contiguous across packets |
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
header magic, version, reserved bytes, exact length, and each message's
length, type, and flags. If any check fails, the whole datagram is rejected
with a specific `DecodeError`, and the receiver counts it by kind. No message
from a rejected packet is delivered.

v1 receivers **reject unknown message types**. The `length` field exists so
that a future version can define skip-unknown semantics, but v1 prefers loud
failure to silently ignoring data.

## Sequencing semantics

* Two independent sequence spaces: `packet_seq` (detects datagram loss,
  duplication, reordering) and message `seq` (detects message-level loss and
  lets consumers order events).
* Heartbeats consume a `packet_seq` but no message `seq`. They exist so that
  loss of the last data packet before an idle period is detectable.
* A change of `session_id` means both sequences restarted. Receivers reset
  their sequence state and count a session change.
* There is no retransmission or snapshot recovery in v1. Loss is detected and
  reported, not repaired.

## Not in v1 (planned or deliberately omitted)

* **No checksum.** UDP's checksum covers corruption in transit. An
  application-level CRC would only matter for storage/replay.
* No retransmission/gap-fill requests, no snapshots.
* No multicast-specific fields (channel ids, A/B line arbitration).
* No order-level (L3) messages.
