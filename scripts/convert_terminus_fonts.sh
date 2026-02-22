#!/bin/bash
# Script reproducible para convertir fuentes Terminus BDF a embedded-graphics
# Fuentes: https://terminus-font.sourceforge.net/
# Herramienta: https://github.com/embedded-graphics/bdf

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TERMINUS_URL="https://sourceforge.net/projects/terminus-font/files/terminus-font-4.49/terminus-font-4.49.1.tar.gz/download"
TERMINUS_TAR="terminus-font-4.49.1.tar.gz"
BDF_REPO="https://github.com/embedded-graphics/bdf.git"
WORK_DIR="${WORK_DIR:-$ROOT_DIR/.font-build}"
OUT_DIR="$ROOT_DIR/eclipse-apps/sidewind_sdk/src"

echo "[1/5] Creando directorio de trabajo: $WORK_DIR"
mkdir -p "$WORK_DIR"
cd "$WORK_DIR"

echo "[2/5] Descargando Terminus (si no existe)..."
if [[ ! -f "$TERMINUS_TAR" ]]; then
    wget -O "$TERMINUS_TAR" "$TERMINUS_URL" || curl -L -o "$TERMINUS_TAR" "$TERMINUS_URL"
fi

echo "[3/5] Extrayendo Terminus..."
if [[ ! -d "terminus-font-4.49.1" ]]; then
    tar xzf "$TERMINUS_TAR"
fi
TERMINUS_DIR="$WORK_DIR/terminus-font-4.49.1"

echo "[4/5] Clonando/actualizando eg-font-converter..."
if [[ ! -d "bdf" ]]; then
    git clone --depth 1 "$BDF_REPO" bdf
fi
cd bdf
cargo build -p eg-font-converter --release
CONVERTER="$(pwd)/target/release/eg-font-converter"
cd ..

convert_font() {
    local bdf="$1"
    local const_name="$2"
    local file_base="$3"
    local out_base="$OUT_DIR/$file_base"
    echo "  Convirtiendo $bdf -> $const_name"
    "$CONVERTER" "$bdf" "$const_name" \
        --mapping ASCII \
        --missing-glyph-substitute ' ' \
        --rust "${out_base}.rs" \
        --data "${out_base}.data"
}

echo "[5/5] Convirtiendo fuentes..."
# 6x12  - labels diminutos, hints
convert_font "$TERMINUS_DIR/ter-u12n.bdf" "FONT_TERMINUS_12" "font_terminus_12"
# 8x14  - cuerpo estándar
convert_font "$TERMINUS_DIR/ter-u14n.bdf" "FONT_TERMINUS_14" "font_terminus_14"
# 8x16  - entre 14 y 18
convert_font "$TERMINUS_DIR/ter-u16n.bdf" "FONT_TERMINUS_16" "font_terminus_16"
# 10x18 - intermedio
convert_font "$TERMINUS_DIR/ter-u18n.bdf" "FONT_TERMINUS_18" "font_terminus_18"
# 10x20 - títulos y valores
convert_font "$TERMINUS_DIR/ter-u20n.bdf" "FONT_TERMINUS_20" "font_terminus_20"
# 12x24 - títulos grandes
convert_font "$TERMINUS_DIR/ter-u24n.bdf" "FONT_TERMINUS_24" "font_terminus_24"

echo "Listo. Archivos en: $OUT_DIR"
ls -la "$OUT_DIR"/font_terminus_*.rs "$OUT_DIR"/font_terminus_*.data 2>/dev/null || true
