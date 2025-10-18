# 🦀 Instalador de Redox OS en Disco

## 📍 Ubicación

El instalador completo en Rust se encuentra en:
```
/home/moebius/redox/redox-disk-installer/
```

## 🚀 Uso Rápido

### Opción 1: Script Automatizado

```bash
cd /home/moebius/redox/redox-disk-installer
sudo ./install_to_disk.sh
```

### Opción 2: Manual

```bash
cd /home/moebius/redox/redox-disk-installer
cargo build --release
sudo ./target/release/redox-disk-installer
```

## 📋 Requisitos Previos

1. **Compilar Redox OS primero:**
   ```bash
   cd /home/moebius/redox
   make all
   ```

2. **Instalar dependencias del sistema:**
   ```bash
   # Ubuntu/Debian
   sudo apt install parted dosfstools

   # Fedora
   sudo dnf install parted dosfstools
   ```

## 🎯 Características

✅ **Interfaz Interactiva** - Menú fácil de usar  
✅ **Particionado Automático** - Crea particiones GPT  
✅ **Bootloader UEFI** - Instalación automática  
✅ **RedoxFS Support** - Sistema de archivos nativo de Redox  
✅ **Validación Completa** - Verifica todo antes de instalar  
✅ **Seguro** - Confirmaciones en cada paso importante  

## 📦 Proceso de Instalación

El instalador realiza automáticamente:

1. ✅ Verifica disco y requisitos
2. 📦 Crea particiones GPT (EFI + Root)  
3. 💾 Formatea particiones
4. ⚙️  Instala bootloader UEFI
5. 🔧 Copia kernel de Redox
6. 📂 Instala sistema de archivos
7. ⚙️  Crea configuración de arranque
8. ✅ Verifica instalación

## ⚠️  Advertencias

- **La instalación BORRARÁ todos los datos del disco seleccionado**
- Haz una copia de seguridad antes de continuar
- Asegúrate de seleccionar el disco correcto
- El sistema debe tener UEFI habilitado

## 📖 Documentación Completa

Para más información, consulta:
```
/home/moebius/redox/redox-disk-installer/README.md
```

## 🛠️ Resolución de Problemas

### El instalador no compila
```bash
cd /home/moebius/redox/redox-disk-installer
cargo clean
cargo build --release
```

### Error "Kernel no encontrado"
```bash
cd /home/moebius/redox
make all
```

### El sistema no arranca
1. Verifica que UEFI esté habilitado en el BIOS
2. Asegúrate de que Secure Boot esté deshabilitado
3. Selecciona el disco correcto en el menú de arranque

## 💡 Consejos

- Usa RedoxFS para mejor compatibilidad
- 512 MB es suficiente para la partición EFI
- Prueba primero en una máquina virtual
- Lee el README completo antes de instalar en hardware real

---

**¡Disfruta usando Redox OS!** 🦀✨

