# caml-networking

[![CI](https://github.com/martindoboreu/caml-networking/actions/workflows/ci.yml/badge.svg)](https://github.com/martindoboreu/caml-networking/actions/workflows/ci.yml)
[![coverage](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/martindoboreu/caml-networking/badges/coverage.json)](https://github.com/martindoboreu/caml-networking/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](Cargo.toml)
[![unsafe](https://img.shields.io/badge/unsafe-none%20in%20production%20code-success)](AGENTS.md)
[![license](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

A low-latency market-data receive engine in Rust, built for CAML, a
student-founded engineering and quantitative research organization at CU
Boulder. It receives a UDP feed, validates every packet, and accounts for
every message it does not deliver. Today it runs against a synthetic feed;
the roadmap is AF_XDP kernel bypass on dedicated hardware
([hardware plan](docs/hardware-plan.md)).

## Highlights

* **Every message accounted for.** A datagram is either delivered or counted
  by reason: 12 decode-error kinds, duplicates, late fills, stale or
  unconfirmed sessions, implausible sequence jumps, kernel drops. On Linux a
  test asserts `delivered + kernel_drops == sent` exactly.
* **Zero heap allocations on the receive path, proven.** A counting global
  allocator test drives a hostile packet stream and a real socket loop and
  asserts zero allocations.
* **Hostile-input tested.** Property tests push about 900,000 adversarial
  datagrams through the pipeline in release CI and check invariants after
  every one; the decoder takes 200,000 fuzz cases; the frame parser is
  differentially tested against `etherparse` on 100,000 mutated frames.
* **Overflow-free by construction.** Plain integer arithmetic on wire values
  is a compile error on the receive path (`clippy::arithmetic_side_effects`
  is denied there), and `unsafe` is forbidden outside one test-only crate.
* **A wire protocol with a reason for every field.** v2 follows MoldUDP64:
  heartbeats carry the next sequence number so tail loss is detectable,
  sessions are ordered by a monotonic epoch, and one bad packet cannot
  poison receiver state ([spec](docs/protocol.md)).
* **Measured, not guessed.** Parse plus sequence costs 13.5 ns for a
  one-message packet; every number in [`docs/benchmarks.md`](docs/benchmarks.md)
  comes with its command, build, and environment. Design choices, including
  a bug the first Linux CI run caught, are in the [decision log](docs/decisions.md).

## Documentation

* Architecture: [`docs/architecture.md`](docs/architecture.md)
* Wire protocol (v2): [`docs/protocol.md`](docs/protocol.md)
* Decisions and their reasons: [`docs/decisions.md`](docs/decisions.md)
* Benchmarks, method and results: [`docs/benchmarks.md`](docs/benchmarks.md)
* Hardware plan and purchase requirements: [`docs/hardware-plan.md`](docs/hardware-plan.md)
* Linux development with WSL2: [`docs/linux-dev.md`](docs/linux-dev.md)
* Design notes and explanations: [`docs/learning-log.md`](docs/learning-log.md)
* Research: [`rust-dpdk-net` review](docs/research/rust-dpdk-net-review.md)
* Rules for contributors and AI agents: [`AGENTS.md`](AGENTS.md)

## Status

Labels: **implemented**, **tested** (automated, on Windows and Linux), **tested
on Linux** (Linux-only code; CI and WSL2), **benchmarked**, **planned**,
**blocked**.

| Component | State |
|---|---|
| Wire protocol v2 (MoldUDP64-style sequencing, session epoch, end-of-session) | implemented, tested |
| Session gate, sequencer, integrity verdict | implemented, tested (unit, adversarial, property, end-to-end) |
| Zero allocations on the receive path | tested (counting allocator) |
| Portable UDP backend (`recv_from`), multicast, source allowlist | implemented, tested (Windows and Linux loopback) |
| Linux `recvmmsg` backend (batching, kernel receive timestamps) | implemented, tested on Linux |
| Kernel drop counts on Linux (both socket backends, exact accounting) | implemented, tested on Linux |
| Frame parser for raw-packet backends (`caml-frame`) | implemented, tested (incl. differential vs `etherparse`) |
| pcap output of the synthetic feed | implemented, tested |
| Benchmark harness (`caml-bench`) and Windows baseline | implemented, benchmarked (Windows loopback only) |
| CI (Ubuntu 24.04 + Windows, debug + release, MSRV, `cargo deny`) | in use: [Actions](https://github.com/martindoboreu/caml-networking/actions) |
| Line coverage of library crates (`cargo-llvm-cov`), badge, per-run report | in use (CI and `scripts/coverage.sh`) |
| Linux dev environment: WSL2 Ubuntu 24.04, `scripts/wsl-setup.sh` | in use ([guide](docs/linux-dev.md)) |
| AF_XDP backend (`caml-transport-xdp`) | planned (next; develop on WSL2 `veth` pairs) |
| External Thunderbolt NIC for zero-copy bypass | planned; see hardware plan |
| DPDK backend | design only; **not on the onboard I219** (D8) |
| SPSC queue / multi-thread pipeline | planned, pending Linux measurements |
| Deployment to `camlcompute` | **not authorized**; do not deploy |

## Setup

Windows: the toolchain is installed per user with rustup, using the GNU target (no
Visual Studio needed). `rust-toolchain.toml` pins the exact compiler; rustup
installs it on first use.

```powershell
# Already done on the CAML dev machine. Shown here for new contributors.
Invoke-WebRequest https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-gnu/rustup-init.exe -OutFile rustup-init.exe
.\rustup-init.exe -y --default-host x86_64-pc-windows-gnu --default-toolchain stable --profile minimal -c clippy -c rustfmt
```

Linux, including WSL2 on Windows: `bash scripts/wsl-setup.sh` installs everything
(see [`docs/linux-dev.md`](docs/linux-dev.md)). macOS: use the standard `rustup`
installer.

## Build and test

The same gates CI runs:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace            # debug: overflow checks on
cargo test --workspace --release  # release: 10x larger property test
cargo +1.88 check --workspace --all-targets   # MSRV
cargo deny check                  # licenses, advisories, sources (needs cargo-deny)
```

On Linux (or WSL2) the same gates, plus coverage, are scripted:

```bash
bash scripts/check.sh            # every CI gate
bash scripts/coverage.sh --html  # line coverage of the library crates
```

Type-check the Linux-only code from Windows:

```powershell
rustup target add x86_64-unknown-linux-gnu
cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu -- -D warnings
```

## Run locally

Use two terminals and **release builds**:

```powershell
# Terminal 1: receiver. Ctrl-C prints the report and the integrity verdict.
cargo run --release -p caml-recv -- --print-events 5

# Terminal 2: publish 100k packets at 10k packets/s (ends with end-of-session)
cargo run --release -p caml-feed -- --packets 100000 --rate 10000

# With injected faults; the verdict accounts for every one
cargo run --release -p caml-feed -- --packets 10000 --drop-every 997 --corrupt-every 1499 --duplicate-every 701 --reorder-every 503
```

Useful options:

* `caml-recv --fail-on-loss`: exit status 2 on any loss, rejection, or
  malformed datagram (for scripts and CI).
* `caml-recv --backend recvmmsg` (Linux): batched receive with kernel
  receive timestamps. On Linux both backends report kernel drop counts.
* Multicast on loopback: `caml-recv --group 239.77.0.9 --iface 127.0.0.1
  --reuse-address` and `caml-feed --target 239.77.0.9:40000`.
* `caml-feed --no-end-of-session` simulates a publisher crash;
  `--pcap-out feed.pcap` writes Ethernet frames for replay with `tcpreplay`.

Both default to `127.0.0.1:40000`. Loopback traffic needs no firewall
changes. Run any binary with `--help` for all options.

## Benchmarks

```powershell
cargo run --release -p caml-bench -- micro --runs 7
cargo run --release -p caml-bench -- loopback --runs 10
```

Results and method: [`docs/benchmarks.md`](docs/benchmarks.md).

## Layout

```text
crates/protocol        wire format v2 (no I/O, no deps)
crates/engine          transports, session gate, sequencer, metrics, receive loop
crates/frame           Ethernet/VLAN/IPv4/UDP parser for raw-packet backends (no deps)
crates/sim             synthetic market, fault injection, Ethernet/pcap output (test tooling)
crates/alloc-count     test-only counting allocator (the one crate with unsafe)
apps/receiver          caml-recv CLI
apps/feed-generator    caml-feed CLI
apps/bench             caml-bench CLI
scripts/              check.sh (CI gates), coverage.sh, wsl-setup.sh (Linux dev environment)
docs/                  architecture, protocol, decisions, benchmarks, hardware plan, learning log
```

## License

MIT; see [`LICENSE`](LICENSE).
