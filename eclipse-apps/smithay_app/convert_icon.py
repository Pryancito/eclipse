import sys
from PIL import Image

def convert_to_raw(input_path, output_path, size=(64, 64)):
    img = Image.open(input_path).convert("RGBA")
    
    # Recortar al contenido (usando el canal alfa si lo tiene, o intensidad)
    # Si la imagen viene con fondo negro (no alfa), creamos uno.
    # Pero usualmente DALL-E devuelve fondo opaco.
    
    # Para imágenes con fondo negro, buscamos el bounding box de píxeles no negros
    gray = img.convert("L")
    bbox = gray.getbbox()
    if bbox:
        img = img.crop(bbox)
        print(f"[CONVERT] Cropped to {bbox}")
    
    # Redimensionar al tamaño deseado manteniendo el aspecto?
    # De momento redimensionar directamente
    img = img.resize(size, Image.Resampling.LANCZOS)
    
    # Asegurar fondo negro puro para los píxeles con poco color
    # (Opcional, pero ayuda a la transparencia key en Rust)
    final = img.convert("RGB")
    
    with open(output_path, "wb") as f:
        f.write(final.tobytes())

if __name__ == "__main__":
    if len(sys.argv) < 3:
        print("Usage: python3 convert.py <input> <output> [width] [height]")
        sys.exit(1)
    
    width = int(sys.argv[3]) if len(sys.argv) > 3 else 64
    height = int(sys.argv[4]) if len(sys.argv) > 4 else 64
    convert_to_raw(sys.argv[1], sys.argv[2], size=(width, height))
