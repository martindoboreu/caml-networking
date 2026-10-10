# CAML Low-Latency Networking Engine

Internal CAML infrastructure for receiving, validating, and measuring UDP
market-data feeds. It currently runs against a synthetic feed on the local
machine.

* Architecture: [`docs/architecture.md`](docs/architecture.md)
* Wire protocol: [`docs/protocol.md`](docs/protocol.md)
* Design notes and explanations: [`docs/learning-log.md`](docs/learning-log.md)
* Research: [`rust-dpdk-net` review and DPDK backend design](docs/research/rust-dpdk-net-review.md)
* Rules for contributors and AI agents: [`AGENTS.md`](AGENTS.md)

## Status

| Component | State |
|---|---|
| Wire protocol v1 (encoder, validating zero-copy decoder) | Implemented, tested |
| Portable UDP receiver (`recv_from`, `SO_RCVBUF`) | Implemented, tested on Windows loopback |
| Packet- and message-sequence tracking (gap / late / duplicate) | Implemented, tested |
| Latency histograms, counters, reports | Implemented, tested |
| Synthetic feed generator with fault injection | Implemented, tested |
| Repeatable benchmark suite and baseline report | Planned (Stage 4) |
| SPSC queue / multi-thread pipeline | Planned, pending measurements |
| Linux `recvmmsg`, busy-poll, CPU affinity | Planned; blocked on a Linux environment |
| AF_XDP experimental backend | Planned; blocked on a Linux environment |
| DPDK backend (`caml-transport-dpdk`) | Designed only (see architecture section 9); not worth implementing for performance without a supported NIC |
| Deployment to `camlcompute` | **Not authorized**; do not deploy |

## Setup (Windows)

The toolchain is installed per user with rustup, using the GNU target (no
Visual Studio needed):

```powershell
# Already done on the CAML dev machine. Shown here for new contributors.
Invoke-WebRequest https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-gnu/rustup-init.exe -OutFile rustup-init.exe
.\rustup-init.exe -y --default-host x86_64-pc-windows-gnu --default-toolchain stable --profile minimal -c clippy -c rustfmt
```

Open a new terminal afterwards so `%USERPROFILE%\.cargo\bin` is on `PATH`.
Linux and macOS: use the standard `rustup` installer; nothing here is
Windows-specific.

## Build and test

```powershell
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Run locally

Use two terminals and **release builds** (debug builds skip optimization
and add overflow checks, so their timings are meaningless):

```powershell
# Terminal 1: receiver. Ctrl-C prints the final report.
cargo run --release -p caml-recv -- --print-events 5

# Terminal 2: publish 100k packets at 10k packets/s
cargo run --release -p caml-feed -- --packets 100000 --rate 10000

# Same, with injected faults; the receiver report should account for every one
cargo run --release -p caml-feed -- --packets 10000 --drop-every 997 --corrupt-every 1499 --duplicate-every 701 --reorder-every 503
```

Both default to `127.0.0.1:40000`. Loopback traffic needs no firewall
changes. Run either binary with `--help` for all options.

## Layout

```text
crates/protocol        wire format (no I/O, no deps)
crates/engine          transport trait + UDP backend, sequencing, metrics, receive loop
crates/sim             deterministic synthetic market + fault injection (test tooling)
apps/receiver          caml-recv CLI
apps/feed-generator    caml-feed CLI
docs/                  architecture, protocol spec, learning log
```
