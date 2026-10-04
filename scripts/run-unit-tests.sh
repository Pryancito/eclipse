#!/usr/bin/env bash
#
# run-unit-tests.sh — host-side unit tests matching CI job `unit-test`.
#
# The workspace `default-members = ["xtask"]`, so bare `cargo test` only runs
# xtask. This script names every crate the CI job runs so a green local run
# means the same suite as GitHub Actions.
#
# Usage (from repo root):
#   scripts/run-unit-tests.sh
#
# See docs/ci-testing.md ("Unit tests host").

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

python3 -m unittest discover -s scripts/tests
cargo test --no-fail-fast
# `elf` too: util::elf_loader is behind that feature (same as CI).
cargo test -p zircon-object --lib --features libos,aspace-separate,elf
cargo test -p zircon-syscall --features libos,zircon-object/aspace-separate
# The kernel links vendor/PreemptiveScheduler ([patch] → executor). The
# lowercase tree is pristine upstream that nothing depends on — test both,
# fork first (CI order).
cargo test --manifest-path vendor/PreemptiveScheduler/Cargo.toml --lib -- --test-threads=1
cargo test --manifest-path vendor/preemptive-scheduler/Cargo.toml --lib
cargo test -p lock --lib -- --test-threads=1
cargo test --manifest-path vendor/trapframe/Cargo.toml
# Single-threaded: process-wide driver state (DRM_STATE, CE staging, …).
# graphic + virtio + xhci-usb-hid: without them those modules are not built.
cargo test -p zcore-drivers --lib --features graphic,virtio,xhci-usb-hid -- --test-threads=1
cargo test -p linux-object --lib --features mock-disk -- --test-threads=1
cargo test -p linux-syscall --lib -- --test-threads=1
cargo test -p kernel-hal --lib -- --test-threads=1
cargo test -p hunter --lib -- --test-threads=1
cargo test -p nvidia-rm-sys -- --test-threads=1
cargo test -p rcore-fs --features std
cargo test --release -p btrfs --features std
cargo test -p rcore-fs-sfs --features std
cargo test -p rcore-fs-mountfs --lib
cargo test -p rcore-fs-ramfs --lib
cargo test -p rcore-fs-devfs --lib
cargo test -p rcore-fs --lib
cargo test -p virtio-drivers --lib -- --test-threads=1
cargo test -p region-alloc --lib -- --test-threads=1
# Host-safe cmdline parsers (zcore binary stays test=false).
cargo test -p zcore-boot-opts --lib

echo "run-unit-tests.sh: all host unit suites passed."
