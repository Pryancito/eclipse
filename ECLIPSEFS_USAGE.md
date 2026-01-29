# Uso de EclipseFS con FUSE

## Problema Resuelto

El sistema de archivos EclipseFS ahora se puebla correctamente con archivos durante la construcción de la imagen. Previamente, `/sbin/` y otros directorios aparecían vacíos al montar con FUSE.

## Construcción de la Imagen

Para construir la imagen de Eclipse OS con el filesystem poblado:

```bash
./build.sh image
```

Esto:
1. Compila todos los componentes (kernel, bootloader, systemd, userland, etc.)
2. Compila las herramientas `mkfs-eclipsefs` y `populate-eclipsefs`
3. Crea la imagen `eclipse-os.img` con dos particiones:
   - Partición 1: ESP (FAT32) con bootloader y kernel
   - Partición 2: EclipseFS poblado con el sistema de archivos completo

## Montaje con FUSE

Para montar y explorar el filesystem EclipseFS:

```bash
# Montar la partición EclipseFS
sudo ./eclipsefs-fuse/target/debug/eclipsefs-fuse /dev/sda2 /mnt/

# Ver contenido de directorios
sudo ls -la /mnt/
sudo ls -la /mnt/sbin/
sudo ls -la /mnt/usr/sbin/
sudo ls -la /mnt/bin/

# Ver información del filesystem
sudo ./eclipsefs-cli/target/release/eclipsefs info /dev/sda2

# Listar contenido de un directorio
sudo ./eclipsefs-cli/target/release/eclipsefs ls /dev/sda2 /sbin

# Ver árbol completo del filesystem
sudo ./eclipsefs-cli/target/release/eclipsefs tree /dev/sda2

# Desmontar
sudo umount /mnt/
```

## Estructura del Filesystem

El filesystem EclipseFS poblado contiene:

```
/
├── bin/              # Binarios del sistema
├── sbin/             # Binarios de sistema (incluyendo eclipse-systemd)
├── usr/
│   ├── bin/          # Binarios de usuario
│   ├── sbin/         # Binarios de sistema (incluyendo eclipse-systemd)
│   └── lib/          # Bibliotecas compartidas
├── etc/              # Configuración del sistema
├── var/              # Datos variables
├── tmp/              # Archivos temporales
├── home/             # Directorios de usuarios
├── root/             # Directorio del superusuario
├── dev/              # Dispositivos (vacío, se puebla en tiempo de ejecución)
├── proc/             # Sistema de archivos de procesos (vacío)
└── sys/              # Sistema de archivos del sistema (vacío)
```

## Herramientas Incluidas

### mkfs-eclipsefs
Formatea un dispositivo o imagen con el filesystem EclipseFS:
```bash
sudo mkfs-eclipsefs -f -L "Eclipse OS" -N 10000 /dev/sda2
```

### populate-eclipsefs
Puebla un filesystem EclipseFS ya formateado con archivos desde un directorio:
```bash
sudo populate-eclipsefs /dev/sda2 /path/to/source/dir
```

### eclipsefs-cli
Herramienta de línea de comandos para interactuar con filesystems EclipseFS:
```bash
# Ver información
eclipsefs info /dev/sda2

# Listar directorio
eclipsefs ls /dev/sda2 /sbin

# Ver contenido de archivo
eclipsefs cat /dev/sda2 /etc/hostname

# Ver árbol completo
eclipsefs tree /dev/sda2
```

### eclipsefs-fuse
Driver FUSE para montar EclipseFS en Linux:
```bash
sudo eclipsefs-fuse /dev/sda2 /mnt
```

## Solución de Problemas

### El directorio aparece vacío después de montar

Asegúrate de haber construido la imagen con `./build.sh image` y que `populate-eclipsefs` se haya ejecutado correctamente. Verifica los logs de construcción para mensajes como:

```
✓ Filesystem EclipseFS poblado exitosamente
```

### Error de permisos al montar

FUSE requiere permisos de root para acceder a dispositivos de bloque:
```bash
sudo eclipsefs-fuse /dev/sda2 /mnt/
```

### Ver el contenido sin montar

Usa `eclipsefs-cli` para ver el contenido sin necesidad de montar:
```bash
sudo eclipsefs ls /dev/sda2 /
sudo eclipsefs tree /dev/sda2
```

## Integración en el Instalador

El instalador de Eclipse OS (`installer/`) utilizará automáticamente estas herramientas para:
1. Formatear la partición de destino con `mkfs-eclipsefs`
2. Poblar el filesystem con `populate-eclipsefs`
3. Copiar el bootloader y kernel a la partición ESP

No se requiere intervención manual al usar el instalador.
