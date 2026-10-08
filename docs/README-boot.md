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

## 1. La mitad del núcleo: `boot timeline`

El arranque ya recorría una barra de progreso del 52 % al 100 %
(`kernel_hal::console::early_progress_bar`), con marcas al final de cada tramo
caro. Esas marcas se dibujaban en el framebuffer y se olvidaban; ahora también
se **fechan** (`kernel-hal/src/common/boot_marks.rs`). No hay ni una llamada
nueva: el coste es un `store` relajado por marca, y en un arranque entero hay
menos de veinte.

La tabla sale por consola al final del arranque, cuando `init(1)` ya corre, y
queda además al final de `/proc/perf/kernel`, que es donde uno se pregunta por el
arranque en una máquina que ya está encendida:

```
boot timeline — 16 marks, 1832.441ms to the last one
  gap = time spent in the stretch ENDING at that mark;
  marks below 81% are timed with the provisional TSC frequency.
   mark        at       gap  stretch
    87%  912.330ms  402.118ms  PCI scan
    91%  1402.11ms  310.882ms  root filesystem mounted
   ...
```

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

Añadir una marca nueva es una llamada a `early_progress_bar` y una línea en
`boot_marks::label`. Una marca sin nombre sale como su número, no como un
panic: nadie tiene que pasar por ese fichero antes de instrumentar algo.

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

## 3. Lo que ya se sabía medir, y no es esto

- `BOOTTRACE=<comm>` en la línea de órdenes del núcleo graba **cada fichero que
  abre un proceso** (`linux-object/src/boot_trace.rs`) y lo publica en
  `/proc/bootprofile`, con una lista de precarga deduplicada. Es la herramienta
  para «¿por qué tarda labwc en dar el primer fotograma?», no para «¿dónde se va
  el arranque?».
- `/proc/perf/kernel` (ver `docs/README-performance.md`) mide la máquina
  **encendida**: ocupación, IRQ, planificador.
