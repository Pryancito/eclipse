#!/usr/bin/env bash
#
# Run the EclipseFS benchmarks: every workload on EclipseFS and on the
# in-tree btrfs, side by side (vendor/eclipsefs-rs/tests/performance.rs).
#
# Usage:
#   scripts/eclipsefs-bench.sh                       # release build, default scale
#   ECLIPSEFS_BENCH_SCALE=4 scripts/eclipsefs-bench.sh   # heavier workloads
#   scripts/eclipsefs-bench.sh bench_random_reads    # one scenario

set -euo pipefail

cd "$(dirname "$0")/../vendor/eclipsefs-rs"

exec cargo test --release --features std --test performance -- \
    --nocapture --test-threads=1 "$@"
