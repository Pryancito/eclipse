# Diseño: escritura de vuelta de `mmap` sobre ficheros, `msync(2)` y una caché de páginas compartida

**Español** · [English](mmap-page-cache-design.md)

Estado (noviembre de 2026): **casi todo implementado.** Esto empezó como un
plan solo de diseño; desde entonces el código ha alcanzado la mayor parte:

- **La etapa A está hecha.** `sys_msync` (`linux-syscall/src/vm.rs`) valida
  sus argumentos como dice la página de manual y escribe de vuelta las páginas
  comprometidas de cada mapeo de fichero `MAP_SHARED` del rango
  (`walk_mapped_msync` → `linux_object::fs::sync_shared_file_vmo`). Un VMO
  compartido también se escribe de vuelta antes de desalojarlo
  (`writeback_shared_vmo`, limitado al tamaño actual del inodo para no
  agrandar nunca el fichero).
- **La etapa B está hecha.** `MAP_SHARED` pasa por `get_vmo_shared`, que da a
  todos los que mapean un inodo el mismo `VmObject` del registro
  `SHARED_FILE_VMOS` (`linux-object/src/fs/file.rs`), así que los mapeos
  compartidos son coherentes entre sí y a través de `fork`. Las cachés
  privadas de fichero se quedan en el registro como caché de páginas, y
  `page_cache_reclaim` devuelve sus marcos cuando falta memoria.
- **La etapa C está hecha, en forma de superposición.** `read(2)`/`write(2)`
  no pasan por el VMO, pero `cache_overlay_read` y `cache_overlay_write` los
  mantienen coherentes con lo que la caché tenga comprometido, y
  `cache_truncate` tira las páginas en caché que quedan más allá de un nuevo
  final.
- **Falta todavía:** el bit de suciedad del hardware (§3.1, opción 2). La
  escritura de vuelta sigue siendo «todas las páginas comprometidas», lo que
  reescribe páginas limpias que solo se leyeron.

El resto del documento es el plan original, que se conserva porque explica por
qué las piezas tienen la forma que tienen.

Se apoya en la lectura anticipada por demanda que ya está en
`linux-object/src/fs/file.rs` (`Readahead` / `FileFrameFiller`).

---

## 1. Estado de entonces y el hueco

### Cómo funcionaba un `mmap` de fichero cuando se escribió esto

`sys_mmap` (`linux-syscall/src/vm.rs`), para un mapeo respaldado por fichero:

1. Llama a `file_like.get_vmo(offset, len)`.
2. `File::get_vmo` (`linux-object/src/fs/file.rs`) construye un VMO **nuevo**,
   `VmObject::new_paged_with_source(pages(len), FileFrameFiller{..})`: un VMO
   paginado que se rellena por demanda desde el inodo mediante
   `FrameFiller::fill_page`.
3. Mapea ese VMO en el VMAR del proceso con `map_ext(.., map_range=false)`,
   así que las páginas se leen del fichero al tocarlas por primera vez.

Consecuencias clave:

- **No hay escritura de vuelta.** Nada copia nunca las páginas modificadas de
  vuelta al inodo.
- **`MAP_SHARED` no se respeta.** Nunca se mira `MmapFlags::SHARED` frente a
  `PRIVATE`; cada mapeo recibe su propio VMO privado. Las escrituras en una
  región `MAP_SHARED` se **pierden en silencio** al hacer `munmap` o salir.
- **No hay coherencia.** Dos `mmap` del mismo inodo reciben dos VMO
  independientes, así que no ven las escrituras en memoria del otro, y
  `read(2)`/`write(2)` sobre el fd no son coherentes con el mapeo.
- **`msync(2)` no existe.** No está en el despacho de syscalls
  (`linux-syscall/src/lib.rs`); quien lo llama cae en el camino de syscall
  desconocida.

### Lo que el kernel ya ofrece como base

- `VMObjectTrait` (`zircon-object/src/vm/vmo/mod.rs`) expone `read(offset,
  buf)` / `write(offset, buf)` y `committed_pages_in_range(start, end)`.
- `VMObjectPaged` guarda los marcos comprometidos en `frames:
  BTreeMap<page_idx, PageState>`. **No hay bit de suciedad**: `PageState`
  solo lleva un `PageStateTag` (`Owned`/…), no el estado de escritura.
- `LinuxProcessInner` (`linux-object/src/process.rs`) ya cuelga contabilidad
  de memoria virtual por proceso (`brk`, `mapped_brk`); es el sitio natural
  para un registro de mapeos. **Hoy no hay una lista de mapeos respaldados
  por fichero.**

---

## 2. Decisión de alcance: por etapas, no «la caché de páginas entera de Linux»

Una caché de páginas totalmente coherente (un único conjunto de páginas
compartidas por inodo, que respalde `read`/`write`/todos los `mmap`, con
escritura de vuelta) es el objetivo final, pero es un cambio profundo de la
memoria virtual. Se proponen **tres etapas**, cada una publicable por
separado y cada una estrictamente mejor que lo de hoy:

- **Etapa A — escritura de vuelta de un solo mapeo `MAP_SHARED` +
  `msync(2)`.** Persistir los cambios de un mapeo de fichero compartido y
  escribible en `msync` y en `munmap`/salida. *Todavía* sin coherencia entre
  mapeos. Cubre el caso común (un proceso mapea un fichero, escribe,
  persiste) y quita el bug de pérdida silenciosa de datos.
- **Etapa B — caché de VMO compartida por inodo.** `get_vmo` devuelve el
  *mismo* VMO para el mismo inodo y rango, así que todos los mapeos
  `MAP_SHARED` comparten páginas y son coherentes entre sí. La escritura de
  vuelta pasa a ser por inodo.
- **Etapa C — unificar `read`/`write` con la caché de páginas.** Hacer pasar
  `File::read_at`/`write_at` por el VMO compartido para que la E/S por fd y el
  mmap sean totalmente coherentes. El cambio más grande; el último.

Este documento especifica la **etapa A** entera y esboza la B y la C.

---

## 3. Etapa A — escritura de vuelta de `MAP_SHARED` + `msync(2)`

### 3.1 Seguimiento de suciedad

Dos opciones; elegir según cuánto pueda exponer la capa de memoria virtual
con seguridad:

1. **Escritura de vuelta de páginas comprometidas (la más simple, correcta,
   derrochadora).** Al volcar, escribir de vuelta todas las páginas
   *comprometidas* del rango mapeado. Leer una página de origen no
   comprometida solo volvería a traer los bytes del propio fichero, así que
   basta con escribir de vuelta las *comprometidas*; y escribir una
   comprometida pero no modificada reescribe bytes idénticos (no cambia nada
   en el disco). Hace falta una forma de enumerar los índices de las páginas
   comprometidas (añadir `VMObjectTrait::for_each_committed_page` o reutilizar
   `committed_pages_in_range` con un iterador de índices).
   - Coste: reescribe páginas limpias que se tocaron. Vale para una primera
     versión; hay que anotarlo.

2. **Cosecha del bit de suciedad del hardware (lo propio).** Añadir un flag
   `dirty` de verdad a `PageState`, ponerlo a partir del bit de suciedad de la
   PTE (`MMUFlags`/`GenericPageTable`) al cosechar, y escribir de vuelta solo
   las páginas sucias, borrando el bit. Así lo hace Linux y es la base buena
   para las etapas B y C, pero toca el código de tablas de páginas de cada
   arquitectura; hacerlo cuando haya una compilación con la que validar la
   fontanería de las PTE en cada arquitectura (x86_64/aarch64/riscv64).

**Recomendación:** publicar la etapa A con la opción 1 (todo en código de
objetos y syscalls, sin cambios de arquitectura) y pasar a la opción 2 en la
etapa B.

### 3.2 El registro de mapeos

Añadir a `LinuxProcessInner`:

```rust
/// File-backed shared writable mappings, for msync/munmap write-back.
/// Keyed by mapping start address.
shared_file_maps: BTreeMap<usize, SharedFileMap>,
```

```rust
struct SharedFileMap {
    vmo: Arc<VmObject>,     // the mapping's VMO (already held by the VMAR)
    inode: Arc<dyn INode>,  // write-back target
    file_offset: usize,     // inode offset that VMO offset 0 maps to
    len: usize,             // mapping length in bytes
    source_len: usize,      // writable/in-file span (== FileFrameFiller.source_len)
}
```

`File::get_vmo` ya calcula `source_len`; hay que devolverlo (o el
`FileFrameFiller`) para que `sys_mmap` pueda rellenar el registro. Dar a
`LinuxProcess` accesores como los de `brk`/`mapped_brk`:
`record_shared_file_map`, `take_shared_file_maps_in(addr, len)`,
`shared_file_maps_overlapping(addr, len)`.

### 3.3 Cambios en `sys_mmap`

En la rama de `sys_mmap` para ficheros, tras un `map_ext` correcto:

```rust
if flags.contains(MmapFlags::SHARED) && prot.contains(MmapProt::WRITE) {
    self.linux_process().record_shared_file_map(addr, SharedFileMap {
        vmo: vmo.clone(), inode, file_offset: offset as usize, len, source_len,
    });
}
```

Los mapeos de fichero `MAP_PRIVATE` escribibles **no** se deben registrar
(sus escrituras son copia en escritura y nunca llegan al fichero). Ojo: zCore
todavía no hace copia en escritura de esos mapeos privados; queda fuera de
alcance, pero merece un seguimiento, porque hoy un mapeo de fichero privado y
escribible comparte el VMO de origen.

### 3.4 Rutina de escritura de vuelta

```rust
/// Copy the mapping's committed pages back to its inode. Idempotent.
fn writeback(map: &SharedFileMap) -> LxResult {
    let mut buf = [0u8; PAGE_SIZE];
    let end = map.source_len;                 // never write past the in-file span
    for page_start in (0..end).step_by(PAGE_SIZE) {
        let idx = page_start / PAGE_SIZE;
        if !map.vmo.is_committed(idx) { continue; }   // skip never-touched pages
        let n = (end - page_start).min(PAGE_SIZE);
        map.vmo.read(page_start, &mut buf[..n])?;     // VMO -> buf
        map.inode.write_at(map.file_offset + page_start, &buf[..n])?;
    }
    Ok(())
}
```

- Limitar a `source_len` para no escribir nunca la cola BSS que queda más
  allá del EOF.
- `is_committed(idx)` es el accesor nuevo (opción 1); con la opción 2 pasa a
  ser `is_dirty(idx)` y el bit se borra tras escribir.
- Errores: quedarse con el primero pero seguir, y devolverlo al final, para
  que un bloque malo no se salte en silencio el resto (igual que el `msync`
  de Linux, que hace lo que puede).

### 3.5 `msync(2)`

Añadir `Sys::MSYNC => self.sys_msync(a0, a1, a2)` al despacho y:

```rust
pub fn sys_msync(&self, addr: usize, len: usize, flags: usize) -> SysResult {
    // flags: MS_ASYNC=1, MS_INVALIDATE=2, MS_SYNC=4. MS_ASYNC|MS_SYNC is EINVAL.
    if addr % PAGE_SIZE != 0 { return Err(LxError::EINVAL); }
    if flags & !(MS_ASYNC|MS_SYNC|MS_INVALIDATE) != 0 { return Err(LxError::EINVAL); }
    if (flags & MS_ASYNC != 0) && (flags & MS_SYNC != 0) { return Err(LxError::EINVAL); }
    for map in self.linux_process().shared_file_maps_overlapping(addr, len) {
        writeback(&map)?;       // MS_ASYNC may defer; with no writeback queue, treat as sync
    }
    Ok(0)
}
```

Como no hay un hilo de escritura de vuelta asíncrono, `MS_ASYNC` se respeta
escribiendo de forma síncrona (está permitido: significa «programar la
escritura de vuelta», y hacerla ya es un superconjunto correcto).

### 3.6 `munmap` / salida

En `sys_munmap`, antes de `vmar.unmap`, volcar y soltar los mapeos de fichero
compartidos que se solapen:

```rust
for map in self.linux_process().take_shared_file_maps_in(addr, len) {
    let _ = writeback(&map);   // best-effort on unmap, like Linux
}
```

Al desmontar el proceso, volcar todos los mapeos registrados que queden.
Buscar el camino de salida/desmontaje del `vmar` y añadir ahí el barrido.

### 3.7 Casos límite

- **`munmap` parcial** de una región registrada: partir o recortar la entrada
  del registro (volcar y olvidar solo el subrango solapado; conservar el
  resto). La primera versión más simple: volcar la entrada entera y volver a
  registrar el subrango que sobrevive.
- **`mprotect` que quita `PROT_WRITE`**: opcionalmente volcar y quitar del
  registro; dejarlo registrado es seguro (la escritura de vuelta sigue siendo
  correcta, solo quizá redundante).
- **Fichero truncado tras el mmap**: un `write_at` más allá del tamaño actual
  o lo agranda o falla según el sistema de ficheros; limitar al `source_len`
  capturado en el mmap y aceptar la semántica del sistema de ficheros.
  Documentarlo.
- **`fork`**: un mapeo `MAP_SHARED` sigue compartido con el hijo. El registro
  se tiene que heredar (el fork del VMAR ya comparte el VMO); copiar las
  entradas al hijo en el camino del fork.

---

## 4. Etapa B — caché de VMO compartida por inodo (coherencia)

- Guardar un `Weak<VmObject>` por `(id de inodo, file_offset, len)` (o por
  inodo, con un solo VMO de todo el fichero y VMO hijos para los subrangos
  mediante `VMObject::create_child`).
- `File::get_vmo` devuelve el VMO en caché en vez de uno nuevo, así que todos
  los mapeos `MAP_SHARED` del mismo inodo comparten páginas → coherencia
  mutua.
- La escritura de vuelta pasa a ser por inodo (volcar el VMO del inodo),
  movida por los mismos ganchos de `msync`/`munmap`; el registro usa el inodo
  como clave.
- Hay que cuidar los tiempos de vida: la caché guarda referencias débiles para
  que el VMO de un fichero cerrado se recupere; el último `munmap` vuelca.
- Conviene combinarla con el seguimiento por **bit de suciedad del hardware**
  (§3.1, opción 2) para que una caché compartida grande no reescriba páginas
  limpias.

## 5. Etapa C — unificar `read`/`write` con la caché

- Hacer pasar `File::read_at`/`write_at` por el VMO en caché del inodo
  (`vmo.read`/`vmo.write`) en vez de ir directos a
  `inode.read_at`/`write_at`, con el VMO como única fuente de verdad y un
  camino de escritura de vuelta al inodo.
- Da coherencia total entre `read`/`write` y `mmap` (POSIX).
- El mayor radio de impacto (cada lectura y escritura por fd); hacerlo el
  último, con tests, y con la maquinaria del bit de suciedad de la etapa B.

---

## 6. Estrategia de tests

Lo que ya se puede probar de forma pura o unitaria (sin arrancar el kernel):

- **Aritmética del rango de escritura de vuelta**: paso por páginas, límite
  `source_len`, última página parcial, saltar las no comprometidas. Imitar el
  enfoque de tests de equivalencia de `Readahead` con un inodo simulado (un
  `Vec<u8>`) y un conjunto simulado de «páginas comprometidas»; comprobar que
  los bytes escritos son las páginas modificadas y que no se toca nada más
  allá de `source_len`.
- **Validación de flags de `msync`**: `MS_SYNC|MS_ASYNC` → EINVAL, bits
  desconocidos → EINVAL, dirección desalineada → EINVAL (puro, como los tests
  de `madvise_advice_known`).
- **Solape y partición del registro**: `shared_file_maps_overlapping` /
  `take_..._in` y la lógica de partición del `munmap` parcial como operaciones
  puras de `BTreeMap`.

Lo que necesita un kernel en marcha (el motivo de que esto fuera solo diseño):

- Escribir en `MAP_SHARED` + `msync` + volver a hacer `read` ve el cambio;
  escribir en `MAP_PRIVATE` **no** llega al fichero; dos mapeos coherentes
  (etapa B); `read`/`mmap` coherentes (etapa C); cosecha del bit de suciedad
  de la PTE en varias arquitecturas (etapa B, opción 2).

---

## 7. Resumen de riesgos

| Cambio | Radio de impacto | Modo de fallo si está mal |
|---|---|---|
| Registro + `msync` + volcado en munmap (etapa A) | syscalls + estado del proceso | escritura de vuelta mala o perdida → **corrupción de ficheros del usuario** |
| Accesor de enumeración de páginas comprometidas | código de objetos VMO | escritura de vuelta de más o de menos (derroche o datos perdidos) |
| Cosecha del bit de suciedad de la PTE (etapa B) | tablas de páginas ×3 arquitecturas | página sucia perdida → escrituras perdidas |
| Caché de VMO por inodo (etapa B) | camino de mmap | alias o uso tras liberar de páginas compartidas |
| `read`/`write` a través de la caché (etapa C) | toda la E/S por fd | regresiones en el camino de datos |

Cada fila modifica datos del usuario, así que cada etapa tenía que entrar
contra un kernel que compilara y con los tests en ejecución del §6 en verde.
Por eso se escribió como documento y no como código.
