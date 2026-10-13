#!/usr/bin/env bash
# AF_XDP receiver on a veth pair (Linux or WSL2). Needs sudo for the parts
# that touch the kernel; cargo itself runs as you.
#
#   1. Builds the AF_XDP tests and runs them as root. They create their own
#      veth pairs and remove them afterwards.
#   2. End to end with the real binaries: caml-feed multicasts a synthetic
#      feed out of one veth end; caml-recv --backend xdp receives it on the
#      other end through AF_XDP. Exits non-zero unless every message arrives.
#
# Only veth pairs are touched, never a physical interface (AGENTS.md).
#
# Environment: PACKETS (default 20000), RATE (packets/s, default 10000).
set -euo pipefail
cd "$(dirname "$0")/.."

packets=${PACKETS:-20000}
rate=${RATE:-10000}
tx=caml-xdp-tx
rx=caml-xdp-rx
tx_ip=10.77.0.1
group=239.77.0.9
port=40000

step() { printf '\n==> %s\n' "$*"; }

step "building AF_XDP tests"
mapfile -t tests < <(
  cargo test -p caml-transport-xdp --features xdp --locked --no-run --message-format=json |
    jq -r 'select(.reason == "compiler-artifact" and .profile.test == true) | .executable // empty'
)
for t in "${tests[@]}"; do
  step "sudo $(basename "$t") --ignored"
  sudo "$t" --ignored --test-threads=1
done

step "building caml-recv (with AF_XDP) and caml-feed"
cargo build --release --locked -p caml-recv --features caml-recv/xdp -p caml-feed

cleanup() { sudo ip link del "$tx" 2>/dev/null || true; }
trap cleanup EXIT
cleanup
step "veth pair $tx <-> $rx"
sudo ip link add "$tx" type veth peer name "$rx"
for dev in "$tx" "$rx"; do
  sudo sysctl -qw "net.ipv6.conf.$dev.disable_ipv6=1"
  sudo ip link set "$dev" up
done
sudo ip addr add "$tx_ip/24" dev "$tx"
# veth offloads TX checksums, leaving UDP checksums partial on the wire; the
# receiver verifies them and would (correctly) reject every frame. Real NICs
# send complete checksums, so make veth do the same.
sudo ethtool -K "$tx" tx off > /dev/null

step "caml-recv --backend xdp on $rx"
sudo target/release/caml-recv --backend xdp --xdp-iface "$rx" \
  --bind "0.0.0.0:$port" --group "$group" --allow-source "$tx_ip" \
  --max-datagrams $((packets + 1)) --duration-secs 60 --report-secs 0 \
  --fail-on-loss &
recv=$!
# Wait until libxdp has attached its program to the receiving end.
for _ in $(seq 100); do
  ip -d link show "$rx" | grep -q 'prog/xdp' && break
  sleep 0.1
done

step "caml-feed: $packets packets at $rate/s to $group:$port via $tx"
target/release/caml-feed --target "$group:$port" --multicast-if "$tx_ip" \
  --packets "$packets" --rate "$rate"

status=0
wait "$recv" || status=$?
if [[ $status -ne 0 ]]; then
  echo "AF_XDP receive over veth FAILED (caml-recv exit $status)" >&2
  exit "$status"
fi
step "AF_XDP over veth: every message delivered, verdict clean"
