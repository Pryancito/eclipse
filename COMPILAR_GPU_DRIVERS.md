# 🚀 Guía de Compilación - Drivers Multi-GPU

## Compilación Rápida

```bash
cd /home/moebius/redox

# Compilar todo el sistema (incluye drivers GPU)
make all

# O solo los drivers
cd cookbook
./cook.sh drivers
```

## Verificación Post-Compilación

### 1. Verificar Binarios Compilados

```bash
# Drivers principales
ls -lh cookbook/recipes/core/drivers/target/x86_64-unknown-redox/release/nvidiad
ls -lh cookbook/recipes/core/drivers/target/x86_64-unknown-redox/release/amdd
ls -lh cookbook/recipes/core/drivers/target/x86_64-unknown-redox/release/inteld
ls -lh cookbook/recipes/core/drivers/target/x86_64-unknown-redox/release/multi-gpud
```

### 2. Verificar Instalación en Stage

```bash
# Binarios instalados
ls -lh cookbook/recipes/core/drivers/target/x86_64-unknown-redox/stage/usr/lib/drivers/
ls -lh cookbook/recipes/core/drivers/target/x86_64-unknown-redox/stage/usr/bin/multi-gpud

# Configuraciones PCI
ls -lh cookbook/recipes/core/drivers/target/x86_64-unknown-redox/stage/etc/pcid.d/
```

### 3. Verificar en la Imagen del Sistema

```bash
# Montar la imagen compilada (ejemplo)
sudo mount -o loop build/x86_64/desktop/harddrive.img /mnt/redox

# Verificar drivers
ls -l /mnt/redox/usr/lib/drivers/{nvidiad,amdd,inteld}
ls -l /mnt/redox/usr/bin/multi-gpud

# Verificar configuraciones
ls -l /mnt/redox/etc/pcid.d/

# Desmontar
sudo umount /mnt/redox
```

## Compilación Selectiva

### Solo los Drivers de GPU

```bash
cd cookbook/recipes/core/drivers/source

# Compilar individualmente
cargo build --release -p nvidiad
cargo build --release -p amdd
cargo build --release -p inteld
cargo build --release -p multi-gpud
```

### Verificar Dependencias

```bash
cd cookbook/recipes/core/drivers/source

# Ver el workspace completo
cargo tree -p nvidiad
cargo tree -p amdd
cargo tree -p inteld
cargo tree -p multi-gpud
```

## Errores Comunes

### Error: "no se encuentra pcid_interface"

**Solución**: Compilar desde el workspace completo:
```bash
cd cookbook/recipes/core/drivers/source
cargo build --release
```

### Error: "no se encuentra driver-graphics"

**Solución**: Asegurarse de que todos los módulos estén en el workspace:
```bash
# Verificar Cargo.toml
grep -A 5 "graphics" cookbook/recipes/core/drivers/source/Cargo.toml
```

### Error al copiar archivos de configuración

**Solución**: Verificar que existen los archivos config.toml:
```bash
find cookbook/recipes/core/drivers/source/graphics -name "config.toml"
```

## Integración con el Sistema

### 1. Compilar Sistema Completo

```bash
cd /home/moebius/redox

# Limpiar compilación anterior (opcional)
make clean

# Compilar todo
make all

# O configuración específica
make all CONFIG=desktop
```

### 2. Generar Imagen de Disco

```bash
# Crear imagen harddrive
make build/x86_64/desktop/harddrive.img

# O live ISO
make build/x86_64/desktop/livedisk.iso
```

### 3. Probar en QEMU

```bash
# Con una GPU (usando vesad por defecto)
make qemu

# Con GPU virtual
make qemu gpu=virtio

# Con múltiples GPUs virtuales
make qemu gpu=multi
```

## Instalación Manual (Desarrollo)

Si compilaste los drivers y quieres copiarlos manualmente al sistema:

```bash
# Copiar binarios
sudo cp cookbook/recipes/core/drivers/target/x86_64-unknown-redox/release/nvidiad \
    /mnt/redox/usr/lib/drivers/

sudo cp cookbook/recipes/core/drivers/target/x86_64-unknown-redox/release/amdd \
    /mnt/redox/usr/lib/drivers/

sudo cp cookbook/recipes/core/drivers/target/x86_64-unknown-redox/release/inteld \
    /mnt/redox/usr/lib/drivers/

sudo cp cookbook/recipes/core/drivers/target/x86_64-unknown-redox/release/multi-gpud \
    /mnt/redox/usr/bin/

# Copiar configuraciones
sudo cp cookbook/recipes/core/drivers/source/graphics/nvidiad/config.toml \
    /mnt/redox/etc/pcid.d/nvidiad.toml

sudo cp cookbook/recipes/core/drivers/source/graphics/amdd/config.toml \
    /mnt/redox/etc/pcid.d/amdd.toml

sudo cp cookbook/recipes/core/drivers/source/graphics/inteld/config.toml \
    /mnt/redox/etc/pcid.d/inteld.toml
```

## Testing

### En QEMU (Desarrollo)

```bash
# Iniciar con gráficos virtuales
make qemu gpu=virtio

# En Redox (una vez booteado):
# Ver logs
dmesg | grep -i gpu

# Verificar drivers cargados
ps aux | grep -E "(nvidia|amd|intel|multi-gpu)d"
```

### En Hardware Real

1. Compilar imagen de disco completa
2. Instalar en USB o disco duro
3. Bootear sistema
4. Verificar detección de GPUs:

```bash
# Ver dispositivos PCI
lspci | grep -i vga

# Ver configuración multi-GPU
cat /etc/multigpu.conf

# Ver logs de drivers
dmesg | tail -n 100
```

## Optimización de Compilación

### Compilación Rápida (Debug)

```bash
cd cookbook/recipes/core/drivers/source
cargo build  # Sin --release, más rápido
```

### Compilación Optimizada (Release)

```bash
cargo build --release  # Optimizado, más lento
```

### Compilación Paralela

```bash
# Usar todos los cores
make -j$(nproc)

# O número específico
make -j8
```

## Logs de Compilación

Los logs se guardan en:
```bash
# Ver último log de compilación
tail -f cookbook/recipes/core/drivers/target/x86_64-unknown-redox/release/build/*/output
```

## Siguiente Paso: Instalación

Una vez compilado, sigue la guía de instalación:
- Para sistema completo: Usa el instalador en `redox-disk-installer/`
- Para testing: Usa QEMU con `make qemu`

## Referencias

- **Código fuente**: `cookbook/recipes/core/drivers/source/graphics/`
- **Receta**: `cookbook/recipes/core/drivers/recipe.toml`
- **Documentación**: `SISTEMA_MULTI_GPU.md`

¡Los drivers están listos para compilar! 🚀

