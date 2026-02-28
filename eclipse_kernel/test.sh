#!/bin/bash
# Script to run kernel unit tests on the host machine

set -e

echo "=== Running Kernel Unit Tests (Host) ==="

# We must bypass the default bare-metal target in .cargo/config.toml
# to avoid "duplicate lang item" errors with build-std.
mv .cargo/config.toml .cargo/config.toml.bak
trap "mv .cargo/config.toml.bak .cargo/config.toml" EXIT

cargo test --lib --target x86_64-unknown-linux-gnu "$@"

echo "=== All tests passed! ==="
