#!/usr/bin/env bash
# Line coverage of the library crates (protocol, engine, frame, sim,
# alloc-count), measured with cargo-llvm-cov. Used by CI and locally (WSL2).
#
# Excluded, so they cannot inflate the number:
#   apps/   CLI binaries (exercised end to end, not by unit tests)
#   tests/  integration-test code itself
#
# Usage: scripts/coverage.sh [--html] [--fail-under PERCENT]
# Writes target/coverage.txt and target/coverage.json (summary) and
# target/lcov.info.
set -euo pipefail
cd "$(dirname "$0")/.."

html=0
fail_under=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --html) html=1 ;;
    --fail-under) fail_under="$2"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

ignore='(^|[/\\])(apps|tests)[/\\]'

# The multicast-on-loopback test is ignored by default on Linux; include it
# when `lo` has the MULTICAST flag (CI and scripts/wsl-setup.sh set it).
test_args=()
if [[ "$(uname -s)" == Linux ]] && ip link show lo 2>/dev/null | grep -q MULTICAST; then
  test_args=(-- --include-ignored)
fi

cargo llvm-cov clean --workspace
cargo llvm-cov --workspace --locked --no-report "${test_args[@]}"
cargo llvm-cov report --ignore-filename-regex "$ignore" --summary-only | tee target/coverage.txt
cargo llvm-cov report --ignore-filename-regex "$ignore" --summary-only --json \
  --output-path target/coverage.json
cargo llvm-cov report --ignore-filename-regex "$ignore" --lcov \
  --output-path target/lcov.info
if [[ $html == 1 ]]; then
  cargo llvm-cov report --ignore-filename-regex "$ignore" --html
  echo "HTML report: target/llvm-cov/html/index.html"
fi

percent=$(jq -r '.data[0].totals.lines.percent * 10 | round / 10' target/coverage.json)
echo "line coverage: ${percent}%"
if [[ -n "$fail_under" ]] && awk -v p="$percent" -v t="$fail_under" 'BEGIN { exit !(p < t) }'; then
  echo "coverage ${percent}% is below the ${fail_under}% floor" >&2
  exit 1
fi
