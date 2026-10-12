# Hardware Plan: Stable Kernel Bypass on the CAML Box

Status: **planned**. Nothing here has been purchased, installed, or run.
No agent may touch the target box without written authorization from the
project lead (AGENTS.md); every command on the box below is for a person to
run.

Goal: a **stable** kernel-bypass receive path in Rust on the CAML box, with
evidence that it beats a tuned kernel-socket path.

---

## 1. Summary

| | |
|---|---|
| **Problem** | The onboard NIC (Intel I219-LM) cannot do real kernel bypass: its Linux driver (e1000e) has no native XDP, and DPDK on it is unvalidated, missing from Ubuntu's DPDK, and would take the box's only port (and SSH) away. |
| **Way out** | The box has **Thunderbolt 4**. Put a proper NIC in a Thunderbolt-to-PCIe enclosure, use **AF_XDP zero-copy** on it, and keep the I219 for SSH/management and as the kernel-socket fallback. |
| **What to buy** | (1) TB3/TB4-to-PCIe enclosure, (2) a NIC with AF_XDP zero-copy and PTP, ideally **dual-port**, (3) a DAC cable or matching cabling, (4) optionally a second Linux machine as a traffic source. |
| **First, answer** | Where does the market data physically come from? (§9). If it arrives over the public internet, kernel bypass cannot help and none of this should be bought. |

---

## 2. What we know about the box

From `lspci -nn` supplied by the project lead (2026-10-09):

| Component | Detail | Consequence |
|---|---|---|
| CPU | 11th-gen Core i7, Tiger Lake-LP (`8086:9a14`), U-series mobile, probably 4 cores / 8 threads | Few cores to isolate; turbo, power limits, and thermals vary. Confirm with `lscpu`. |
| Ethernet | I219-LM (13) `8086:15fb` at `00:1f.6`, part of the chipset's `00:1f` multifunction device (LPC, audio, SMBus, SPI) | Probably shares an IOMMU group with chipset functions (unverified). Keep it for management. |
| Thunderbolt | TB4 root port `00:07.0`, TB4 USB controller and NHI `00:0d.x` | External PCIe via Thunderbolt is possible. |
| Wi-Fi | AX201 (CNVi) at `00:14.3` | Its M.2 slot likely has no PCIe lanes: not an option for a NIC. Disable Wi-Fi on a server. |
| Storage | KIOXIA BG4 NVMe on CPU PCIe (`01:00.0`) | The only other M.2 slot is the boot drive. |
| Expansion slots | none (Ultra form factor; verify model with `sudo dmidecode -s system-product-name`) | Thunderbolt is the only path to a better NIC. |

Verified from source (2026-10-09):

* Linux v6.8 e1000e has no `ndo_bpf` / `ndo_xdp_xmit`: **no native XDP**.
  AF_XDP on the I219 is generic copy mode only.
* DPDK 23.11.4 (Ubuntu 24.04's package) has no entry for `0x15FB`; DPDK
  v25.11 lists it (`drivers/net/intel/e1000/em_ethdev.c:168`) but documents no
  validation on I219, no timestamp offload, 2 queues maximum.
* Linux v6.8 has AF_XDP **zero-copy** support (`xsk_pool` code) in i40e, ice,
  ixgbe, igc, and mlx5.
* DPDK's mlx5 PMD is **bifurcated** (the kernel driver stays bound, so Linux
  keeps the port) and lists timestamp offload and RSS.

---

## 3. Requirements list

Priority: **MUST** (the plan fails without it), **SHOULD** (strongly
recommended), **COULD** (useful later).

### 3.1 Thunderbolt-to-PCIe enclosure

| # | Priority | Requirement | How to check before buying |
|---|---|---|---|
| E1 | MUST | Thunderbolt 3 or 4 host interface with **PCIe tunnelling** (not a USB-only or NVMe-only enclosure) | Spec sheet says "PCIe card expansion" over Thunderbolt |
| E2 | MUST | One PCIe slot: x16 mechanical, **≥ x4 electrical**, Gen 3 or newer | Spec sheet |
| E3 | MUST | Its own power supply with enough power for the NIC (most NICs here draw under 25 W) | Spec sheet: slot power |
| E4 | MUST | Physical fit for the NIC (full-height vs low-profile bracket, card length) | Compare with the NIC's dimensions |
| E5 | MUST | Known to work on Linux (Thunderbolt authorization via `bolt`) | Vendor Linux notes or community reports for the exact model |
| E6 | SHOULD | Airflow across the card (a fan). Server NICs such as ConnectX and Intel X710 are designed for server airflow and run hot in still air | Spec sheet / photos |
| E7 | SHOULD | Short certified TB4 cable (0.5–0.8 m) with strain relief. A bumped cable means the NIC disappears | Buy a certified cable; plan cable routing |
| E8 | AVOID | All-in-one "Thunderbolt 10 GbE adapters" unless their chipset's driver supports AF_XDP zero-copy (many use Aquantia/Marvell or Realtek chips) | Look up the chipset and its Linux driver |

### 3.2 NIC (choose by how the feed arrives)

AF_XDP zero-copy driver support below was verified in the Linux v6.8 source;
confirm on the exact card generation before buying.

| Option | Driver | Ports / medium | AF_XDP zero-copy | HW timestamps (PTP) | DPDK | Notes |
|---|---|---|---|---|---|---|
| **NVIDIA/Mellanox ConnectX-5** (or ConnectX-4 Lx) | mlx5 | 2 × SFP28/SFP+ (10/25 GbE) | yes (mlx5 xsk) | yes | yes, **bifurcated**: Linux keeps the port; RX timestamp offload | **Recommended.** Most flexible. Prefer CX-5+ unless CX-4 Lx zero-copy is confirmed for the exact firmware. Usually accepts third-party optics/DACs. |
| **Intel X710-DA2** | i40e | 2 × SFP+ (10 GbE) | yes | yes | yes (vfio; takes the port) | Mature AF_XDP zero-copy. Often **rejects non-Intel-coded SFP modules**: buy compatible DACs. |
| **Intel E810** | ice | SFP28 / QSFP28 | yes | yes (advanced) | yes | Newest; higher power and cost; overkill below 25 GbE. |
| **Intel X550-T2 / X540** | ixgbe | 2 × RJ45 10GBASE-T | yes | yes | yes | For copper 10G networks; runs hot. |
| **Intel I226 / I225** | igc | 1 × RJ45 2.5 GbE (PCIe x1) | yes (newer, less battle-tested) | yes | partial | **Budget option**, plain copper cabling, low power. |
| Realtek, USB NICs | various | — | no | — | — | **Do not buy** for bypass. |

Requirements:

| # | Priority | Requirement |
|---|---|---|
| N1 | MUST | Driver with AF_XDP **zero-copy** in Ubuntu 24.04's kernel (one of the drivers above) |
| N2 | MUST | Port type matches the network the feed arrives on (RJ45 vs SFP), or the switch you will use |
| N3 | SHOULD | **Dual-port.** Lets you test on one box with hardware timestamps on both ends: port A sends (kernel), port B receives (AF_XDP), connected by a DAC |
| N4 | SHOULD | Hardware timestamping (`ethtool -T` shows hardware RX/TX) |
| N5 | SHOULD | Card power and heat within the enclosure's limits (E3, E6) |
| N6 | COULD | DPDK bifurcated support (ConnectX), in case AF_XDP falls short |

### 3.3 Cabling and network

| # | Priority | Requirement |
|---|---|---|
| C1 | MUST | For SFP NICs: DAC (direct-attach copper) cables or SFP modules compatible with the NIC (X710 is strict) and with the switch |
| C2 | SHOULD | One extra DAC to loop port A to port B on a dual-port card (single-box testbed) |
| C3 | COULD | A switch with IGMP snooping if multicast will cross a switch; not needed for a direct cable |

### 3.4 Traffic source

| # | Priority | Requirement |
|---|---|---|
| T1 | SHOULD | Either a dual-port NIC looped back (N3 + C2), **or** a second Linux machine with a NIC that matches the link, connected by cable |
| T2 | COULD | The second machine's NIC supports hardware TX timestamps, for one-way latency with PTP |

### 3.5 Out of scope for now

PTP grandmaster clock, FPGA, a colocation cross-connect, and a second
Thunderbolt NIC for A/B feeds (revisit after §7's decision gate).

---

## 4. Target topology

```text
             ┌──────────────── CAML box (Ubuntu 24.04) ────────────────┐
  LAN/SSH ───┤ I219-LM (e1000e, kernel)  management + kernel fallback  │
             │                                                          │
             │ TB4 ══ enclosure ══ NIC port B ◀── feed (AF_XDP ZC)      │
             │                      NIC port A ──▶ test traffic (DAC ↺) │
             │ core 3 (isolated): caml-recv receive loop, busy-polling  │
             │ cores 0-2: OS, IRQs, sender, telemetry                   │
             └──────────────────────────────────────────────────────────┘
```

---

## 5. Box configuration (a person runs these, after authorization)

### 5.1 Read-only discovery (run first, before buying)

```text
sudo dmidecode -s system-product-name      # exact model
lscpu ; lscpu -e                            # cores, SMT siblings
uname -r                                    # kernel version
find /sys/kernel/iommu_groups/ -type l | sort -V
cat /sys/bus/thunderbolt/devices/domain0/iommu_dma_protection
boltctl list                                # Thunderbolt devices
ip -br link ; ethtool -i <i219-if> ; ethtool -T <i219-if>
```

### 5.2 BIOS (Dell)

* Thunderbolt: enable PCIe tunnelling; security level "user authorization";
  enable pre-boot support so the NIC enumerates at boot.
* Power/thermal: thermal mode "Ultra Performance" (or the closest);
  C-states limited to C1; measure with turbo on and off and keep whichever
  gives the tighter tail.
* Disable Wi-Fi/Bluetooth and audio.

### 5.3 Ubuntu 24.04

* Thunderbolt: `boltctl enroll --policy auto <uuid>` so the enclosure is
  authorized on every boot.
* Kernel command line (example for a 4-core/8-thread CPU, isolating physical
  core 3 = logical CPUs 3 and 7; check `lscpu -e`):
  `isolcpus=3,7 nohz_full=3,7 rcu_nocbs=3,7 irqaffinity=0-2,4-6 pcie_aspm=off`.
  Keep the change documented and reversible.
* Disable `irqbalance`; pin the NIC's IRQs to housekeeping cores (and
  measure "same core" once).
* NIC: `ethtool -L <if> combined 1`; steer the feed's flow to queue 0
  (`ethtool -N`); disable interrupt coalescing for latency tests
  (`ethtool -C <if> adaptive-rx off rx-usecs 0`); disable EEE.
* Clocks: `phc2sys` to tie NIC hardware clocks to the system clock (and the
  two ports' clocks, if separate) before timestamp measurements.
* Service: systemd unit with `CPUAffinity=3`, `LimitMEMLOCK=infinity`,
  `AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW CAP_BPF`, non-root user,
  `Restart=on-failure`, `WatchdogSec=`.

---

## 6. Measurement campaign

All runs: release build, the same traffic, recorded environment (kernel,
BIOS settings, tuning, NIC firmware). Report send→rx and kernel→rx p50,
p99, p99.9, p99.99, max; kernel and NIC drops (`kernel_drops`,
`ethtool -S`); CPU use. At least 10 runs per configuration (`caml-bench`
style). Rates: 10k, 100k, 500k packets/s, and line rate.

| Step | Configuration | Proves |
|---|---|---|
| M1 | `--backend udp` on the I219 | Portable baseline on real hardware |
| M2 | `--backend recvmmsg` | Batching gain; first kernel-drop and kernel→rx numbers |
| M3 | M2 + `SO_BUSY_POLL` + isolated core + tuned IRQs | Best achievable kernel path |
| M4 | AF_XDP copy mode on the I219 | Whether partial bypass helps at all on e1000e (expected: little) |
| M5 | AF_XDP zero-copy on the Thunderbolt NIC | The real bypass number |
| M6 | M3 on the Thunderbolt NIC | Separates "better NIC" from "bypass" |

**Decision gate:** adopt AF_XDP zero-copy as the production path only if M5
beats M6 on p99 and p99.9 by a margin larger than the run-to-run spread,
with zero unexplained drops at the target rate, over a 72-hour soak.
Otherwise the tuned kernel path (M3/M6) is the production path, which is a
legitimate outcome.

---

## 7. Software roadmap tied to the hardware

| Milestone | Work | Needs | Exit criterion |
|---|---|---|---|
| S1 | Push to GitHub; CI green on Ubuntu 24.04 and Windows | a remote | Linux-only tests (recvmmsg backend, multicast on `lo`) pass in CI |
| S2 | Linux dev environment (WSL2 or VM) with `veth` pairs | owner's decision | `caml-recv --backend recvmmsg` runs locally; `tcpreplay` of `caml-feed --pcap-out` into a `veth` |
| S3 | `caml-transport-xdp` crate: UMEM, fill/RX rings, `bpf_link`-attached XDP redirect program, `caml-frame` filter, per-reason drop counters, fallback to `recvmmsg` on init failure | S2 | Copy mode on `veth`: replaying the same pcap through `udp`, `recvmmsg`, and AF_XDP gives identical event streams and counters |
| S4 | Box bring-up (§5), M1–M3 | authorization | Baseline numbers committed to `docs/benchmarks.md` |
| S5 | Hardware arrives: M4–M6, soak, decision gate | §3 purchases | Decision recorded in `docs/decisions.md` |
| S6 | Production hardening: A/B line arbitration, systemd unit, metrics export, runbooks | S5 | Install/upgrade/rollback rehearsed on the box |

---

## 8. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Thunderbolt link drop or surprise removal | Secured certified cable; boot-time authorization; receiver treats interface loss as an alarm and falls back to the I219 kernel path; ideally the feed reaches both NICs (A/B arbitration) |
| Tunnelling latency | Measure M5 vs M6 on the same NIC; the comparison is fair because both cross Thunderbolt |
| Mobile CPU: turbo, power limits, Intel Dynamic Tuning, heat | Fixed frequency, BIOS thermal mode, `turbostat` during soak, airflow |
| Firmware interrupts (SMIs) causing tail spikes | `hwlatdetect` before measuring; record the result |
| SFP compatibility (X710) | Buy NIC-vendor-compatible DACs |
| IOMMU or Thunderbolt security blocks the device | Check §5.1 outputs before buying; AF_XDP does not need vfio |
| Driver regressions on kernel updates | Pin the kernel for measurements; rerun M1–M6 after upgrades |
| Kernel bypass shows no gain | Valid outcome: ship the tuned kernel path (§6 gate) |

---

## 9. Questions to answer before buying

1. **Where does the market data come from?** Internet (WebSocket/REST) →
   stop; bypass cannot help. LAN or cross-connect UDP (multicast?) → go on.
2. What is the physical link: RJ45 copper or SFP fibre/DAC, and what speed?
3. Is there a switch between the source and the box (IGMP snooping)?
4. Exact box model (`dmidecode`), and which USB-C port is Thunderbolt?
5. Budget: recommended tier (enclosure + dual-port ConnectX-5 or X710 +
   DACs) or budget tier (enclosure + I226)?
