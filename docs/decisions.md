# Decision Log

Short records of significant decisions: what was decided, why, and what
would change it. Newest last. Superseded decisions stay, marked as such.

---

**D1. One thread does receive, parse, sequence, and consume (2026-10-09).**
No queue hop or cross-core transfer; trivially correct. *Revisit when* a
consumer's cost causes kernel drops (observable via `kernel_drops` on Linux);
then add a bounded SPSC ring of indices, with before/after measurements.

**D2. Transports lend datagrams through a callback (`PacketSource::poll`).**
Works for sockets, `recvmmsg`, AF_XDP UMEM frames, and DPDK mbufs without a
copy; the borrow checker stops slices from escaping. *Revisit* only if a
backend needs to hold frames across calls (e.g. reassembly), which the
current scope rejects.

**D3. Protocol v2 uses MoldUDP64-style sequencing (supersedes v1).**
v1 had a per-packet sequence plus a per-message sequence, and a 32-bit
session id. The audit found that this design (a) re-delivered duplicates when
sessions interleaved, (b) dropped reordered data at session start as
"duplicates", (c) could not detect loss of the final packets, and (d) could
not order publisher restarts. v2 has one message sequence starting at 1, the
header carries the first sequence, heartbeats announce the next sequence, an
`END_OF_SESSION` flag marks a clean shutdown, and a monotonic 64-bit
`session_epoch` orders runs. This matches an established industry model.

**D4. Session gate: move forward cautiously, never flap.**
A newer epoch is adopted at once only if the current session ended or went
quiet; otherwise it needs 3 consecutive packets. Older epochs are dropped
while the current session is alive. Epochs far in the future are rejected.
Abandoned gaps of a replaced session are counted as lost, and a replaced
session that never sent end-of-session is reported (`unterminated_sessions`).

**D5. Bounded forward jumps with confirmation.**
One corrupt-but-valid or stray packet must not poison sequence state. Jumps
beyond 2^20 messages are dropped and counted unless 3 consecutive packets
continue each other.

**D6. The message sequencer alone decides delivery.**
v1 dropped any packet whose packet sequence looked old, which discarded data
that had never been delivered. Now every message is classified on its own
sequence number; nothing is dropped by a packet-level guess.

**D7. AF_XDP before DPDK for kernel bypass.**
The kernel keeps the interface (SSH, ARP, IGMP via ordinary sockets); no
hugepages, vfio, or IOMMU grouping; a `bpf_link`-attached program detaches
automatically on crash; copy mode works on `veth` for development and
zero-copy works on supported NICs with the same code. *Revisit* if AF_XDP
zero-copy on a supported NIC measurably misses the target.

**D8. No DPDK on the onboard I219-LM.** Verified: Ubuntu 24.04's DPDK
23.11.4 does not list device `0x15FB`; v25.11 does but the PMD is not
validated on I219, has no timestamp offload, and binding the box's only port
removes SSH. e1000e has no native XDP either, so the I219 stays the
management interface and the kernel-socket fallback.

**D9. External NIC over Thunderbolt 4.**
The target box has a TB4 root port and no PCIe slot. A TB4-to-PCIe enclosure
with an AF_XDP-zero-copy NIC gives real bypass while the I219 keeps
management. Risks (surprise removal, tunnelling latency, power management)
are engineered around in `docs/hardware-plan.md`.

**D10. One test-only crate may use `unsafe`: `caml-alloc-count`.**
Proving "the receive path never allocates" needs a counting global
allocator, which needs `unsafe impl GlobalAlloc`. The crate is a
dev-dependency only, opts out of `forbid(unsafe_code)` explicitly, and has a
`// SAFETY:` comment on every block (enforced by
`clippy::undocumented_unsafe_blocks`). The planned AF_XDP crate is the
second, under the same rules.

**D11. No async runtime on the hot path.**
An async runtime solves "many mostly idle connections"; we have one busy
socket. A dedicated thread has fewer layers. Async may serve control-plane
work (config, metrics export) later.

**D12. Frame-parser policy (`caml-frame`).**
Accept Ethernet II with at most one 802.1Q (`0x8100`) tag, and IPv4 with or
without options. Reject 802.1ad and legacy QinQ TPIDs (`0x88A8`, `0x9100`,
`0x9200`), IPv4 fragments, and a UDP length of 0 (only meaningful for IPv6
jumbograms). Single tags with the legacy `0x9100`/`0x9200` TPIDs and a UDP
length of 0 are the only two differences from `etherparse` found by
100,000-frame differential testing, and both are deliberate.

**D13. Linux backends use `nix`'s safe wrappers.**
`recvmmsg`, `SO_RXQ_OVFL`, and `SO_TIMESTAMPNS` are reachable through `nix`
without `unsafe` in our code, and `nix` was already a dependency (via
`ctrlc`). The engine therefore keeps `forbid(unsafe_code)`.
