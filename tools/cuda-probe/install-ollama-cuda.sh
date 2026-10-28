#!/bin/sh
# Eclipse OS: install official Ollama (glibc + CUDA runners) into /usr/local.
#
# Upstream asset (GitHub releases / ollama.com redirect):
#   ollama-linux-amd64.tar.zst   (~1.4 GiB)  — current
#   ollama-linux-amd64.tgz       — gone (404 "Not Found", 9 bytes)
#
# Needs: curl (or wget), tar, zstd.  apt-get install -y curl zstd ca-certificates
#
# The binary is glibc. Eclipse already ships Ubuntu glibc for NVIDIA userspace
# at /lib64/ld-linux-x86-64.so.2 + /usr/lib/x86_64-linux-gnu. Do NOT install Alpine
# gcompat: it replaces that loader and then fails with
#   Error relocating …: fcntl64: symbol not found
#
# Also writes an eclipse-init service, /etc/eclipse/services/ollama.service,
# so `ollama serve` starts at boot and init restarts it if it dies.
#
# Usage: install-ollama-cuda            download, install, write the service
#        install-ollama-cuda --service  only (re)write the service
#
# Env: OLLAMA_URL=…  override full URL
#      DEST=/usr/local
#      OLLAMA_SERVICE=0  do not write the init service
#      SERVICE_DIR=/etc/eclipse/services
set -eu

SERVICE_ONLY=0
case "${1:-}" in
  "") ;;
  --service) SERVICE_ONLY=1 ;;
  *)
    echo "usage: $0 [--service]" >&2
    exit 2
    ;;
esac

DEST=${DEST:-/usr/local}
BIN="$DEST/bin/ollama"
LIB="$DEST/lib/ollama"
SERVICE_DIR=${SERVICE_DIR:-/etc/eclipse/services}
# Prefer the GitHub release asset name; ollama.com/download redirects here.
URL=${OLLAMA_URL:-https://ollama.com/download/ollama-linux-amd64.tar.zst}

# Same pinned Ubuntu 26.10 libc6 as xtask nvidia_userspace (loader + libresolv).
GLIBC_DEB_URL=${GLIBC_DEB_URL:-https://archive.ubuntu.com/ubuntu/pool/main/g/glibc/libc6_2.44-1ubuntu2_amd64.deb}
GLIBC_DEB_SHA256=${GLIBC_DEB_SHA256:-e4871fc9d5bf5fe0f384c6b90f70e64e9c9bbcb84927aa97c7f70ec6bf05b8c0}
GLIBC_LIBDIR=/usr/lib/x86_64-linux-gnu
GLIBC_LOADER=/lib64/ld-linux-x86-64.so.2

# ~1.4 GiB archive: prefer a disk path if TMPDIR (often tmpfs) is too small.
pick_tmpdir() {
  # df -Pk: POSIX portable, 1 KiB blocks (BusyBox / GNU).
  need_k=$((1600 * 1024))
  for cand in "${TMPDIR:-}" /var/tmp "${HOME:-}" "$DEST" /tmp; do
    [ -n "$cand" ] || continue
    [ -d "$cand" ] || mkdir -p "$cand" 2>/dev/null || continue
    avail_k=$(df -Pk "$cand" 2>/dev/null | awk 'NR==2 {print $4}')
    [ -n "$avail_k" ] || continue
    if [ "$avail_k" -ge "$need_k" ] 2>/dev/null; then
      echo "$cand"
      return 0
    fi
  done
  # Last resort: whatever TMPDIR/tmp is (may ENOSPC mid-download).
  echo "${TMPDIR:-/tmp}"
}

TMP=$(pick_tmpdir)/ollama-cuda-$$
ARCHIVE=$TMP/ollama-linux-amd64.tar.zst

cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

# eclipse-init reads its services only at boot, so this one first runs at the
# next boot. eclipse-ollama picks the cuda_v* runner (init has no env key to
# set OLLAMA_LLM_LIBRARY), but it looks in /usr/local, so any other DEST runs
# the binary directly. A respawn service: `killall ollama` gets it restarted.
write_service() {
  exec_line="$BIN serve"
  if [ "$BIN" = /usr/local/bin/ollama ] && [ -x /usr/bin/eclipse-ollama ]; then
    exec_line="/usr/bin/eclipse-ollama serve"
  fi
  if [ "${OLLAMA_SERVICE:-1}" = 0 ] || [ ! -d "$SERVICE_DIR" ]; then
    if [ "${OLLAMA_SERVICE:-1}" = 0 ]; then
      echo "OLLAMA_SERVICE=0: no init service written."
    else
      echo "No $SERVICE_DIR (not eclipse-init?): no init service written."
    fi
    echo "Start serve (GPU):"
    echo "  killall ollama 2>/dev/null"
    echo "  $exec_line"
    return 0
  fi
  svc=$SERVICE_DIR/ollama.service
  fresh=1
  [ -e "$svc" ] && fresh=0
  cat >"$svc.tmp" <<EOF
# ollama serve, written by install-ollama-cuda. Delete this file to stop
# starting it at boot (OLLAMA_SERVICE=0 install-ollama-cuda skips writing it).
exec = $exec_line
type = respawn
log = /tmp/ollama.log
EOF
  mv -f "$svc.tmp" "$svc"
  echo "Init service: $svc"
  echo "  exec = $exec_line"
  echo "  Starts ollama serve at boot; init restarts it if it dies."
  echo "  Log: /tmp/ollama.log"
  if [ "$fresh" = 1 ]; then
    echo "Init reads it at the next boot. To start serve now:"
    echo "  killall ollama 2>/dev/null; $exec_line >/tmp/ollama.log 2>&1 &"
  else
    echo "If init already runs it, killall ollama restarts it on the new binary."
  fi
}

if [ "$SERVICE_ONLY" = 1 ]; then
  if [ ! -x "$BIN" ]; then
    echo "no $BIN: run install-ollama-cuda without --service first" >&2
    exit 1
  fi
  write_service
  exit 0
fi

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "need $1 — run: apt-get install -y curl zstd ca-certificates" >&2
    exit 1
  }
}

need tar
need zstd
if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
  echo "need curl or wget — run: apt-get install -y curl" >&2
  exit 1
fi

download() {
  dest=$1
  url=$2
  if command -v curl >/dev/null 2>&1; then
    curl -fL --progress-bar -o "$dest" "$url"
  else
    wget -O "$dest" "$url"
  fi
}

# True if path looks like Alpine gcompat's fake glibc loader (must not win).
is_gcompat_loader() {
  p=$1
  [ -e "$p" ] || return 1
  # Symlink into libgcompat / ld-linux that is not Ubuntu's multiarch copy.
  if [ -L "$p" ]; then
    t=$(readlink -f "$p" 2>/dev/null || readlink "$p")
    case "$t" in
      *gcompat*|*libgcompat*) return 0 ;;
    esac
  fi
  # File content / strings heuristic when not a symlink.
  if command -v strings >/dev/null 2>&1; then
    strings "$p" 2>/dev/null | grep -q gcompat && return 0
  fi
  return 1
}

glibc_runtime_ok() {
  [ -e "$GLIBC_LOADER" ] || return 1
  is_gcompat_loader "$GLIBC_LOADER" && return 1
  [ -e "$GLIBC_LIBDIR/libc.so.6" ] || return 1
  [ -e "$GLIBC_LIBDIR/libresolv.so.2" ] || return 1
  return 0
}

# Pull the pinned libc6 .deb and install loader + resolv/nss next to NVIDIA's glibc.
ensure_glibc_for_ollama() {
  if glibc_runtime_ok; then
    echo "glibc runtime OK ($GLIBC_LOADER + libresolv)"
    return 0
  fi

  echo "Ensuring Ubuntu glibc loader + libresolv (ollama is not musl; skip gcompat)..."
  if is_gcompat_loader "$GLIBC_LOADER"; then
    echo "WARNING: $GLIBC_LOADER is gcompat — replacing with real glibc loader" >&2
  fi
  if ! command -v ar >/dev/null 2>&1; then
    echo "need ar to unpack libc6 — run: apt-get install -y binutils" >&2
    exit 1
  fi

  deb=$TMP/libc6.deb
  download "$deb" "$GLIBC_DEB_URL"
  if command -v sha256sum >/dev/null 2>&1; then
    got=$(sha256sum "$deb" | awk '{print $1}')
    if [ "$got" != "$GLIBC_DEB_SHA256" ]; then
      echo "libc6 sha256 mismatch: got $got want $GLIBC_DEB_SHA256" >&2
      exit 1
    fi
  fi

  extract=$TMP/libc6
  mkdir -p "$extract"
  # Ubuntu .deb: ar → data.tar.zst
  (
    cd "$extract"
    ar x "$deb"
    if [ -f data.tar.zst ]; then
      zstd -d -c data.tar.zst | tar -xf -
    elif [ -f data.tar.xz ]; then
      tar -xJf data.tar.xz
    elif [ -f data.tar.gz ]; then
      tar -xzf data.tar.gz
    else
      echo "unexpected libc6.deb layout" >&2
      exit 1
    fi
  )

  mkdir -p /lib64 "$GLIBC_LIBDIR"
  # Loader where every x86_64 glibc PT_INTERP points. Replace a gcompat
  # symlink outright (cp into a symlink would overwrite libgcompat).
  src_ld=$extract/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2
  [ -f "$src_ld" ] || src_ld=$extract/lib64/ld-linux-x86-64.so.2
  rm -f "$GLIBC_LOADER"
  cp -f "$src_ld" "$GLIBC_LOADER"
  chmod 755 "$GLIBC_LOADER"

  for lib in libc.so.6 libm.so.6 libdl.so.2 libpthread.so.0 librt.so.1 \
    libresolv.so.2 libnss_dns.so.2 libnss_files.so.2; do
    src=$extract/usr/lib/x86_64-linux-gnu/$lib
    if [ -e "$src" ]; then
      cp -a "$src" "$GLIBC_LIBDIR/"
    fi
  done

  if ! glibc_runtime_ok; then
    echo "failed to install glibc runtime for ollama" >&2
    echo "  loader: $GLIBC_LOADER" >&2
    ls -la "$GLIBC_LOADER" "$GLIBC_LIBDIR"/libresolv.so.2 2>&1 || true
    exit 1
  fi
  echo "glibc runtime installed under $GLIBC_LIBDIR"
}

echo "Scratch dir: $TMP"
mkdir -p "$TMP" "$DEST"

ensure_glibc_for_ollama

echo "Downloading $URL ..."
download "$ARCHIVE" "$URL"

sz=$(wc -c <"$ARCHIVE" | tr -d ' ')
if [ "$sz" -lt 1000000 ]; then
  echo "download too small (${sz} B) — wrong URL or redirect not followed" >&2
  head -c 200 "$ARCHIVE" >&2 || true
  echo >&2
  exit 1
fi

echo "Extracting into $DEST ..."
zstd -d -c "$ARCHIVE" | tar -xf - -C "$DEST"

if [ ! -x "$BIN" ]; then
  echo "extract failed: no $BIN" >&2
  exit 1
fi

echo "[installed]"
if ! "$BIN" --version 2>&1; then
  echo >&2
  echo "ollama failed to start. If you see fcntl64 / Error relocating:" >&2
  echo "  rm -f gcompat leftovers; re-run install-ollama-cuda 2>/dev/null; re-run $0" >&2
  echo "  (gcompat must not own $GLIBC_LOADER)" >&2
  exit 1
fi
echo "  binary  $BIN"
echo "  libs    $LIB"
ls -d "$LIB"/cuda_v* 2>/dev/null || {
  echo "WARNING: no $LIB/cuda_v* — no CUDA runners in this archive" >&2
  exit 1
}
ls "$LIB"/cuda_v*/libggml-cuda.so 2>/dev/null | head -5

echo
write_service
echo "Then: eclipse-ollama ps         # GPU load, not 100% CPU"
echo "Check: cuda-status -p"
echo
echo "Do not use Alpine gcompat for ollama — use the Ubuntu glibc next to libcuda."
echo "If llama-server dies with SIGILL on CUDA, AVX may be off — see dmesg | grep fpu."
