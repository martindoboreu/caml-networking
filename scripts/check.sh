#!/usr/bin/env bash
# Run the same gates as CI. Works on Linux (including WSL2) and macOS.
# Optional gates are skipped, with a note, if their tool is not installed.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n==> %s\n' "$*"; }

step "fmt"
cargo fmt --all -- --check
step "clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings
step "test (debug, overflow checks on)"
cargo test --workspace --locked
step "test (release)"
cargo test --workspace --locked --release

if [[ "$(uname -s)" == Linux ]] && ip link show lo 2>/dev/null | grep -q MULTICAST; then
  step "ignored tests (multicast on lo)"
  cargo test --workspace --locked -- --ignored
else
  echo "(skipped multicast-on-lo test; enable with: sudo ip link set lo multicast on)"
fi

if rustup toolchain list | grep -q '^1\.88'; then
  step "MSRV 1.88"
  cargo +1.88 check --workspace --all-targets --locked
else
  echo "(skipped MSRV check; install with: rustup toolchain install 1.88 --profile minimal)"
fi

if command -v cargo-deny >/dev/null 2>&1; then
  step "cargo deny"
  cargo deny check
else
  echo "(skipped cargo deny; CI runs it)"
fi

step "all gates passed"
