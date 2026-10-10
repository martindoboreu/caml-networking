# CAML Networking Engine: Architecture

Last updated: Stage 0–2 (first milestone), plus the planned DPDK backend
design (section 9).

## 1. Scope and system boundary

**In scope:** receiving UDP market-data datagrams, decoding them,
validating sequence integrity, measuring the receive path, and handing
normalized events to a consumer inside the same process.

**Out of scope (for now):** order entry, broker/exchange connectivity,
strategy logic, persistence, retransmission/recovery protocols, multicast
group management, and anything that touches the CAML server (`camlcompute`).

```text
                 ┌──────────────────── caml-recv process ───────────────────┐
 caml-feed ──UDP──▶ PacketSource ──&[u8]──▶ FeedProcessor ──MarketEvent──▶ EventSink
 (publisher)     │  (transport)            parse → sequence → metrics       (consumer)
                 └───────────────────────────────────────────────────────────┘
```

## 2. Workspace layout

| Path                  | Crate           | Responsibility | Deps |
|-----------------------|-----------------|----------------|------|
| `crates/protocol`     | `caml-protocol` | Wire format: encoder, zero-copy validating decoder. No I/O. | none |
| `crates/engine`       | `caml-engine`   | Transports, sequence tracking, histogram, metrics, receive loop. | protocol, `socket2` |
| `crates/sim`          | `caml-sim`      | Deterministic synthetic market and fault injection (drop/corrupt/duplicate/reorder). | protocol |
| `apps/receiver`       | `caml-recv`     | CLI receiver with periodic and final reports. | engine, `clap`, `ctrlc` |
| `apps/feed-generator` | `caml-feed`     | CLI publisher with rate pacing and fault flags. | sim, `clap` |

**Why five crates and not more.** The suggested layout split out `core`,
`transport-udp`, and `metrics`. At this size those boundaries would be
module boundaries pretending to be crate boundaries. The two splits that do
pay for themselves are:

* `protocol` is the contract shared by the publisher, the receiver, tests,
  and eventually an FPGA reference model. It must stay dependency-free and
  I/O-free.
* `sim` is test tooling. Keeping it out of `engine` means production builds
  never contain fault-injection code.

Linux-specific transports will be modules in `engine` behind
`#[cfg(target_os = "linux")]`. AF_XDP, which needs `libbpf`/`libxdp` and
`unsafe`, will be its own feature-gated crate so the `unsafe_code = "forbid"`
lint stays in force everywhere else. The same goes for the planned DPDK
backend (section 9).

## 3. Key interfaces

```rust
// Transport: lends each datagram to a callback; the slice cannot escape.
trait PacketSource {
    fn poll<F: FnMut(&[u8], RxMeta)>(&mut self, on_packet: F) -> io::Result<usize>;
}

// Consumer boundary: duplicates are never delivered; everything else is
// delivered exactly once with its ordering status.
trait EventSink {
    fn on_event(&mut self, event: &MarketEvent, delivery: Delivery); // InOrder | AfterGap | Late
}
```

The **lending** design of `PacketSource` was chosen over "fill my buffer"
(`recv(&mut [u8])`) because it fits every backend we plan to evaluate without
forcing a copy:

| Backend | How it implements `poll` |
|---|---|
| `UdpSource` (now) | `recv_from` into an owned 64 KiB buffer, lend `&buf[..n]` |
| `recvmmsg` (Phase B) | one syscall fills N buffers, lend each in turn |
| AF_XDP (Phase C) | lend a UMEM frame slice, return the frame to the fill ring after the callback |
| DPDK (planned, section 9) | lend each mbuf's UDP payload in wire order, bulk-free the burst after the last callback |

Generic (`F: FnMut`) rather than `&mut dyn FnMut` so the per-packet call is
monomorphized and inlined into the receive loop.

`RxMeta` is `#[non_exhaustive]` and built with `RxMeta::now()`, so fields
such as a NIC hardware timestamp can be added later without breaking
backends in other crates. Batched backends capture one `RxMeta` per batch.

## 4. Concurrency model

**Milestone 1: one thread does everything.** The receive loop calls `poll`,
the processor parses and validates inline, and the sink runs inline.

Rationale:

* It is the latency floor: no queue hop, no cross-core cache-line transfer,
  no wakeup.
* It is trivially correct: no shared mutable state, no atomics except the
  shutdown flag.
* We have no measurement yet showing the consumer is slow enough to need
  decoupling.

**When this changes:** when a consumer's per-event cost risks the socket
buffer overflowing (detectable as kernel drops and packet-seq gaps under
load). The planned next step is a bounded SPSC ring between a receive thread
(pinned, possibly busy-polling) and a consumer thread, with explicit overflow
accounting. That will be introduced with before/after measurements, not
speculatively.

**Async/Tokio is not used on the hot path.** An async runtime solves
"thousands of mostly idle connections". We have one socket that is either
busy or idle. A dedicated thread with a blocking or busy-polled socket has
fewer layers between the NIC and our code. Tokio may still be appropriate
later for control-plane work such as config reload or metrics export.

**Shutdown:** an `AtomicBool` checked between polls. The socket read timeout
(default 100 ms) bounds shutdown latency. `Ordering::Relaxed` is sufficient
because the flag publishes no other data.

## 5. Memory and buffer ownership

| Object | Owner | Allocation |
|---|---|---|
| Receive buffer (64 KiB) | `UdpSource` | once, at bind |
| `PacketView<'a>` | borrows the receive buffer | none (stack) |
| `MarketEvent` (40 B, `Copy`) | stack, passed by reference to sink | none |
| Sequence gap tables | `SequenceTracker` | once, pre-sized `Vec` |
| Latency histograms (15 KiB each) | `FeedProcessor` | once, boxed |

**The steady-state receive path performs zero heap allocations.** It is
enforced by construction and code review, not yet by an automated test; a
counting global allocator test is planned. A 64 KiB buffer is used even
though our packets are at most 1472 B, so a datagram can never be truncated
(Linux truncates silently; Windows returns `WSAEMSGSIZE`). Only the bytes
actually received are touched, so the cache cost is proportional to packet
size, not buffer size.

## 6. Reliability semantics

| Condition | Detection | Effect |
|---|---|---|
| Malformed datagram | `parse_packet` error | counted by kind; whole datagram dropped |
| Lost packet | packet-seq gap | counted (`missing`, `outstanding`) |
| Reordered packet | arrives inside a tracked gap | delivered with `Delivery::Late`; gap shrinks |
| Duplicate packet | packet seq behind and not in a gap | counted, **not** delivered |
| Publisher restart | `session_id` change | sequence state reset, counted |
| Gap table full | > 1024 outstanding gaps | oldest gap evicted and counted as lost |
| Clock skew | `send_time > rx_time` | counted; excluded from latency |
| Tail loss | — | **undetectable** without a later packet; publisher sends trailing heartbeats |
| Kernel socket-buffer overflow | indirect: packet-seq gaps | OS drop counters not yet read (Linux: `/proc/net/udp`, `SO_RXQ_OVFL`) |

Accounting invariant, enforced by tests:
`missing_total == outstanding + recovered + evicted`.

## 7. Timestamps and what latency means here

* **wire latency** = receiver wall clock right after `recv_from` returns,
  minus the publisher's `send_time_ns`. Valid only when both processes share
  a clock, i.e. the same host. It includes the publisher's send syscall, the
  kernel loopback path, socket-buffer queueing, receiver wakeup, and the
  receive syscall.
* **processing latency** = monotonic `Instant` from just after receive to
  after the sink returns: parse, sequencing, metrics, and consumer.
* On Windows both clocks have 100 ns resolution, so values below about 1 µs
  are quantized. Each packet costs about three clock reads of instrumentation
  overhead, which is included in what we measure.

## 8. Environment and its limits

| Environment | Status | Valid for |
|---|---|---|
| Windows 11 (build 26200), Ryzen 7 9800X3D, Rust 1.99.0 `x86_64-pc-windows-gnu` | **in use** | functional tests, portable baseline, loopback measurements |
| WSL2 Ubuntu | **not installed** (deferred by decision) | Linux APIs (`recvmmsg`, `SO_BUSY_POLL`, affinity), AF_XDP on `veth` |
| `camlcompute` (Ubuntu 24.04, I219-LM/e1000e) | **untouched; no access by policy** | future deployment and real-NIC measurements |

Loopback numbers measure the OS network stack and scheduler, not a NIC. They
must never be compared with physical-NIC numbers as if they were equivalent.

**AF_XDP outlook, stated up front.** `camlcompute`'s e1000e driver reports no
native XDP and no zero-copy. AF_XDP there would run in generic (SKB) copy
mode, where the kernel has already allocated an `sk_buff` and copies the
frame into UMEM. The expected gain over a tuned `recvmmsg` socket is small
and could be negative. At 1 Gbps with our packet sizes the conventional path
is likely sufficient. We will measure before concluding. Real kernel-bypass
evaluation needs a NIC with native XDP/zero-copy support (e.g. Intel i40e/ice,
Mellanox mlx5).

## 9. Kernel-bypass backends (planned)

Status: **planned, not implemented.** No DPDK code or dependencies exist in
the workspace. The source for this section is the
[rust-dpdk-net review](research/rust-dpdk-net-review.md) (commit `8670af4`).

**Decision on `rust-dpdk-net`: learn from it, don't depend on it.** It is a
TCP/IP-over-DPDK framework (smoltcp plus tokio). Its UDP path copies each
payload twice, adds async wake-ups, verifies checksums in software, has no
receive timestamps, never reads NIC drop counters, hands each RX burst over
newest-first (a spurious reorder for a sequenced feed), and lets a mempool be
freed while its mbufs are still in use.

| Reuse (pattern only) | Implement ourselves | Unnecessary for CAML |
|---|---|---|
| `build.rs`: pkg-config → bindgen allowlist → `cc`-compiled C shim that re-exports DPDK's `static inline` functions (`rte_eth_rx_burst`, `rte_pktmbuf_*`). MIT, so keep the notice if we copy a file substantially | `caml-transport-dpdk` crate: EAL/port/mempool lifecycle owned by one `DpdkSource` with ordered teardown | smoltcp or any TCP/IP stack |
| Shape of its `EalBuilder`, `EthDevBuilder`, and `Mbuf` RAII wrappers, as reference | Own Ethernet/802.1Q/IPv4/UDP parser (~150 lines), differentially tested against `etherparse` 0.21.0 with `proptest` 1.11.0 | tokio, async reactor, cross-thread bridge |
| | Per-reason drop counters, plus NIC `imissed` / `rx_nombuf` / `ierrors` read about once per second | Multi-queue RSS/RETA, shared ARP cache |
| | pcap replay fixtures (`pcap-file` 2.0.0) for `net_pcap` tests | HTTP/gRPC/QUIC adapters |

**Design summary:**

* **Isolation:** a Linux-only crate behind an off-by-default `ffi` feature
  (build deps `bindgen` 0.73.2, `pkg-config` 0.3.34, `cc` 1.7.0). Without the
  feature it compiles to nothing, so `cargo test --workspace` keeps working
  on Windows. It has its own lint table (`unsafe_code = "deny"`, allowed only
  in the FFI modules, with a `// SAFETY:` comment on every block).
  `caml-engine` never depends on DPDK.
* **Hot loop:** one pinned EAL lcore. `poll()` busy-polls `rte_eth_rx_burst`
  with a burst of 32 and processes packets **front to back, in wire order**.
  It takes one `RxMeta` per burst, lends each UDP payload to the existing
  callback, and frees the whole burst with one `rte_pktmbuf_free_bulk` from a
  guard's `Drop`, so a panicking callback can't leak mbufs. It returns
  `Ok(0)` after a short poll budget so the existing `run` loop's shutdown and
  report checks still fire.
* **Fits today's trait:** the `PacketSource` lending callback needs no change.
  The higher-ranked `FnMut(&[u8], RxMeta)` bound already stops payload slices
  from outliving the burst.
* **Planned interface change, deferred:** a transport-level statistics
  accessor (filter drops by reason, kernel or NIC drops). It will be added
  together with the first backend that can actually observe such drops,
  Linux `recvmmsg` with `SO_RXQ_OVFL`, rather than designed now against a
  single implementation that has nothing to report. The current Windows UDP
  backend cannot read kernel drop counts.
* **Testing without hardware:** in a Linux VM, use the `net_pcap` (replaying
  `caml-sim` captures), `net_ring`, `net_tap`, `net_af_packet`, and `net_null`
  virtual devices. These validate **correctness only**; none of them shows
  kernel-bypass performance.

**Hardware: correcting a common assumption.** DPDK v25.11's e1000 (`em`) PMD
**does** list I219-LM/V device IDs (`em_ethdev.c:112-190`). However, DPDK
documents validation only on emulated e1000 devices; the PMD offers at most 2
queues and no RSS or timestamp offload; and the link is 1 GbE. Binding
`camlcompute`'s only port to `vfio-pci` would most likely remove it, and SSH
with it, from Linux. **Conclusion: don't pursue DPDK for performance until a
supported NIC (Intel X710/E810, NVIDIA ConnectX) is available,** and only
after the kernel-socket baseline shows the kernel path is the bottleneck.

## 10. Testing strategy

* **Protocol:** a golden byte fixture pins the layout, plus every rejection
  path and 50,000 deterministic random/corrupted inputs to prove the decoder
  never panics.
* **Sequencing:** unit tests for each outcome, gap splitting, eviction,
  saturation, and a randomized accounting-invariant test.
* **Pipeline:** byte-level tests (no sockets) for duplicates, reordering,
  heartbeats, session changes, and clock anomalies.
* **End to end:** real UDP sockets on 127.0.0.1 with a deterministic fault
  schedule whose expected counters are derived by hand in the test.
* Everything is seeded and single-threaded where possible, so failures are
  reproducible.

## 11. Main risks

1. **Measurement validity on Windows.** Scheduler and timer behavior add
   jitter that is not present on a tuned Linux host. Mitigation: repeated
   runs, warmup exclusion, recording environment details, and Linux
   validation via WSL2 and later bare metal.
2. **No Linux environment yet.** Phases B and C are blocked until WSL2 is
   installed (needs admin and a reboot, which is your action).
3. **WSL2 is not bare metal.** It uses a Hyper-V virtual NIC and its own
   kernel build; AF_XDP support depends on that kernel's config. Treat it as
   a functional test bed, not a performance reference.
4. **Single-threaded consumer coupling.** A slow sink stalls the receive
   path. Mitigation: measure, then introduce an SPSC queue.
5. **Hardware ceiling on `camlcompute`.** The I219-LM/e1000e has no native
   XDP. DPDK's `em` PMD probe-lists the I219, but it is unvalidated there,
   limited to 2 queues with no RSS or timestamp offload, and would take the
   server's only port away from Linux. Either way, kernel bypass can show
   little on this machine (section 9).
