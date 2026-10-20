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
# The source of truth is `.github/workflows/test.yml`, job `unit-test`:
# every `cargo test` and `cargo bench` line of that job is a line here,
# in its order. Add one there, add it here.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

python3 -m unittest discover -s scripts/tests
cargo test --no-fail-fast
# `elf` too: util::elf_loader is behind that feature (same as CI).
cargo test -p zircon-object --lib --features libos,aspace-separate,elf
cargo test -p zircon-syscall --features libos,zircon-object/aspace-separate
# The kernel links vendor/PreemptiveScheduler ([patch] → executor).
# `--tests` and not `--lib`: the crate has integration tests of its own
# under `tests/`, and they are half of what CI runs here.
cargo test --manifest-path vendor/PreemptiveScheduler/Cargo.toml --tests -- --test-threads=1
cargo test -p lock --lib -- --test-threads=1
cargo test --manifest-path vendor/trapframe/Cargo.toml
# Single-threaded: process-wide driver state (DRM_STATE, CE staging, …).
# graphic + virtio + xhci-usb-hid: without them those modules are not built.
cargo test -p zcore-drivers --lib --features graphic,virtio,xhci-usb-hid -- --test-threads=1
cargo test -p linux-object --lib --features mock-disk -- --test-threads=1
cargo test -p linux-syscall --lib -- --test-threads=1
cargo test -p kernel-hal --lib -- --test-threads=1
cargo test -p hunter --lib -- --test-threads=1
# The DOC tests of those same crates. A `cargo test -p <crate> --lib`
# line builds the lib target alone, so a ``` example in a doc comment
# is compiled by nothing -- and these crates document their syscalls
# with examples that call them.
cargo test -p linux-object --doc --features mock-disk
cargo test -p linux-syscall --doc
cargo test -p kernel-hal --doc
cargo test -p hunter --doc
cargo test -p zircon-object --doc --features libos,aspace-separate,elf
cargo test -p nvidia-rm-sys -- --test-threads=1
cargo test -p rcore-fs --features std
cargo test --release -p btrfs --features std
cargo test --release -p eclipsefs --features std
cargo test -p rcore-fs-sfs --features std
cargo test -p rcore-fs-mountfs --lib
cargo test -p rcore-fs-ramfs --lib
cargo test -p rcore-fs-devfs --lib
cargo test -p rcore-fs --lib
cargo test -p virtio-drivers --lib -- --test-threads=1
cargo test -p region-alloc --lib -- --test-threads=1
# The TCP/IP stack the kernel links through the root `[patch]`. It is in
# the workspace `exclude` list, so no bare `cargo test` reaches it.
# Default features on purpose -- that is what upstream tests -- and
# nothing in the suite opens a raw socket, so it needs no privileges.
cargo test --manifest-path smoltcp/Cargo.toml
# The text console the kernel prints through. `--no-default-features`
# on purpose: that is how `zcore-drivers` takes it, and it swaps the
# real `log` macros for the dummy ones in `src/log.rs`, which accept a
# narrower shape of argument. This line builds what the kernel builds.
cargo test -p rcore-console --lib --no-default-features -- --test-threads=1
# The loader, the code around user mode. Its default features pull in
# `zircon`, whose `include_bytes!` of the prebuilt userboot blobs is not in
# a plain checkout, so the test target is built for the Linux personality
# alone -- the same line CI runs.
cargo test -p zcore-loader --lib --no-default-features --features linux,libos
# The vDSO. Not `--lib`: the integration test is the half that maps the
# linked image and calls it, which is where a wrong fixed-point
# multiply shows up.
cargo test -p linux-vdso
# rboot, the UEFI bootloader, and the first code that runs on the
# machine. It is `exclude`d from the workspace (its own toolchain file
# and target). Its default features pull in `uefi-services`, whose
# panic handler collides with `std`'s `panic_impl` lang item in a test
# binary, so the library target is built without them.
cargo test --manifest-path rboot/Cargo.toml --lib --no-default-features
# The four packages of the installed desktop. Each has its own
# `[workspace]` table and its own target, so a bare `cargo test` in the
# root workspace builds none of them, and `xtask` builds them
# best-effort: a break here fails no build, it just leaves the desktop
# without that piece.
cargo test --manifest-path tools/eclipse-init/Cargo.toml
cargo test --manifest-path tools/lunarbg/Cargo.toml
cargo test --manifest-path tools/lunarbar/Cargo.toml
cargo test --manifest-path tools/eclipse-dbusd/Cargo.toml
# The kernel binary crate itself: the dmesg ring, the physical frame
# allocator and the exit status CI reads. Built for the libos
# personality, which is the one that links against the host, with
# `linux` as well, since the Linux personality is what this kernel
# boots.
cargo test -p zcore --features libos,linux -- --test-threads=1
# Host-safe cmdline parsers. Not a line of the `unit-test` job: nothing
# in CI names this crate, and the workspace `default-members` hide it
# from the bare `cargo test` above.
cargo test -p zcore-boot-opts --lib

# The `#[bench]` rows. COMPILED, not run: a row takes tens of seconds and its
# figure is meaningless on a shared machine, but a bench target that nothing
# builds rots, and these rows live in `#[cfg(test)]` modules beside private
# code rather than in a `benches/` target, so nothing else reaches them. These
# are the same lines the `unit-test` job runs; `--no-run` costs seconds.
cargo bench -p linux-object --bench vfs --features mock-disk --no-run
cargo bench -p linux-object --bench signal --features mock-disk --no-run
cargo bench -p linux-object --bench process --features mock-disk --no-run
cargo bench -p linux-object --bench process_census --features mock-disk --no-run
cargo bench -p linux-object --lib --features mock-disk --no-run
cargo bench -p zircon-object --bench vm --features libos,aspace-separate --no-run
cargo bench -p zircon-object --bench futex --features libos,aspace-separate --no-run
cargo bench -p zircon-object --bench task --features libos,aspace-separate --no-run
cargo bench -p linux-syscall --no-run
cargo bench -p kernel-hal --no-run
cargo bench -p zcore-drivers --features graphic,virtio,xhci-usb-hid --no-run
cargo bench -p hunter --lib --no-run

echo "run-unit-tests.sh: all host unit suites passed."
