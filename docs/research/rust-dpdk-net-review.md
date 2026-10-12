# Review: `youyuanwu/rust-dpdk-net` — lessons for a CAML DPDK backend

Status: **research note**. Nothing in this document is implemented in the CAML
workspace. The `DpdkSource` design below is **design only**.

> **Addendum (2026-10-09).** An independent audit re-checked this note's key
> claims against upstream commit `8670af4` and DPDK v25.11 and found them
> accurate: the LIFO `rx_batch.pop()` (`dpdk_device.rs:256`; the "may have
> reversed the batch" comment at `:251` is stale, since `RxQueue::rx` stores
> the burst in wire order), the unconditional `rte_mempool_free` in
> `MemPool::drop`, `TxQueue::tx`'s `remove(0)` loop, and the em PMD citations
> (`em_ethdev.c:112-190`, `:1032`, `:1150`). New fact: the CAML box's NIC is
> the I219-LM (13), device `0x15FB`, which Ubuntu 24.04's DPDK 23.11.4 does
> **not** list (v25.11 does, at `em_ethdev.c:168`). **Decision changed:**
> kernel bypass will use AF_XDP on an external Thunderbolt NIC, not DPDK
> (`docs/decisions.md` D7–D9, `docs/hardware-plan.md`). The lessons below
> (ownership of pool memory, wire-order bursts, shim pattern) still apply to
> any future DPDK work.

Method: read-only review of a shallow clone (outside the workspace) plus the
GitHub, crates.io, DPDK, and smoltcp sources fetched over HTTPS. DPDK was not
installed and the reviewed repo was not built. Citations use
`path:start-end` against the commit listed below. Each claim is labelled
**[V]** (verified by reading source or official docs) or **[I]** (inferred or
reasoned, not executed).

---

## 1. Summary verdict

`rust-dpdk-net` is a **TCP/IP-over-DPDK application framework**:
DPDK packet I/O → `smoltcp` (user-space TCP/IP stack) → a runtime-agnostic async
reactor → a tokio `current_thread` + `LocalSet` per lcore → hyper / axum /
tonic / Quinn adapters. It is well-organized, actively maintained by one
author, MIT-licensed, and CI-green. But its design goals (TCP servers, async
Rust, HTTP/gRPC) are close to the opposite of CAML's (receive-mostly UDP, one
pinned thread, deterministic latency).

- **Do not depend on it.** Its UDP receive path makes two payload copies, goes
  through an async waker and executor hop, uses software checksums, and exposes
  no receive timestamp. It also processes each RX burst **in reverse order**
  (`ArrayVec::pop()`), which would show up as sequence reordering in a
  market-data feed.
- **Learn from its lowest layer.** The `dpdk-net-sys` build (pkg-config →
  bindgen allowlist → a small C shim for DPDK's `static inline` functions) and
  the thin `EalBuilder` / `EthDevBuilder` / `Mbuf` RAII wrappers are a good
  template for a ~1–2 kLoC CAML-owned backend.
- **Hardware:** DPDK's `net_e1000_em` PMD *does* list many I219-LM/V PCI IDs
  [V], which contradicts the common assumption that the I219 isn't supported.
  However, DPDK documents no validation on I219. The PMD has no RSS, no timestamp
  offload, and at most 2 queues. The I219 is also almost certainly the server's
  only/management port. **DPDK isn't worth pursuing for performance on
  `camlcompute`.** Measure the kernel-socket baseline first. Revisit DPDK only
  with a NIC such as Intel X710/E810 or NVIDIA/Mellanox ConnectX.

---

## 2. Facts

| Item | Value |
|---|---|
| Repo | https://github.com/youyuanwu/rust-dpdk-net |
| Commit reviewed | `8670af48d14c801b2ae52be316360ef4a1c36022` (2026-10-05, Dependabot bump) [V] |
| Last human commit | `77456e3` 2026-09-17 "Update bindgen to 0.73" (youyuanwu) [V] |
| Created / activity | Repo created 2026-01-01; ~80 commits by the author, 23 Dependabot, 3 Copilot; 22 stars, 5 forks; 3 open issues/PRs (#1 "Async optimization, bug fixes, and UDP support", #4 IP fragmentation question, #33 toolchain bump) [V, GitHub API 2026-10-09] |
| CI | GitHub Actions `build` workflow: last 6 runs on `main` succeeded [V]. Jobs: `cargo build -p dpdk-net-sys`, check/fmt/clippy/`cargo test --all` against Ubuntu 26.04 `libdpdk-dev`, plus a no-KVM libvirt two-VM e2e HTTP test (`.github/workflows/CI.yml:13-249`) |
| License | MIT (`Cargo.toml` `package.license = "MIT"`, `LICENSE`) [V] |
| DPDK version | v25.11.2 when built from source via CMake (`CMakeLists.txt:60,66`, pinned git SHA). CI uses the distro `libdpdk-dev` on Ubuntu 26.04 instead [V] |
| Bindings | Own `dpdk-net-sys` crate: bindgen 0.73 at build time with a function/type allowlist, plus a C shim compiled by `cc` (`dpdk-net-sys/build.rs:1-113`) [V] |
| `static inline` handling | Hand-written C wrappers re-exported as real symbols, e.g. `rust_eth_rx_burst` → `rte_eth_rx_burst`, `rust_pktmbuf_free`, `rust_pktmbuf_mtod`, `rust_rte_lcore_id` (`dpdk-net-sys/include/wrapper.h:16-41`, `src/wrapper.c:73-81`). `RTE_BIT64` RSS macros are exposed as `static const` (`wrapper.h:43-62`) [V] |
| DPDK discovery / link | `pkgconf` crate probes `libdpdk`; all cflags are forwarded to bindgen and `cc` (`build.rs:9-21`). README says the static `.pc` metadata is used (needs `libjitterentropy3-dev`). I didn't verify the `pkgconf` crate's internals |
| Toolchain | `rust-toolchain.toml` pins 1.98.1; edition 2024 [V] |
| crates.io | `dpdk-net` 0.1.0 and `dpdk-net-sys` 0.1.0, published 2026-01-18 and never updated (34 / 153 downloads), so the published crates are far behind `main` [V] |
| Key deps (Cargo.lock) | smoltcp 0.14.0, tokio 1.53.1, arrayvec 0.7.8, arc-swap, nix 0.31, bindgen 0.73.2 [V] |
| Platform | Linux only in practice. No `cfg(target_os)` gates; it needs `nix::sched_setaffinity`, pkg-config `libdpdk`, hugepages, and root/vfio (`README.md:83-87`) [V] |

---

## 3. What it actually is: layers

| Layer | Crate / file | Notes |
|---|---|---|
| FFI | `dpdk-net-sys` | bindgen output + `wrapper.c` shim |
| Safe-ish DPDK wrappers | `dpdk-net/src/api/rte/{eal,eth,pktmbuf,mbuf,queue,lcore,thread}.rs` | Builders and RAII guards. The only layer relevant to CAML |
| smoltcp device adapter | `dpdk-net/src/device/dpdk_device.rs` | `impl smoltcp::phy::Device for DpdkDevice` |
| Multi-queue ARP workaround | `dpdk-net/src/device/arp_cache.rs` | `ArcSwap<HashMap>` + version counter. Queue 0 learns ARP, other queues get synthetic ARP replies injected |
| Async reactor | `dpdk-net/src/runtime/reactor.rs` | `Rc<RefCell<ReactorInner>>`, poll loop with `yield_now()` |
| Async sockets | `dpdk-net/src/socket/{tcp,udp}.rs` | Futures over smoltcp sockets + wakers |
| App framework | `dpdk-net-util/src/app.rs` | `DpdkApp`: one queue per EAL lcore, a tokio `current_thread` runtime per lcore |
| OS-thread bridge | `dpdk-net-util/src/bridge/*` | `tokio::sync::mpsc` relays between ordinary threads and lcore-owned `!Send` sockets |
| Integrations | `dpdk-net-tonic`, `dpdk-net-quinn` | axum/tonic serve; Quinn `AsyncUdpSocket` over the bridge (1 datagram per call, no GSO/GRO, `dpdk-net-quinn/src/socket.rs:70-80`) |
| Tests/bench | `dpdk-net-test`, `tests/dpdk-bench-*`, `tests/e2e` | vdev-based (`net_ring0`, `--no-huge --no-pci`) integration tests. Benchmarks are HTTP on Azure ConnectX-5 VFs (`docs/Bench/Benchmark.md:13`) |

There's also an `archive/` with abandoned AF_XDP experiments. Its notes record
that libxdp's capability check and mlx4 VFs blocked XDP on Azure
(`archive/xdp-test/README.md`). That's a useful data point: the author also hit
"driver does not support native XDP" before moving to DPDK.

---

## 4. Findings by focus area

### 4.1 Architecture and crate layout
- Clean separation between the FFI layer, the wrappers, the stack, and the
  runtime [V]. The wrapper layer (`api::rte`) has no dependency on smoltcp or
  tokio, so it could in principle be used alone. `dpdk-net`'s own `Cargo.toml`
  does pull in smoltcp, futures-io, and arc-swap unconditionally, though; there's
  no feature to get just the wrappers [V].
- `docs/Architecture.md` is partly stale: it links `dpdk-net/src/tcp/...`, but
  the code now lives in `device/`, `runtime/`, `socket/` [V].

### 4.2 Receive path, end to end (UDP via `dpdk_net::socket::UdpSocket`)
1. `Reactor::run_with_batch_size` loops: `poll_ingress_single()` up to 32 times,
   then `poll_egress()`, `cleanup_orphaned()`, `yield_now().await`
   (`runtime/reactor.rs:199-240`). It reads `smoltcp::time::Instant::now()` once per
   loop iteration, which smoltcp uses as protocol time, not as an RX timestamp
   (`reactor.rs:201`) [V].
2. smoltcp calls `DpdkDevice::receive()`. This calls `poll_rx()`, which first
   calls `flush_tx()`, then calls `RxQueue::rx` **only if `rx_batch` is empty**
   (drain-then-refill) (`device/dpdk_device.rs:125-146`) [V].
3. `RxQueue::rx` builds a stack array of 64 raw pointers and calls
   `rust_eth_rx_burst(port, queue, ptrs, min(remaining_capacity, 64))`. It wraps
   each pointer in an `Mbuf` and pushes it into `ArrayVec<Mbuf, 64>`
   (`api/rte/queue.rs:10,45-72`). The burst size is effectively 64 [V].
4. `receive()` takes **`self.rx_batch.pop()`**: the *last* packet of the burst
   first (`dpdk_device.rs:256`). Within each burst, packets are therefore
   handed to smoltcp in reverse arrival order [V]. The comment at `:251-253`
   ("pushed to back, popped first with FIFO") contradicts the code: `pop()` is
   LIFO. TCP tolerates this through reassembly, but **a sequenced UDP feed would
   see spurious reordering**. The test-only raw `UdpSocket` does the same
   (`dpdk-net-test/src/udp.rs:232`) [V].
5. `DpdkRxToken::consume` lends `mbuf.data()` (a slice into the mbuf's data
   room, which the NIC wrote by DMA) to smoltcp (`dpdk_device.rs:26-34`). No copy
   happens here [V].
6. smoltcp parses Ethernet/IPv4/UDP, verifies checksums in software (the
   default `DeviceCapabilities`, `dpdk_device.rs:291-296`; smoltcp's
   `Checksum::default()` is `Both`, `smoltcp v0.14.0 src/phy/mod.rs:187-199`). It
   then **copies the payload** into the socket's `udp::PacketBuffer` ring
   (**copy #1**) and wakes the registered waker [V for checksum default; copy
   inferred from smoltcp's `PacketBuffer` design, I].
7. The `RxToken` is dropped, so `rte_pktmbuf_free` runs **once per packet**
   through the C shim. `rte_pktmbuf_free_bulk` is allowlisted (`build.rs:56`)
   but never called [V].
8. The tokio `LocalSet` later polls `UdpRecvFuture`, which calls
   `socket.recv_slice(buf)`, i.e. **copy #2** into the caller's buffer
   (`socket/udp.rs:150-167`) [V].
9. If you go through the OS-thread bridge, add a `Bytes` allocation and a
   `tokio::sync::mpsc` hop (**copy #3**). The repo itself estimates "~1-5µs latency
   per hop" (`docs/Limitations.md:96-102`) [V, number is the author's].

Allocations: UDP socket buffers are allocated once at `bind`
(`socket/udp.rs:45-48`) [V]. The TX token falls back to `vec![0u8; len]` and
silently drops the packet when the mempool is exhausted
(`dpdk_device.rs:325-329`) [V]. `TxQueue::tx` removes sent packets with
`ArrayVec::remove(0)` in a loop, which is O(n²) element shifting
(`queue.rs:140-144`) [V].

Per-packet overhead on the hot path [I]: about 4–6 non-inlinable FFI calls
(`rust_pktmbuf_mtod`, `rust_pktmbuf_data_len`, `rust_pktmbuf_free`, ...),
smoltcp's interface dispatch and checksums, two memcpys, `RefCell` borrow
checks, and the waker/executor round-trip.

### 4.3 DPDK integration
- **EAL:** `EalBuilder` renders typed options (`-l`, `-a`, `--vdev`,
  `--no-huge`, `--in-memory`, `--file-prefix`, ...) into argv. `Eal::init` guards
  against double initialization with a global `AtomicBool`, and `Drop` calls
  `rte_eal_cleanup` (`api/rte/eal.rs:14-46,320-365`) [V]. One wart: `Drop` resets
  the flag, which suggests re-init is possible, but the type's own doc comment
  says it isn't (`eal.rs:276-277,359-364`).
- **Port/queue:** `EthDevBuilder::build` does `rte_eth_dev_configure` → per-queue
  `rx/tx_queue_setup` (1024 descriptors by default, NUMA socket taken from
  `rte_eth_dev_socket_id`) → optional RETA/RSS-hash update → promiscuous →
  `rte_eth_dev_start` (`api/rte/eth.rs:438-474,727-776`) [V]. `EthDev` has **no
  `Drop`**, so stop/close are manual (`eth.rs:375-654`) [V].
- **Mempool:** `rte_pktmbuf_pool_create`. Defaults are 8191 mbufs, cache 256,
  data room `RTE_MBUF_DEFAULT_DATAROOM + RTE_PKTMBUF_HEADROOM`,
  `SOCKET_ID_ANY` (`api/rte/pktmbuf.rs:81-92,145-163`). `DpdkApp` creates one
  shared pool of `8192 × n_queues` and doesn't set `socket_id`, so it isn't
  explicitly NUMA-local (`dpdk-net-util/src/app.rs:177-184`) [V].
- **Hugepages:** the README requires them. Tests use `--no-huge`
  (`dpdk-net-test/src/dpdk_test.rs:73-77`) [V].
- **Build:** pkg-config → bindgen (needs libclang; CI sets `LIBCLANG_PATH`) →
  `cc`. `cc_builder.compile` is called twice on the same file under two library
  names (`build.rs:38,122`). That's harmless but sloppy [V]. The leading
  allowlist regex `"rte_.*_"` (`build.rs:43`) is, as far as I know, anchored by
  bindgen, so it only matches names *ending* in `_`. Its comment ("generate all
  the wrapper functions") looks stale [I].
- **unsafe:** about 119 occurrences of `unsafe` across the repo [V, grep count].
  `unsafe impl Send for Mbuf` (`mbuf.rs:22`), `Send+Sync for MemPool`
  (`pktmbuf.rs:15-16`) and `Lcore` (`lcore.rs:129-130`). Most `unsafe` blocks
  lack `// SAFETY:` justifications. `Lcore::launch` boxes the closure and passes
  it through `rte_eal_remote_launch` with a monomorphized `extern "C"`
  trampoline, freeing the box on failure (`lcore.rs:308-340`) [V]. A panic inside
  the closure crosses an `extern "C"` boundary, which aborts the process on
  current Rust [I, language rule].

### 4.4 Memory management and ownership
- `Mbuf` is an owning RAII wrapper over `NonNull<rte_mbuf>`. `Drop` calls
  `rte_pktmbuf_free`, and `into_raw` / `mem::forget` handle transfers to TX
  (`api/rte/mbuf.rs:16-57,216-222`; `queue.rs:139-144,155-168`) [V]. That's the
  right basic shape.
- **Soundness gap:** an `Mbuf` carries no lifetime tied to its `MemPool`, and
  `MemPool::drop` calls `rte_mempool_free` unconditionally
  (`pktmbuf.rs:244-250`). Dropping the pool while mbufs are outstanding, or while
  a started port's RX ring still holds mbufs from that pool, is a
  use-after-free that safe Rust allows [V by reading]. The repo's own
  `udp_echo_test` moves the pool into a socket and drops it *before*
  `eth_dev.stop()` (`dpdk-net-test/tests/udp_echo_test.rs:61,87-89`). It's
  harmless with `net_ring` (which doesn't refill from the pool) but would be a
  real UAF on a hardware PMD [I].
- `data()` returns only the first segment (`data_len`), so multi-segment mbufs
  are silently truncated. That's fine as long as scatter RX stays disabled
  (`mbuf.rs:91-99`) [V].
- RX payloads are zero-copy only as far as smoltcp; after that they're copied
  (see 4.2).

### 4.5 Queue architecture
- One RX/TX queue pair per EAL lcore. RSS uses `NONFRAG_IPV4_TCP |
  NONFRAG_IPV6_TCP` and a round-robin RETA (`app.rs:186-206`, `eth.rs:561-593`).
  The **Microsoft Toeplitz key** is forced "for Azure NICs" when multi-queue
  (`eth.rs:752-764`) [V]. RSS is TCP-only, so UDP flows aren't hashed on ports.
- No `rte_ring`. Cross-thread traffic only goes through the bridge's
  `tokio::sync::mpsc` (capacity 256–1024) and `ArcSwap` for ARP
  (`dpdk-net-util/src/bridge/worker.rs:17-27`) [V].
- Backpressure: TX stays in `tx_batch` (256) when the hardware ring is full, and
  `transmit()` returns `None` (`dpdk_device.rs:268-289`). On RX, smoltcp drops
  the packet if the socket's ring is full [I]. **RX drops aren't surfaced
  anywhere:** `EthDev::stats()` exists (`eth.rs:430-435`) but nothing reads
  `imissed` or `rx_nombuf` [V, grep].

### 4.6 CPU affinity and threading
- `DpdkApp` uses native EAL lcores. EAL pins them during `rte_eal_init`
  according to `-l` (`app.rs:155-160,235-275`). Each lcore runs a tokio
  `current_thread` runtime with a `LocalSet`, plus the reactor task
  (`app.rs:343-375`) [V].
- `set_cpu_affinity` (nix `sched_setaffinity`) and `ThreadRegistration`
  (`rte_thread_register`, `!Send` guard) let non-EAL threads use DPDK
  (`api/rte/thread.rs:30-34,65-111`) [V].
- The reactor busy-polls: it always yields and is immediately rescheduled
  (`reactor.rs:22-40,237`), so a core sits at 100% even when idle. The author
  acknowledges this (`docs/Limitations.md:37-39`) [V].

### 4.7 Latency overhead sources (ranked for a UDP receive workload) [I]
1. **Async scheduling:** packet → waker → `LocalSet` run-queue → future poll.
   Between ingress and the application, the reactor processes up to 32 packets,
   then egress, then yields, so delivery is batch-delayed.
2. **Copies:** mbuf → smoltcp ring → user buffer (+ bridge).
3. **smoltcp processing:** generic interface dispatch, software IPv4/UDP
   checksum verification, socket lookup.
4. **FFI shims:** several non-inlined C calls per packet. With cross-language LTO
   this would be minor; without it, it's measurable at Mpps rates.
5. **LIFO within a burst:** a correctness issue for sequenced feeds, not just a
   latency one.
6. **No timestamping:** you can't measure wire→app latency at all.
7. Shared ARP cache injection allocates mbufs and copies on queues ≠ 0
   (`dpdk_device.rs:157-200`). It's irrelevant for CAML.

### 4.8 Code quality, tests, maturity, risks
- Readable and documented, idiomatic builders, clippy `-D warnings` in CI [V].
- Tests: about 60 `#[test]` functions outside `archive/`, almost all in
  `dpdk-net-test` and run on `net_ring0` with `--no-huge`. Only 8 are unit
  tests in `dpdk-net` itself (lcore 2, queue 1, error 1, arp_cache 4)
  [V, grep]. `udp_echo_test` never asserts a
  received packet (`udp_echo_test.rs:82-84`) [V]. The UDP stack is described as
  "only partially implemented" (`docs/Limitations.md:42-45`) [V].
- Risks of depending on it: single maintainer, API marked unstable
  (`README.md:166`), the published crate is stale, and TCP/HTTP is the focus. It
  pins its own Rust toolchain, and DPDK has to match its bindgen allowlist.

---

## 5. Recommendations

### (a) Reuse directly

| Item | Decision | Reason |
|---|---|---|
| `dpdk-net` / `dpdk-net-sys` as dependencies | **No** | Pulls in smoltcp/async, LIFO RX order, pool-lifetime soundness gap, stale crates.io release, single maintainer |
| Shim pattern: `wrapper.h`/`wrapper.c` + `build.rs` (pkg-config → bindgen allowlist → `cc`) | **Copy the pattern** (re-type it; if any file is copied substantially, keep the MIT copyright + permission notice) | It's the standard, minimal way to reach `rte_eth_rx_burst` and the `rte_pktmbuf_*` inlines from Rust. MIT is compatible with any CAML license |
| `EalBuilder` argv rendering, `EthDevBuilder` call order, `Mbuf` RAII shape | **Use as reference** | Small and correct in shape. We need fewer options and stricter lifetimes |

### (b) Implement ourselves, with established crates (versions from crates.io, 2026-10-09)

| Need | Crate (verified) | Note |
|---|---|---|
| FFI generation | `bindgen` 0.73.2 (build-dep) | Allowlist only what we call. bindgen also has an *experimental* `wrap_static_fns` option that could replace the hand-written shim [I, not tried] |
| DPDK discovery | `pkg-config` 0.3.34 (or `pkgconf` 0.2.0 as the reviewed repo uses) | `libdpdk.pc`. Prefer dynamic linking first to keep builds simple [I] |
| C shim | `cc` 1.7.0 (build-dep) | Compile `wrapper.c` with DPDK's cflags (`-march`, `-include rte_config.h`) |
| Raw libc (clock_gettime, sched) | `libc` 0.2.190 | Already implied by `socket2` |
| L2/L3/L4 parsing | Hand-written fast-path parser (~150 lines) in the DPDK crate, with **`etherparse` 0.21.0** (MIT/Apache-2.0, maintained, zero-alloc slicing, 802.1Q) as a **dev-dependency test oracle** | The fast path only needs Ethernet [+ one 802.1Q tag] → IPv4 → UDP. etherparse's README doesn't advertise checksum verification during slicing, so checksum policy stays ours either way [V README; I for the design]. `smoltcp::wire` (0BSD) is an alternative |
| Property tests for the parser | `proptest` 1.11.0 | Differential test against etherparse |
| Test pcaps for `net_pcap` | `pcap-file` 2.0.0 (3.0.0-rc1 is pre-release) | `caml-sim` would need a small extension (not implemented) to write Ethernet-framed pcaps of the synthetic feed |
| TSC clock (optional) | `quanta` 0.13.0 or `minstant` 0.1.7 | Only if `Instant::now()` (vDSO `clock_gettime`) shows up in profiles |
| Pinning for non-DPDK backends | `core_affinity` 0.8.3 | Not needed for DPDK; EAL pins the main lcore |
| Histograms | existing `caml-engine` histogram (`hdrhistogram` 7.6.0 if ever needed) | — |

### (c) Unnecessary for CAML

| Component | Why not |
|---|---|
| smoltcp / any TCP/IP stack | Receive-mostly UDP needs header validation, not a stack. Stack processing adds copies and checksums and needs ARP state. No demonstrated requirement |
| Async reactor / tokio / `LocalSet` | Adds waker and scheduling latency and non-determinism. CAML's single inline pinned loop is already the lower-latency design |
| OS-thread bridge (`mpsc`, `Bytes`) | An extra copy plus cross-core traffic. If CAML ever needs a hand-off, use an SPSC ring of *indices/offsets*, not a general channel |
| Multi-queue RSS, shared ARP cache, RETA tuning | One feed, one queue, one core. RSS matters only with many flows and many cores |
| hyper/axum/tonic/Quinn adapters | Out of scope |
| TX path beyond, at most, IGMP reports / ARP replies | Receive-mostly. See §6.4 for the kernel features we'd lose |

---

## 6. Proposed design: `DpdkSource` (**Planned, not implemented**)

### 6.1 Placement and build isolation
- New crates (not created yet): `crates/transport-dpdk` (package
  `caml-transport-dpdk`), containing a private `sys` module (bindgen + shim)
  and `DpdkSource`.
- **Default workspace never needs DPDK:** the crate has an off-by-default
  feature `ffi`. Without it, `build.rs` returns immediately and `lib.rs`
  compiles to an empty crate. That keeps `cargo test --workspace` working on
  Windows. Everything DPDK-related is behind `#[cfg(all(target_os = "linux",
  feature = "ffi"))]`. `apps/receiver` gets a feature `dpdk =
  ["dep:caml-transport-dpdk", "caml-transport-dpdk/ffi"]`.
- **Lints:** the workspace sets `unsafe_code = "forbid"`, and `forbid` can't be
  relaxed per module. So this crate must declare its own `[lints]` table instead
  of `workspace = true`: `unsafe_code = "deny"` with `#[allow(unsafe_code)]`
  only in `sys.rs`/`port.rs`, plus `unsafe_op_in_unsafe_fn = "deny"` and clippy
  `undocumented_unsafe_blocks = "deny"`. Every `unsafe` block gets a
  `// SAFETY:` comment.
- `caml-engine` stays free of DPDK. The DPDK crate depends on `caml-engine` only
  for `PacketSource` and `RxMeta`.

### 6.2 Lifecycle (single thread, no remote launch)
1. `rte_eal_init` with e.g. `-l 3 -a 0000:xx:00.0 --in-memory
   --file-prefix caml` makes the calling thread the main lcore, pinned to core 3.
   **Gotcha:** threads the app creates *afterwards* from this thread inherit the
   single-core affinity. Spawn any helper threads first, or reset their affinity
   [I, Linux affinity inheritance].
2. Mempool on `rte_eth_dev_socket_id(port)` (NUMA-local). Size ≥ `nb_rx_desc +
   burst + cache + slack`, so 8191 mbufs with cache 256 is ample for one queue
   at 1024 descriptors. Data room `RTE_MBUF_DEFAULT_BUF_SIZE`.
3. `rte_eth_dev_configure(port, 1 rxq, 1 txq)`. Request
   `RTE_ETH_RX_OFFLOAD_{IPV4,UDP}_CKSUM` only if `dev_info.rx_offload_capa`
   has them. Request `RTE_ETH_RX_OFFLOAD_TIMESTAMP` only if supported (the
   DPDK feature matrix lists it for mlx5 = Y and ice = Partial; **not** for
   e1000/em) [V, `doc/guides/nics/features/*.ini` v25.11].
4. For multicast feeds: `rte_eth_dev_set_mc_addr_list` or
   `rte_eth_allmulticast_enable`, and promiscuous off.
5. `rte_eth_dev_start`. Teardown runs in the reverse order in an explicit `Drop`:
   stop → close → free mempool → `rte_eal_cleanup`. That fixes the reviewed
   repo's pool-lifetime hole by construction, because `DpdkSource` owns all of
   them and lends nothing beyond a callback.

### 6.3 Hot loop semantics
- `poll()` busy-spins on `rte_eth_rx_burst(port, 0, pkts, BURST)` with
  **BURST = 32** (DPDK's conventional default; tune 16–64 by measurement).
  It returns as soon as a burst produces ≥1 delivered payload, or returns
  `Ok(0)` when a TSC/`Instant` deadline (`poll_budget`, e.g. 1 ms) expires.
  That way the existing outer loop's shutdown and metrics checks still run. It
  never sleeps.
- **Lending:** each mbuf's data slice is passed to `on_packet` as `&[u8]`
  pointing at the UDP payload inside the mbuf. All mbufs of the burst are freed
  with **one `rte_pktmbuf_free_bulk`** after the last callback returns. The
  free sits in a guard's `Drop`, so a panicking callback can't leak mbufs. The
  higher-ranked `FnMut(&[u8], RxMeta)` bound already prevents the slice from
  escaping.
- **Order:** iterate `pkts[0..n]` front to back to preserve wire order (cf. the
  LIFO bug in §4.2).
- **Field access:** read `buf_addr + data_off`, `data_len`, `pkt_len`,
  `nb_segs`, `ol_flags` directly from the bindgen `rte_mbuf` struct instead of
  through per-packet shim calls. Treat `nb_segs > 1` as a drop with a reason
  (scatter is disabled).

### 6.4 Header parsing, filtering, policy
Parse order and drop reasons, each with its own counter:
`Truncated` → Ethernet type `0x0800`, or `0x8100` + one inner tag
(`0x88A8`/QinQ → `UnsupportedVlan`) → IPv4 `version == 4`, `IHL ≥ 5`,
`total_length ≤ frame` (**use `total_length`, not the frame length: Ethernet
pads short frames to 60 bytes**) → fragments (`MF` or offset ≠ 0) →
`Fragment` (dropped; exchange feeds don't fragment, and reassembly isn't
justified) → `proto == 17` → optional `dst_ip` match (unicast or multicast
group) → `dst_port` match → UDP `8 ≤ len ≤ ip_payload` → checksum.

Checksum policy: `TrustNic` checks `ol_flags` for `RTE_MBUF_F_RX_IP_CKSUM_BAD`
/ `RTE_MBUF_F_RX_L4_CKSUM_BAD` (these are macros, so expose them as
`static const` the way the reviewed repo does for RSS). `Software` verifies
IPv4 and UDP in Rust (IPv4 UDP checksum 0 means "absent"). `Skip` exists for
benchmarking only.

Kernel features you **lose** under DPDK and must replace or decide on: IGMP
joins (send reports yourself or use static switch config), ARP replies for
unicast feeds (static ARP on the sender, or answer ARP via the TX queue),
`tcpdump` visibility (use `dpdk-dumpcap` or our own pcap tap), firewall,
SSH on that port.

### 6.5 Timestamps and accounting
- `RxMeta` is captured **once per burst**. Every packet in a burst left the NIC
  ring at the same `rx_burst` call, so per-packet software stamps inside a burst
  only measure our loop. `rx_instant = Instant::now()`, `rx_wall_ns =
  clock_gettime(CLOCK_REALTIME)`. If profiling shows clock reads matter, switch
  to raw TSC (requires `constant_tsc`/`nonstop_tsc` and calibration).
- NIC hardware timestamps (future, NIC-dependent) come from the mbuf
  timestamp dynfield when `RX_OFFLOAD_TIMESTAMP` is enabled. That would need a
  future `RxMeta` extension (e.g. `rx_hw_ns: Option<u64>`). That's a change to
  the public `RxMeta` struct, not the `PacketSource` trait, so it's not proposed
  now. (Since this review, `RxMeta` is `#[non_exhaustive]` with an
  `RxMeta::now()` constructor, so adding such a field later won't break
  backends.)
- About every 1 s, off the per-packet path, read `rte_eth_stats_get` and report
  `ipackets`, **`imissed`** (the NIC dropped because our ring was full: "we were
  too slow"), `ierrors`, **`rx_nombuf`** (mempool exhausted while refilling).
  Report those next to our parse drop counters. The em PMD maps `imissed`
  from the MPC register (`drivers/net/intel/e1000/em_ethdev.c:1032`) [V].

### 6.6 Sketch (**illustrative only — not compiled, not implemented**)

```rust
// crates/transport-dpdk/src/source.rs — SKETCH
#![cfg(all(target_os = "linux", feature = "ffi"))]

pub enum ChecksumPolicy { TrustNic, Software, Skip }

pub struct DpdkConfig {
    pub eal_args: Vec<String>,   // ["caml-recv", "-l", "3", "-a", "0000:03:00.0", "--in-memory"]
    pub port_id: u16,
    pub nb_rx_desc: u16,         // 1024
    pub burst: u16,              // 32, <= MAX_BURST
    pub mbufs: u32,              // 8191
    pub dst_ip: Option<std::net::Ipv4Addr>,
    pub dst_port: u16,
    pub checksum: ChecksumPolicy,
    pub poll_budget: std::time::Duration,
}

const MAX_BURST: usize = 64;

pub struct DpdkSource {
    port: u16,
    burst: u16,
    poll_budget: std::time::Duration,
    filter: UdpFilter,           // pure Rust, unit-tested on any OS
    drops: DropCounters,
    pool: core::ptr::NonNull<sys::rte_mempool>,
    _eal: EalGuard,              // Drop order handled explicitly in `impl Drop`
    _not_send: core::marker::PhantomData<*mut ()>, // per-lcore mempool cache: stay on init thread
}

impl caml_engine::transport::PacketSource for DpdkSource {
    fn poll<F>(&mut self, mut on_packet: F) -> std::io::Result<usize>
    where
        F: FnMut(&[u8], caml_engine::transport::RxMeta),
    {
        let deadline = std::time::Instant::now() + self.poll_budget;
        loop {
            let mut pkts = [core::ptr::null_mut::<sys::rte_mbuf>(); MAX_BURST];
            // SAFETY: port started with rx queue 0; `pkts` has MAX_BURST slots and burst <= MAX_BURST.
            let n = unsafe { sys::rust_eth_rx_burst(self.port, 0, pkts.as_mut_ptr(), self.burst) } as usize;
            if n == 0 {
                if std::time::Instant::now() >= deadline { return Ok(0); }
                core::hint::spin_loop();
                continue;
            }
            let meta = rx_meta_now();                 // one stamp per burst
            let burst = BurstGuard::new(&pkts[..n]);  // Drop => rte_pktmbuf_free_bulk (also on panic)
            let mut delivered = 0;
            for m in burst.iter() {                   // wire order: front to back
                // SAFETY: `m` came from rx_burst and stays valid until `burst` drops.
                let (frame, ol_flags, nb_segs) = unsafe { mbuf_view(m) };
                match self.filter.udp_payload(frame, ol_flags, nb_segs) {
                    Ok(payload) => { on_packet(payload, meta); delivered += 1; }
                    Err(reason) => self.drops.bump(reason),
                }
            }
            drop(burst);
            if delivered > 0 || std::time::Instant::now() >= deadline { return Ok(delivered); }
        }
    }
}
```

### 6.7 Testing without supported hardware
- **Any OS (no DPDK):** `UdpFilter` is pure Rust over `&[u8]`. Unit tests cover
  VLAN, padding, fragments, bad lengths, and checksums, plus proptest
  differential tests against `etherparse`.
- **Linux VM (or WSL2 if installed later), DPDK from distro packages, no
  hardware:** EAL `--no-huge --in-memory --no-pci` plus a vdev:
  - `net_pcap0,rx_pcap=feed.pcap[,infinite_rx=1]`: deterministic replay of a
    `caml-sim`-generated, Ethernet-framed pcap through the *real* `DpdkSource`
    code path, including sequence and gap logic. **Best functional test.**
    [V devargs, `doc/guides/nics/pcap.rst`]
  - `net_ring0`: in-process loopback, so a test can TX crafted frames and RX
    them (the approach the reviewed repo uses).
  - `net_tap0`: a kernel TAP interface; `caml-feed` sends UDP through the kernel
    into DPDK. End-to-end, but kernel-mediated.
  - `net_af_packet0,iface=veth0`: kernel `PACKET_MMAP` underneath
    [V devargs, `af_packet.rst`].
  - `net_null0`: an empty/synthetic packet source/sink that measures the
    loop's fixed overhead. The contents aren't valid feed packets, so expect all
    of them to be filtered.
  - `virtio_user0,path=/dev/vhost-net`: exercises the virtio PMD, but the data
    path is still the kernel's vhost.
- **None of these demonstrate kernel-bypass performance.** They validate
  correctness, lifecycle, and accounting only. Performance claims require a
  supported physical NIC bound to `vfio-pci` on isolated cores.

---

## 7. Hardware reality: DPDK on the I219-LM / e1000e target

**Verified from DPDK source:** DPDK v25.11's `net_e1000_em` PMD PCI table
includes `E1000_DEV_ID_PCH_*_I219_LM*/V*` entries from SPT (`I219_LM`) through
ARL (`I219_LM24`) (`drivers/net/intel/e1000/em_ethdev.c:112-190`, registered
at `:1961`). v23.11 and v24.11 also contain I219 IDs (in
`drivers/net/e1000/em_ethdev.c`). So DPDK **will probe** an I219-LM whose
device ID is in that list. This contradicts the common "I219 isn't supported"
assumption.

**But it's not a performance platform:**
- DPDK's em guide documents validation only for *emulated* 82540EM / 82545EM /
  82574L devices (`doc/guides/nics/e1000em.rst`). I219 operation is
  probe-listed, not validated [V doc; actual behaviour **unverified**].
- The em PMD reports `max_rx_queues = 2` (`em_ethdev.c:1150`). The `e1000.ini`
  feature matrix lists no RSS hash and no timestamp offload. It does list
  L3/L4 RX checksum offload, VLAN offload, and Rx interrupt [V].
- 1 GbE tops out at about 1.49 Mpps with 64-byte frames, and real feeds are far
  lower. At that rate a tuned kernel socket path is not throughput-bound. DPDK
  could only help tail latency and jitter [I].
- Operational risk [I]: binding the I219 to `vfio-pci` removes it from Linux.
  If it's the only port, SSH and everything else on the box goes with it. Also
  check `/sys/kernel/iommu_groups` (integrated PCH devices may share a group)
  and IOMMU enablement. These checks must be done by a human on the server;
  this review didn't touch `camlcompute`.
- The exact PCI device ID of the CAML I219-LM is **unknown** to this review
  (it would come from `lspci -nn`).

**Recommendation:** don't pursue DPDK for performance until a supported
multi-queue NIC with good PMD support exists (e.g. Intel X710 / E810, NVIDIA
ConnectX-5/6). First collect a **kernel baseline** on the current hardware:

1. Current blocking `recv_from` path: per-packet latency histogram using
   `SO_TIMESTAMPNS` (kernel RX stamp) → user-space stamp, plus processing time.
2. `recvmmsg` batching (the planned next backend) on the same metrics.
3. Tuning matrix: `isolcpus`/`nohz_full`/`rcu_nocbs` for the RX core, IRQ
   affinity of the e1000e IRQ onto a neighbouring core (or the same core,
   measured both ways), `ethtool -C rx-usecs` interrupt coalescing,
   `SO_RCVBUF`, `SO_BUSY_POLL` / `net.core.busy_poll` (e1000e is a NAPI
   driver [I]).
4. Hardware RX timestamps via `SO_TIMESTAMPING` if `ethtool -T` reports support
   on the I219 (unverified).
5. Drops: `/proc/net/snmp` UDP `RcvbufErrors`/`InErrors`, `ethtool -S`
   (`rx_missed_errors`, `rx_no_buffer_count`), plus our sequence-gap counter.

DPDK becomes worth building once those numbers show the kernel path's tail
latency or jitter is the bottleneck *and* a supported NIC is available.

---

## 8. What to learn from this (concepts)

- **Hugepages and the TLB.** The TLB caches virtual→physical translations.
  With 4 KiB pages, a 1 GiB packet-buffer region needs 262,144 translations;
  with 2 MiB pages it needs 512, and with 1 GiB pages just one. DPDK allocates
  mempools and rings from hugepages so the RX path doesn't take TLB misses and
  page walks. Hugepages are also pinned (never swapped), and their
  physical/IOVA addresses are stable, which DMA needs.
- **mbuf and mempool.** An `rte_mbuf` is a fixed metadata header (pointer,
  `data_off`, `data_len`, `pkt_len`, `ol_flags`, RSS hash, ...) plus a data
  buffer with headroom. A mempool is a preallocated fixed-size object pool: a
  lockless ring plus a **per-lcore cache**, so the common alloc/free touches
  only core-local memory. No `malloc` happens per packet, and freeing is
  returning to the pool. That's why an mbuf must be freed exactly once and
  never outlive its pool. In Rust, the natural encoding is ownership (RAII)
  plus lifetimes.
- **DMA descriptor rings.** Each RX queue is a ring of descriptors in host
  memory. The driver writes each descriptor with an empty mbuf's IOVA. The NIC
  DMA-writes the frame there and sets a "done" bit. The driver reads done
  descriptors, hands those mbufs up, refills with fresh mbufs, and advances the
  tail register (an MMIO write, batched per burst). If the ring is full because
  software is too slow, the NIC drops the frame and counts it (`imissed`). If
  the mempool is empty at refill time, you get `rx_nombuf`.
- **PMD polling vs interrupts.** A kernel driver takes an interrupt (moderated
  by NAPI/coalescing), runs softirq processing, goes through the stack, and
  wakes a blocked thread (scheduler latency). A DPDK Poll Mode Driver is a
  user-space function that checks the descriptor ring in a loop. There's no
  interrupt, no context switch, and no wakeup, but the core spins at 100% even
  when idle.
- **RSS.** The NIC hashes header fields (Toeplitz with a key) and uses a
  redirection table (RETA) to pick an RX queue. One flow always lands on one
  queue, so per-core processing needs no locks. With one feed, RSS is
  irrelevant; it matters when many flows must spread across cores.
- **lcores, pinning, isolcpus.** An EAL lcore is a thread pinned to one CPU.
  `isolcpus` (or cpusets) keeps the scheduler from placing other tasks there.
  `nohz_full` stops the periodic tick on that core, and `rcu_nocbs` offloads
  RCU callbacks. Keep device IRQs off the core too. Together these remove most
  sources of jitter on the polling core. Also keep the core, the NIC, and the
  mempool on the same NUMA node.
- **Why kernel bypass helps.** It removes, per packet or per batch: the
  syscall entry/exit, softirq and protocol processing, socket-buffer queueing,
  the kernel→user copy, and wakeup latency. It also makes batching explicit
  (`rx_burst` amortizes the MMIO tail update and cache misses across 32
  packets).
- **What it costs.** Dedicated cores at 100%, root/vfio/IOMMU setup, hugepage
  reservation, and the NIC disappears from Linux (no `ip`, `tcpdump`,
  firewall, ARP, IGMP, or SSH on it). You own every protocol behaviour you
  still need, PMD support is NIC-specific, and debugging is harder. This is
  why "measure the kernel path first" is the rule, not a formality.

---

## 9. Verification notes

- **Verified from source/docs:** everything cited with a path/line in the
  reviewed repo; DPDK v25.11 `em_ethdev.c` PCI table, `max_rx_queues`,
  `imissed` mapping; DPDK feature matrices (`e1000`, `mlx5`, `ice`, `i40e`,
  `igc`); pcap/af_packet devargs; smoltcp v0.14.0 `Checksum::default() == Both`
  and IPv4 fragments rejected without `proto-ipv4-fragmentation`
  (`src/wire/ipv4.rs:557-559`; the repo doesn't enable that feature,
  `Cargo.toml:20`); crate versions and metadata from crates.io; repo activity
  from the GitHub API.
- **Inferred, not executed:** all latency and overhead rankings; smoltcp's
  payload copy into `PacketBuffer`; the UAF consequence on hardware PMDs;
  affinity inheritance; bindgen regex anchoring and `wrap_static_fns`;
  e1000e busy-poll and timestamping capabilities; I219 behaviour under DPDK in
  practice.
- **Not determined:** the CAML I219-LM PCI device ID and IOMMU grouping; the
  `pkgconf` crate's static-vs-dynamic linking behaviour; runtime behaviour of
  any code (nothing was built or run).
