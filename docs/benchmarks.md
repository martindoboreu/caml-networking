# Benchmarks

Every number here has a command, a release build, and an environment
(AGENTS.md). Numbers from different environments are never compared as if
equivalent: **loopback measures the OS stack and scheduler, not a NIC.**

## How to run

```powershell
cargo run --release -p caml-bench -- micro --runs 7
cargo run --release -p caml-bench -- loopback --runs 10
```

`caml-bench` prints its own environment table (OS, CPU count, compiler,
target, build profile). Record the CPU model, power plan, and other load
yourself. The release profile is `codegen-units = 1`, `lto = "fat"`
(workspace `Cargo.toml`).

Methodology:

* **micro**: each benchmark times a batch (≥100,000 items) and divides, so
  results are not limited by clock resolution. Median of N runs, with the
  min–max range across runs.
* **loopback**: N independent runs (fresh sockets and processor each). A
  sender thread paces packets open-loop against an absolute schedule
  (no coordinated omission at the sender) and stamps `send_time` immediately
  before `send`. The first `--warmup` datagrams are excluded from latency
  statistics. Per-run percentiles are reported, then the median and range
  across runs. Every run also reports loss and the integrity verdict; a run
  with loss is flagged, not silently averaged.

## Results: 2026-10-09, Windows dev box

| | |
|---|---|
| Machine | AMD Ryzen 7 9800X3D (8C/16T), 62 GB RAM |
| OS | Windows 11 Pro, build 26200 |
| Compiler / target | rustc 1.99.0 (b940084d7 2026-09-28), `x86_64-pc-windows-gnu` |
| Build | release, fat LTO, 1 codegen unit |
| Other load | interactive desktop session; not isolated |

### Micro (in-process, no network)

`caml-bench micro --runs 7`. Median [min – max] of 7 runs.

| benchmark | median | range | unit |
|---|---:|---:|---|
| `Instant::now()` | 23.3 | 23.2 – 24.6 | ns/call |
| `wall_clock_ns()` | 24.3 | 24.2 – 25.1 | ns/call |
| `RxMeta::now()` (both clocks) | 48.3 | 47.5 – 48.9 | ns/call |
| `process_packet`, 1 msg/pkt, full instrumentation | 105.8 | 102.2 – 115.6 | ns/pkt |
| `process_packet`, 1 msg/pkt, no processing sample | 78.1 | 73.2 – 106.7 | ns/pkt |
| `process_packet`, 1 msg/pkt, parse+sequence only | 13.5 | 12.7 – 20.3 | ns/pkt |
| `process_packet`, 4 msg/pkt, full instrumentation | 129.2 | 127.8 – 130.9 | ns/pkt |
| `process_packet`, 4 msg/pkt, no processing sample | 98.6 | 96.0 – 100.1 | ns/pkt |
| `process_packet`, 4 msg/pkt, parse+sequence only | 40.8 | 40.1 – 52.3 | ns/pkt |
| `process_packet`, 36 msg/pkt, full instrumentation | 228.6 | 225.8 – 240.8 | ns/pkt |
| `process_packet`, 36 msg/pkt, no processing sample | 200.4 | 198.7 – 208.5 | ns/pkt |
| `process_packet`, 36 msg/pkt, parse+sequence only | 163.1 | 159.4 – 172.5 | ns/pkt |
| `SequenceTracker`, in order | 0.8 | 0.8 – 1.0 | ns/msg |
| `SequenceTracker`, new gap per message, 1024-gap table full | 3.3 | 3.2 – 3.4 | ns/msg |
| `SequenceTracker`, 1024 open gaps, late fills newest-first | 7.8 | 7.6 – 9.0 | ns/msg |

What this shows:

* Parsing, validation, session gating, and sequencing cost **13.5 ns** for a
  one-message packet and roughly **4–9 ns per additional message** (about
  9 ns/message going from 1 to 4 messages, about 4.3 ns/message from 1 to
  36).
* Instrumentation dominates small packets: two clock reads in `RxMeta::now()`
  (~48 ns) plus one processing sample (~23–28 ns). Set
  `--processing-sample-every 0` (or N > 1) when the per-packet cost matters.
* The milestone-1 gap table cost **64 ns** per new gap when full and **95 ns**
  per late fill with 1,024 open gaps (`Vec::remove(0)` and a linear scan from
  the oldest gap; measured during the audit on the same machine with a
  similar workload in a scratch harness). The `VecDeque` + binary-search
  table costs **3.3 ns** and **7.8 ns**.

### Loopback (127.0.0.1)

`caml-bench loopback --runs 10` (22,000 packets per run, 2,000 warmup,
10,000 packets/s, 4 messages per packet, processing sampled every packet,
`UdpSource` backend).

| run | sent pkt/s | send→rx p50 (µs) | p99 | p99.9 | max | lost | clean |
|---:|---:|---:|---:|---:|---:|---:|:---:|
| 1 | 10000 | 9.0 | 33.8 | 112.6 | 225.0 | 0 | yes |
| 2 | 10000 | 9.0 | 33.8 | 114.7 | 393.6 | 0 | yes |
| 3 | 10000 | 9.0 | 33.8 | 100.4 | 268.2 | 0 | yes |
| 4 | 10000 | 9.0 | 34.8 | 104.4 | 288.7 | 0 | yes |
| 5 | 10000 | 9.0 | 31.2 | 55.3 | 88.9 | 0 | yes |
| 6 | 10000 | 9.0 | 30.7 | 50.2 | 3515.6 | 0 | yes |
| 7 | 10000 | 9.0 | 29.2 | 46.1 | 152.0 | 0 | yes |
| 8 | 10000 | 9.0 | 29.2 | 50.2 | 271.6 | 0 | yes |
| 9 | 10000 | 9.0 | 29.2 | 46.1 | 2329.8 | 0 | yes |
| 10 | 10000 | 9.0 | 29.2 | 41.0 | 82.5 | 0 | yes |

Across runs, median [min – max], µs: p50 8.96 [8.96 – 8.96]; p99 31.2
[29.2 – 34.8]; p99.9 55.3 [41.0 – 114.7]; max 272 [83 – 3516].

What this shows:

* The median is stable to the histogram bucket. Tails are not: two of ten
  runs saw a single multi-millisecond outlier, consistent with Windows
  scheduler preemption of an unpinned receiver thread. **This is a property
  of the environment, not a baseline for the target.**
* Processing p50 / p99 report 0.10 / 0.50 µs: that is the 100 ns `Instant`
  resolution on Windows, not a measurement. Use the micro numbers above.

### Corrections to milestone-1 claims

* The learning log's smoke run reported a clean-run p99 of **8.1 ms** and a
  max of 20.2 ms. Four repeated runs during the 2026-10-09 audit, and the ten
  runs above, show p99 of 25–35 µs. The 8.1 ms figure did not reproduce; its
  cause is unknown.
* "Processing time about 100 ns per packet" was the clock resolution. The
  batch-timed figure is 106 ns with full instrumentation, of which about
  13.5 ns is the actual work for a one-message packet.

## Planned measurements (blocked on Linux and hardware)

See `docs/hardware-plan.md` §6 for the full campaign. In short, on the target
box with a second machine sending over a cable, compare on identical traffic:

1. `udp` (one `recv_from` per datagram), 2. `recvmmsg`, 3. `recvmmsg` +
`SO_BUSY_POLL`, 4. AF_XDP copy mode, 5. AF_XDP zero-copy (external NIC),

each at fixed rates up to line rate, reporting send→rx and kernel→rx
percentiles (p50 … p99.99, max), kernel/NIC drops, and CPU use, with the
host tuning recorded.
