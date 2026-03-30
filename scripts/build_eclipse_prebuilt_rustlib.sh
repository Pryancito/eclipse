#!/usr/bin/env bash
# Genera un juego de rlibs (core, alloc, compiler_builtins) para el target Eclipse
# usando una sola corrida de -Z build-std, y opcionalmente un sysroot completo listo
# para RUSTFLAGS=--sysroot=... sin pasar -Z build-std en cada build.
#
# Uso:
#   ./scripts/build_eclipse_prebuilt_rustlib.sh
#   ./scripts/build_eclipse_prebuilt_rustlib.sh --merge   # copia el nightly actual y añade el triple Eclipse
#   OUT_DIR=~/eclipse-rust PREBUILT_TARBALL=1 ./scripts/build_eclipse_prebuilt_rustlib.sh
#
# Después (mismo nightly que el que usaste al generar):
#   export RUSTFLAGS="--sysroot=$(pwd)/eclipse-sysroot-merged"
#   cargo +nightly build --release --target x86_64-unknown-eclipse.json
#
# rustup toolchain link (opcional): el directorio debe tener bin/rustc y lib/ como un sysroot;
#   con --merge ya es una copia del nightly + rustlib Eclipse; luego:
#   rustup toolchain link eclipse-sysroot "$(pwd)/eclipse-sysroot-merged"
#   cargo +eclipse-sysroot build --target x86_64-unknown-eclipse.json
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

TARGET_JSON="${ECLIPSE_TARGET_JSON:-$REPO_ROOT/x86_64-unknown-eclipse.json}"
TRIPLE="${ECLIPSE_TARGET_TRIPLE:-x86_64-unknown-eclipse}"
STAGING="${ECLIPSE_RUSTLIB_STAGING:-$REPO_ROOT/eclipse-rustlib-staging}"
OUT_LIB="${OUT_DIR:-$REPO_ROOT/eclipse-prebuilt-rustlib}/lib/rustlib/$TRIPLE/lib"
MERGE=0
PREBUILT_TARBALL="${PREBUILT_TARBALL:-0}"
# Canal rustup (sin el '+'): ej. nightly, nightly-2025-01-01
RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-nightly}"
# Debe coincidir con lo que uséis en build-std si activáis mem intrinsics:
BUILD_STD_FEATURES="${BUILD_STD_FEATURES:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --merge) MERGE=1; shift ;;
    --tarball) PREBUILT_TARBALL=1; shift ;;
    -h|--help)
      sed -n '1,35p' "$0"
      exit 0
      ;;
    *) echo "Opción desconocida: $1"; exit 1 ;;
  esac
done

if [[ ! -f "$TARGET_JSON" ]]; then
  echo "No existe el target: $TARGET_JSON"
  exit 1
fi

NIGHTLY_INFO="$(rustc +"$RUSTUP_TOOLCHAIN" -vV)"
NIGHTLY_HASH="$(echo "$NIGHTLY_INFO" | awk '/commit-hash/ {print $2}')"
echo "[eclipse-prebuilt-rustlib] rustc +$RUSTUP_TOOLCHAIN commit $NIGHTLY_HASH"

rm -rf "$STAGING/target"
mkdir -p "$STAGING" "$OUT_LIB"

export CARGO_TARGET_DIR="$STAGING/target"

BUILD_STD_ARGS=(-Z "build-std=core,alloc")
if [[ -n "$BUILD_STD_FEATURES" ]]; then
  BUILD_STD_ARGS+=(-Z "build-std-features=$BUILD_STD_FEATURES")
fi

RUSTFLAGS_PRE="${RUSTFLAGS:-}"
export RUSTFLAGS="-Z unstable-options ${RUSTFLAGS_PRE}"

# Crate mínimo ya en el repo (no_std, sin dependencias pesadas).
cd "$REPO_ROOT/eclipse-syscall"
cargo +"$RUSTUP_TOOLCHAIN" build --release \
  --target "$TARGET_JSON" \
  "${BUILD_STD_ARGS[@]}"

DEPS="$CARGO_TARGET_DIR/$TRIPLE/release/deps"
shopt -s nullglob
Rlibs=( "$DEPS"/libcore-*.rlib "$DEPS"/liballoc-*.rlib "$DEPS"/libcompiler_builtins-*.rlib )
if [[ ${#Rlibs[@]} -eq 0 ]] || [[ ! -f "${Rlibs[0]}" ]]; then
  echo "No se encontraron rlibs en $DEPS"
  exit 1
fi

cp -a "${Rlibs[@]}" "$OUT_LIB/"
META_DIR="$(dirname "$OUT_LIB")"
{
  echo "triple=$TRIPLE"
  echo "rustc_version=$NIGHTLY_INFO"
  echo "generated=$(date -Iseconds)"
  echo "build_std_features=${BUILD_STD_FEATURES:-(none)}"
} > "$META_DIR/PREBUILT.txt"

echo "[eclipse-prebuilt-rustlib] Copiadas ${#Rlibs[@]} rlibs a $OUT_LIB"
ls -la "$OUT_LIB"

if [[ "$PREBUILT_TARBALL" == "1" ]]; then
  PRE_ROOT="${OUT_DIR:-$REPO_ROOT/eclipse-prebuilt-rustlib}"
  TAR_BASE="eclipse-prebuilt-rustlib-${TRIPLE}-${NIGHTLY_HASH:0:9}"
  tar -cJf "$REPO_ROOT/${TAR_BASE}.tar.xz" -C "$(dirname "$PRE_ROOT")" "$(basename "$PRE_ROOT")"
  echo "[eclipse-prebuilt-rustlib] Tarball: $REPO_ROOT/${TAR_BASE}.tar.xz"
fi

if [[ "$MERGE" == "1" ]]; then
  HOST_SYSROOT="$(rustc +"$RUSTUP_TOOLCHAIN" --print sysroot)"
  MERGED="${ECLIPSE_MERGED_SYSROOT:-$REPO_ROOT/eclipse-sysroot-merged}"
  echo "[eclipse-prebuilt-rustlib] Fusionando nightly sysroot -> $MERGED"
  rm -rf "$MERGED"
  cp -a "$HOST_SYSROOT" "$MERGED"
  mkdir -p "$MERGED/lib/rustlib/$TRIPLE/lib"
  cp -a "$OUT_LIB"/*.rlib "$MERGED/lib/rustlib/$TRIPLE/lib/"
  cp -a "$META_DIR/PREBUILT.txt" "$MERGED/lib/rustlib/$TRIPLE/PREBUILT.txt"
  echo "Listo. Ejemplo:"
  echo "  export RUSTFLAGS=\"--sysroot=$MERGED \${RUSTFLAGS:-}\""
  echo "  cargo +${RUSTUP_TOOLCHAIN} build --release --target $TARGET_JSON"
fi
