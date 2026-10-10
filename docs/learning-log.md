# Learning Log

Design explanations for each milestone, written for CAML members who want to
understand why the code looks the way it does.

---

## Milestone 1: Synthetic feed → validated, measured receiver (Stages 0–2)

### What we built

A publisher (`caml-feed`) encodes a random-walk market into a versioned binary
protocol and sends it over UDP, optionally dropping, corrupting, duplicating,
or reordering packets. A receiver (`caml-recv`) decodes each datagram,
validates both sequence spaces, delivers events to a consumer, and reports
counts and latency percentiles.

### What happens to one packet

1. The publisher's `PacketWriter` writes the header and messages into a
   stack array, stamps `send_time_ns`, and calls `send()`. That is one system
   call: the CPU switches to kernel mode, and the kernel copies the bytes
   into an `sk_buff` (Linux) or the Windows equivalent, then runs the UDP/IP
   stack.
2. On loopback no NIC is involved: the kernel queues the buffer directly on
   the receiving socket's receive queue. That queue is what `SO_RCVBUF`
   sizes. On a real NIC there is an earlier stage: the NIC DMA-writes the
   frame into a ring buffer in RAM that the driver pre-posted, raises an
   interrupt (or is polled by NAPI), and the driver wraps it in an `sk_buff`.
3. The receiver thread is blocked inside `recv_from`. The kernel wakes it and
   copies the datagram from kernel memory into our 64 KiB user buffer: the
   second copy, crossing the kernel/user boundary.
4. `UdpSource::poll` reads both clocks and lends `&buf[..n]` to the callback.
5. `parse_packet` validates everything and returns a `PacketView<'_>` that
   points into that buffer. Nothing is copied.
6. The packet sequence is checked; a duplicate stops processing here.
7. Each 40-byte message is decoded into a `MarketEvent` on the stack, its
   sequence is checked, and the sink receives `&MarketEvent` plus a
   `Delivery` tag.
8. Processing time is recorded, the callback returns, and the buffer is free
   for the next `recv_from`.

The two copies and one or two system calls per packet in steps 1 and 3 are
exactly what `recvmmsg` (fewer syscalls) and AF_XDP (fewer copies, no
`sk_buff`) attack. That is why we measure this path first.

### Rust concepts in the code

**Lending and borrowing: `PacketSource::poll`.** The transport calls
`on_packet(&self.buf[..len], meta)`. The closure receives a shared borrow
that lives only for that call. If a consumer tried to stash the slice in a
`Vec<&[u8]>` for later, it would not compile, because the borrow cannot
outlive the call while `self.buf` is still mutably owned by the transport and
will be overwritten by the next `recv_from`. In C this is a classic
use-after-overwrite bug. Here the type system rules it out at zero runtime
cost.

**Lifetimes: `PacketView<'a>`.** `parse_packet(buf: &[u8]) -> PacketView<'_>`
says "the view borrows from `buf`". The view holds
`&'a [[u8; 40]]`, a slice of fixed-size arrays produced by
`as_chunks::<40>()`. Because each element is a `[u8; 40]`, every field offset
is a compile-time constant on a known-length array, so the optimizer can
drop the bounds checks.

**Validate first, then infallible iteration.** `parse_packet` checks every
message before returning. The iterator then cannot fail, so downstream code
never sees half a packet, and sequence tracking never sees messages 1–4 of a
packet whose message 5 was garbage. Re-reading three bytes per message costs
nanoseconds; the syscall costs microseconds.

**Stack vs heap.**
* `MarketEvent` is 40 bytes and `Copy`. It lives in registers or on the stack;
  creating one is a few loads. Allocating it on the heap would cost far more
  than decoding it.
* `LatencyHistogram` is 1,920 `u64` buckets, about 15 KiB. It is
  `Box<[u64]>`, so the struct itself is 48 bytes and cheap to move, and we
  never risk a large stack frame. It is allocated once at startup.
* The 64 KiB receive buffer is also allocated once. The rule is: allocate at
  startup, never in the loop.

**Plain integers instead of atomics for counters.** Atomics are for
communication between threads. Today one thread writes and reads every
counter, so `counters.datagrams += 1` is an ordinary register increment. An
atomic `fetch_add` would compile to a `lock`-prefixed instruction on x86.
That acts as a full memory barrier, costs on the order of 20 cycles even
uncontended, and stops the compiler from keeping the counter in a register.
Paying that for no reason on every packet is the wrong default.

**The one atomic: the stop flag.** `AtomicBool` with `Ordering::Relaxed`.
Relaxed guarantees the load sees *some* value the store wrote, eventually,
with no ordering relative to other memory. That is all a shutdown flag needs.
`Acquire`/`Release` would be needed if the flag also published data, e.g.
"the config is ready, go read it".

**Errors as values, not panics.** Malformed input is an expected condition
on a network. `DecodeError` carries the reason, and `DecodeErrorKind::index()`
turns it into a counter-array index. Sequence arithmetic uses
`saturating_add`, because `seq + 1` on attacker-controlled `u64::MAX` would
panic in debug builds.

### Design choices worth defending

* **Little-endian wire format:** our CPUs are little-endian, and SBE/CME made
  the same choice.
* **Two sequence spaces:** packet seq finds transport loss and duplicates;
  message seq gives consumers an order. Duplicate packets are filtered before
  message sequencing, so one retransmitted datagram doesn't show up as four
  duplicate messages.
* **Late data is delivered, tagged:** whether a reordered trade is useful
  depends on the consumer. A book builder applying incremental updates must
  not apply a stale one; a trade recorder wants it. The engine reports and
  the consumer decides.
* **Bounded gap table:** remembering every missing sequence forever is a
  memory-exhaustion bug waiting for a bad feed. The cost is explicit: after
  1,024 outstanding gaps the oldest is counted as lost.

### How a production HFT stack would differ

* **Multicast with A/B line arbitration.** Exchanges publish every packet on
  two independent multicast feeds; the receiver takes whichever copy arrives
  first. Our duplicate handling is the core of that logic.
* **Recovery.** Gaps trigger retransmission requests (e.g. MoldUDP64 rerequest
  servers) or a snapshot plus replay. We only detect.
* **Kernel bypass on the right NIC.** Solarflare/AMD ef_vi/Onload, Mellanox
  VMA, DPDK, or AF_XDP zero-copy, with the receive thread busy-polling a
  pinned isolated core (`isolcpus`, `nohz_full`) and IRQs steered elsewhere.
* **Hardware timestamps.** The NIC stamps packets at the PHY using a
  PTP-disciplined clock, so latency excludes the host scheduler. Our
  timestamps are software timestamps taken in user space.
* **FPGA feed handlers.** The parse → sequence → normalize pipeline you see in
  `FeedProcessor` is commonly implemented in FPGA fabric directly behind the
  MAC, emitting normalized events over PCIe DMA. Our fixed-size,
  8-byte-aligned message layout was chosen partly so it maps cleanly onto
  that.

### Smoke-run observations (not benchmarks)

Windows 11, Ryzen 7 9800X3D, loopback, release build, single run each, no
warmup exclusion. These are reported only to show the system works. They are
**not** a baseline.

| Run | Delivered | Integrity | wire p50 / p99 / max | processing p50 / p99 |
|---|---|---|---|---|
| Clean, 20k pkts @ 10k/s, 4 msgs/pkt | 80,000 / 80,000 | 0 gaps | 9.0 µs / 8.1 ms / 20.2 ms | 0.1 µs / 0.4 µs |
| Faults, 10k pkts @ 10k/s | 39,936 (= (10,000 − 16) × 4) | every injected fault accounted for | 9.0 µs / 30.2 µs / 215 µs | 0.1 µs / 0.4 µs |

The two runs have very different latency tails, so tails on this setup are
noisy. Candidate causes include Windows scheduler preemption of the receiver
thread, timer/CPU power-state transitions, and first-run effects (page
faults, console output). Separating them is a Stage 4 task: repeated runs,
warmup exclusion, and recording run conditions. Processing time, about 100 ns
per packet including three clock reads, is far below the network-path
latency. The current bottleneck is the OS path, not our parsing.

### Check your understanding

1. `FeedProcessor::process_packet` takes `bytes: &[u8]`. Why can't it return
   the `PacketView` it creates to its caller? What would you change to make
   that legal, and what would that do to the transport's buffer reuse?
2. In the fault test, packet 92 is reordered and packet 93 is normal. If 93
   had instead been *corrupted*, the receiver would report 92 as in-order with
   no gap or late arrival. Why? Is that a bug?
3. `LatencyHistogram::percentile` returns a bucket's upper bound. For a true
   value of 1,000,000 ns, what is the largest value it can report? Why is
   over-reporting preferable to under-reporting for latency?
