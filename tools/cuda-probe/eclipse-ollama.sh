#!/bin/sh
# Eclipse OS: wrapper that launches ollama preferring cuda_v* runners when available.
set -eu

if [ -x /usr/local/bin/ollama ]; then
  OLLAMA=/usr/local/bin/ollama
elif command -v ollama >/dev/null 2>&1; then
  OLLAMA=$(command -v ollama)
else
  echo "ollama not found in PATH or /usr/local/bin" >&2
  exit 1
fi

BINDIR=$(dirname "$OLLAMA")
LIBDIR=$(CDPATH= cd "$BINDIR/../lib/ollama" 2>/dev/null && pwd || true)

# If CUDA runners exist, default to the detected cuda_v* runner.
if [ -d "$LIBDIR" ]; then
  for d in "$LIBDIR"/cuda_v*; do
    if [ -d "$d" ] && [ -e "$d/libggml-cuda.so" ]; then
      runner=$(basename "$d")
      export OLLAMA_LLM_LIBRARY=${OLLAMA_LLM_LIBRARY:-$runner}
      break
    fi
  done
fi

exec "$OLLAMA" "$@"
