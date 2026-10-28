# Formato en disco de EclipseFS 1.0

Este documento es la especificación del formato que escribe `mkfs::format`
(y `mkfs::format_devices`). Con él, cualquiera puede escribir un driver que
lea y escriba EclipseFS sin mirar el código.

**El formato 1.0 está congelado.** En el campo de versión pone
`0x0004_0000` (por eso el código lo llama «v0.4») y ese número ya no
cambia. A partir de aquí el formato solo crece con bits de funcionalidad,
siguiendo las reglas de [Cómo crece el formato](#cómo-crece-el-formato).
Las imágenes de `tests/format/` se escribieron el día de la congelación
y `tests/format.rs` comprueba en cada cambio que se siguen montando, se
leen igual y aceptan escrituras.

El formato v0.3 anterior (en el sitio, con journal) sigue montando y queda
igualmente congelado tal como está; se describe al final del
[README](README.md#formato-v03-in-situ-con-journal) y en `src/format.rs`.
Ninguna imagen nueva lo usa.

## Convenciones

- Todo entero es **little-endian**. `u8/u16/u32/u64` sin signo.
- Un **bloque** son 4096 bytes. Las direcciones en disco son números de
  bloque (byte = bloque × 4096), salvo que se diga otra cosa.
- **CRC32C** es el CRC de Castagnoli (polinomio reflejado `0x82F63B78`,
  valor inicial `0xFFFFFFFF`, XOR final `0xFFFFFFFF`; el de iSCSI, ext4 y
  btrfs). **CRC32** es el de IEEE 802.3 (zlib), que solo usa el journal.
- Los campos marcados *reservado* se escriben a cero. Un driver 1.0 no les
  da significado, pero muchos los comprueba (un extent con un byte
  reservado distinto de cero se considera dañado), así que no se pueden
  reutilizar sin un bit de funcionalidad.
- Los tiempos son `(segundos u64, nanosegundos u32)` desde 1970.

## Disposición general

El espacio se parte en **grupos** de 32768 bloques (128 MiB, lo que cabe
en un bloque de bitmap). El último grupo puede ser más corto; uno de menos
de 64 bloques no se crea.

```text
grupo k (bloques k·32768 ..)
  +0   superbloque (el primario en el grupo 0, copias en los demás)
  +1   bitmap de asignación del grupo
  +2.. resto: nodos de árbol, datos, y en el grupo 0 además el journal,
       la tabla de checksums de bitmaps, el área del log de fsync y (en un
       volumen) el mapa de chunks
```

El journal empieza en el bloque 2 del grupo 0. Todo lo demás vive donde
lo pone el asignador y se encuentra siguiendo punteros desde el
superbloque.

Las **copias del superbloque** de los grupos 1 en adelante se escriben al
formatear y al crecer el sistema de archivos. Sirven para reconocerlo
(magia, UUID, geometría), **no** se actualizan en cada commit y un driver
no debe montar desde ellas.

En un **volumen** (varios dispositivos, ver [Volúmenes](#volúmenes-feat_volume))
los bloques de arriba son *lógicos* y el mapa de chunks dice en qué
dispositivo y bloque físico está cada uno.

## Superbloque

Bloque 0 (del dispositivo, o del dispositivo 1 de un volumen; cada
dispositivo de un volumen lleva su copia en su bloque físico 0 con su
propio `devid`).

| Byte | Tipo | Campo |
|---|---|---|
| 0 | `[u8; 9]` | magia `"ECLIPSEFS"` (bytes 9..12 reservados) |
| 12 | u32 | versión: `0x0004_0000`. Un driver 1.0 mira solo los 16 bits altos (4) |
| 16 | u32 | tamaño de bloque: 4096 |
| 20 | u32 | bloques por grupo: 32768 |
| 24 | u64 | bloques totales |
| 32 | u64 | bloques libres |
| 40 | u64 | generación (número del último commit) |
| 48 | u64 | creación (segundos) |
| 56 | u64 | **funcionalidades incompatibles** (ver abajo) |
| 64 | `[u8; 16]` | UUID del sistema de archivos |
| 80 | `[u8; 64]` | etiqueta, UTF-8 terminada en NUL |
| 144 | u64 | bloque de la raíz del árbol raíz |
| 152 | u8 | nivel de esa raíz |
| 160 | u64 | generación de esa raíz |
| 168 | u64 | primer bloque del journal |
| 176 | u32 | bloques del journal (mínimo 8) |
| 180 | u8 | tipo de checksum: 1 = CRC32C |
| 184 | u64 | subvolumen por defecto (el que se monta como `/`; 5 al formatear) |
| 192 | u32 | con `FEAT_VOLUME`: id de este dispositivo |
| 196 | u32 | con `FEAT_VOLUME`: número de dispositivos |
| 200 | u64 | con `FEAT_VOLUME`: bloque del mapa de chunks (en el grupo 0) |
| 208 | u32 | con `FEAT_VOLUME`: bytes del mapa de chunks |
| 212 | u8 | con `FEAT_VOLUME`: perfil de los chunks de datos nuevos |
| 213 | u8 | con `FEAT_VOLUME`: perfil de los chunks de metadatos nuevos |
| 216 | 64 bytes | con `FEAT_VOLUME`: el chunk del grupo 0 (formato corto, abajo) |
| 288 | u64 | con `FEAT_BITMAP_CSUM`: primer bloque de la tabla de checksums de bitmaps |
| 296 | u32 | con `FEAT_BITMAP_CSUM`: sus bloques |
| 304 | u64 | **funcionalidades compatibles** |
| 312 | u64 | **funcionalidades compatibles en solo lectura** |
| 320..508 | | reservado (cero) |
| 508 | u32 | CRC32C de los bytes 0..508 |

Un superbloque se rechaza si la magia, la versión, el CRC, la geometría o
el tipo de checksum no cuadran, si la raíz cae fuera del disco o tiene una
generación mayor que la del superbloque, o si el journal o la tabla de
checksums de bitmaps no caben en el grupo 0.

### Bits de funcionalidad

Tres clases, como en ext4:

- **Incompatibles** (byte 56): cambian cómo se lee algo. Un driver que
  encuentra uno que no conoce **no monta** ni en solo lectura.
- **Compatibles en solo lectura** (byte 312): un driver que no los conoce
  puede leer bien pero estropearía algo al escribir. **Monta solo en
  lectura.**
- **Compatibles** (byte 304): un driver que no los conoce puede leer y
  escribir sin estropear nada. Se **ignoran y se conservan** al reescribir
  el superbloque.

Bits de 1.0 (todos en el byte 56; los bytes 304 y 312 están a cero):

| Bit | Nombre | Significado |
|---|---|---|
| `1 << 0` | `FEAT_QUOTA` | cuotas activas: `referenced` de cada `ROOT_ITEM` está al día |
| `1 << 1` | `FEAT_VOLUME` | volumen: mapa de chunks y campos 192..280 |
| `1 << 2` | `FEAT_FIXED_SIZE` | el tamaño lo puso un `resize`; montar no crece hasta el dispositivo |
| `1 << 3` | `FEAT_BITMAP_CSUM` | los bitmaps tienen checksum (campos 288..300) |
| `1 << 4` | `FEAT_ENCRYPT` | algún directorio tiene política de cifrado |
| `1 << 8` | `FEAT_RAID56` | hay chunks RAID5 o RAID6 |
| `1 << 9` | `FEAT_TIERS` | dispositivos con nivel: el mapa de chunks es versión 2 |

Algunos de estos serían de otra clase por su naturaleza (`FEAT_FIXED_SIZE`
es compatible), pero todos los drivers desde 1.0 los conocen, así que da
igual: quedan donde están.

## Árboles B+

Todo lo demás vive en árboles B+ copy-on-write de nodos de un bloque,
con claves `(objectid u64, tipo u8, offset u64)` ordenadas por ese orden
(17 bytes en disco: objectid, tipo, offset).

### Nodo

| Byte | Tipo | Campo |
|---|---|---|
| 0 | `[u8; 4]` | magia `"EFSN"` |
| 4 | u32 | CRC32C de los bytes 8..4096 |
| 8 | u64 | bloque donde está el nodo (lógico) |
| 16 | u64 | generación en la que se escribió |
| 24 | u64 | árbol dueño |
| 32 | u32 | número de ítems o punteros |
| 36 | u8 | nivel (0 = hoja, máximo 8) |
| 37..40 | | reservado |
| 40 | `[u8; 16]` | UUID del sistema de archivos |
| 56..64 | | reservado |
| 64.. | | cuerpo |

- **Hoja** (nivel 0): una tabla de ítems de 25 bytes `[clave 17 | offset u32
  | tamaño u32]` desde el byte 64, y los datos de los ítems empaquetados
  desde el final del bloque hacia atrás. Claves estrictamente crecientes;
  cada dato cae entre el final de la tabla y el final del bloque. El ítem
  más grande cabe dos veces en una hoja (`MAX_ITEM` = 1991 bytes).
- **Interno** (nivel > 0): punteros de 33 bytes `[clave 17 | bloque hijo u64
  | generación del hijo u64]` desde el byte 64 (como mucho 122). La clave
  es la primera del hijo; claves estrictamente crecientes. Al leer un hijo,
  su generación y su bloque tienen que ser los del puntero.

### Copy-on-write y referencias

Un nodo o bloque de datos ya confirmado nunca se sobrescribe: cambiarlo
escribe una copia en otro bloque y actualiza el padre, hasta la raíz. Un
nodo o extent puede estar compartido (snapshots, reflinks). Los contadores
de referencias son perezosos (Rodeh, como btrfs): solo se guardan los que
son ≥ 2, en el árbol de referencias; un bloque en uso sin entrada tiene
una sola referencia. Copiar un nodo compartido suma una referencia a cada
hijo.

### Árboles

| Id | Árbol | Qué guarda |
|---|---|---|
| 1 | raíz | un `ROOT_ITEM` por cada otro árbol, los `ROOT_BACKREF` y el área del log |
| 2 | referencias | `REF_ITEM` de los bloques con más de una referencia |
| 5 | subvolumen por defecto | inodos, nombres, extents, xattrs |
| 7 | checksums | CRC32C de cada bloque de datos |
| 8 | (objeto del árbol raíz) | `(8, EXTENT_DATA, 0)`: el área del log de fsync |
| ≥ 256 | subvolúmenes y snapshots | como el 5 |

### Tipos de ítem

| Tipo | Nombre | Clave | Valor |
|---|---|---|---|
| 1 | `INODE_ITEM` | `(ino, 1, 0)` | inodo, 128 bytes |
| 12 | `INODE_REF` | `(ino, 12, ref_hash(dir, índice))` | `(dir, índice, tipo, nombre)` |
| 24 | `XATTR_ITEM` | `(ino, 24, name_hash(nombre))` | lista de atributos |
| 26 | `ENC_CONTEXT` | `(ino, 26, 0)` | contexto de cifrado, 40 bytes |
| 48 | `ORPHAN_ITEM` | `(2^64 - 5, 48, ino)` | nada: inodo sin nombre aún abierto |
| 84 | `DIR_ITEM` | `(dir, 84, name_hash(nombre))` | lista de entradas |
| 96 | `DIR_INDEX` | `(dir, 96, índice)` | `(ino, 0, tipo, nombre)` |
| 108 | `EXTENT_DATA` | `(ino, 108, byte del archivo)` | extent |
| 128 | `CSUM_ITEM` | `(primer bloque del cubo, 128, 0)` | checksums de 256 bloques |
| 132 | `ROOT_ITEM` | `(árbol, 132, 0)` | raíz y datos del árbol, 168 bytes |
| 144 | `ROOT_BACKREF` | `(subvolumen, 144, subvolumen padre)` | `(dir, índice, 2, nombre)` |
| 200 | `REF_ITEM` | `(primer bloque, 200, 0)` | `largo u64, cuenta u32` |

Hashes de nombres: `name_hash(n) = crc32c(n) | crc32(n) << 32`, y
`ref_hash(dir, índice) = name_hash(dir_le64 ‖ índice_le64)`.

### Inodo (`INODE_ITEM`, 128 bytes)

| Byte | Tipo | Campo |
|---|---|---|
| 0 | u32 | modo (tipo `S_IF*` y permisos, como Linux) |
| 4 | u32 | uid |
| 8 | u32 | gid |
| 12 | u32 | enlaces |
| 16 | u64 | tamaño en bytes |
| 24 | u64 | bytes en disco |
| 32 | u64 | rdev: número de dispositivo (archivos de dispositivo), tal como lo da el sistema |
| 40 | u64 | banderas (abajo) |
| 48 | u64 | generación que lo creó |
| 56 | u64 | directorio que lo contiene (para un directorio, su `..`) |
| 64 | u64 | siguiente índice de `DIR_INDEX` (directorios; empieza en 2) |
| 72 | u64, u32 @80 | atime (segundos, nanosegundos) |
| 88 | u64, u32 @84 | mtime (segundos @88, nanosegundos @84) |
| 96 | u64, u32 @104 | ctime |
| 112 | u64, u32 @108 | otime, la creación (segundos @112, nanosegundos @108) |
| 120 | u64 | secuencia: sube en cada cambio |

Banderas (bits 0..31, los valores de `FS_IOC_GETFLAGS` de Linux):
`0x4` comprimir (`chattr +c`, zlib), `0x8` sync, `0x10` inmutable,
`0x20` solo añadir, `0x40` nodump, `0x80` noatime, `0x400` no comprimir,
`0x800` cifrado, `0x80_0000` no-COW. Bits 32..39: algoritmo con que se
comprimen los datos nuevos (la propiedad `compression`; 0 = ninguno, si
no el id de algoritmo de abajo). El resto, reservado.

Números de inodo: el directorio raíz de cada subvolumen es el 256 y los
siguientes los da `next_ino` de su `ROOT_ITEM`. Tipos de nodo (en
`DIR_ITEM`, `DIR_INDEX` e `INODE_REF`): 1 archivo, 2 directorio, 3
symlink, 4 dispositivo de caracteres, 5 de bloques, 6 FIFO, 7 socket.

### Nombres

Un `DIR_ITEM` guarda todas las entradas de un directorio cuyo nombre da
el mismo hash, una tras otra: `[ino u64 | índice u64 | tipo u8 | largo u16
| nombre]`. `DIR_INDEX` e `INODE_REF` guardan una sola con el mismo
formato (en `DIR_INDEX` el segundo número es 0; en `INODE_REF` los números
son el directorio y el índice). Nombres de 1 a 255 bytes, sin `/` ni NUL.

Cada nombre existe tres veces: `DIR_ITEM` para buscar, `DIR_INDEX` para
listar en orden de creación e `INODE_REF` en el inodo (un enlace duro más
es un `INODE_REF` más).

**Huérfanos**: cuando el último nombre de un archivo desaparece
(`unlink`, o un `rename` encima) mientras alguien lo tiene abierto, el
inodo se queda con 0 enlaces hasta el último cierre, como en Linux, y un
`ORPHAN_ITEM` lo apunta. Al cerrarse se libera y el ítem se va; si el
sistema cae antes, el siguiente montaje de lectura y escritura libera
cada inodo que su `ORPHAN_ITEM` apunte. Un snapshot no se lleva los
huérfanos de su origen.

**Subvolúmenes en directorios**: la entrada que nombra un subvolumen
apunta a un *handle*, `id << 40 | 256`, con tipo directorio. El árbol
raíz guarda el camino de vuelta en `ROOT_BACKREF`.

### Extents (`EXTENT_DATA`)

El offset de la clave es el byte del archivo donde empieza el extent
(múltiplo de 4096 salvo el en línea). Byte 0: tipo. Byte 1: algoritmo
(solo en comprimidos; 0 en los demás). Bytes 2..8 reservados.

| Tipo | Nombre | Tamaño | Resto |
|---|---|---|---|
| 0 | en línea | 8 + datos | los bytes del archivo desde el 8; solo en el offset 0, hasta 1024 bytes |
| 1 | normal | 24 | `@8 bloque en disco u64`, `@16 bloques u64` (1..32768) |
| 2 | reservado (`fallocate`) | 24 | igual que el normal; se lee como ceros y no lleva checksums |
| 3 | comprimido | 48 | `@8 bloque en disco`, `@16 bloques en disco`, `@24 bloques de archivo del extent entero` (≤ 32), `@32 primero que mapea este ítem`, `@40 cuántos` |

Lo que no cubre ningún extent es un hueco (se lee como ceros). Un extent
comprimido guarda en disco `[largo del flujo u32 | flujo]` rellenado a
bloques enteros, siempre menos bloques que los de archivo. Algoritmos:
1 zlib (RFC 1950), 3 zstd (una trama), 4 LZ4 (bloque crudo, sin trama).
Varios ítems pueden mapear trozos distintos del mismo extent comprimido.

### Checksums de datos (árbol 7)

Cada `CSUM_ITEM` cubre un cubo de 256 bloques alineado a 256: 32 bytes de
bitmap de presencia (bit `i` = el bloque `inicio + i` tiene checksum) y
luego un CRC32C u32 por bloque, hasta el último presente. Todo bloque de
datos en uso tiene checksum salvo los de archivos no-COW y los extents
reservados. En un archivo cifrado el checksum es del texto cifrado.

### Atributos extendidos (`XATTR_ITEM`)

Todos los atributos de un inodo cuyo nombre da el mismo hash: `[largo del
nombre u16 | largo del valor u16 | nombre | valor]...`. Los nombres
empiezan por `user.`, `trusted.`, `security.` o `system.`; las ACL POSIX
son `system.posix_acl_access` y `system.posix_acl_default` con el formato
binario de Linux.

### Árboles y subvolúmenes (`ROOT_ITEM`, 168 bytes)

| Byte | Tipo | Campo |
|---|---|---|
| 0 | u64 | bloque raíz del árbol |
| 8 | u8 | nivel |
| 16 | u64 | generación de la raíz |
| 24 | u64 | siguiente número de inodo |
| 32 | u64 | inodos |
| 40 | u64 | inodo del directorio raíz (256) |
| 48 | u64 | banderas: 1 solo lectura, 2 recibiéndose (se borra al montar si quedó así) |
| 56 | u64, u32 @64 | creación |
| 72 | u64 | subvolumen del que es snapshot (0 si no) |
| 80 | u64 | generación en que se hizo el snapshot |
| 88 | `[u8; 16]` | UUID |
| 104 | `[u8; 16]` | UUID del origen del snapshot |
| 120 | `[u8; 16]` | UUID del subvolumen del que se recibió (`receive`) |
| 136 | u64, u32 @144 | cuándo se recibió |
| 152 | u64 | cuota: bytes de archivo como máximo (0 = sin límite) |
| 160 | u64 | bytes de archivo que referencia (al día con `FEAT_QUOTA`) |

El árbol de referencias (2) y el de checksums (7) usan solo los tres
primeros campos.

### Cifrado (`FEAT_ENCRYPT`)

Como fscrypt v2 de Linux. Un directorio vacío recibe una política y todo
lo que se crea debajo la hereda con un nonce propio, en su `ENC_CONTEXT`:

| Byte | Campo |
|---|---|
| 0 | versión: 2 |
| 1 | contenido: 1 = AES-256-XTS |
| 2 | nombres: 4 = AES-256-CTS-CBC |
| 3 | banderas: relleno de nombres a `4 << (banderas & 3)` bytes |
| 4..8 | reservado (cero) |
| 8 | identificador de la clave maestra, 16 bytes |
| 24 | nonce, 16 bytes |

Claves por inodo: HKDF-SHA512 de la clave maestra con info
`"fscrypt\0" ‖ 2 ‖ nonce`; identificador de la clave maestra: HKDF con
info `"fscrypt\0" ‖ 1`. Contenido cifrado por bloque con XTS (el tweak es
el número de bloque en el archivo); los nombres se rellenan con NUL y se
cifran con CTS-CBC (IV cero), y en `DIR_ITEM`, `DIR_INDEX` e `INODE_REF`
se guarda el **cifrado** (el hash es del cifrado). El destino de un
symlink cifrado se guarda como datos del archivo. Los archivos cifrados
nunca van en línea ni comprimidos.

## Mapas de bits y su tabla de checksums

El bloque 1 de cada grupo es su bitmap: un bit por bloque del grupo,
**1 = en uso**, el bit `i` en el byte `i / 8`, posición `i % 8`. Los bits
más allá del final de un grupo corto van a 1. Los bloques 0 y 1 del grupo
siempre están en uso.

Con `FEAT_BITMAP_CSUM`, la tabla del grupo 0 que apunta el superbloque
guarda el CRC32C del bitmap de cada grupo (0 para un grupo sin chunk en
un volumen): cada bloque lleva hasta 1023 checksums u32 y en los bytes
4092..4096 el CRC32C de los 4092 anteriores.

## Journal y commits

Un commit escribe los nodos nuevos, sincroniza, y luego cambia el
superbloque, los bitmaps y su tabla **juntos** a través del journal:

```text
bloque 0        cabecera
bloques 1..d    descriptores: el bloque de destino de cada bloque de carga, u64
                (512 por bloque)
bloques d+1..   carga: las imágenes de los bloques, en orden
```

| Byte (cabecera) | Tipo | Campo |
|---|---|---|
| 0 | u32 | magia `"EFSJ"` |
| 4 | u32 | versión: 1 |
| 8 | u64 | secuencia |
| 16 | u32 | bloques de carga |
| 20 | u32 | bloques de descriptores (`ceil(carga / 512)`) |
| 24 | `[u8; 16]` | UUID del sistema de archivos |
| 40 | u32 | CRC32 (IEEE) de descriptores + carga |
| 44 | u32 | CRC32 de los bytes 0..44 |

Orden: descriptores, carga y cabecera (la cabecera lleva el CRC de todo
lo demás, así que no hace falta sincronizar entre medias), sincronizar,
escribir cada bloque en su sitio, sincronizar. Al montar se rejuega la transacción
que haya si la cabecera y el CRC de la carga cuadran y ningún destino cae
fuera del disco o dentro del journal; rejugarla dos veces da lo mismo. Una
transacción que no cabe en el journal se escribe directa tras invalidar
el journal (cabecera a cero). Un bloque liberado no se reutiliza hasta
que su transacción se confirma.

## Log de fsync

`fsync` de un archivo normal puede escribir un registro en lugar de un
commit. El área es un extent reservado del árbol raíz, ítem
`(8, EXTENT_DATA, 0)` (1/256 del disco, entre 64 y 4096 bloques). Los
registros van seguidos desde su principio; cada uno es una cabecera de un
bloque y la carga:

| Byte (cabecera) | Tipo | Campo |
|---|---|---|
| 0 | u32 | magia `"EFSL"` |
| 4 | u32 | versión: 1 |
| 8 | `[u8; 16]` | UUID |
| 24 | u64 | generación base (la del superbloque cuando se escribió) |
| 32 | u64 | número de secuencia (0, 1, 2... desde el último commit) |
| 40 | u32 | bloques de carga |
| 44 | u32 | banderas: 1 = lleva bloques sin checksum (no-COW) |
| 48 | u64 | bytes de carga |
| 56 | u32 | CRC32C de la carga |
| 60 | u32 | CRC32C de los bytes 0..60 |

Carga: subvolumen u64, inodo u64, el `INODE_ITEM` (largo u32 + bytes) y
entradas con etiqueta: `1` rango tocado `(primer bloque u64, fin u64)`,
`2` extent `(bloque de archivo u64, bloque en disco u64, bloques u64,
reservado u8, con checksums u8, y un CRC32C u32 por bloque si los lleva)`,
`0` fin.

Al montar se leen los registros en orden mientras la generación base sea
la del superbloque, la secuencia siga y los CRC (de la cabecera, la carga
y cada bloque de datos al que apuntan) cuadren; el primero que falla
termina el log. Los que quedan se aplican (por inodo, el último gana en
cada bloque) y se confirman. En solo lectura se confirman en memoria.

## Volúmenes (`FEAT_VOLUME`)

El espacio lógico (los grupos de siempre) se reparte en **chunks**, uno
por grupo en uso, cada uno con un tipo (0 mixto, 1 datos, 2 metadatos),
un perfil y sus *stripes* (dispositivo, bloque físico). Los primeros 16
bloques de cada dispositivo son suyos (superbloque).

Perfiles: 0 SINGLE, 1 DUP (2 copias en un dispositivo), 2 RAID0, 3 RAID1,
4 RAID10, 5 RAID1C3, 6 RAID1C4, 7 RAID5, 8 RAID6.

Dónde está el bloque lógico `off` de un chunk con `n` stripes:

- SINGLE, DUP, RAID1, RAID1C3, RAID1C4: en cada stripe, en `phys + off`.
- RAID0: unidad `u = off / 16` (64 KiB); stripe `u % n`, en
  `phys + (u / n)·16 + off % 16`.
- RAID10: `w = n / 2` parejas; unidad `u = off / 16`, pareja `i = u % w`,
  stripes `2i` y `2i+1`, en `phys + (u / w)·16 + off % 16`.
- RAID5 / RAID6: `k = n − 1` (o `n − 2`) datos por fila; fila `r = off / k`,
  columna `c = off % k`; stripe `(c + r) % n`, en `phys + r`. P es la
  columna `k` y Q la `k + 1`, con la misma rotación. P es el XOR de los
  datos de la fila y Q `Σ gⁱ·Dᵢ` en GF(2⁸) con generador 2 y polinomio
  `0x11d` (como md y btrfs).

Mapa de chunks (en el grupo 0, `chunk_map`/`chunk_map_len` del
superbloque):

```text
"EFSCHMAP" | versión u32 (1, o 2 con niveles) | dispositivos u32 | chunks u32 | reservado u32
por dispositivo: id u32 | tamaño en bloques u64
versión 2: niveles u32, y por cada uno: id u32 | nivel u8 (1 rápido, 2 lento) | 3 reservados
por chunk: grupo u64 | tipo u8 | perfil u8 | stripes u16 | reservado u32 | bloques u64
           | por stripe: dispositivo u32 | bloque físico u64
CRC32C de todo lo anterior, u32
```

El superbloque guarda además el chunk del grupo 0 en formato corto (byte
216): `tipo u8, perfil u8, stripes u8, reservado, bloques u64 @8`, y hasta
4 stripes de 12 bytes desde el 16. El grupo 0 nunca va en franjas ni con
paridad. Un volumen sin niveles usa la versión 1 del mapa.

## Flujo de send/receive

No está en disco pero es parte de 1.0 para que un `receive` futuro acepte
lo que envía un driver de hoy: cabecera `"EFSSEND1"` (banderas, identidad
del snapshot y de su padre, nombre, siguiente inodo, CRC32C) y comandos
`[tipo u8 | largo u32 | CRC32C u32 | carga]` hasta `END` (0). Detalle en
`src/cow/send.rs`.

## Cómo crece el formato

1. **La versión no cambia.** `0x0004_0000` es 1.0 para siempre. Cambiarla
   rompería todo driver anterior sin decir por qué.
2. **Todo lo nuevo va detrás de un bit.** Un ítem de un tipo nuevo, un
   campo nuevo del superbloque (de los bytes 320..508), una versión nueva
   del journal, del log, del mapa de chunks o del contexto de cifrado, un
   algoritmo de compresión nuevo... Se elige la clase según qué haría un
   driver que no lo conoce:
   - si leería mal → **incompatible** (byte 56);
   - si leería bien pero al escribir lo dejaría incoherente (un contador
     que no mantendría, una estructura que no actualizaría) →
     **compatible en solo lectura** (byte 312);
   - si no pasaría nada → **compatible** (byte 304).
3. El bit se pone **cuando se usa** la funcionalidad, no al formatear, para
   que un sistema de archivos que no la usa siga montando con drivers
   anteriores (como el mapa de chunks, que sigue en la versión 1 mientras
   no haya niveles).
4. **Nada de lo existente cambia de significado**: ni los números de esta
   página, ni los campos, ni sus tamaños. `tests/format.rs` lo comprueba
   (`the_numbers_of_the_format_stay`).
5. **Una imagen nueva por funcionalidad nueva** en `tests/format/`, hecha
   con `bless_missing_images` (solo crea las que faltan). Las que ya están
   no se rehacen nunca.
6. Esta página se actualiza en el mismo cambio.

## Garantías que se prueban

- `tests/format.rs`: cada imagen congelada (básica, subvolúmenes y
  snapshots, comprimida, cifrada, log de fsync pendiente, journal
  pendiente, redimensionada, RAID1, RAID10, RAID5, RAID6, niveles y v0.3)
  monta, se lista exactamente igual que el día de la congelación, pasa
  `check` y `scrub`, no se modifica al montarla en solo lectura, y acepta
  escrituras que tras remontar siguen ahí y pasan `check`. Los bits
  desconocidos de cada clase hacen lo que dice esta página.
- `tests/corruption.rs`: cada imagen congelada, dañada cientos de veces
  (bits al azar, nodos dañados con el CRC rehecho para que el daño pase
  el checksum, bloques a cero o cambiados de sitio), se monta, se recorre,
  se escribe, se comprueba y se remonta **sin un solo pánico**: un disco
  dañado da un error, nunca tumba el kernel.
- `tests/crash.rs`, `tests/fsync_log.rs`, `tests/raid56.rs` y
  `tests/faults.rs`: cortes de luz y discos que fallan en cada punto.
