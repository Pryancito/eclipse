#!/usr/bin/env python3
"""
Convierte iconos JPEG/PNG a raw RGB888 (.bin) para Eclipse OS.
Iconos separados: sistema.jpg, archivos.jpg, aplicaciones.jpg, red.jpg
iconos-eclipse-2 = 1x3 (close, min, max)
logo-eclipse = logo completo
"""
from pathlib import Path
from typing import Optional
from PIL import Image

SRC = Path("/home/moebius/Imágenes")
# Fuentes alternativas para iconos de ventana (minimizar, maximizar, cerrar)
SRC_ALT = [Path("/home/moebius/Documentos"), Path("/home/moebius/Imagenes")]
DST = Path(__file__).resolve().parent.parent / "eclipse-apps" / "sidewind_sdk" / "assets"

# (x, y, w, h) en px, o None = imagen completa resize
LAYOUTS = {
    # Iconos separados (uno por archivo)
    "sistema.jpg": [(None, "system.bin")],
    "archivos.jpg": [(None, "files.bin")],
    "aplicaciones.jpg": [(None, "apps.bin")],
    "red.jpg": [(None, "network.bin")],
    # Fallback: imagen combinada 1x3 (close, min, max) - se usa si no hay archivos individuales
    "iconos-eclipse-2.jpg": [
        ((0, 0, 341, 1024), "btn_close.bin"),
        ((341, 0, 342, 1024), "btn_min.bin"),
        ((682, 0, 342, 1024), "btn_max.bin"),
    ],
    # Iconos de barra de título: archivos individuales (se buscan en SRC o SRC_ALT)
    # Procesados después para sobrescribir el fallback
    "minimizar.jpg": [(None, "btn_min.bin")],
    "maximizar.jpg": [(None, "btn_max.bin")],
    "cerrar.jpg": [(None, "btn_close.bin")],
    # Logo: recorte centrado 600x600, negro transparente (color-key en UI)
    # IMPORTANTE: el motivo (S/logo) debe estar centrado en la imagen fuente
    # para que se vea bien en pantalla. Si aparece descentrado, ajustar
    # LOGO_SRC_OFFSET_X/Y en sidewind_sdk/src/ui.rs
    "logo-eclipse.jpg": [
        ((150, 150, 724, 724), "logo.bin", 600),  # centro 574x574 -> resize 600x600
    ],
}

# Umbral para transparencia: píxeles con r,g,b < THRESH se fuerzan a negro (0,0,0)
# La UI trata negro como transparente (color-key). Umbral 24 coincide con ui.rs TRANSPARENT_THRESH.
BLACK_THRESH = 24

def to_rgb888(img: Image.Image, size: tuple[int, int], black_to_transparent: bool = True) -> bytes:
    """Redimensiona y devuelve raw RGB888.
    Si black_to_transparent: píxeles con r,g,b < BLACK_THRESH se fuerzan a (0,0,0)
    para que la UI los trate como transparentes (color-key).
    """
    if img.mode in ("RGBA", "LA", "P"):
        img = img.convert("RGBA")
        resized = img.resize(size, Image.Resampling.LANCZOS)
    else:
        img = img.convert("RGB")
        resized = img.resize(size, Image.Resampling.LANCZOS)

    out = bytearray(size[0] * size[1] * 3)
    pix = resized.load()
    for y in range(size[1]):
        for x in range(size[0]):
            p = pix[x, y]
            if len(p) == 4:
                r, g, b, a = p
                if a < 128:
                    r, g, b = 0, 0, 0
            else:
                r, g, b = p
            if black_to_transparent and r < BLACK_THRESH and g < BLACK_THRESH and b < BLACK_THRESH:
                r, g, b = 0, 0, 0
            i = (y * size[0] + x) * 3
            out[i], out[i + 1], out[i + 2] = r, g, b
    return bytes(out)

def find_source(src_name: str) -> Optional[Path]:
    """Busca el archivo en SRC o en las carpetas alternativas."""
    candidate = SRC / src_name
    if candidate.exists():
        return candidate
    for alt in SRC_ALT:
        candidate = alt / src_name
        if candidate.exists():
            return candidate
    return None

def main():
    DST.mkdir(parents=True, exist_ok=True)
    for src_name, items in LAYOUTS.items():
        src_path = find_source(src_name)
        if src_path is None:
            print(f"Omitiendo {src_name} (no existe en SRC ni en alternativas)")
            continue
        img = Image.open(src_path)
        for item in items:
            spec = item[0]
            out_name = item[1]
            size = (item[2], item[2]) if len(item) == 3 else ((64, 64) if "btn_" not in out_name else (20, 20))
            if spec is None:
                region = img
            else:
                x, y, w, h = spec
                region = img.crop((x, y, x + w, y + h))
            data = to_rgb888(region, size)
            out_path = DST / out_name
            out_path.write_bytes(data)
            print(f"  {out_name} <- {src_name}")

if __name__ == "__main__":
    main()
    print(f"\nIconos en: {DST}")
