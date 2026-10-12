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
# The kernel links vendor/PreemptiveScheduler ([patch] → executor).
cargo test --manifest-path vendor/PreemptiveScheduler/Cargo.toml --lib -- --test-threads=1
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
# The loader, the code around user mode. Its default features pull in
# `zircon`, whose `include_bytes!` of the prebuilt userboot blobs is not in
# a plain checkout, so the test target is built for the Linux personality
# alone -- the same line CI runs.
cargo test -p zcore-loader --lib --no-default-features --features linux,libos
# Host-safe cmdline parsers (zcore binary stays test=false).
cargo test -p zcore-boot-opts --lib

# The `#[bench]` rows. COMPILED, not run: a row takes tens of seconds and its
# figure is meaningless on a shared machine, but a bench target that nothing
# builds rots, and these rows live in `#[cfg(test)]` modules beside private
# code rather than in a `benches/` target, so nothing else reaches them. These
# are the same lines the `unit-test` job runs; `--no-run` costs seconds.
cargo bench -p linux-object --bench vfs --features mock-disk --no-run
cargo bench -p linux-object --bench signal --features mock-disk --no-run
cargo bench -p zircon-object --bench vm --features libos,aspace-separate --no-run
cargo bench -p zircon-object --bench futex --features libos,aspace-separate --no-run
cargo bench -p linux-syscall --no-run
cargo bench -p kernel-hal --no-run
cargo bench -p zcore-drivers --features graphic,virtio,xhci-usb-hid --no-run
cargo bench -p hunter --lib --no-run

echo "run-unit-tests.sh: all host unit suites passed."
