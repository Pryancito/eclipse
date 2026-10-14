# EclipseFS (`vendor/eclipsefs-rs`)

Sistema de archivos en disco de Eclipse, portado desde `eclipsefs-lib` (v0.3.0)
de [eclipse-old](https://github.com/Pryancito/eclipse-old/tree/main/eclipsefs-lib)
para el kernel nuevo. Es `no_std` + `alloc` y expone **la misma interfaz que
`vendor/btrfs-rs`**, de modo que el kernel y `xtask` pueden usar uno u otro
detrás del mismo adaptador.

## Interfaz (idéntica a `btrfs`)

| `btrfs`                         | `eclipsefs`                        |
|---------------------------------|------------------------------------|
| `BlockDevice` (`read_at`, `write_at`, `sync`, `size`) | igual |
| `device::FileDevice` (feature `std`) | igual |
| `Btrfs::mount(dev, read_only)`  | `EclipseFS::mount(dev, read_only)` |
| `set_clock`, `grow_to_device`, `sync`, `root_ino`, `fsinfo`, `label` | igual |
| `stat`, `set_attr`, `lookup`, `readdir`, `parent` | igual |
| `create`, `symlink`, `write_symlink`, `read_link`, `link`, `unlink`, `rename` | igual |
| `read`, `write` (parcial con disco lleno), `truncate` | igual |
| `mkfs::format(dev, &MkfsOptions)` | igual (mismos campos) |
| `Error`, `FileKind`, `InodeStat`, `DirEntry`, `FsStat` | mismos variantes y campos |

En el kernel, `linux-object/src/fs/eclipsefs_mount.rs` lo monta con
`mount -t eclipsefs`, y la búsqueda de la raíz lo reconoce si el disco no es
btrfs. btrfs sigue siendo el sistema por defecto.

## Qué viene de eclipsefs-lib y qué cambia

`eclipsefs-lib` guardaba la imagen entera en memoria y la escribía de una vez
(cabecera + tabla de inodos + registros TLV con el contenido dentro); bajo
`no_std` estaba limitada a 16 extents por archivo y 4096 bloques en total. No
podía modificarse en el sitio, que es lo que necesita una raíz montada.

Se conserva: la firma `ECLIPSEFS`, CRC32, el inodo raíz 1, los tipos de nodo
1/2/3, los **extents** (`extent.rs`) y los **grupos de asignación** con mapa de
bits (`blocks.rs`), ahora sin límites y persistidos en disco.

Formato v0.3 (bloques de 4 KiB, todo little-endian):

```text
grupo k (32768 bloques = 128 MiB)
  bloque 0   superbloque (primario en el grupo 0, copia en los demás)
  bloque 1   mapa de bits del grupo (1 = en uso)
  bloque 2.. datos, directorios, tabla de inodos, bloques de extents
```

- Inodos de 256 bytes con CRC32 en un archivo-tabla mapeado por extents; el
  registro de la tabla vive en el superbloque.
- Seis extents dentro del inodo; el resto en una cadena de bloques con CRC32.
- Asignación con objetivo: los bloques nuevos de un archivo se buscan justo
  detrás de los anteriores, así un archivo secuencial queda en un extent.
- Archivos dispersos, enlaces duros, symlinks cortos dentro del inodo,
  rename con detección de ciclos, compactación de directorios y crecimiento
  automático al tamaño de la partición.

## Próximos pasos

Journal (consistencia ante cortes), copy-on-write y snapshots, compresión,
checksums de datos y directorios indexados: las piezas de v0.2/v0.3 que aún no
están portadas.

## Tests y benchmarks

```sh
cargo test --release -p eclipsefs --features std
```

- Tests de unidad en cada módulo (`crc`, `extent`, `blocks`, `format`, `fs`).
- `tests/integration.rs` y `tests/edge_cases.rs`: la API pública de punta a
  punta, límites del formato (extents en línea y en cadena, symlinks en línea,
  cachés), validación de argumentos, contabilidad de espacio y una prueba
  aleatoria contra un modelo en memoria con remontajes.
- `tests/performance.rs`: las mismas cargas sobre EclipseFS y btrfs, con tiempo
  y bytes leídos/escritos en el dispositivo. `scripts/eclipsefs-bench.sh` las
  ejecuta en release; `ECLIPSEFS_BENCH_SCALE=4` las agranda.
