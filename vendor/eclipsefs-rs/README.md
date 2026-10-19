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
- **send/receive** (fase 6): `send(snap, padre, salida)` escribe un
  snapshot de solo lectura como flujo (`EFSSEND1`: cabecera y comandos
  con CRC32C cada uno), entero o, con un snapshot padre, solo los cambios:
  `Store::diff` salta los subárboles que ambos comparten y los datos se
  afinan bloque a bloque. Los reflinks dentro del snapshot viajan como
  `CLONE`, y se envían xattrs, flags, huecos y extents reservados.
  `receive(dir, entrada)` construye un subvolumen oculto, lo pasa por el
  fsck y solo entonces lo nombra, de solo lectura y con `received_uuid`; un
  flujo cortado o dañado no deja nada (y un corte de luz a mitad se limpia
  al montar). Un incremental exige que el padre se haya recibido antes
  (por su UUID), como en btrfs.
- **Cuotas** (fase 6): `quota_enable`/`quota_disable`/`quota_rescan`
  (bit `FEAT_QUOTA` del superbloque). Con las cuotas activas cada
  subvolumen lleva en su `ROOT_ITEM` los bytes de archivo que referencia,
  al día en cada transacción y comprobado por el fsck.
  `set_qgroup_limit(id, límite)` hace que las escrituras, `fallocate` y
  reflinks que lo superarían fallen con `NoSpace` (EDQUOT); reescribir lo
  que ya existe siempre se puede. `qgroup_show` da, por subvolumen, el
  contador, el límite, los bytes en disco que referencia y los exclusivos
  (que ningún otro subvolumen comparte). Un snapshot empieza con el mismo
  contador y sin límite.
- **Varios dispositivos** (fase 7, bit `FEAT_VOLUME`, `volume.rs`): el
  espacio lógico se reparte en *chunks* (los grupos de 128 MiB del
  asignador), cada uno de datos, de metadatos o mixto, con su perfil:
  `single`, `dup`, `raid0`, `raid1`, `raid10`, `raid1c3` y `raid1c4`
  (y `raid5`/`raid6`, ver «Mejoras tras la paridad»). El bloque 0 de cada
  dispositivo lleva una copia del superbloque con su id y el mapeo del
  grupo 0, que guarda el journal y el mapa de chunks (con CRC32C), así
  que cualquier dispositivo basta para encontrar el resto. Los chunks se
  crean al hacer falta (de un octavo del dispositivo más pequeño, hasta
  128 MiB).
  - `mkfs::format_devices(devs, opts, datos, metadatos)`,
    `EclipseFS::mount_devices` y `mount_degraded` (monta si cada bloque
    conserva una copia; un dispositivo que vuelve tras perderse commits se
    resincroniza al montar).
  - Al leer, una copia que no cuadra con su checksum (o con la generación
    del nodo) se sustituye por otra y se reescribe; `scrub` lee todas las
    copias y repara las malas (`ScrubReport::repaired`).
  - `add_device`, `remove_device` (también uno perdido), `replace_device`
    y `balance(datos, metadatos)` (convierte perfiles, libera chunks vacíos
    y reparte entre dispositivos). Mover un chunk copia sus bloques en uso
    a sitio nuevo, conserva las copias que pueden quedarse y no reutiliza
    el sitio viejo hasta que la confirmación está en disco: un corte de luz
    deja una disposición u otra entera.
  - `devices()`, `chunks()` y `profiles()` (como `btrfs fi usage`).
  - Un sistema de archivos de un dispositivo se convierte en volumen al
    primer `add_device` o `balance`, sin mover nada; sus grupos quedan
    mixtos y siguen el perfil de metadatos. El grupo 0 nunca va en
    franjas (con `raid0`/`raid10` va en `raid1`). Los mapas de bits del
    asignador no llevan checksum, así que de ellos se lee la primera copia.
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
6. ~~send/receive incremental y cuotas~~.
7. ~~Multidispositivo (DUP, RAID0/1/10/1C3/1C4), añadir/quitar/reemplazar y balance~~.
8. ~~ioctls del kernel e imágenes del instalador con EclipseFS~~.

Con esto EclipseFS tiene todo lo de btrfs. Lo siguiente ya es propio.

## Mejoras tras la paridad

- **Lecturas más rápidas**: solo se ponen a cero los huecos entre extents y
  el inodo se busca una vez por lectura. En lectura aleatoria de 4 KiB
  EclipseFS pasa de ~20 % más lento que btrfs a más rápido (callgrind:
  142 M instrucciones frente a 189 M).
- **FIEMAP y SEEK_DATA/SEEK_HOLE**: `fiemap(ino, inicio, largo)` da los
  extents (`FileMapping`, con las banderas de Linux: en línea, reservado,
  comprimido, compartido, último) y `seek_data`/`seek_hole` saltan huecos
  y espacio reservado. En el kernel: `FS_IOC_FIEMAP` (`filefrag`) y
  `lseek(SEEK_DATA/SEEK_HOLE)`, con una respuesta genérica (todo datos) en
  los demás sistemas de archivos.
- **Desfragmentar y recomprimir** (`btrfs filesystem defragment [-c]`):
  `defrag(ino, inicio, largo, umbral, compresión)` junta en un solo extent
  contiguo cada racha de extents pequeños que no están seguidos en disco, o
  los reescribe comprimidos. Huecos y espacio reservado se quedan; lo
  reescrito deja de compartirse, como en btrfs. Kernel:
  `BTRFS_IOC_DEFRAG` y `BTRFS_IOC_DEFRAG_RANGE`.
- **Redimensionar en caliente** (`btrfs filesystem resize`):
  `resize(devid, tamaño)` crece hasta un tamaño o hasta el dispositivo
  (`None`), y **encoge**: primero vacía lo que se corta. El asignador deja
  de dar bloques de esa zona, los nodos se copian de arriba abajo
  (`Store::relocate`, así un nodo compartido con un snapshot sigue
  compartido debajo) y cada bloque de datos se copia una sola vez y todas
  sus referencias (snapshots, reflinks, extents comprimidos) pasan a la
  copia. En un volumen, los chunks que quedan fuera se mueven dentro si
  caben y si no se vacían y se eliminan. Un tamaño puesto a mano se
  mantiene (`FEAT_FIXED_SIZE`: el montaje ya no crece hasta el
  dispositivo) hasta un `resize` a `max`. Kernel: `BTRFS_IOC_RESIZE`
  (`[devid:][+|-]tamaño[kmgtpe]` o `max`).
- **Checksums de los mapas de bits del asignador** (`FEAT_BITMAP_CSUM`):
  el bloque de bitmap de cada grupo no tiene sitio para un checksum, así
  que el CRC32C de cada uno va en una tabla del grupo 0 a la que apunta el
  superbloque, escrita por el journal junto con los bitmaps. Al montar se
  comprueba cada bitmap; uno que no cuadra (o un bloque de la tabla dañado)
  se toma de la otra copia en un volumen espejado, o se reconstruye desde
  los árboles (cabeceras de grupo, journal, mapa de chunks, la tabla,
  nodos y extents), así un bitmap dañado nunca entrega un bloque en uso.
  En solo lectura la reparación queda en memoria. Los sistemas de archivos
  creados antes reciben la tabla en su siguiente commit, y fsck comprueba
  la tabla frente a cada bitmap.
- **CRC32C en tres flujos**: con la instrucción `crc32`, cada bloque se
  reparte en tres trozos que se calculan a la vez y se juntan con
  multiplicaciones módulo el polinomio (constantes calculadas en tiempo de
  compilación). Un bloque de 4 KiB pasa de 524 ns a 246 ns, y la lectura
  aleatoria queda a la par con btrfs, que no comprueba los checksums de
  datos al leer.
- **Cifrado por directorio** (`FEAT_ENCRYPT`, como fscrypt v2 en ext4 y
  f2fs): `set_encryption_policy(dir, &EncryptionPolicy::new(id))` cifra un
  directorio vacío y todo lo que se cree debajo hereda la política con un
  nonce propio. Contenido en AES-256-XTS (el tweak es el número de bloque),
  nombres en AES-256-CTS-CBC rellenos a 32 bytes, y claves por archivo con
  HKDF-SHA512 de la clave maestra y el nonce, con el mismo formato de
  contexto que Linux (`fscrypt_context_v2`, item `ENC_CONTEXT`). Las claves
  maestras viven solo en memoria (`add_encryption_key` devuelve su
  identificador, `remove_encryption_key` las olvida con las derivadas).
  Sin la clave, el directorio se lista con nombres sin clave (base64url del
  cifrado, con SHA-256 de la cola en los nombres largos) que sirven para
  buscar y borrar; leer, escribir o crear da `NoKey` (`ENOKEY`). Los
  archivos cifrados nunca van en línea ni comprimidos, los symlinks guardan
  el destino cifrado, y reflink, dedupe y send se niegan (compartir bloques
  entre claves distintas o enviar texto claro); un subvolumen no puede ir
  dentro de un directorio cifrado. fsck y scrub funcionan sin la clave.
  `aes` va en `vendor/aes` con AES-NI solo si el objetivo tiene SSE2,
  porque el kernel se compila sin coma flotante.
- **RAID5 y RAID6 sin agujero de escritura** (`Profile::Raid5`/`Raid6`,
  bit `FEAT_RAID56`, `raid56.rs`): cada fila tiene un bloque de 4 KiB por
  dispositivo, `k` de datos más P (XOR) y, en RAID6, Q (Reed-Solomon en
  GF(2^8), generador 2 y polinomio 0x11d como md y btrfs, con tablas y sin
  SIMD porque el kernel es soft-float). La paridad rota de un dispositivo
  a otro en cada fila. RAID5 aguanta un dispositivo perdido (mínimo 2) y
  RAID6 dos (mínimo 3); `mount_degraded` lo comprueba chunk a chunk.
  - **El agujero de escritura de btrfs no existe aquí**: en btrfs un corte
    entre un bloque de datos y la paridad de su fila deja esa paridad mal
    para los *otros* bloques de la fila, que se reconstruyen mal si luego
    se pierde un disco. En EclipseFS el asignador solo da bloques de un
    chunk con paridad en filas enteramente libres o que la transacción
    abierta ya empezó (`blocks.rs`), así que nunca se escribe en una fila
    con datos confirmados. Lo que un corte deja a medias son filas que
    solo tenían bloques libres o sin confirmar, y la siguiente escritura
    recalcula la paridad desde lo que hay en disco. La cola libre de una
    fila que una transacción dejó a medias espera a que se libere la fila
    entera. La única fila que se reescribe en su sitio, la del bitmap del
    grupo, va por el journal. `tests/raid56.rs` corta la luz en cada punto
    de una carga y monta sin cada uno de los discos: con la regla quitada
    el test falla.
  - Al leer, un bloque que falta o no cuadra con su checksum se
    reconstruye de la fila: «copia» 1 con P, 2 con Q, y en RAID6 3.. con
    cada otro bloque de la fila supuesto perdido también, así se arregla
    un bloque podrido junto a un disco perdido. `scrub` comprueba cada
    reconstrucción, así encuentra también P y Q malas, y reescribe lo que
    falle (dato, P o Q). Un dispositivo que vuelve tras un montaje
    degradado recibe su parte reconstruida.
  - `balance` convierte entre cualquier perfil y RAID5/6 (el grupo 0 va en
    `raid1`, o `raid1c3` con RAID6). Kernel: `BTRFS_BLOCK_GROUP_RAID5/6`
    en `BALANCE_V2`.
- **Niveles rápido y lento** (bit `FEAT_TIERS`, mapa de chunks v2), como
  los targets de bcachefs: `set_device_tier(devid, Tier::Fast|Slow|Any)`,
  `add_device_tier` o `mkfs::format_devices_tiered`. Con dispositivos de
  los dos niveles, los metadatos y los datos nuevos van a chunks en los
  rápidos mientras tengan sitio (si no, a cualquiera), y
  `tier_migrate(todos)` lleva a los lentos los chunks de datos llenos (los
  que ya no se están escribiendo; todos con `todos`) y a los rápidos los
  de metadatos que no estén. Se mueven chunks enteros, así los bloques
  lógicos y todas sus referencias (snapshots, reflinks) no cambian, y
  `balance` deja cada chunk en su nivel. Un mapa sin niveles sigue en la
  versión 1, legible por drivers anteriores. Kernel: `EFS_IOC_SET_TIER`,
  `EFS_IOC_GET_TIER` y `EFS_IOC_TIER_MIGRATE`. Los archivos pequeños no
  tienen trato aparte: van a los rápidos como todo dato nuevo y se mueven
  con su chunk.
- TRIM/discard queda pendiente: los drivers de disco del kernel todavía no
  tienen discard.

## En el kernel (fase 8)

- `mount -t eclipsefs` entiende `compress=`, `compress-force=`, `degraded`,
  `device=`, `subvol=` y `subvolid=`. Busca los demás dispositivos del
  volumen entre los discos registrados (`eclipsefs::probe`, sin montar).
- ioctls con los números de Linux/btrfs (`linux-object/src/fs/eclipsefs_ioctl.rs`),
  así que `chattr`, `cp --reflink`, `btrfs subvolume|filesystem|device|scrub|balance|qgroup|send`
  funcionan sin cambios. Todas menos receive, que va por `EFS_IOC_RECEIVE`.
  Incluye `FS_IOC_GETFLAGS`/`SETFLAGS`, `FICLONE`/`FICLONERANGE`, `FIDEDUPERANGE`,
  subvolúmenes y snapshots, subvolumen por defecto, `INO_LOOKUP`, `FS_INFO`,
  `DEV_INFO`, scrub, cuotas, `ADD_DEV`/`RM_DEV`/`DEV_REPLACE`, `BALANCE_V2`,
  `SEND`, `FS_IOC_FIEMAP`, `DEFRAG`/`DEFRAG_RANGE` y `RESIZE`. Los
  niveles de los dispositivos van por `EFS_IOC_SET_TIER`/`GET_TIER` y
  `EFS_IOC_TIER_MIGRATE`.
- Las ioctls de fscrypt con políticas v2 (`SET_ENCRYPTION_POLICY`,
  `GET_ENCRYPTION_POLICY(_EX)`, `ADD_ENCRYPTION_KEY`,
  `REMOVE_ENCRYPTION_KEY(_ALL_USERS)`, `GET_ENCRYPTION_KEY_STATUS`,
  `GET_ENCRYPTION_NONCE`), así que la herramienta `fscrypt` y
  `fscryptctl` sirven tal cual; los nonces salen del RNG del kernel.
- xattrs (`getxattr` y familia; `btrfs.compression` como en btrfs),
  `fallocate` y `copy_file_range` con reflink.
- Imágenes del instalador: `make iso ROOTFS_FS=eclipsefs` genera
  `rootfs.eclipsefs.gz`/`home.eclipsefs.gz` con este crate, e
  `install-eclipse` usa el FS del payload que lleve el medio. btrfs sigue
  siendo el valor por defecto.

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
- `tests/send.rs`: flujos completos e incrementales entre dos sistemas de
  archivos (contenido, metadatos, xattrs, flags, enlaces, renombres,
  reflinks como clones, compresión), padres equivocados o ausentes, flujos
  cortados o con bits cambiados al azar, cortes de luz a mitad de un
  receive, y rondas aleatorias de incrementales que deben dejar ambos
  lados iguales.
- `tests/quota.rs`: el contador frente a un recuento tras cada tipo de
  cambio, límites (escrituras, fallocate, reflinks), snapshots y bytes
  exclusivos, desactivar y reactivar, archivos comprimidos y subvolúmenes
  recibidos. Las cargas de crash con semilla impar llevan las cuotas
  activas, así el fsck comprueba los contadores tras cada corte.
- `tests/raid56.rs`: RAID5/6 con 2 a 6 dispositivos, cualquier disco (o
  par, en RAID6) perdido, escrituras degradadas y el disco que vuelve,
  bloques y paridad podridos (reparados al leer y en scrub), podredumbre
  junto a un disco perdido, más daño del que la paridad cubre (se
  informa), conversiones de ida y vuelta, mucho archivo pequeño creado y
  borrado (el espacio vuelve) y cortes de luz en cada punto sin cada uno
  de los discos (el agujero de escritura). En `volume.rs` hay además
  pruebas de la aritmética, de la reconstrucción de cualquier uno o dos
  bloques y de la E/S de filas.
- `tests/tiering.rs`: metadatos y datos nuevos en el rápido, migración a
  los lentos (todo o solo los chunks llenos), desbordar el rápido, RAID1
  de metadatos en la pareja rápida, niveles puestos después y en
  dispositivos añadidos, y niveles con RAID5.
- `tests/volume.rs`: los nueve perfiles sobre cuatro dispositivos,
  dispositivos perdidos (montaje degradado y qué perfiles lo permiten),
  bloques podridos en una copia y en todas (reparación al leer y en
  scrub, datos y metadatos), crecer de un dispositivo a RAID1, quitar,
  reemplazar uno perdido, resincronizar uno que vuelve, balance con
  conversión y chunks vacíos, disco lleno, y cortes de luz en cada punto
  de una carga que añade y quita dispositivos y convierte perfiles.
- `tests/mapping.rs`, `tests/defrag.rs` y `tests/resize.rs`: FIEMAP y
  SEEK frente a un modelo; defrag con huecos, reservas, umbral, rangos,
  compresión, no-COW y snapshots; resize creciendo, encogiendo con datos y
  nodos en la cola, sin sitio, con snapshots, reflinks, compresión y
  cuotas (la compartición se mantiene), volúmenes y una prueba aleatoria.
  La carga de crash también desfragmenta y redimensiona.
- `tests/bitmap_sums.rs`: la tabla en un sistema nuevo y en uno anterior,
  bitmaps con bits cambiados (reconstruidos desde los árboles, también con
  la cabecera marcada libre), un bloque de la tabla dañado, montaje de
  solo lectura sin escrituras, la tabla al día tras cada commit, al crecer
  y al encoger, y en RAID1 la copia buena reescribe la mala.
- `tests/encrypt.rs`: políticas (directorio vacío, clave presente, la misma
  otra vez), herencia con nonces distintos, que ni el contenido ni los
  nombres aparezcan en claro en el disco, remontajes, nombres sin clave
  (largos incluidos) para buscar y borrar, `NoKey` al leer, escribir o
  crear, quitar y volver a poner la clave, symlinks, truncados sin
  alinear, huecos, defrag, compresión que no se aplica, snapshots,
  renombres y enlaces entre políticas, y lo que se niega. La carga de crash
  cifra algunos directorios y comprueba cada imagen sin la clave y lee con
  ella.
- Los tests que dependen del formato corren en las dos versiones (`*_v3`).
- `tests/performance.rs`: las mismas cargas sobre EclipseFS y btrfs, con tiempo
  y bytes leídos/escritos en el dispositivo. `scripts/eclipsefs-bench.sh` las
  ejecuta en release; `ECLIPSEFS_BENCH_SCALE=4` las agranda.
- `scripts/eclipsefs-linux-bench.sh` (como root): EclipseFS contra los
  sistemas de archivos del kernel Linux (ext4, xfs, btrfs) con fio y tar,
  cada uno en una imagen del mismo tamaño en un dispositivo loop. EclipseFS se
  monta con `tools/eclipsefs-fuse`; la fila `ext4-fuse` (ext4 detrás de
  bindfs) mide lo que cuesta FUSE por sí solo.
