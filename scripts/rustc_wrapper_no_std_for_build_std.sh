#!/usr/bin/env bash
set -euo pipefail

# Cargo invokes this wrapper as:
#   rustc_wrapper.sh <path-to-rustc> <rustc-args...>
#
# We just forward to the provided rustc binary, keeping all arguments intact.
RUSTC_BIN="${1:-}"
if [ -z "${RUSTC_BIN}" ]; then
  echo "rustc wrapper: missing rustc binary argument" >&2
  exit 1
fi
shift

exec "${RUSTC_BIN}" "$@"

