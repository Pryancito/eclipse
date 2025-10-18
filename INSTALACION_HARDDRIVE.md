# Instalación de Redox OS desde harddrive.img

## ✅ Cambios realizados

### 1. Modificación de relibc (getrandom fallback)
- **Archivo**: `/home/moebius/redox/relibc/src/platform/redox/mod.rs`
- **Cambio**: Añadido fallback para `getrandom()` cuando `/scheme/rand` no está disponible
- **Beneficio**: El sistema ya no requiere `randd` para arrancar, usa entropía basada en tiempo como respaldo

### 2. Modificación del instalador
- **Archivo**: `/home/moebius/redox/redox-disk-installer/src/direct_installer.rs`
- **Cambios**:
  - Nueva función `format_efi_partition()`: Formatea solo la partición EFI
  - Nueva función `copy_harddrive_image()`: Copia `harddrive.img` directamente a la partición RedoxFS
  - Simplificada función `create_boot_config()`: Solo crea configuración de arranque
- **Beneficio**: El instalador ahora copia directamente la imagen compilada en lugar de construir el sistema archivo por archivo

## 📋 Proceso de instalación modificado

### Antes (8 pasos):
1. Crear particiones
2. Formatear ambas particiones
3. Montar particiones
4. Instalar bootloader
5. Instalar filesystem (crear directorios)
6. Instalar kernel
7. Crear configuración
8. Desmontar

### Ahora (7 pasos):
1. Crear particiones
2. Formatear partición EFI
3. **Copiar harddrive.img a partición RedoxFS** ⭐
4. Montar particiones
5. Instalar bootloader en EFI
6. Instalar kernel en EFI
7. Crear configuración de arranque

## 🚀 Cómo usar

### Opción 1: Instalación directa a disco
```bash
cd /home/moebius/redox/redox-disk-installer
sudo ./target/release/redox-disk-installer /dev/sda
```

### Opción 2: Probar en QEMU primero
```bash
cd /home/moebius/redox
./run_harddrive.sh
```

## 📂 Archivos importantes

- **harddrive.img**: `/home/moebius/redox/build/x86_64/desktop/harddrive.img` (650 MB)
- **Instalador**: `/home/moebius/redox/redox-disk-installer/target/release/redox-disk-installer`
- **Script QEMU**: `/home/moebius/redox/run_harddrive.sh`

## ⚙️ Configuración de arranque

El instalador crea:
- `/boot/kernel` - Kernel de Redox
- `/boot/initfs` - Sistema de archivos inicial
- `/boot/redox.conf` - Configuración de arranque
- `/EFI/BOOT/BOOTX64.EFI` - Bootloader UEFI
- `/startup.nsh` - Script de arranque automático UEFI

## 🔍 Verificación

Después de instalar, el sistema debería:
✅ Arrancar sin error `from_entropy failed`
✅ Arrancar sin error `ipcd failed to daemonize`
✅ Usar fallback de entropía basado en tiempo si `randd` no está presente
✅ Tener todos los archivos del `harddrive.img` disponibles en RedoxFS

## 📝 Notas

- La imagen `harddrive.img` contiene el sistema completo con `relibc` modificado
- No es necesario recompilar nada más, solo ejecutar el instalador
- La partición RedoxFS contiene una copia exacta de `harddrive.img`

