# e1000e: auditoría de bugs (2026-08-06)

Barrido de `drivers/src/net/e1000e.rs` (2796 líneas) buscando fallos
funcionales concretos: pérdida/corrupción de paquetes, UB, deadlocks,
programación incorrecta del hardware o estado inconsistente. No es una
pasada de estilo — cada punto tiene un escenario de fallo reproducible por
lectura de código, o una comparación directa con el driver hermano
`e1000.rs` y con el mapa de registros real de Intel/Linux e1000e.

**Estado: los 13 hallazgos están corregidos** en `drivers/src/net/e1000e.rs`.
Verificado con `cargo test -p zcore-drivers --lib --features mock e1000e`
(11 tests, incluyendo un nuevo módulo `tx_ring_tests` que cubre el cambio de
detección de fin de TX) y `cargo build -p zcore-drivers` (build `no_std`
real). Este documento se mantiene como registro histórico de la auditoría;
cada sección de abajo describe el bug tal como se encontró.

## Crítico

### 1. `static mut ROUTES_STORAGE` — UB por aliasing + corrupción de rutas entre NICs
`e1000e.rs:2189-2191`

```rust
static mut ROUTES_STORAGE: [Option<(IpCidr, Route)>; 4] = [None; 4];
let mut routes = unsafe { Routes::new(&mut ROUTES_STORAGE[..]) };
```

`init()` se ejecuta una vez por cada NIC detectada por PCI. Cada llamada
toma un `&'static mut` sobre el **mismo** almacenamiento: con dos e1000e en
la máquina hay dos referencias exclusivas vivas simultáneamente a la misma
memoria (UB en el modelo de aliasing de Rust) y, en la práctica, la tabla
de rutas de una tarjeta pisa la de la otra (`add_route`/`del_route` de una
interfaz corrompe el estado de la otra).

## Alto

### 2. Detección de fin de TX vía lectura de TDH en vez del bit DD
`e1000e.rs:1398-1410`, `1456`

```rust
fn tx_slots_free(&self) -> usize {
    let head = unsafe { mmio_read(self.base, E1000E_TDH) as usize };
    ...
}
```

`tx_slots_free()` calcula slots libres leyendo TDH por MMIO en vez de
comprobar el bit DD del descriptor (que sí se activa, porque se marca
`TX_CMD_RS`). El datasheet de Intel advierte que TDH refleja el prefetch
interno del NIC, no la finalización real de la transmisión. El driver
hermano `e1000.rs` sí usa el bit DD (`status & 1`, líneas 276 y 311) — aquí
nunca se lee. Invisible bajo QEMU (completa TX de forma síncrona); en
hardware real puede reutilizar un buffer/descriptor antes de que el DMA
haya terminado de leerlo → corrupción o pérdida de frames en ráfagas TX
sostenidas.

### 3. Offsets de FEXTNVM6/FEXTNVM7 incorrectos
`e1000e.rs:64-65`

```rust
const E1000E_FEXTNVM6: usize = 0x01014 / 4;
const E1000E_FEXTNVM7: usize = 0x01018 / 4;
```

Según el mapa de registros de Linux e1000e, FEXTNVM6 está en `0x00010` y
FEXTNVM7 en `0x000E4` — no cerca de PBA (`0x1000`). El archivo muerto
`e1000e_pch.rs:30` tiene el mismo `0x01018` erróneo, lo que sugiere un
origen compartido del error (posible confusión con la región de PBA).
Efecto: los workarounds de ULP/SPT (líneas 683-684, 995-996) hacen
read-modify-write sobre MMIO reservado en vez de tocar el registro real, y
`FEXTNVM7.DISABLE_SMB_PERST` nunca se limpia de verdad — en hardware real
sin firmware ME activo, el PHY puede quedar atascado en modo SMBus tras un
ciclo de PERST#/suspend.

*Nota de confianza:* verificado por comparación cruzada de código (mismo
error en dos archivos) y por conocimiento del mapa de registros de Linux
e1000e; conviene contrastarlo contra el datasheet oficial de Intel antes de
tocarlo.

### 4. `RCTL_SECRC` solo se activa si `is_pch()`
`e1000e.rs:1215-1218`

```rust
let mut rctl = RCTL_EN | RCTL_UPE | RCTL_MPE | RCTL_BAM;
if self.is_pch() { rctl |= RCTL_SECRC; }
```

SECRC (strip FCS) es un bit RCTL estándar en toda la familia 8254x/e1000e,
y `e1000.rs:228` lo activa incondicionalmente. En 82574L y en el propio
modelo `e1000e` de QEMU (no-PCH), cada frame entregado a smoltcp/AF_PACKET
lleva los 4 bytes de FCS pegados al final del payload.

### 5. `matched()` incluye IDs de la familia igb (I210/I211) que el resto del driver no soporta
`e1000e.rs:2240-2241` vs `is_pch()` / `is_pch_spt_or_later()`

`0x1533` (I210), `0x1539` (I211), `0x157b`/`0x157c` (I210 flashless) hacen
match en `matched()`, pero `is_pch_spt_or_later()` los excluye, así que
`RXDCTL.QUEUE_ENABLE` nunca se programa para ellos. En la familia igb, sin
ese bit la cola no arranca aunque `RCTL.EN` esté puesto: la tarjeta queda
completamente muerta (RX y TX) sin ningún error reportado — el driver
reclama soporte que no puede entregar.

### 6. El trabajo diferido puede evictarse dejando `poll_pending`/IMS enmascarado para siempre
`e1000e.rs:1735` + `drivers/src/utils/deferred_job.rs:36-40`

La cola global de jobs diferidos tiene cupo 256 y `evict_oldest_job`
descarta el más antiguo sin ejecutarlo. Solo el propio job de e1000e pone
`poll_pending=false` y rearma IMS (líneas 1745-1746); si lo evictan bajo
presión de otros drivers, no hay ningún camino de recuperación — la NIC
deja de recibir interrupciones permanentemente. El mismo patrón afecta a
`watchdog_job_scheduled` (línea 1606/1617-1618): un watchdog evictado mata
la vigilancia de enlace para siempre.

## Medio

### 7. Reensamblado multi-descriptor inalcanzable y sin contabilizar
`e1000e.rs:1316-1327`

Un fragmento no-EOP siempre llena el buffer completo (2048 B), así que
`pending.len() + len > BUF_SIZE` es **siempre verdadero** en el segundo
fragmento — la rama que fusiona (línea 1321) nunca se ejecuta, y el frame
se descarta **sin** incrementar `rx_dropped` (a diferencia de los otros
descartes, líneas 1287/1297). Peor: para un frame de 3+ descriptores, el
último fragmento se entrega solo, como si fuera el paquete completo
(contado como `rx_packets` válido). Hoy está inerte porque `RCTL.LPE` no se
activa (frames ≤1522B caben en un solo buffer de 2048B), pero es una bomba
de tiempo latente para cualquier futuro soporte de jumbo frames.

### 8. `TX_DROPPED`/`RX_CSUM_BAD` son estáticos globales del archivo
`e1000e.rs:345, 351`

Compartidos por todas las instancias de e1000e. Con 2 NICs, el contador de
"ACK perdido" del watchdog de una tarjeta (añadido específicamente para
diagnosticar el deadlock de descarga silenciosa) refleja los descartes de
la otra tarjeta.

### 9. `name = format!("eth{}", dev.loc.bus)` ignora device/function
`e1000e.rs:2274`

Dos funciones NIC en el mismo bus PCI (p. ej. una tarjeta multi-puerto)
reciben el mismo nombre de interfaz `ethN`.

### 10. `CTRL_FRCSPD`/`CTRL_FRCDPX` con los bits intercambiados
`e1000e.rs:119-120`

El valor real (datasheet/Linux) es `FRCSPD=bit11` (`0x800`),
`FRCDPX=bit12` (`0x1000`); aquí están al revés. Inofensivo hoy porque
ambos siempre se usan juntos (líneas 748, 1019), pero es una trampa para
cualquier código futuro que toque uno solo (p. ej. un workaround de
velocidad forzada).

## Bajo

- **`FWSM_FW_VALID` definida como bit 14 en vez de bit 15** (`e1000e.rs:202`) —
  la constante correcta (`ICH_FWSM_FW_VALID`, bit 15) coexiste sin usarse
  la primera; constante muerta y engañosa.
- **`TIPG` con IPGR2=12 en vez del valor de datasheet 6** (`e1000e.rs:1101`) —
  solo relevante en enlaces half-duplex, prácticamente extintos en Gigabit.
- **Timeout de latch de `TXDCTL.QUEUE_ENABLE` en SPT ignorado en silencio**
  (`e1000e.rs:1108-1113`) — si nunca se activa, el TX queda muerto sin
  ningún log de error ni fallo reportado por `init_tx`.

## Metodología y cobertura

Búsqueda por 6 lentes independientes (RX, TX, inicialización de hardware,
concurrencia, seguridad de memoria, integración con el resto del SO) sobre
`drivers/src/net/e1000e.rs`, comparando contra `e1000.rs` (driver hermano),
`drivers/src/utils/dma_sync.rs`, `drivers/src/utils/deferred_job.rs`, el
crate `lock` (`vendor/kernel-sync`) y el mapa de registros conocido de
Intel 82574/e1000e/I219 (PCH).

Las lentes de "seguridad de memoria" e "integración" no llegaron a
completar su verificación adversarial automática (falta de créditos del
modelo a mitad de sesión); los 10 hallazgos anteriores fueron verificados
manualmente releyendo el código y contrastándolo contra el driver hermano
y el conocimiento del mapa de registros de Intel/Linux — no contra el
datasheet oficial en vivo, así que las afirmaciones sobre offsets de
registro (hallazgos #3, #10, y los de baja severidad) conviene
contrastarlas antes de aplicar una corrección.

## Correcciones aplicadas

| # | Hallazgo | Corrección |
|---|----------|------------|
| 1 | `ROUTES_STORAGE` aliasing | `Box::leak` de un buffer fresco por NIC en vez de un `static mut` compartido. |
| 2 | TX completion vía TDH | `can_send()` lee el bit DD del descriptor en `tx_tail` (con `dma_sync` FromDevice) en vez de la aritmética TDH; `init_tx` pre-marca todos los descriptores TX con DD=1. |
| 3 | Offsets FEXTNVM6/7 | Corregidos a `0x00010`/`0x000E4` (mapa de registros real). |
| 4 | `RCTL_SECRC` solo en PCH | Ahora incondicional, como `e1000.rs`. |
| 5 | IDs igb en `matched()` | Se retiraron `0x1533`/`0x1539`/`0x157b`/`0x157c`. |
| 6 | Eviction de la cola diferida deja `poll_pending`/IMS atascados | `heal_stuck_poll_pending()`, llamado desde `NetScheme::poll()` (alcanzado por el polling periódico independiente de IRQ), limpia el flag si lleva >500 ms atascado. |
| 7 | Tope de reensamblado RX inalcanzable | Nuevo `MAX_RX_FRAME_BYTES = BUF_SIZE * 16`; el descarte por tope ahora también incrementa `rx_dropped`. |
| 8 | `TX_DROPPED`/`RX_CSUM_BAD` globales de archivo | Movidos a campos `tx_dropped`/`rx_csum_bad` de `E1000eHw` (por instancia). |
| 9 | Colisión de nombre `eth{bus}` | Se añade `_{device}_{function}` cuando no son ambos cero. |
| 10 | Bits `CTRL_FRCSPD`/`CTRL_FRCDPX` intercambiados | Corregidos a bit 11 / bit 12 respectivamente. |
| 11 | `FWSM_FW_VALID` con bit incorrecto | Constante duplicada y sin uso, eliminada (queda `ICH_FWSM_FW_VALID`, correcta). |
| 12 | `TIPG` IPGR2=12 | Corregido a 6 (valor de datasheet). |
| 13 | Timeout de `TXDCTL.QUEUE_ENABLE` silencioso | Ahora emite `klog_warn!` si no llega a activarse en 10 ms. |

Cobertura de test nueva: módulo `tx_ring_tests` (4 tests) que ejercita el
nuevo camino DD-bit de TX contra un NIC simulado — arranque con todos los
slots libres, reutilización bloqueada hasta el write-back de DD, uso del
anillo completo sin slot de guarda, y vuelta de anillo intercalada con
completions.

## Rendimiento (2026-08-06, segunda pasada)

Tras la corrección de bugs, el usuario reportó que el driver "va muy lento".
No era una regresión de los fixes anteriores — era un patrón preexistente en
el camino RX que nunca se había medido: **cero agrupamiento de accesos MMIO
por paquete**. Cada paquete recibido, sin importar cuántos llegaran en la
misma ráfaga, pagaba su propio round-trip MMIO completo.

### Lo que hacía antes, por paquete

En `process_rx_slot` / `receive` / `recycle_rx_slot`:

1. `process_rx_slot` releía RDH (`mmio_read`) al empezar, para comprobar si
   había algo nuevo que procesar.
2. Si no había paquete completo, `receive`'s bucle de drenaje **volvía a leer
   RDH** una segunda vez para decidir si cortar — hasta 2 lecturas MMIO de
   RDH por iteración.
3. Al reciclar el descriptor consumido, `recycle_rx_slot` hacía
   `mmio_write(RDT, i)` seguido de un `mmio_read(RDT)` de "flush" —
   **una lectura síncrona que fuerza al CPU a esperar la vuelta completa
   PCIe** — en cada paquete individual, nunca agrupado.
4. `ensure_rx_armed_if_link_up`, invocada en cada poll (con o sin tráfico),
   releía STATUS por MMIO incluso cuando el enlace ya se sabía activo.
5. La invalidación de caché del buffer RX (`dma_sync_region`, ruta
   write-back) siempre invalidaba los `BUF_SIZE` (2048) bytes completos del
   slot, sin importar que el frame real midiera 60 bytes (p. ej. un ACK) —
   hasta 32 líneas de caché invalidadas para leer 1.

En hardware real un `mmio_read` fuerza al CPU a esperar una transacción de
finalización PCIe (cientos de ns a varios µs según la topología). Bajo
emulación (QEMU, el objetivo de pruebas habitual de este driver) **cada
acceso MMIO — lectura o escritura — típicamente dispara una VM exit
completa**, con un coste de varios µs solo de entrada/salida del hipervisor,
antes de que el modelo del dispositivo haga nada. Con 3-4 transacciones MMIO
por paquete recibido, la sobrecarga de "contabilidad" dominaba por completo
el coste real de mover los bytes, sobre todo bajo ráfagas.

### Cambios aplicados

Todos en `drivers/src/net/e1000e.rs`, todos solo en el camino RX (el TX ya
posteaba el doorbell TDT sin lectura de flush, así que no tenía el mismo
patrón):

1. **`receive()` cachea RDH una sola vez por llamada** (`let rdh =
   self.rx_rdh();`) en vez de releerlo por MMIO en cada iteración del bucle
   de drenaje. `process_rx_slot` ya no relee RDH — el invariante (`head !=
   rdh`) lo garantiza quien llama. Un paquete que llegue justo después del
   snapshot simplemente se recoge en la siguiente llamada a `receive()`;
   es el comportamiento normal de un drenaje por presupuesto, no un fallo de
   corrección.
2. **El doorbell RDT se difiere y se agrupa** (`rx_doorbell_dirty` +
   `flush_rx_doorbell()`): reciclar un descriptor ya no escribe RDT
   inmediatamente, solo marca el flag. El flush real ocurre una vez por
   ráfaga — al final de `poll_with_irq_hint` (tras el `iface.poll()` de
   smoltcp, que puede haber drenado muchos paquetes) y al final de
   `NetScheme::recv()` (que no pasa por `poll_with_irq_hint`). Diferirlo es
   seguro: solo retrasa avisar al hardware de buffers ya liberados, nunca
   bloquea el avance de nada. Se eliminó también la lectura de flush
   síncrona — TX nunca la tuvo, tampoco hacía falta aquí.
3. **`ensure_rx_armed_if_link_up` no relee STATUS si `link_up` ya es true**
   — el único efecto de la función es ponerlo a `true`, así que si ya lo
   está, la lectura no hace nada. Las transiciones a enlace caído las sigue
   detectando `watchdog_tick` en su propio ciclo.
4. **La invalidación de caché del buffer RX usa `len` real, no `BUF_SIZE`**
   — nada lee más allá de `len` (el slice construido justo después es
   `..len`), así que invalidar el buffer completo en cada paquete no
   aportaba nada, solo coste.

### Verificación

Nuevo test `rx_doorbell_is_batched_not_rung_per_packet` que entrega 5
paquetes, drena los 5 sin flush intermedio y comprueba que RDT no se mueve
hasta llamar a `flush_rx_doorbell()` explícitamente, y que un segundo flush
sin nada nuevo reciclado es un no-op. `rx_single_packet_roundtrips` se
actualizó para reflejar el nuevo contrato (hay que flushear antes de mirar
RDT). Los 13 tests existentes (RX, TX, coherency bench) siguen en verde.
`cargo build -p zcore-drivers` (build `no_std` real) y `cargo clippy`
limpios.

## Auditoría 2026-09-03 (segunda pasada de bugs)

Nueva lectura completa de `drivers/src/net/e1000e.rs` (3690 líneas, tras el
commit "mejoras en el driver e1000e y en la velocidad de red"), de nuevo
comparando con `e1000.rs`, `utils/deferred_job.rs`, `utils/dma_sync.rs`, el
uso que hace `linux-object/src/net` de la interfaz y el comportamiento
documentado de Intel/Linux e1000e. **Los 5 hallazgos están corregidos**;
verificado con `cargo test -p zcore-drivers --lib --features mock e1000e`
(23 tests, 3 nuevos), `cargo build -p zcore-drivers` (`no_std`) y `cargo
clippy` limpio para este archivo.

### Alto

#### 14. El anillo TX se llenaba del todo y TDT alcanzaba a TDH: TX muerto para siempre
`can_send` / `post_tx_frame` (antes `tx_dd_at_tail`)

La corrección #2 pasó a decidir "slot libre" solo por el bit DD del
descriptor, y el test `tx_fills_the_full_ring_with_no_guard_slot` consagraba
que se pudieran postear los 256 slots. Pero el NIC no tiene bit de propiedad
propio: procesa descriptores desde TDH hasta TDT (exclusivo) y **interpreta
`TDH == TDT` como anillo vacío**. Con 256 en vuelo la cola de software da la
vuelta hasta la cabeza y el `flush_tx_doorbell` escribe un TDT igual a TDH:
el hardware no ve trabajo, nunca busca esos descriptores, sus DD nunca
vuelven y cada `send` posterior espera `TX_SEND_SPIN_LIMIT` y descarta —
TX inutilizable hasta reiniciar. Requiere que el NIC esté parado mientras el
software postea una vuelta completa (enlace oscilando durante una subida con
`link_up` aún en caché, pausa de flow control), cosa que QEMU (TX síncrono)
no reproduce: solo aparece en hardware real. Linux acota lo pendiente a
`count - 1` en `e1000_desc_unused` por esta misma razón.

*Corrección:* `tx_can_post` exige DD en `tx_tail` **y** en `tx_tail + 1`
(slot de guarda), de modo que nunca hay más de `NUM_TX - 1` descriptores en
vuelo y TDT no puede alcanzar a TDH por detrás. Tests reescritos:
`tx_keeps_one_guard_slot_so_tdt_never_meets_tdh` comprueba que el envío 256
se rechaza y que `TDT != TDH`, y los demás tests TX modelan la guarda.

### Medio

#### 15. El offload de checksum RX confiaba a ciegas en el NIC
`capabilities()` → `Checksum::Tx`, `process_rx_slot`

Al activar `RXCSUM.IPOFLD|TUOFLD` se le dijo a smoltcp que no verifique
IPv4/TCP/UDP en recepción. Eso solo es válido para tramas que el NIC ha
validado de verdad, y lo indica por descriptor: `status.IPCS/TCPCS/UDPCS`
("lo calculé"; el fallo aparece en `errors.IPE/TCPE` y ya se descartaba) e
`IXSM` ("ignora mi indicación"). El driver nunca miraba esos bits, así que
cualquier trama que el hardware no validase — TCP/UDP sobre IPv6 en partes
sin offload IPv6, cualquier cosa marcada IXSM — llegaba a smoltcp **sin
ninguna comprobación de checksum** y un segmento corrupto se entregaba a
userspace como datos buenos. Linux toma esta decisión por paquete en
`e1000_rx_checksum`; las capacidades de smoltcp son estáticas, así que la
verificación de respaldo tiene que hacerla el driver.

*Corrección:* `rx_csum_needs_sw_check(status)` + `rx_sw_csum_bad(frame,
status)`: solo cuando faltan los bits (o hay IXSM) se verifica en software
la cabecera IPv4 y el checksum TCP/UDP (IPv4 e IPv6, saltando extension
headers comunes); el caso común IPv4 TCP/UDP validado sigue costando una
comparación de bits. Descarte contado en `rx_dropped` y `rx_csum_bad`.
Tests: `rx_sw_csum_accepts_good_and_rejects_corrupt_frames`,
`rx_drops_unvalidated_corrupt_frame_and_keeps_validated_ones`; el mock
`hw_deliver_with_status` modela qué validó el NIC.

#### 16. Un watchdog evictado de la cola diferida mataba la vigilancia de enlace para siempre
`schedule_watchdog`

Era la segunda mitad del hallazgo #6, que quedó sin corregir: el `Guard`
que limpia `watchdog_job_scheduled` se construía **dentro** del closure, así
que solo se ejecutaba si el job llegaba a correr. `deferred_job.rs` evicta
(dropea) entradas sin ejecutarlas al llegar a 256; un watchdog evictado
dejaba el flag en `true` y ningún `schedule_watchdog` volvía a encolar nada:
sin detección de link up/down ni logs del watchdog hasta reiniciar.

*Corrección:* el guard se crea fuera y se mueve dentro del closure, de
modo que se dropea (y limpia el flag) tanto si el job corre como si es
evictado. El mismo patrón se aplica al bottom-half de IRQ con
`PollPendingGuard`: al dropearse limpia `poll_pending` y rearma IMS, así una
eviction se recupera al instante en vez de esperar los 500 ms de
`heal_stuck_poll_pending` (que se mantiene como red de seguridad).

### Bajo

#### 17. `TxToken::consume` giraba 4096 iteraciones con IRQs apagadas si el enlace estaba caído
Con `link_up == false`, `post_tx_frame` devolvía `NotReady` tras releer
STATUS por MMIO, y el bucle de espera lo trataba como "anillo lleno":
4096 lecturas MMIO (varios ms bajo QEMU, ~4 ms en hardware) por cada trama
que smoltcp intentara enviar, con el lock de `hw` (IRQs off) sostenido.
*Corrección:* comprobación de enlace antes del bucle; sin enlace se devuelve
`Exhausted` inmediatamente (sin contar en `tx_dropped`, que significa
"perdido con enlace arriba").

#### 18. `restart_autoneg` probaba primero la dirección MDIO 1 en partes PCH
En 82577/8/9, I217/I218/I219 los registros MII estándar están en la
dirección 2 (Linux `e1000_get_phy_addr_for_hv_page`: páginas < 768 → 2); la
dirección 1 es el bloque MAC-side/wakeup. Probar la 1 primero "tenía éxito"
(MDIC_READY sin error), hacía `break`, y el reinicio de autoneg por BMCR
nunca llegaba al PHY real. *Corrección:* orden `[2, 1]` en `is_pch()`,
`[1, 2]` en discretas (82574). Sin efecto en QEMU; en I219 real el enlace ya
subía por SLU/ASDE + LANPHYPC, así que solo debería notarse como un
reinicio de autoneg efectivo.

### Observaciones sin cambio

- `NetScheme::recv` (usado por `netdev_drain_rx`/`drain_all_nic_rx` en
  `linux-object/src/net`) saca tramas del anillo **sin pasar por smoltcp**:
  cualquier segmento TCP que llegue mientras ICMP/AF_PACKET drenan se pierde
  para los sockets TCP (retransmisión). Es un problema de la capa de red del
  kernel, no del driver.
- `E1000E_SRRCTL` (0x280C) es un registro de la familia igb; en 82574/I219 ese
  offset está reservado en el mapa e1000e. La escritura es inocua en el
  hardware probado, se deja tal cual.
- `IMS` se habilita en `reset_and_init` antes de que el vector MSI quede
  registrado (`pci_finish_msi_registrations`); un LSC temprano se pierde,
  pero el polling periódico y el watchdog lo cubren.
- Con el anillo TX en modo write-back (fallback si el remapeo UC falla) el
  sync ToDevice diferido de descriptores amplía la ventana de false sharing
  que motivó los anillos UC; el camino de producción (UC) no se ve afectado.
