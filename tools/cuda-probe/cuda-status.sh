#!/bin/sh
# Eclipse OS: one-screen CUDA readiness (hatch, nodes, libcuda, ollama libs).
# Exit 0 when driver nodes + libcuda look ready. Exit 2 if driver OK but
# Ollama lacks CUDA runner libs (the usual reason for 100% CPU after cuda-probe OK).
set -eu

PROBE=0
for a in "$@"; do
  case "$a" in
    -p|--probe) PROBE=1 ;;
    -h|--help)
      echo "usage: cuda-status [-p|--probe]"
      echo "  hatch, /dev/nvidia*, libcuda, ollama CUDA runners; -p runs cuda-probe"
      exit 0
      ;;
  esac
done

ok=1
ollama_ok=1

echo "[cmdline]"
if [ -r /proc/cmdline ]; then
  if grep -q 'nvidia\.cuda' /proc/cmdline; then
    echo "  nvidia.cuda present"
  elif grep -q 'nvidia\.nvidiactl' /proc/cmdline && grep -q 'nvidia\.uvm' /proc/cmdline; then
    echo "  nvidia.nvidiactl + nvidia.uvm present"
  else
    echo "  (no nvidia.cuda — enable on the ESP rboot.conf if /dev/nvidia* is missing)"
  fi
else
  echo "  (no /proc/cmdline)"
fi

echo "[devices]"
for n in /dev/nvidiactl /dev/nvidia-uvm /dev/nvidia0; do
  if [ -e "$n" ]; then
    echo "  $n ok"
  else
    echo "  $n MISSING"
    ok=0
  fi
done

echo "[libcuda]"
if [ -e /usr/lib/x86_64-linux-gnu/libcuda.so.1 ] || [ -e /usr/lib/libcuda.so.1 ]; then
  ls -l /usr/lib/x86_64-linux-gnu/libcuda.so* 2>/dev/null | head -5 || \
    ls -l /usr/lib/libcuda.so* 2>/dev/null | head -5
else
  echo "  libcuda.so.1 MISSING (NVIDIA userspace not in this rootfs)"
  ok=0
fi

# Ollama on Linux resolves LibOllamaPath as dirname(exe)/../lib/ollama.
echo "[ollama]"
if ! command -v ollama >/dev/null 2>&1; then
  echo "  ollama not in PATH"
  ollama_ok=0
else
  OLLAMA=$(command -v ollama)
  # Resolve one level of symlink like Ollama's EvalSymlinks for the common case.
  if [ -L "$OLLAMA" ]; then
    OLLAMA=$(readlink -f "$OLLAMA" 2>/dev/null || readlink "$OLLAMA")
  fi
  VER=$(ollama --version 2>&1 || true)
  echo "  binary  $OLLAMA"
  echo "  version $VER"
  case "$VER" in
    *'0.0.0'*)
      echo "  NOTE: version 0.0.0 usually means a non-release build without CUDA libs"
      ;;
  esac
  BINDIR=$(dirname "$OLLAMA")
  # /usr/local/bin -> /usr/local/lib/ollama ; /usr/bin -> /usr/lib/ollama
  LIBDIR=$(CDPATH= cd "$BINDIR/../lib/ollama" 2>/dev/null && pwd || true)
  if [ -z "$LIBDIR" ] || [ ! -d "$LIBDIR" ]; then
    echo "  libs    MISSING ($BINDIR/../lib/ollama)"
    echo "          install official tarball: install-ollama-cuda.sh"
    ollama_ok=0
  else
    echo "  libs    $LIBDIR"
    found=
    for d in "$LIBDIR"/cuda_v*; do
      if [ -d "$d" ] && [ -e "$d/libggml-cuda.so" ]; then
        echo "  runner  $d/libggml-cuda.so ok"
        found=1
      fi
    done
    if [ -z "$found" ]; then
      echo "  runner  MISSING cuda_v*/libggml-cuda.so — Ollama will stay on CPU"
      echo "          fix: install-ollama-cuda"
      ollama_ok=0
    fi
  fi
  # Official ollama is glibc; gcompat on /lib64 breaks it with fcntl64.
  if [ -L /lib64/ld-linux-x86-64.so.2 ]; then
    t=$(readlink -f /lib64/ld-linux-x86-64.so.2 2>/dev/null || true)
    case "$t" in
      *gcompat*|*libgcompat*)
        echo "  glibc   BROKEN: /lib64/ld-linux-x86-64.so.2 → gcompat"
        echo "          rm -f gcompat leftovers; re-run install-ollama-cuda; install-ollama-cuda"
        ollama_ok=0
        ;;
    esac
  fi
  if [ ! -e /usr/lib/x86_64-linux-gnu/libresolv.so.2 ]; then
    echo "  glibc   MISSING libresolv.so.2 (needed by ollama)"
    echo "          fix: install-ollama-cuda  # restores Ubuntu glibc pieces"
    ollama_ok=0
  fi
fi

if [ "$ok" -ne 1 ]; then
  echo "NOT READY for CUDA (driver / libcuda)"
  exit 1
fi

if [ "$ollama_ok" -ne 1 ]; then
  echo "DRIVER OK; Ollama cannot use GPU until cuda_v* runners are installed"
  echo "(cuda-probe can still pass — that only tests libcuda, not Ollama's libs)"
  exit 2
fi

echo "READY for cuda-probe / Ollama GPU discovery"
if [ "$PROBE" -eq 1 ]; then
  if command -v cuda-probe >/dev/null 2>&1; then
    echo "[cuda-probe]"
    exec cuda-probe
  fi
  echo "cuda-probe not installed"
  exit 1
fi
exit 0
