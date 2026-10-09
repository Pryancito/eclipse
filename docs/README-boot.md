# El arranque de Eclipse: cómo se mide

«¿Se puede arrancar más rápido?» no se contesta leyendo el código. Las partes
caras del arranque no son las que calculan, son las que **esperan**, y hasta
ahora no había forma de verlas: el núcleo sella cada línea de `dmesg`, lo que
dice *cuándo habló*, no dónde se fue el tiempo entre dos líneas; y `eclipse-init`
imprimía sus líneas **sin hora ninguna**, así que un log de arranque pegado en el
chat enseñaba el *orden* del arranque y ocultaba su *forma* — una puerta de diez
segundos y otra de diez milisegundos se leían igual.

Este documento describe las dos medidas que ya existen. No propone recortes: un
recorte se justifica con una de estas tablas delante, no al revés.

## 1. Las dos mitades de la barra, en una tabla

La barra de progreso del arranque ya iba de punta a punta: **rboot pinta del 0 %
al 51 %** (`rboot::progress::bar`) y **el núcleo del 52 % al 100 %**
(`kernel_hal::console::early_progress_bar`), con marcas al final de cada tramo
caro. Esas marcas se dibujaban en el framebuffer y se olvidaban; ahora también
se **fechan**, y las dos mitades salen en la misma tabla. No hay ni una llamada
nueva: el coste es un `store` relajado por marca, y en un arranque entero hay
menos de treinta.

Las dos mitades no pueden medir igual, porque no tienen el mismo reloj:

- **rboot guarda `rdtsc` en crudo** (`rboot/src/marks.rs`), sin calibrar nada. El
  único reloj que el firmware ofrece calibrado es `BootServices::stall`, y
  llamarlo **gastaría** 10-20 ms del arranque para poder medirlo: medir no puede
  costar lo que se intenta recortar. Los contadores viajan al núcleo en
  `BootInfo::loader_marks`, que crece por la cola de la estructura (la
  convención de ABI de este `BootInfo`), y el núcleo los convierte a nanosegundos
  con `kernel_hal::cpu::tsc_hz()`.
- **El núcleo fecha con `timer_now()`**, ya en nanosegundos.

La tabla sale por consola al final del arranque, cuando `init(1)` ya corre, y
queda además al final de `/proc/perf/kernel`, que es donde uno se pregunta por el
arranque en una máquina que ya está encendida:

```
boot timeline — 26 marks, 8127.226ms to the last one
  gap = time spent in the stretch ENDING at that mark;
  marks below 81% are timed with the provisional TSC frequency.
  before 0%: about 2777.288ms in the firmware, from processor reset to rboot
  (the TSC counts from reset -- a warm reset or a hypervisor need not start it at 0).
   mark          at         gap  stretch
     0%     0.000ms     0.000ms  rboot: config read, GOP mode set, splash drawn
    15%   190.897ms   190.041ms  rboot: kernel ELF read off the ESP
    45%  3748.113ms  3557.216ms  rboot: initramfs read off the ESP
    46%  7191.059ms  3442.945ms  rboot: memory map walked, kernel ELF and stack mapped
    47%  7199.858ms     8.799ms  rboot: all of physical memory mapped
    49%  7299.951ms    99.793ms  rboot: ExitBootServices
    52%  7300.396ms     0.000ms  entered the kernel (arch entry)
    87%  7838.000ms   454.012ms  PCI scan
   100%  8127.226ms   154.744ms  init(1) running
```

Esa línea `before 0%` es el **firmware antes de rboot**: el TSC de la marca 0 %
cuenta desde el reinicio del procesador, así que su valor ya es lo que tardó la
UEFI en llegar a nuestro cargador. Es la única parte del arranque que no es
nuestra, y conviene saber cuánto vale antes de celebrar un recorte.

La columna que se lee es **`gap`**: lo que costó el tramo que *termina* en esa
marca. `at` está solo para alinear la fila con una hora de `dmesg`.

Dos advertencias que la propia tabla imprime, porque quien no las sepa sacará la
conclusión equivocada:

- **Las marcas por debajo del 81 % van fechadas con la frecuencia *provisional*
  del TSC.** La de verdad se mide contra el temporizador PM de ACPI al empezar el
  sondeo de dispositivos (`recalibrate_tsc_hz`, la marca 81 %); en una máquina
  cuya estimación provisional estaba mal, esas fechas tempranas están mal por el
  mismo factor — y la línea `[tsc]` de `dmesg` dice por cuánto.
- **Un `gap` es tiempo de reloj, no trabajo.** Un tramo que espera a un firmware
  o a un dispositivo se ve exactamente igual que uno que calcula.

Añadir una marca nueva es una llamada a `early_progress_bar` (o a `step` en
`rboot/src/main.rs`) y una línea en `boot_marks::label`. Una marca sin nombre
sale como su número, no como un panic: nadie tiene que pasar por ese fichero
antes de instrumentar algo. Las filas se ordenan **por hora, no por
porcentaje**, porque una marca que se pinta dos veces o fuera de orden es
precisamente lo que se quiere ver.

## 2. La mitad del init: `boot timeline` de `eclipse-init`

Dos cambios en `tools/eclipse-init`:

1. **Cada línea lleva su hora** desde la primera instrucción del PID 1:
   `[eclipse-init] [   3.214s] respawn: labwc (starting)`. Es la mitad barata de
   hacer medible el arranque, y basta para leer un log pegado.
2. **Cada espera se cronometra** y al acabar el bucle de arranque se imprime la
   tabla, de peor a mejor, junto con lo que las filas **no** explican. Queda
   también en `/run/eclipse-boot-timeline`.

```
boot timeline: 6.204s to the supervision loop, 4.930s of it waiting
     2.011s   32%  at    3.918s  labwc: wait for /dev/input to settle
     1.902s   30%  at    1.004s  gtk-caches: run the oneshot
     ...
     1.274s   20%            everything not timed above (forks, execs, short steps)
```

Lo que se cronometra son **hojas**, nunca un envoltorio alrededor de varias:
cada puerta `wait_socket =`, cada `wait_path =`, la ventana de reposo de un
directorio aparte de su aparición (son costes distintos con curas distintas: uno
es el núcleo enumerando dispositivos, el otro una ventana fija que elige este
init), la ejecución de cada `oneshot`, los montajes de pseudo-sistemas y los
cuatro `apply_*`. Así las filas suman **menos** que el total y la diferencia es
tiempo real sin explicar.

Esa última fila, `everything not timed above`, es la cifra que dice si la
instrumentación sigue estando corta: si `rest` es la mayor parte del arranque,
las esperas no son donde se va el tiempo y la siguiente medida tiene que ir a
otro sitio.

- Las filas por debajo de 5 ms no se listan (un arranque tiene una cola larga de
  pasos de microsegundos, y una tabla que los enumera es una que nadie lee hasta
  el final), pero **sí se contabilizan**, o `rest` dejaría de significar «tiempo
  que nada explica».
- Nada se graba una vez impresa la tabla. `start_service` sirve igual al arranque
  que a cada reinicio posterior, así que un `respawn` que se caiga una hora
  después pasa por las mismas puertas; grabar esas esperas haría crecer la lista
  mientras la máquina viva y reescribiría la historia de un arranque ya acabado.

## 3. Lo que las medidas justificaron

### El init: los tres ajustes a la vez

Los tres `apply_*` del init — teclado, idioma, zona — eran 1,13 s de 1,46 s, y
son independientes entre sí: ahora se lanzan a la vez. La cuenta, el cerrojo que
lo hace seguro y la escotilla (`init.serial_setup`) están en
`docs/README-init.md`.

### rboot: el mapa físico en páginas de 2 MiB

La primera tabla completa dio un resultado que cambia la pregunta: **de los 11,8 s
del arranque medido, 10,9 s eran de rboot**, y todo lo que se había medido antes
—núcleo e init juntos— era el 15 % del arranque. Dentro de rboot, el tramo que
acababa en la marca 47 % costaba **6,98 s**, pero esa marca medía dos cosas a la
vez, así que lo primero fue **partirla**: la 46 % cierra con el ELF y la pila del
núcleo mapeados, la 47 % con la memoria física. Partida, el reparto sale **mitad y
mitad**: unos 3,4 s el ELF y unos 3 s el mapa físico.

Esos 3 s eran una sola función: `page_table::map_physical_memory` mapeaba
**cada marco de 4 KiB** de la memoria física, uno por uno. Doce gigas son
3.145.728 llamadas, cada una recorriendo cuatro niveles de tabla y pidiendo
marcos nuevos al firmware. El coste es **lineal en la RAM de la máquina** y el
suelo son 4 GB pase lo que pase (`max` sobre `0x1_0000_0000` en `main.rs`), así
que una máquina grande arranca más despacio por tenerla.

Los mismos doce gigas son **6.144 mapeos con páginas de 2 MiB**, y eso es lo que
hace ahora: la marca 47 % pasó de **2800,8 ms a 8,8 ms**, 318 veces menos. Esas
dos cifras son el mismo binario arrancado dos veces, con y sin la escotilla de
abajo, que es la única comparación que vale: bajo TCG un mismo tramo varía un
30 % de arranque a arranque, así que una cifra de hoy contra una de ayer no
prueba nada. Lo que no es gratis, y es la razón de que la firma tenga un
argumento nuevo:

- `map_physical_memory(offset, max_addr, fine, m)` recibe en `fine` los rangos
  **físicos que tienen que conservar hojas de 4 KiB**. Hay uno y no es opcional:
  el núcleo repasa las entradas del framebuffer de arranque para ponerlas en
  *write-combining* después del sondeo PCI (`pat::enable_framebuffer_wc`), y eso
  **solo puede hacerlo sobre una hoja de 4 KiB** — una hoja grande la salta,
  correctamente, pero deja el framebuffer en *write-back*, que en hardware de
  verdad se midió como 300 FPS contra 2000 en `glxgears`. Cualquier bloque de
  2 MiB que solape un rango de `fine` se mapea a la antigua.
- Un bloque para el que el firmware (o una pasada anterior) ya tiene una entrada
  también cae a 4 KiB, donde `map_page` tolera una entrada que ya apunta al mismo
  marco. Así sobreviven los megas bajos que la UEFI mapeó ella misma.
- `Machine::map_huge` **no tiene implementación por omisión a propósito**. Una
  reserva silenciosa haría que los tests del anfitrión estuvieran contentos con
  un cargador que hubiera vuelto a tardar siete segundos.
- La escotilla es **`PHYSMAP4K`** en la línea de órdenes, que pasa un `fine` que
  cubre todo: exactamente el comportamiento anterior. Como `justrun` reescribe
  el `cmdline` del `rboot.conf` del ESP sin recompilar, `make justrun
  KOPTS=PHYSMAP4K` da el A/B con un mismo binario.

### Lo que queda sin tocar

Los dos tramos gordos que quedan en rboot, por orden:

- **La lectura del initramfs**, la marca 45 %: 3,49 s — y esta fila es una
  trampa que conviene leer entera antes de intentar recortarla.

  Lo que `make qemu` pone en `\EFI\zCore\initramfs.img` **no es el initramfs de
  arranque de 75 MiB**, es la imagen viva de la variante, **396 MiB**. Una
  máquina instalada lee la de 75 MiB. Y el driver FAT del firmware **no es
  lento**: 396 MiB en 3486 ms son **119 MB/s**, y la lectura del ELF del núcleo
  de la marca 15 % da **122 MB/s** por separado — dos medidas independientes que
  concuerdan. A ese ritmo los 75 MiB de una máquina instalada son unos **660 ms**.

  Así que no hay nada que recortar aquí hasta tener la tabla de una máquina de
  verdad: lo que decide esta fila es **qué fichero se lee**, no a qué velocidad, y
  en QEMU se lee uno cinco veces más grande que el que se arranca de verdad. De
  los 75 MiB instalados, unos 23 son el blob de firmware GSP de NVIDIA, que está
  ahí a propósito porque la GPU lo necesita antes del pivote a la raíz btrfs
  (ver `xtask/src/linux/image.rs`).
- **El mapeo del ELF del núcleo**, la marca 46 %: 3,44 s. No es el `.text`: el
  último `PT_LOAD` del núcleo tiene `FileSiz` 0 y **`MemSiz` 548 MiB**, y de
  ellos 512 MiB son un solo objeto, `zcore::memory::init::HEAP` — el montón del
  núcleo, reservado en `.bss`. rboot lo mapea marco a marco (133.936 marcos, un
  `allocate_pages` del firmware por cada uno) y lo pone a cero entero, en cada
  arranque. Hay dos recortes independientes ahí: pedirle al firmware trozos
  grandes en vez de un marco por llamada, y mapear ese segmento con páginas de
  2 MiB como ya se hace con el mapa físico. Que el montón sea de 512 MiB
  estáticos es una decisión aparte, y no es nuestra.

  **El primero de los dos ya está hecho**: `rboot/src/frames.rs` pide la memoria
  al firmware de cuatro mebibytes en cuatro mebibytes y reparte marcos bumpeando
  un puntero, así que las ~134.000 llamadas a `allocate_pages` son 137. La marca
  46 % pasó de **3442,9 ms a 2238,6 ms**. Lo que queda ahí dentro son dos cosas
  que ninguna marca separa todavía: los ~134.000 `map_to` de 4 KiB y el
  `write_bytes` de 548 MiB. Las dos son caras **sobre todo bajo TCG**, así que el
  reparto de una máquina real puede hacer que no valga la pena tocarlas.
- **El sondeo PCI del núcleo**: 441 ms de 882 ms en QEMU, pero esa cifra es de un
  PCI *emulado*. Antes de tocarlo hace falta la tabla de una máquina de verdad.

Y las dos advertencias que valen para todas estas cifras:

- **Son de QEMU con TCG**, sin aceleración. Una pasada que *mapea* (el mapa
  físico) es trabajo del MMU y se recorta igual en hardware; una que *copia o
  pone a cero* mucha memoria la emulación la castiga mucho más de lo que lo hará
  una máquina de verdad.
- **No es la misma imagen.** `make qemu` arranca la imagen viva de la variante;
  una máquina instalada arranca el initramfs de 75 MiB y pivota a btrfs. La fila
  del 45 % es la que más cambia por esto, pero el tamaño de la imagen también
  mueve `max_phys_addr` y con él el mapa físico.

Por eso el reparto de una máquina real puede ser distinto, y es la tabla que
falta.

## 4. Lo que ya se sabía medir, y no es esto

- `BOOTTRACE=<comm>` en la línea de órdenes del núcleo graba **cada fichero que
  abre un proceso** (`linux-object/src/boot_trace.rs`) y lo publica en
  `/proc/bootprofile`, con una lista de precarga deduplicada. Es la herramienta
  para «¿por qué tarda labwc en dar el primer fotograma?», no para «¿dónde se va
  el arranque?».
- `/proc/perf/kernel` (ver `docs/README-performance.md`) mide la máquina
  **encendida**: ocupación, IRQ, planificador.
