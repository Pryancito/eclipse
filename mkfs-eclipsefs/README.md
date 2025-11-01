# mkfs-eclipsefs - EclipseFS Formatter v2.0

Utilidad para formatear particiones con el filesystem EclipseFS v2.0.

## 🚀 Características

- ✅ Crea filesystems EclipseFS v2.0 completos
- ✅ Magic number correcto ("ECFS")
- ✅ Tabla de inodos inicializada
- ✅ Bitmap de bloques libres
- ✅ Directorio raíz creado
- ✅ UUID único generado
- ✅ Label personalizable
- ✅ Compatible con el kernel Eclipse OS

## 📦 Compilación

```bash
cd mkfs-eclipsefs
cargo build --release
```

Binario generado: `target/release/mkfs-eclipsefs`

## 🎯 Uso

### Formatear un dispositivo
```bash
sudo mkfs-eclipsefs /dev/sdb2
```

### Con opciones personalizadas
```bash
# Con label específico
sudo mkfs-eclipsefs -L "Mi Disco" /dev/sdb2

# Con más inodes
sudo mkfs-eclipsefs -N 50000 /dev/sdb2

# Tamaño de bloque diferente
sudo mkfs-eclipsefs -b 8192 /dev/sdb2

# Forzar sin confirmación
sudo mkfs-eclipsefs -f /dev/sdb2

# Modo verbose
sudo mkfs-eclipsefs -v /dev/sdb2

# Combinación
sudo mkfs-eclipsefs -f -L "Eclipse Root" -N 20000 -b 4096 /dev/sdb2
```

### Formatear una imagen de disco
```bash
# Crear loop device
sudo losetup -fP --show eclipse_os.img
# Supongamos que retorna /dev/loop0

# Formatear partición 2
sudo mkfs-eclipsefs -f -L "Eclipse OS" /dev/loop0p2

# Limpiar
sudo losetup -d /dev/loop0
```

## 📊 Opciones

| Opción | Descripción | Por defecto |
|--------|-------------|-------------|
| `-L, --label` | Label del filesystem | "Eclipse OS" |
| `-b, --block-size` | Tamaño de bloque (bytes) | 4096 |
| `-N, --inodes` | Número de inodes | 10000 |
| `-f, --force` | Forzar sin confirmación | false |
| `-v, --verbose` | Modo verbose | false |

## 🔧 Estructura del Header

```
Offset  | Tamaño | Campo
--------|--------|----------------------------------
0x0000  | 4      | Magic: "ECFS" (0x45434653)
0x0004  | 4      | Version: 0x00020000 (v2.0)
0x0008  | 8      | Timestamp (Unix epoch)
0x0010  | 4      | Block size
0x0014  | 8      | Total blocks
0x001C  | 8      | Inode table offset
0x0024  | 8      | Inode table size
0x002C  | 8      | Data area offset
0x0034  | 8      | Free blocks bitmap offset
0x003C  | 8      | Free inodes bitmap offset
0x0044  | 8      | Total inodes
0x004C  | 8      | Free blocks
0x0054  | 8      | Free inodes
0x0064  | 100    | Label (null-terminated)
0x00C8  | 16     | UUID
0x00D8  | 3680   | Reserved
Total:    4096 bytes (1 bloque)
```

## 📁 Estructura Creada

Después de formatear, el filesystem tiene:

1. **Header** (4KB) - Metadatos del filesystem
2. **Inode Table** - Tabla de todos los inodes
3. **Free Blocks Bitmap** - Bitmap de bloques libres
4. **Free Inodes Bitmap** - Bitmap de inodes libres
5. **Data Area** - Área de datos
6. **Root Directory** (inode 0) - Directorio raíz

## 🧪 Verificación

Para verificar que el formateo fue exitoso:

```bash
# Ver el header
sudo dd if=/dev/sdb2 bs=4096 count=1 2>/dev/null | hexdump -C | head -20

# Debe mostrar:
# 00000000  45 43 46 53 00 00 02 00  ...  ← "ECFS" + version 2.0
```

## 🔗 Integración con build.sh

El script `build.sh` usa automáticamente `mkfs-eclipsefs` si está compilado:

```bash
./build.sh  # Compila mkfs-eclipsefs y lo usa para formatear
```

## 📚 Referencias

- EclipseFS Specification v2.0
- eclipsefs-lib - Librería del filesystem
- Eclipse OS Kernel - Usa este filesystem

## 🏆 Ventajas vs Script Simple

| Característica | Script Python | mkfs-eclipsefs |
|----------------|---------------|----------------|
| Magic number | ✓ | ✓ |
| Header básico | ✓ | ✓ |
| Inode table | ✗ | ✓ |
| Bitmaps | ✗ | ✓ |
| Directorio raíz | ✗ | ✓ |
| UUID único | ✓ | ✓ |
| Verificación | ✗ | ✓ |
| Opciones | ✗ | ✓ |
| Profesional | ✗ | ✓ |

## 🎯 Estado

- ✅ Compilado exitosamente
- ✅ Integrado en build.sh
- ✅ Listo para usar

---

**Versión:** 2.0.0  
**Autor:** Eclipse OS Team  
**Licencia:** MIT  

