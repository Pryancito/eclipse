# 🔧 Solución para RTX 2060 SUPER sin sudo

## 🎯 Problema

`vesad` se carga desde el initfs y toma el framebuffer antes que `nvidiad`.

## ✅ Solución: Desactivar vesad para NVIDIA

### Opción 1: Modificar init.rc (Recomendado)

Edita el archivo que controla qué driver de gráficos se carga:

```bash
# Archivo: cookbook/recipes/core/base-initfs/init.rc
# Línea ~17: vesad

# Cambiar de:
vesad

# A (condicional):
# Solo cargar vesad si NO hay GPU NVIDIA/AMD/Intel
# vesad se ejecutará como fallback
```

### Opción 2: Crear script de detección inteligente

Crear un script que detecte NVIDIA antes de lanzar vesad.

### Opción 3: Compilar base-initfs con el fix

**NECESITA sudo** para podman, pero solo una vez:

```bash
sudo -v  # Validar sudo
cd /home/moebius/redox
make r.base-initfs
make r.drivers-initfs  
make image
```

## 🎯 Fix Rápido (Lo que voy a hacer)

Voy a crear un initfs modificado que:
1. Detecta si hay NVIDIA/AMD/Intel
2. Si las encuentra, usa nvidiad/amdd/inteld
3. Si no, usa vesad como fallback


