#!/usr/bin/env python3
"""
Genera el set completo de fuentes Eclipse para embedded-graphics.
Uso:
  python3 gen_eclipse_fonts.py                    # genera desde hex embebido (tamaño 12)
  python3 gen_eclipse_fonts.py --hex-file F       # lee hex desde archivo F
  python3 gen_eclipse_fonts.py --scale-from 12    # escala 12 -> 14,16,18,20,24

Salida: eclipse-apps/sidewind_sdk/src/font_eclipse_*.data y font_eclipse_*.rs
"""

import argparse
import struct
import os

# Hex para font_eclipse_12 (6x12) - proporcionado por Gemini
# Atlas 96px ancho, 12px alto por glifo, 96 glifos ASCII
FONT_12_HEX = (
    "0000000000000000000000000000000000001000280028007c00100010001000000010002800280010001000100010000000000000000000380044004400440038000000000000000000100010007c0010001000000000000000000000000000000000007c0000000000000000000000000000000000000010000000000000000408102040000000000038444c544438000000000010301010107c000000000038440438407c00000000003844180444380000000000081828487c0800000000007c4078044438000000000038407844443800000000007c0408102020000000000384438444438000000000038443c04043800000000000010000010000000000000100000102000000000000810201008000000000000007c007c00000000000020100810200000000003844081000100000"
)

# Dimensiones por tamaño (igual que Terminus para compatibilidad)
SIZES = {
    12: {"w": 6, "h": 12, "atlas_w": 96, "baseline": 9, "underline": 11, "strikethrough": 6},
    14: {"w": 8, "h": 14, "atlas_w": 128, "baseline": 11, "underline": 13, "strikethrough": 7},
    16: {"w": 8, "h": 16, "atlas_w": 128, "baseline": 11, "underline": 13, "strikethrough": 8},
    18: {"w": 10, "h": 18, "atlas_w": 160, "baseline": 14, "underline": 16, "strikethrough": 9},
    20: {"w": 10, "h": 20, "atlas_w": 160, "baseline": 15, "underline": 17, "strikethrough": 10},
    24: {"w": 12, "h": 24, "atlas_w": 192, "baseline": 18, "underline": 20, "strikethrough": 12},
}


def hex_to_bytes(hex_str: str) -> bytes:
    hex_str = hex_str.replace(" ", "").replace("\n", "")
    return bytes.fromhex(hex_str)


def scale_1bpp_atlas(
    src_data: bytes,
    src_glyph_w: int,
    src_glyph_h: int,
    dst_glyph_w: int,
    dst_glyph_h: int,
    glyphs_per_row: int = 16,
) -> bytes:
    """Escala atlas 1bpp: cada glifo de (src_glyph_w, src_glyph_h) -> (dst_glyph_w, dst_glyph_h)."""
    rows = 6  # 96 glifos ASCII
    src_atlas_w = glyphs_per_row * src_glyph_w
    dst_atlas_w = glyphs_per_row * dst_glyph_w
    dst_atlas_h = rows * dst_glyph_h
    dst_bytes_per_row = (dst_atlas_w + 7) // 8
    dst = bytearray(dst_bytes_per_row * dst_atlas_h)

    def get_pixel(data: bytes, bw: int, x: int, y: int) -> int:
        bpr = (bw + 7) // 8
        byte_idx = y * bpr + x // 8
        if byte_idx >= len(data):
            return 0
        return 1 if (data[byte_idx] >> (7 - (x % 8))) & 1 else 0

    def set_pixel(buf: bytearray, bw: int, x: int, y: int, v: int):
        bpr = (bw + 7) // 8
        byte_idx = y * bpr + x // 8
        if byte_idx < len(buf) and v:
            buf[byte_idx] |= 1 << (7 - (x % 8))

    for gr in range(rows):
        for gc in range(glyphs_per_row):
            gidx = gr * glyphs_per_row + gc
            if gidx >= 96:
                break
            for dy in range(dst_glyph_h):
                for dx in range(dst_glyph_w):
                    sx = (dx * src_glyph_w) // dst_glyph_w if dst_glyph_w > 0 else 0
                    sy = (dy * src_glyph_h) // dst_glyph_h if dst_glyph_h > 0 else 0
                    gx = gc * src_glyph_w + sx
                    gy = gr * src_glyph_h + sy
                    p = get_pixel(src_data, src_atlas_w, gx, gy)
                    ox = gc * dst_glyph_w + dx
                    oy = gr * dst_glyph_h + dy
                    set_pixel(dst, dst_atlas_w, ox, oy, p)

    return bytes(dst)


def ensure_atlas_size(data: bytes, expected_bytes: int) -> bytes:
    """Rellena con ceros si data es menor que expected_bytes."""
    if len(data) >= expected_bytes:
        return data[:expected_bytes]
    return data + b"\x00" * (expected_bytes - len(data))


def make_rs_content(size: int) -> str:
    dim = SIZES[size]
    return f'''pub const FONT_ECLIPSE_{size}: ::embedded_graphics::mono_font::MonoFont = ::embedded_graphics::mono_font::MonoFont {{
    image: ::embedded_graphics::image::ImageRaw::new(
        include_bytes!("font_eclipse_{size}.data"),
        {dim["atlas_w"]}u32,
    ),
    glyph_mapping: &::embedded_graphics::mono_font::mapping::ASCII,
    character_size: ::embedded_graphics::geometry::Size::new({dim["w"]}u32, {dim["h"]}u32),
    character_spacing: 0u32,
    baseline: {dim["baseline"]}u32,
    underline: ::embedded_graphics::mono_font::DecorationDimensions::new({dim["underline"]}u32, 1u32),
    strikethrough: ::embedded_graphics::mono_font::DecorationDimensions::new({dim["strikethrough"]}u32, 1u32),
}};
'''


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out-dir", default=None, help="Directorio de salida")
    ap.add_argument("--hex-file", help="Archivo con hex para tamaño 12")
    ap.add_argument("--scale-from", type=int, default=12, help="Escalar desde este tamaño para generar 14-24")
    args = ap.parse_args()

    script_dir = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(script_dir)
    out_dir = args.out_dir or os.path.join(root, "eclipse-apps", "sidewind_sdk", "src")
    os.makedirs(out_dir, exist_ok=True)

    # 1. Generar font_eclipse_12
    hex_data = FONT_12_HEX
    if args.hex_file:
        with open(args.hex_file) as f:
            hex_data = f.read().replace(" ", "").replace("\n", "")

    raw_12 = hex_to_bytes(hex_data)
    dim_12 = SIZES[12]
    atlas_bytes_12 = (dim_12["atlas_w"] * 6 * 12) // 8  # 6 rows of glyphs, 12px each
    font_12_data = ensure_atlas_size(raw_12, atlas_bytes_12)

    path_12_data = os.path.join(out_dir, "font_eclipse_12.data")
    with open(path_12_data, "wb") as f:
        f.write(font_12_data)
    print(f"  Generado: font_eclipse_12.data ({len(font_12_data)} bytes)")

    with open(os.path.join(out_dir, "font_eclipse_12.rs"), "w") as f:
        f.write(make_rs_content(12))
    print(f"  Generado: font_eclipse_12.rs")

    # 2. Escalar para 14, 16, 18, 20, 24
    for sz in [14, 16, 18, 20, 24]:
        dim = SIZES[sz]
        dst_atlas_w = dim["atlas_w"]
        dst_atlas_h = 6 * dim["h"]
        dst_total = (dst_atlas_w * dst_atlas_h + 7) // 8

        scaled = scale_1bpp_atlas(
            font_12_data,
            dim_12["w"],
            dim_12["h"],
            dim["w"],
            dim["h"],
        )
        scaled = ensure_atlas_size(scaled, dst_total)

        path_data = os.path.join(out_dir, f"font_eclipse_{sz}.data")
        with open(path_data, "wb") as f:
            f.write(scaled)
        print(f"  Generado: font_eclipse_{sz}.data ({len(scaled)} bytes, escalado)")

        with open(os.path.join(out_dir, f"font_eclipse_{sz}.rs"), "w") as f:
            f.write(make_rs_content(sz))
        print(f"  Generado: font_eclipse_{sz}.rs")

    print(f"\nListo. Archivos en: {out_dir}")


if __name__ == "__main__":
    main()
