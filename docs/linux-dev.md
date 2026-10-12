# Linux development with WSL2

The engine targets Linux (Ubuntu 24.04 on the CAML box). On a Windows
machine, WSL2 provides a real Linux kernel for development: the `recvmmsg`
backend, kernel drop counting, multicast on loopback, coverage, and (later)
AF_XDP experiments on `veth` pairs.

## What WSL2 is and isn't valid for

| Valid | Not valid |
|---|---|
| Building and testing Linux-only code (`recvmmsg`, `/proc` drop counts, `SO_TIMESTAMPNS`) | Performance numbers: WSL2 runs in a Hyper-V VM with a virtual NIC and its own kernel build. Never compare its latency with the target box |
| Coverage measurement (same script as CI) | Anything involving a physical NIC (AF_XDP zero-copy, hardware timestamps) |
| Running `caml-recv --backend recvmmsg` against `caml-feed` on loopback | |
| AF_XDP copy mode on `veth` pairs (planned; depends on the WSL kernel's config) | |

## One-time setup

**1. Windows side** (administrator PowerShell; may need a reboot if the
Virtual Machine Platform feature was off):

```powershell
wsl --install --distribution Ubuntu-24.04
```

Then open **Ubuntu 24.04** from the Start menu once and create your Linux
user and password.

**2. Linux side** (inside Ubuntu; clone into the Linux filesystem, not
`/mnt/c`, which is slow and loses file permissions):

```bash
git clone https://github.com/martindoboreu/caml-networking ~/caml-networking
cd ~/caml-networking
bash scripts/wsl-setup.sh
```

`wsl-setup.sh` asks for your sudo password once (apt packages and the
loopback multicast flag), then installs Rust with rustup (the toolchain
pinned by `rust-toolchain.toml`, plus 1.88 for the MSRV check) and
`cargo-llvm-cov`, all per user.

## Everyday commands

```bash
source ~/.cargo/env
bash scripts/check.sh            # every CI gate: fmt, clippy, tests (debug + release), MSRV
bash scripts/coverage.sh --html  # line coverage, as CI measures it
explorer.exe target/llvm-cov/html/index.html   # open the report in Windows
```

Run the receiver with the Linux batched backend:

```bash
cargo run --release -p caml-recv -- --backend recvmmsg --report-secs 0 &
cargo run --release -p caml-feed -- --packets 100000 --rate 10000
kill -INT %1   # prints the report, including kernel drops and kernel→rx latency
```

## Notes

* The loopback `MULTICAST` flag resets when WSL restarts
  (`wsl --shutdown`); `wsl-setup.sh` sets it, or run
  `sudo ip link set lo multicast on`. Without it, `check.sh` skips the
  multicast test and says so.
* `uname -r` shows the WSL kernel version; record it with any measurement.
* Files in `~/caml-networking` are reachable from Windows at
  `\\wsl$\Ubuntu-24.04\home\<user>\caml-networking`.
