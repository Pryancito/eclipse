# 🌙 Eclipse OS - Instalador

Sistema de instalación completo para Eclipse OS que permite instalar el sistema operativo en disco duro.

## 📋 Características

- **Instalación automática** - Proceso completamente automatizado
- **Particionado inteligente** - Crea particiones EFI y root automáticamente
- **Bootloader UEFI** - Instala bootloader personalizado compatible con UEFI
- **Sistema de archivos optimizado** - Configura FAT32 para EFI y EXT4 para root
- **Instalación directa** - No requiere ISO, instala directamente desde el sistema
- **ISO de instalación** - Opción para crear ISO booteable

## 🚀 Instalación Rápida

### Opción 1: Instalación Directa (Recomendado)

```bash
# 1. Compilar kernel y bootloader
cargo build --release --target x86_64-unknown-none --manifest-path eclipse_kernel/Cargo.toml
cd bootloader-uefi && ./build.sh && cd ..

# 2. Instalar Eclipse OS
sudo ./install_eclipse_os.sh /dev/sda
```

### Opción 2: Crear ISO de Instalación

```bash
# 1. Crear ISO de instalación
./build_installer.sh

# 2. Grabar ISO en USB
sudo dd if=eclipse-os-installer.iso of=/dev/sdX bs=4M status=progress

# 3. Arrancar desde USB y seguir las instrucciones
```

### Opción 3: Instalador Interactivo

```bash
# 1. Compilar instalador
cd installer && cargo build --release && cd ..

# 2. Ejecutar instalador interactivo
sudo ./installer/target/release/eclipse-installer
```

## 📁 Estructura del Proyecto

```
eclipse/
├── installer/                    # Instalador en Rust
│   ├── src/
│   │   ├── main.rs              # Instalador principal
│   │   ├── disk_manager.rs      # Gestión de discos
│   │   ├── partition_manager.rs # Gestión de particiones
│   │   ├── bootloader_installer.rs # Instalación de bootloader
│   │   └── filesystem_manager.rs # Gestión de sistema de archivos
│   └── Cargo.toml
├── install_eclipse_os.sh        # Script de instalación directa
├── build_installer.sh           # Script para crear ISO
├── demo_installation.sh         # Demostración de instalación
└── INSTALLER_README.md          # Este archivo
```

## 🔧 Requisitos del Sistema

### Mínimos
- Disco duro con al menos 1GB de espacio libre
- Sistema UEFI compatible
- 512MB de RAM
- Procesador x86_64

### Recomendados
- Disco duro con 2GB+ de espacio libre
- 1GB+ de RAM
- Procesador x86_64 moderno

## 📋 Proceso de Instalación

### 1. Preparación
- Compilar kernel Eclipse
- Compilar bootloader UEFI
- Verificar permisos de root

### 2. Selección de Disco
- Mostrar discos disponibles
- Verificar que el disco no esté montado
- Confirmar selección

### 3. Particionado
- Limpiar tabla de particiones existente
- Crear tabla GPT
- Crear partición EFI (100MB, FAT32)
- Crear partición root (resto del disco, EXT4)

### 4. Instalación de Bootloader
- Montar partición EFI
- Crear estructura de directorios EFI
- Instalar bootloader UEFI
- Configurar archivos de boot

### 5. Instalación de Sistema
- Copiar kernel Eclipse
- Crear archivos de configuración
- Configurar permisos
- Desmontar particiones

## 🛠️ Comandos Disponibles

### Instalación Directa
```bash
# Instalación básica
sudo ./install_eclipse_os.sh /dev/sda

# Instalación automática (sin confirmación)
sudo ./install_eclipse_os.sh --auto /dev/sda

# Ver ayuda
./install_eclipse_os.sh --help
```

### Crear ISO de Instalación
```bash
# Crear ISO completa
./build_installer.sh

# La ISO se creará como: eclipse-os-installer.iso
```

### Instalador Interactivo
```bash
# Compilar instalador
cd installer && cargo build --release

# Ejecutar instalador
sudo ./installer/target/release/eclipse-installer
```

## ⚠️ Advertencias Importantes

### ANTES DE INSTALAR:
- **Haz una copia de seguridad** de tus datos importantes
- **Verifica el disco correcto** - la instalación borrará todos los datos
- **Asegúrate de que UEFI esté habilitado** en tu BIOS
- **Desmonta todas las particiones** del disco de destino

### DURANTE LA INSTALACIÓN:
- **No interrumpas el proceso** una vez iniciado
- **Mantén la alimentación** del sistema
- **No uses el disco** mientras se instala

## 🔧 Resolución de Problemas

### Eclipse OS no arranca
1. Verifica que UEFI esté habilitado en el BIOS
2. Asegúrate de que el disco esté en la lista de arranque
3. Verifica que las particiones se crearon correctamente:
   ```bash
   lsblk /dev/sda
   ```

### Error durante la instalación
1. Verifica que tienes permisos de root
2. Asegúrate de que el disco no esté montado
3. Verifica que el kernel y bootloader se compilaron correctamente

### Problemas con UEFI
1. Verifica que tu sistema soporte UEFI
2. Asegúrate de que Secure Boot esté deshabilitado (si es necesario)
3. Verifica que la partición EFI se creó correctamente

## 📊 Estructura de Particiones

Después de la instalación, el disco tendrá la siguiente estructura:

```
/dev/sda
├── /dev/sda1    # Partición EFI (FAT32, 100MB)
│   ├── /EFI/BOOT/BOOTX64.EFI
│   ├── /EFI/eclipse/eclipse-bootloader.efi
│   ├── /eclipse_kernel
│   └── /boot.conf
└── /dev/sda2    # Partición root (EXT4, resto del disco)
    └── (sistema de archivos root)
```

## 🎯 Características del Instalador

### Instalador Directo (`install_eclipse_os.sh`)
- ✅ Instalación rápida y simple
- ✅ Verificación automática de requisitos
- ✅ Particionado automático
- ✅ Instalación de bootloader UEFI
- ✅ Configuración automática del sistema

### Instalador Interactivo (`eclipse-installer`)
- ✅ Interfaz de usuario amigable
- ✅ Selección de discos con información detallada
- ✅ Confirmación paso a paso
- ✅ Manejo de errores robusto
- ✅ Logging detallado

### ISO de Instalación (`eclipse-os-installer.iso`)
- ✅ Instalación desde USB/DVD
- ✅ Bootloader UEFI integrado
- ✅ Instalador automático
- ✅ Compatible con hardware real

## 🚀 Próximas Características

- [ ] Instalación dual con otros sistemas operativos
- [ ] Configuración de red durante la instalación
- [ ] Instalación de paquetes adicionales
- [ ] Configuración de usuarios
- [ ] Instalación en sistemas BIOS (no UEFI)
- [ ] Verificación de integridad del sistema

## 📝 Licencia

Este proyecto está licenciado bajo la Licencia MIT. Ver el archivo `LICENSE` para más detalles.

## 🤝 Contribuciones

Las contribuciones son bienvenidas. Por favor:

1. Fork el proyecto
2. Crea una rama para tu característica
3. Commit tus cambios
4. Push a la rama
5. Abre un Pull Request

## 📞 Soporte

Si tienes problemas con la instalación:

1. Revisa la sección de resolución de problemas
2. Verifica que cumples todos los requisitos
3. Asegúrate de que el kernel y bootloader se compilaron correctamente
4. Abre un issue en el repositorio

---

**¡Disfruta usando Eclipse OS!** 🌙✨
