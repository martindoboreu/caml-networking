# Learning Log

Design explanations for each milestone, written for CAML members who want to
understand why the code looks the way it does.

---

## Milestone 1: Synthetic feed → validated, measured receiver (Stages 0–2)

> This entry describes protocol **v1** as originally built (packet sequence
> plus message sequence, 32-bit session id). Milestone 2 below explains why
> v2 replaced it.

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

> **Correction (2026-10-09 audit).** The 8.1 ms p99 / 20.2 ms max in the
> clean run did **not** reproduce: 14 later runs of the same scenario gave
> p99 of 25–35 µs (`docs/benchmarks.md`). Its cause is unknown. "About 100 ns
> per packet" was the 100 ns resolution of `Instant` on Windows, not a
> measurement; batch timing gives 106 ns with instrumentation, of which
> 13.5 ns is parsing and sequencing. Treat single runs as anecdotes.

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

---

## Research note: what to take from the `rust-dpdk-net` review

The full analysis is in
[`research/rust-dpdk-net-review.md`](research/rust-dpdk-net-review.md); its
section 8 covers hugepages, DMA rings, RSS, and core isolation. This entry
picks out the four ideas that most directly shape our own code.

### 1. mbuf/mempool ownership is a lifetime problem

An `rte_mbuf` is not separately heap-allocated. It is a slot inside a
**mempool**, a fixed block of hugepage memory created at startup. "Freeing" an
mbuf returns the slot to the pool, and freeing the *pool* unmaps the whole
block. So every mbuf has an implicit lifetime bound: **it must not outlive its
pool.** The NIC holds pool slots too: the RX descriptor ring is pre-filled with
empty mbufs, so a started port references the pool even when software holds
no mbufs.

The reviewed repo wraps an mbuf as an owning pointer with no lifetime tied to
the pool, and the pool's `Drop` frees it unconditionally. Safe code can
therefore drop the pool first and then touch, or let the NIC DMA into, freed
memory. That is a use-after-free that compiles, and its own test does it in
the wrong order. In Rust you rule this out with one of two shapes:

* **Borrowing:** `Mbuf<'pool>` holds a `&'pool MemPool`, so the borrow checker
  refuses to drop the pool while any mbuf exists. This is precise, but the
  lifetime spreads through every type that stores mbufs.
* **Single owner, lend only:** one struct owns EAL, port, and pool, tears them
  down in a fixed order in `Drop` (stop port → close → free pool → EAL
  cleanup), and never hands out an mbuf, only a `&[u8]` inside a callback.
  This is our planned design. It needs no new lifetime parameters, because
  `PacketSource::poll`'s `F: FnMut(&[u8], RxMeta)` bound is *higher-ranked*
  (`for<'a> FnMut(&'a [u8], RxMeta)`): the callback must accept a slice of any
  lifetime, so it can't assume the slice lives past the call.

The general lesson: whenever C hands you memory that belongs to something
else, ask "what must outlive what?" and encode the answer in ownership or
lifetimes. Don't rely on drop order happening to be right.

### 2. Burst order is data, not an implementation detail

`rte_eth_rx_burst` returns up to N packets in the order the NIC received
them. The reviewed repo pushes them into an array and then calls `pop()`,
which takes the **last** element first, so each burst of 32 is handed up in
reverse. TCP doesn't notice because it reassembles by sequence number. Our
`SequenceTracker` would see, for a burst carrying packets 33–64, packet 64
first, then 63, …, 33. That reports a 31-packet gap followed by 31 "late"
recoveries, every burst. (In the very first burst it's worse: packet 32
becomes the baseline, so 31..1 are classified as duplicates and never
delivered.) It inflates exactly the
counters we use to judge feed health and makes every consumer see
`Delivery::Late` data. A sequenced-feed receiver must iterate
`pkts[0..n]` front to back. It's a one-word bug with system-wide
consequences, which is why the DPDK test plan replays a known pcap and asserts
zero reordering.

### 3. Poll Mode Drivers vs interrupts

Our `UdpSource` *blocks*: the thread sleeps in `recv_from`, and the kernel
wakes it when a packet arrives. The wake path is NIC interrupt → driver/NAPI
softirq → UDP stack → socket queue → scheduler makes our thread runnable →
context switch → copy to user space. Every arrow can add microseconds of
jitter, which is one plausible source of the tail we saw in the smoke runs.

A DPDK **Poll Mode Driver** removes the interrupt and the kernel entirely. The
NIC DMA-writes into an RX descriptor ring in our process's memory, and our
thread spins reading the ring's "done" bits. There's no interrupt, no syscall,
no wakeup, and no copy. The costs: the core runs at 100% even when the feed is
idle; the port vanishes from Linux, along with `tcpdump`, ARP, IGMP, and SSH;
and we must implement whatever protocol behavior we still need. Note that
`SO_BUSY_POLL` on a kernel socket is a middle ground: the kernel polls the
driver on our behalf while we're in `recv`, and it stays on the list of
things to measure before DPDK.

### 4. Why a C shim exists for `static inline` functions

DPDK's hottest functions (`rte_eth_rx_burst`, `rte_pktmbuf_free`,
`rte_pktmbuf_mtod`) are `static inline` in headers so that C callers get them
inlined with no call overhead. `static inline` means **no symbol is exported**
from `libdpdk.so`, so there is nothing for Rust's linker to bind to. bindgen
can generate the declaration, but linking fails. The fix is a tiny C file
compiled into our crate (via the `cc` crate) that wraps each one in a real
function:

```c
uint16_t rust_eth_rx_burst(uint16_t port, uint16_t q, struct rte_mbuf **pkts, uint16_t n) {
    return rte_eth_rx_burst(port, q, pkts, n);
}
```

The cost is one non-inlined call per use, since Rust can't inline across the
C boundary without cross-language LTO. That's why the planned design calls
the shim once per *burst* (`rx_burst`, `free_bulk`) and reads per-packet mbuf
fields (`data_off`, `data_len`, `ol_flags`) directly from the bindgen-generated
struct instead of through a shim call each.

### Check your understanding

4. Suppose `DpdkSource::poll` delivered a burst in wire order but the
   callback panicked on packet 5 of 32. Under the planned guard design, what
   happens to the 32 mbufs, and what would happen without the guard?
5. Our `UdpSource` takes a fresh `RxMeta::now()` per datagram, but the DPDK
   design takes one per burst. Why is the per-burst stamp *not* less accurate
   for DPDK? What does a per-packet stamp inside a burst actually measure?

---

## Milestone 2: integrity hardening, protocol v2, Linux groundwork

### Why this milestone happened

An independent audit wrote small adversarial tests against milestone 1 and
found that the receiver could be fooled in ways the original tests never
tried. All of these were reproduced before anything was fixed:

* One stray packet from another "session" reset all sequence state, so
  A → B → A delivered A's message twice.
* Reordering at session start (packet 2 before packet 1) dropped packet 1 as
  a "duplicate", and the loss was not counted anywhere.
* Four valid packets with huge sequence numbers crashed a debug build
  (`attempt to add with overflow`) and corrupted counters in release.
* One packet far in the future made every later message look "late".
* Loss followed by a publisher restart printed no warning.

The lesson: **tests that only exercise cooperative inputs prove very
little.** The new suite feeds the receiver hostile streams on purpose.

### What changed and why

**Protocol v2 copies a proven design (MoldUDP64).** One sequence space per
session, starting at 1. The header says which sequence number the packet
starts at; heartbeats say which number comes next. That one idea fixes
three problems at once:

1. *Tail loss.* If the last two packets before a quiet period are lost, the
   next heartbeat says "next is 41" while we expected 33, so the gap is
   visible. v1 could not see this at all.
2. *Start-of-session reordering.* Because sequences start at 1, receiving 3
   first means 1–2 are *missing* (and may arrive late), not "older than our
   baseline".
3. *Fewer moving parts.* The packet-level sequence (and its separate gap
   table, which heartbeats filled up) is gone. One tracker decides.

**Sessions are ordered.** `session_epoch` is the publisher's start time in
nanoseconds, so a restart always has a larger epoch. The receiver moves
forward only when the old session said goodbye (`END_OF_SESSION`), went
quiet, or the new one has proved itself with three consecutive packets.
Stale packets from an old run are dropped and counted, never allowed to
reset state.

**Don't trust a single packet.** A jump of more than 2^20 messages is held
back until three consecutive packets agree. This is the same idea as the
session rule: one data point is noise; several consistent ones are a fact.

### Rust concepts in this milestone

**Lints as proofs.** `#![deny(clippy::arithmetic_side_effects)]` on the
receive-path modules makes the compiler reject any plain `+`/`-` on
integers there. You must write `saturating_add`, `checked_sub`, and so on,
so "wire input cannot overflow" is enforced by the build, not by review.

**Counting allocations with a global allocator.** Rust lets a binary replace
its allocator with `#[global_allocator]`. The test crate `caml-alloc-count`
wraps the system allocator and counts calls made by the current thread
(thread-local counters, so parallel tests don't interfere). Implementing
`GlobalAlloc` requires `unsafe impl`, so this tiny test-only crate is the one
place `unsafe` is allowed, with a `// SAFETY:` comment on every block that
the linter enforces.

**`VecDeque` + binary search instead of `Vec::remove(0)` + linear scan.**
Evicting the oldest gap from a full `Vec` shifts all 1,023 others (16 KiB
of memmove). A ring buffer pops the front in O(1), and because gaps are
sorted, `partition_point` finds the right one in about 10 comparisons. The
benchmark went from 64 ns to 3.3 ns per new gap.

**Safe syscalls through `nix`.** `recvmmsg` and `SO_TIMESTAMPNS` are C APIs,
but `nix` wraps them safely and promises no allocation in `recvmmsg`, so the
engine still has zero `unsafe`. One subtle point: `nix` macros expand inside
*our* crate, so we checked that `cmsg_space!` calls a safe function rather
than emitting an `unsafe` block, which `forbid(unsafe_code)` would reject.

**What the first Linux run taught us.** The code compiled for Linux on
Windows and looked right, and the very first CI run on real Linux still
failed one test: "every sent datagram is either delivered or counted as a
kernel drop". The cause was a stateful API. `recvmmsg` writes the number of
control-message bytes it used back into each reusable header, and `nix`
never resets it, so from the second call on there was only room for the
timestamp, and the drop counter (`SO_RXQ_OVFL`) was silently truncated. Two
lessons: *compiling is not running* (that's why the status label "CI-only"
exists), and *a test that asserts exact accounting catches what a test that
only checks "something arrived" never will*. The fix reads the kernel's
per-socket drop counter from `/proc/net/udp` instead (decision D14).

**Differential testing.** The new Ethernet/IPv4/UDP parser is compared with
an independent library (`etherparse`) on 100,000 randomly mutated frames.
The first run found a real difference (a legacy VLAN tag reported as "not
IPv4"); the second found a deliberate one (a UDP length of 0, which only
means something for IPv6 jumbograms). Both are now documented in
`docs/decisions.md`.

### Check your understanding

4. A heartbeat says the next sequence is 50, but we have only seen up to 45.
   What does the receiver record, and what changes if packets 46–49 then
   arrives?
5. Why does the session gate accept an *older* epoch only when the current
   session is quiet **and** the old one sends three packets in a row? What
   failure does each half of that condition prevent?
6. The property test asserts that the generator produced session changes,
   resyncs, and late deliveries at least once. Why is that assertion as
   important as the invariants themselves?
