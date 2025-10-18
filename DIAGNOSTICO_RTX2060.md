# 🔍 Diagnóstico - NVIDIA RTX 2060 SUPER

## Hardware Detectado
- **GPU**: 2x NVIDIA RTX 2060 SUPER
- **Arquitectura**: Turing (TU104/TU106)
- **Device IDs conocidos**:
  - RTX 2060 SUPER: `0x1F47` (TU106)
  - RTX 2060: `0x1F08` (TU106)
  - RTX 2060: `0x1F15` (TU106)

## Pasos de Diagnóstico

### 1. Verificar compilación de drivers
```bash
# Verificar que los binarios existen
ls -lh cookbook/recipes/core/drivers/target/x86_64-unknown-redox/build/target/release/nvidiad

# Verificar instalación en stage
ls -lh cookbook/recipes/core/drivers/target/x86_64-unknown-redox/stage/usr/lib/drivers/nvidiad
```

### 2. Verificar configuración PCI
```bash
# Ver config de nvidiad
cat cookbook/recipes/core/drivers/source/graphics/nvidiad/config.toml
```

### 3. Ver logs del sistema (en Redox)
```bash
# Logs generales
dmesg | grep -i nvidia
dmesg | grep -i vesa
dmesg | grep -i framebuffer

# Ver qué drivers se cargaron
ps aux | grep -E "(nvidia|vesa|fbcon)"

# Ver esquemas activos
ls -l /scheme/
```

### 4. Verificar variables de entorno
```bash
# En Redox, verificar que UEFI pasó el framebuffer
echo $FRAMEBUFFER_WIDTH
echo $FRAMEBUFFER_HEIGHT
echo $FRAMEBUFFER_ADDR
```

## Posibles Problemas

### A) Driver no compilado
- Verificar que cook drivers terminó correctamente
- Revisar errores de compilación

### B) Config PCI no encuentra RTX 2060 SUPER
- Agregar device ID específico
- Verificar vendor ID (debe ser 0x10DE)

### C) vesad tomó prioridad
- nvidiad puede no haber arrancado
- Verificar orden de carga en pcid.d

### D) Variables FRAMEBUFFER_* no están
- El bootloader no pasó el framebuffer
- Verificar configuración UEFI

## Soluciones Rápidas

### Forzar arranque con vesad (temporal)
Si vesad funciona, el framebuffer UEFI está ok.
Problema: nvidiad no arranca correctamente.

### Verificar en QEMU primero
```bash
make qemu gpu=virtio
# Si arranca, el problema es hardware-específico
```

