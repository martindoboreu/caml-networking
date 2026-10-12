#!/usr/bin/env bash
# One-time setup of a WSL2 (Ubuntu 24.04) development environment.
#
# Run inside WSL, from a clone in the Linux filesystem (not /mnt/c, which is
# slow and loses file permissions):
#
#   git clone https://github.com/martindoboreu/caml-networking ~/caml-networking
#   cd ~/caml-networking && bash scripts/wsl-setup.sh
#
# Asks for your sudo password once (apt and `ip link`). Everything else is
# installed per user in ~/.cargo and ~/.rustup.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> system packages"
sudo apt-get update -qq
sudo apt-get install -y -qq --no-install-recommends \
  build-essential pkg-config git curl ca-certificates iproute2 jq

echo "==> Rust (rustup; the toolchain is pinned by rust-toolchain.toml)"
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --profile minimal --default-toolchain none
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"
rustup toolchain install
rustup component add llvm-tools-preview
rustup toolchain install 1.88 --profile minimal

echo "==> cargo-llvm-cov (prebuilt release binary)"
if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSfL \
    https://github.com/taiki-e/cargo-llvm-cov/releases/latest/download/cargo-llvm-cov-x86_64-unknown-linux-gnu.tar.gz |
    tar xzf - -C "$HOME/.cargo/bin"
fi

echo "==> multicast on loopback (for the multicast test; resets when WSL restarts)"
sudo ip link set lo multicast on

cat <<'EOF'

==> done. Next:
    source ~/.cargo/env
    bash scripts/check.sh        # every CI gate
    bash scripts/coverage.sh     # line coverage (add --html for a browsable report)
EOF
