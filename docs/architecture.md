# CAML Networking Engine: Architecture

Last updated: 2026-10-09 (milestone 2: integrity hardening, protocol v2,
Linux batched backend, frame parser, benchmark harness).

Status labels used throughout: **implemented**, **tested** (automated tests
run on Windows), **CI-only** (tests exist but run only on Linux CI; not yet
executed), **benchmarked**, **planned**, **blocked**.

## 1. Scope and system boundary

**In scope:** receiving UDP market-data datagrams, decoding them,
validating sequence and session integrity, measuring the receive path, and
handing normalized events to a consumer inside the same process.

**Out of scope (for now):** order entry, broker/exchange connectivity,
strategy logic, persistence, retransmission/recovery protocols, and anything
that touches the CAML server (`camlcompute`) without written authorization.

```text
                 ┌──────────────────────── caml-recv process ─────────────────────────┐
 caml-feed ──UDP──▶ PacketSource ──&[u8], RxMeta──▶ FeedProcessor ──MarketEvent──▶ EventSink
 (publisher)     │  (transport)                     parse → session gate → sequence   (consumer)
                 │   TransportStats                  → metrics, Integrity verdict      │
                 └─────────────────────────────────────────────────────────────────────┘
```

## 2. Workspace layout

| Path                  | Crate              | Responsibility | Deps |
|-----------------------|--------------------|----------------|------|
| `crates/protocol`     | `caml-protocol`    | Wire format v2: encoder, zero-copy validating decoder. No I/O. | none |
| `crates/engine`       | `caml-engine`      | Transports (`UdpSource`, Linux `RecvMmsgSource`), session gate, sequencer, histogram, metrics, receive loop. | protocol, `socket2`; Linux: `nix` |
| `crates/frame`        | `caml-frame`       | Ethernet/802.1Q/IPv4/UDP parsing and filtering for raw-packet backends (AF_XDP, DPDK). No I/O, no deps. | none (dev: `etherparse` as test oracle) |
| `crates/sim`          | `caml-sim`         | Deterministic synthetic market, fault injection, Ethernet framing + pcap writer. Test tooling. | protocol |
| `crates/alloc-count`  | `caml-alloc-count` | Test-only counting allocator proving the receive path never allocates. The only crate with `unsafe` (see D10). | none |
| `apps/receiver`       | `caml-recv`        | CLI receiver: backends, multicast, source allowlist, integrity verdict, exit codes. | engine, `clap`, `ctrlc` |
| `apps/feed-generator` | `caml-feed`        | CLI publisher: pacing, heartbeats, end-of-session, faults, multicast, pcap output. | sim, `socket2`, `clap` |
| `apps/bench`          | `caml-bench`       | Reproducible micro and loopback benchmarks. | engine, sim, `clap` |

**Why these crates and not more.** `protocol` and `frame` are contracts
shared by several backends and tools (and, later, an FPGA reference model),
so they stay dependency-free and I/O-free. `sim` and `alloc-count` are test
tooling and never linked into production binaries. Everything else is a
module of `engine` until a real reason appears.

The **planned** AF_XDP backend will be its own crate (`caml-transport-xdp`),
the second crate allowed to contain `unsafe`, with a `// SAFETY:` comment on
every block (enforced by `clippy::undocumented_unsafe_blocks`). `caml-engine`
will not depend on it.

## 3. Key interfaces

```rust
// Transport: lends each datagram to a callback; the slice cannot escape.
trait PacketSource {
    fn poll<F: FnMut(&[u8], RxMeta)>(&mut self, on_packet: F) -> io::Result<usize>;
    fn stats(&self) -> TransportStats; // every drop below the protocol layer
}

struct RxMeta { rx_wall_ns: u64, rx_instant: Instant, rx_kernel_ns: Option<u64> } // non_exhaustive

// Consumer boundary: within a session every seq is delivered at most once,
// tagged InOrder | AfterGap | Late. Nothing is dropped without a counter.
trait EventSink {
    fn on_event(&mut self, event: &MarketEvent, delivery: Delivery);
}
```

The **lending** design of `PacketSource` fits every backend without forcing a
copy:

| Backend | How it implements `poll` | Status |
|---|---|---|
| `UdpSource` | `recv_from` into an owned 64 KiB buffer, lend `&buf[..n]` | implemented, tested |
| `RecvMmsgSource` (Linux) | one `recvmmsg` fills 32 slots; lend each; per-datagram kernel timestamp; kernel drop counter | implemented; **CI-only** (compiles for Linux, never executed here) |
| AF_XDP | lend a UMEM frame's UDP payload (via `caml-frame`), recycle to the fill ring after the callback | planned |
| DPDK | lend each mbuf's UDP payload in wire order, bulk-free the burst | design only; not on the I219 (D8) |

Generic (`F: FnMut`) rather than `&mut dyn FnMut` so the per-packet call is
monomorphized and inlined into the receive loop.

## 4. Concurrency model

**One thread does everything** (implemented): `run` calls `poll`, the
processor parses, gates, sequences and accounts inline, and the sink runs
inline. This is the latency floor (no queue hop, no cross-core transfer) and
trivially correct (no shared mutable state; the only atomic is the stop flag,
`Relaxed`, because it publishes no data).

**When this changes:** when a consumer's per-event cost risks socket-buffer
overflow, now directly observable as `TransportStats::kernel_drops` on Linux.
The planned step is a bounded SPSC ring of indices between a pinned receive
thread and a consumer thread, with overflow counted. It will be introduced
with before/after measurements on Linux, not speculatively.

**Async/Tokio is not used on the hot path** (D11).

**Shutdown:** the stop flag is checked between polls; the poll timeout
(default 100 ms) bounds shutdown latency.

## 5. Memory and buffer ownership

| Object | Owner | Allocation |
|---|---|---|
| Receive buffer (64 KiB) / 32 × 2 KiB slots + mmsghdrs | backend | once, at bind |
| `PacketView<'a>` | borrows the receive buffer | none |
| `MarketEvent` (40 B, `Copy`) | stack | none |
| Gap table (`VecDeque`, capacity + 1) | `SequenceTracker` | once |
| Latency histograms (3 × 15 KiB) | `FeedProcessor` | once, boxed |

**The steady-state receive path performs zero heap allocations: tested.**
`crates/engine/tests/alloc.rs` installs a counting global allocator and
asserts zero allocations over (a) about 2,400 adversarial datagrams through
`process_packet` (gaps, late fills, duplicates, heartbeats, implausible
jumps, malformed and foreign-session datagrams, end of session, session
change) and (b) the full `run` loop over a real UDP socket.

## 6. Reliability semantics

### Session gate (implemented, tested)

| Situation | Behavior | Counter |
|---|---|---|
| First valid packet | adopt its epoch | — |
| Same epoch | process | — |
| Epoch > receiver clock + 300 s | drop | `implausible_epoch_packets` |
| Newer epoch, current session ended (EOS) or silent ≥ 500 ms | adopt at once | `session_changes` (+ `unterminated_sessions` if no EOS was seen) |
| Newer epoch, current session active | drop until 3 consecutive packets confirm it | `unconfirmed_session_packets` |
| Older epoch, current session active | drop | `stale_session_packets` |
| Older epoch, current silent ≥ 500 ms **and** 3 consecutive packets | adopt (un-sticks a receiver that latched onto a stray packet) | `session_changes` |

On a session change, outstanding gaps of the old session are counted as
`abandoned` (lost), never silently forgotten.

### Sequencer (implemented, tested)

| Condition | Detection | Effect |
|---|---|---|
| Malformed datagram | `parse_packet` error | counted by kind; whole datagram dropped |
| Lost messages | forward gap (message or heartbeat) | counted `missing`, tracked for late recovery |
| Tail loss before idle/end | heartbeat / EOS announces next seq | counted as a gap (v1 could not detect this) |
| Reordered packet | inside a tracked gap | delivered `Late`; gap shrinks |
| Reordering at session start | seqs start at 1, so early packets are gaps, not "duplicates" | delivered `Late` |
| Duplicate | behind, not in a gap | counted, not delivered |
| Jump > `max_forward_jump` (2^20) | implausible | packet dropped and counted; state untouched unless 3 consecutive consistent packets confirm (`resyncs`) |
| Late join (first seq > `join_window` + 1) | baseline = first seq | `missed_before_join` (informational); earlier seqs → `before_baseline` |
| Gap table full (1024) | oldest gap evicted | counted lost (`evicted`) |
| Clock skew | `send_time > rx_time` | counted; excluded from latency |
| Kernel socket-buffer overflow | Linux: `SO_RXQ_OVFL` | `TransportStats::kernel_drops` (CI-only) |
| Datagram > slot (Linux backend) | `MSG_TRUNC` | `TransportStats::truncated` |
| Source not allowed | `allowed_sources` | `TransportStats::filtered_source` |

All arithmetic on wire values is checked or saturating, enforced with
`#![deny(clippy::arithmetic_side_effects)]` in `sequence.rs` and
`pipeline.rs`. Accounting invariant, checked after **every** datagram by the
property test: `missing_total == outstanding + recovered + evicted + abandoned`.

### Integrity verdict

`Snapshot::integrity()` reports lost messages (`outstanding + evicted +
abandoned`), malformed, implausible, and rejected-session packets,
`before_baseline`, and whether the session ended cleanly. `caml-recv` prints
it and, with `--fail-on-loss`, exits with status 2 on any proven problem.
Sessions replaced without an end-of-session heartbeat produce a warning:
their tail loss cannot be proven or ruled out.

## 7. Timestamps and what latency means here

* **send_to_rx** = receiver wall clock right after the receive call returns,
  minus the publisher's `send_time_ns` (stamped immediately before `send`).
  Valid only when both clocks agree (same host, or PTP-synchronized hosts).
  Includes the sender's syscall, the network or loopback path, socket-buffer
  queueing, and receiver wakeup. Not a NIC-to-NIC number.
* **kernel_to_rx** (Linux `RecvMmsgSource`) = user-space receive minus the
  kernel's `SO_TIMESTAMPNS` arrival stamp: socket queueing plus wakeup.
* **processing** = `Instant` from receive to sink return, sampled 1-in-N
  (`--processing-sample-every`; each sample is one clock read).
* Clock costs on the dev box: `Instant::now()` 23 ns, wall clock 24 ns,
  resolution 100 ns (benchmarked, `docs/benchmarks.md`). With full
  instrumentation a packet costs **3 clock reads** (two in `RxMeta::now`, one
  processing sample); `run` adds none (it reuses the receive stamp).

## 8. Environment and its limits

| Environment | Status | Valid for |
|---|---|---|
| Windows 11 Pro 26200, Ryzen 7 9800X3D, rustc 1.99.0 `x86_64-pc-windows-gnu` | **in use** | functional tests, loopback and in-process measurements |
| GitHub Actions `ubuntu-24.04` + `windows-latest` | **configured**, not yet run (no remote push) | all tests incl. Linux-only backend and multicast-on-`lo` |
| WSL2 / Linux VM | **not installed** (owner's decision) | local Linux runs, AF_XDP on `veth` |
| Target box: Dell OptiPlex (Ultra) 11th-gen i7, 32 GB, Ubuntu 24.04, onboard I219-LM (13) `8086:15fb`, Thunderbolt 4 | **untouched; no access by policy** | deployment, real-NIC measurements |

Loopback numbers measure the OS stack and scheduler, not a NIC. They must
never be compared with physical-NIC numbers as if equivalent.

## 9. Kernel bypass: decision and plan

**Decision (D7, D8, D9): AF_XDP first, on an external Thunderbolt NIC; no
DPDK on the I219.** Full hardware plan and purchase requirements:
[`docs/hardware-plan.md`](hardware-plan.md).

Verified facts behind it:

* e1000e (the I219's driver) has **no native XDP** (no `ndo_bpf` in Linux
  v6.8 `e1000e/netdev.c`), so AF_XDP on the I219 runs only in generic copy
  mode, which is not kernel bypass.
* Ubuntu 24.04 ships DPDK 23.11.4, which has **no entry for the I219-LM (13)
  `0x15FB`**; DPDK v25.11 does (`em_ethdev.c:168`). The em PMD is not
  validated on I219, has no timestamp offload, and binding the box's only
  port would take SSH with it.
* AF_XDP zero-copy exists in Linux v6.8 for i40e, ice, ixgbe, igc and mlx5.
  The box has a Thunderbolt 4 root port, so an external NIC is possible.

AF_XDP before DPDK because the kernel keeps the interface (SSH, ARP, IGMP
joins via an ordinary socket), no hugepages or vfio are needed, a
`bpf_link`-attached XDP program detaches automatically if the process dies,
and the same code runs in copy mode on `veth` for development and zero-copy
on a capable NIC in production. DPDK stays design-only
(`docs/research/rust-dpdk-net-review.md`) until AF_XDP zero-copy measurably
falls short.

Raw-packet groundwork that is **implemented and tested** now:
`caml-frame` (Ethernet/VLAN/IPv4/UDP parse + filter + per-reason counters,
differentially tested against `etherparse` on 100,000 mutated frames) and
`caml-sim::pcap` + `caml-feed --pcap-out` (Ethernet-framed captures for
`tcpreplay` onto `veth` or DPDK `net_pcap`).

## 10. Testing strategy

| Layer | What | Where |
|---|---|---|
| Protocol | golden fixtures, every reject path, 200k fuzz cases | `crates/protocol/tests/wire.rs` |
| Frame parser | round trips, every reject reason, checksums, filter, 100k fuzz, 100k differential vs `etherparse` | `crates/frame/tests/frames.rs` |
| Sequencer | unit tests for every outcome, eviction, late join, resync, `u64::MAX`, accounting | `crates/engine/src/sequence.rs` |
| Processor | session gate cases, heartbeats/EOS, sampling | `crates/engine/src/pipeline.rs` |
| Audit regressions | P1–P7 | `crates/engine/tests/adversarial.rs` |
| Properties | 60 (debug) / 600 (release) hostile random streams; invariants after every datagram; coverage of every path asserted | `crates/engine/tests/properties.rs` |
| Allocation | zero allocations, processor and socket loop | `crates/engine/tests/alloc.rs` |
| End to end | real sockets, burst-drained (no reliance on kernel buffering), faults, tail loss | `crates/engine/tests/loopback.rs` |
| Linux backend | batching, kernel timestamps, exact kernel-drop accounting, truncation, filtering, full pipeline | `transport/recvmmsg.rs` (CI-only) |

Gates on every commit (CI): fmt, clippy `-D warnings`, tests in debug
**and** release on Ubuntu 24.04 and Windows, MSRV 1.88 build, `cargo deny`.

## 11. Main risks

1. **Nothing has run on Linux yet.** The Linux backend and multicast-on-`lo`
   tests are compile-checked only until CI runs or a Linux environment
   exists.
2. **Thunderbolt NIC stability.** Surprise removal, link retraining, and
   power management on the TB4 path must be engineered around (hardware plan
   §5).
3. **Mobile CPU on the target** (Tiger Lake U, 15–28 W): busy-polling plus
   turbo means thermal and power-limit swings; must be measured
   (`turbostat`) and pinned down (fixed frequency).
4. **Single-threaded consumer coupling.** A slow sink stalls the receive
   path; now detectable via kernel drops on Linux.
5. **Data source unknown.** If the feed arrives over the public internet,
   kernel bypass cannot matter (milliseconds of WAN jitter versus
   microseconds of kernel cost).
