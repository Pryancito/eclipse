# EclipseFS (`vendor/eclipsefs-rs`)

Sistema de archivos en disco de Eclipse (formato v0.4), portado desde `eclipsefs-lib` (v0.3.0)
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

## Formato v0.4: copy-on-write (lo que crea `mkfs::format`)

Paso 1 de la paridad con btrfs. Todo vive en árboles B+ copy-on-write
(`btree.rs`), con claves `(objectid, tipo, offset)` como btrfs:

- **Árbol raíz** (`ROOT_ITEM` de cada árbol), **árbol de referencias**
  (contadores ≥ 2, `REF_ITEM`) y **árbol del sistema de archivos** (subvolumen
  5: `INODE_ITEM`, `INODE_REF`, `DIR_ITEM` por hash, `DIR_INDEX` por orden,
  `EXTENT_DATA`, `XATTR`).
- Nodos de 4 KiB con cabecera `EFSN` y CRC32C; hojas con ítems empaquetados
  desde el final.
- **Nada confirmado se sobrescribe**: ni metadatos ni datos. Una escritura va a
  bloques nuevos y el árbol se actualiza para apuntarlos; los bloques viejos se
  liberan al confirmar la transacción. Solo los bloques creados por la
  transacción abierta se reescriben en el sitio.
- **Contadores de referencias perezosos** (Rodeh, como btrfs): copiar un nodo
  compartido incrementa a sus hijos; es la base de snapshots y reflinks.
- Archivos de hasta 1 KiB dentro del árbol (extent en línea).
- **Checksums de datos** (fase 2): árbol de checksums (7) con el CRC32C de
  cada bloque de datos, en ítems de 256 bloques. Toda lectura se verifica y
  un bloque dañado da `Corrupt("data checksum")` (EIO), sin afectar al resto
  del archivo. `EclipseFS::scrub()` lee del disco cada nodo y cada bloque de
  datos y devuelve los dañados; `check()` exige que cada bloque de datos
  tenga checksum. En x86_64 el CRC32C usa la instrucción `crc32` de SSE4.2,
  con ensamblador en línea para que también compile en el target soft-float
  del kernel.
- Confirmación: nodos nuevos → `sync` → superbloque y mapas de bits juntos por
  el journal. Tras un corte de luz se ve el estado viejo o el nuevo entero,
  **contenido de archivos incluido** (sobrescrituras y truncados a mitad de
  bloque también).
- Espacio reservado para metadatos (1/64 del disco), así un disco lleno da
  `ENOSPC` en los datos sin dejar el árbol a medias.
- **Subvolúmenes y snapshots** (fase 3): `create_subvolume`,
  `snapshot(src, dir, nombre, solo_lectura)`, `delete_subvolume`,
  `list_subvolumes`, `set_subvolume_read_only` y `set_default_subvolume`
  (el que se monta como `/`). Cada subvolumen es un árbol con sus propios
  inodos; aparece como un directorio y el árbol raíz guarda un backref
  (`ROOT_BACKREF`) con su nombre. Un snapshot comparte la raíz del árbol
  (cuesta unos pocos nodos) y cada lado copia solo lo que cambia. Como en
  btrfs, los subvolúmenes anidados no entran en el snapshot y no se puede
  enlazar ni renombrar entre subvolúmenes. Los números de inodo de la API son
  *handles*: `id << 40 | inodo`, y el inodo tal cual en el subvolumen 5.
- **Reflinks y dedupe**: `clone_range` (`FICLONERANGE`, también entre
  subvolúmenes) y `dedupe_range` (`FIDEDUPERANGE`, solo si los bytes son
  iguales); los bloques quedan compartidos hasta que uno de los dos escribe.
- **Atributos extendidos** (fase 4): `get_xattr`, `set_xattr` (con
  `XATTR_CREATE`/`XATTR_REPLACE`), `list_xattrs` y `remove_xattr` en los
  espacios `user.`, `trusted.`, `security.` y `system.`; las ACL POSIX se
  guardan como `system.posix_acl_access`/`_default`, igual que en btrfs. Los
  atributos con el mismo hash de nombre comparten un `XATTR_ITEM`, y un ítem
  cabe en una hoja (unos 1,9 KiB; más da `NoSpace`, el E2BIG de Linux).
- **Atributos de inodo** (`chattr`, `get_flags`/`set_flags` con los valores
  `FS_*_FL` de Linux): inmutable (nada cambia, ni nombre ni xattrs),
  append-only (solo escrituras al final), no-COW, compresión, `noatime`,
  `nodump` y `sync`. No-COW, compresión y no-compresión se heredan del
  directorio. Un archivo no-COW se sobrescribe en el sitio y no lleva
  checksums, salvo los bloques que comparte con un snapshot o reflink, que se
  copian; solo se puede cambiar con el archivo vacío.
- **fallocate**: reservar bloques (con o sin `KEEP_SIZE`), `PUNCH_HOLE`
  (libera los bloques enteros y escribe ceros en los bordes) y `ZERO_RANGE`.
  Lo reservado son extents `PREALLOC`, que se leen como ceros hasta que se
  escriben.
- **Compresión** (fase 5): zlib (como btrfs), LZ4 y zstd, en `no_std`
  (`miniz_oxide`, `lz4_flex`, `ruzstd`). Se activa con la opción de montaje
  `set_mount_compression` (`compress=`, no se guarda en disco), con la
  propiedad por archivo `set_compression` (heredada por lo que se crea en un
  directorio) o con `chattr +c` (`FL_COMPR`, zlib). No-COW y `FL_NOCOMP`
  la desactivan. Un extent comprimido guarda hasta 128 KiB de archivo
  (`EXT_COMPRESSED`: algoritmo, bloques en disco, bloques de archivo y el
  trozo que mapea); solo se guarda comprimido si ahorra al menos un bloque.
  Las escrituras de 128 KiB se comprimen al momento; las pequeñas se
  escriben tal cual y se comprimen juntas al confirmar la transacción, así
  un archivo escrito poco a poco también acaba comprimido. Los checksums
  cubren los bloques comprimidos, los snapshots y reflinks comparten extents
  comprimidos (también trozos de ellos) y `compsize` da bytes en disco y sin
  comprimir.
- `EclipseFS::mount` detecta la versión: las imágenes v0.3 siguen montando con
  su driver (`v3.rs`). `mkfs::format_v3` sigue disponible.

## Formato v0.3 (in situ con journal)

Bloques de 4 KiB, todo little-endian:

```text
grupo k (32768 bloques = 128 MiB)
  bloque 0   superbloque (primario en el grupo 0, copia en los demás)
  bloque 1   mapa de bits del grupo (1 = en uso)
  bloque 2.. (grupo 0) journal
  resto      datos, directorios, tabla de inodos, bloques de extents
```

- **Journal de metadatos** (bit `SB_FLAG_JOURNAL` del superbloque, área
  contigua tras el mapa de bits del grupo 0, 1/256 del disco entre 64 y 4096
  bloques). Cada operación se aplica entera o no se aplica tras un corte de
  luz. Los datos de archivo van directos al disco antes que los metadatos que
  los apuntan (modo "ordered" de ext4). Las operaciones se agrupan en una
  transacción hasta `sync`, 64 operaciones o media capacidad del journal; al
  montar se rejuega la última transacción válida (CRC32). Las imágenes v0.3
  sin journal siguen montando y se escriben en el sitio como antes.
- `EclipseFS::check()`: fsck sin reparación (bloques con un único dueño,
  mapas de bits, contadores de enlaces, alcanzabilidad).
- Inodos de 256 bytes con CRC32 en un archivo-tabla mapeado por extents; el
  registro de la tabla vive en el superbloque.
- Seis extents dentro del inodo; el resto en una cadena de bloques con CRC32.
- Asignación con objetivo: los bloques nuevos de un archivo se buscan justo
  detrás de los anteriores, así un archivo secuencial queda en un extent.
- Archivos dispersos, enlaces duros, symlinks cortos dentro del inodo,
  rename con detección de ciclos, compactación de directorios y crecimiento
  automático al tamaño de la partición.

## Próximos pasos (paridad con btrfs)

1. ~~Núcleo CoW: árbol B+, refcounts, datos nunca sobrescritos~~ (v0.4).
2. ~~Checksums de datos y scrub~~ (la reparación llega con DUP/RAID en la fase 7).
3. ~~Subvolúmenes, snapshots rw/ro, reflinks y dedupe~~.
4. ~~xattrs/ACL, atributos (inmutable, append, nocow), fallocate y huecos~~.
5. ~~Compresión zlib, lz4 y zstd~~.
6. send/receive incremental y cuotas.
7. Multidispositivo (DUP, RAID0/1/10), añadir/quitar, balance y encoger.
8. ioctls del kernel e imágenes del instalador con EclipseFS.

## Tests y benchmarks

```sh
cargo test --release -p eclipsefs --features std
```

- Tests de unidad en cada módulo (`crc`, `extent`, `blocks`, `format`,
  `btree`, `space`, `cowfmt`, `v3`), incluido un árbol B+ contra un modelo.
- `tests/cow.rs`: ningún bloque confirmado se reescribe entre dos
  confirmaciones, sobrescrituras de todas las formas, archivos en línea y
  compatibilidad con imágenes v0.3.
- `tests/integration.rs` y `tests/edge_cases.rs`: la API pública de punta a
  punta, límites del formato (extents en línea y en cadena, symlinks en línea,
  cachés), validación de argumentos, contabilidad de espacio y una prueba
  aleatoria contra un modelo en memoria con remontajes.
- `tests/crash.rs`: cortes de luz simulados en cada punto de escritura (y con
  pérdidas arbitrarias de lo no sincronizado); cada imagen debe montar, pasar
  `check()` y mostrar el estado tras alguna operación completa. En v0.4 la
  carga incluye sobrescrituras y truncados sin alinear; en v0.3, solo
  añadidos.
- `tests/csum.rs`: bits podridos en datos y en nodos, detectados al leer y
  por `scrub`; los checksums siguen a sobrescrituras, truncados y borrados.
  Los tests de crash también pasan `scrub` en cada imagen.
- `tests/subvol.rs`: subvolúmenes, snapshots aislados en ambos sentidos,
  snapshots de solo lectura y anidados, subvolumen por defecto, reflinks y
  dedupe. La carga de los tests de crash incluye snapshots, borrado de
  snapshots y reflinks.
- `tests/attrs.rs`: xattrs y ACL, límites de nombres y valores, flags
  inmutable/append/no-COW (incluida la escritura en el sitio y la falta de
  checksums), fallocate en todos sus modos y extents reservados a través de
  snapshots y reflinks. La carga de crash también hace fallocate y xattrs, y
  los xattrs forman parte del estado que se compara.
- `tests/compress.rs`: los tres algoritmos ahorran espacio y se leen sin la
  opción de montaje, datos incompresibles, propiedad por archivo y herencia,
  `chattr +c`/`+m`, escrituras pequeñas y entre confirmaciones, sobrescrituras
  y truncados dentro de un extent, snapshots y reflinks de trozos, bloques
  comprimidos dañados y disco lleno. Dos cargas de crash comprimen todo (zlib
  y zstd), incluida la compactación al confirmar.
- Los tests que dependen del formato corren en las dos versiones (`*_v3`).
- `tests/performance.rs`: las mismas cargas sobre EclipseFS y btrfs, con tiempo
  y bytes leídos/escritos en el dispositivo. `scripts/eclipsefs-bench.sh` las
  ejecuta en release; `ECLIPSEFS_BENCH_SCALE=4` las agranda.
