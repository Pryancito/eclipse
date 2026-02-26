#!/usr/bin/env bash
# Ejecuta los tests de eclipse_ipc en host (x86_64-unknown-linux-gnu).
# El workspace principal usa build-std; para tests en host hay que desactivarlo
# temporalmente para evitar "duplicate lang item in crate core".

set -e
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CONFIG="$ROOT/.cargo/config.toml"
BACKUP="$ROOT/.cargo/config.toml.bak"

if [[ ! -f "$CONFIG" ]]; then
  echo "No se encontró $CONFIG"
  exit 1
fi

# Desactivar build-std (comentar líneas)
sed -i.bak -e 's/^build-std = /# build-std = /' -e 's/^build-std-features = /# build-std-features = /' "$CONFIG"
trap "mv -f '$CONFIG.bak' '$CONFIG'" EXIT

cd "$ROOT"
cargo test -p eclipse_ipc_tests --target x86_64-unknown-linux-gnu --no-fail-fast
