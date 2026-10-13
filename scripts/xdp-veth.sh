#!/usr/bin/env bash
# AF_XDP receiver on a veth pair (Linux or WSL2). Needs sudo for the parts
# that touch the kernel; cargo itself runs as you.
#
#   1. Builds the AF_XDP tests and runs them as root. They create their own
#      veth pairs and remove them afterwards.
#   2. End to end with the real binaries: caml-feed multicasts a synthetic
#      feed out of one veth end; caml-recv --backend xdp receives it on the
#      other end through AF_XDP (fallback disabled, so only AF_XDP can pass).
#   3. Fallback: caml-recv --backend xdp on an interface that does not exist
#      must fall back to recvmmsg and still receive a feed on loopback.
#
#   Exits non-zero unless every message arrives in both runs.
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
  step "sudo $(basename "$t") --include-ignored"
  sudo "$t" --include-ignored --test-threads=1
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
  --fail-on-loss --xdp-no-fallback &
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

# 3. Fallback. No sudo on purpose: as a normal user AF_XDP fails for lack of
# privileges; on a missing interface it fails as root too. Either way the
# receiver must fall back to recvmmsg, say so, and still get every message.
fb_packets=5000
fb_port=40001
fb_log=$(mktemp)
step "caml-recv --backend xdp on a missing interface (expect recvmmsg fallback)"
target/release/caml-recv --backend xdp --xdp-iface caml-xdp-none \
  --bind "127.0.0.1:$fb_port" --max-datagrams $((fb_packets + 1)) \
  --duration-secs 60 --report-secs 0 --fail-on-loss > "$fb_log" 2>&1 &
recv=$!
sleep 1
target/release/caml-feed --target "127.0.0.1:$fb_port" \
  --packets "$fb_packets" --rate "$rate"
status=0
wait "$recv" || status=$?
grep -E 'falling back|recvmmsg fallback|verdict|WARNING' "$fb_log"
if [[ $status -ne 0 ]] || ! grep -q 'falling back to recvmmsg' "$fb_log"; then
  echo "recvmmsg fallback FAILED (caml-recv exit $status)" >&2
  cat "$fb_log" >&2
  exit 1
fi
rm -f "$fb_log"
step "fallback: AF_XDP unavailable, recvmmsg took over, every message delivered"
