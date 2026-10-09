# `eclipse-init`: qué hace, cómo se compara y qué le falta

`tools/eclipse-init/src/main.rs` es el PID 1 del sistema instalado. El kernel de
Eclipse ya monta la raíz, levanta la red y abre los intérpretes de cada VT, así
que el init no necesita un gestor de servicios pesado (el `fork`/`exec` de
`busybox sh` por paso de OpenRC es justamente lo que estresaba las rutas
frágiles del kernel). Hace lo que PID 1 tiene que hacer: cosechar huérfanos,
montar los pseudo-sistemas que falten, arrancar lo declarado en
`/etc/eclipse/services/*.service` y apagar o reiniciar la máquina con las
señales de busybox.

Las claves de un `.service` son `exec`, `type` (`oneshot` | `respawn`),
`after`, `requires`, `wait_socket`, `wait_path`, `desktop`, `cmdline`, `log` y
`timeout`. Están documentadas en el propio `example.service.txt` que la imagen
publica en `/etc/eclipse/services/`.

## Comparación honesta

| Mecanismo | systemd | runit | s6-rc | OpenRC | launchd | SMF | eclipse-init |
|---|---|---|---|---|---|---|---|
| Supervisión y reinicio | sí | sí | sí | no (solo arranque) | sí | sí | sí |
| Backoff del lazo de crash | `RestartSec` | 1 s fijo | 1 s fijo | — | 10 s mínimo | sí | 250 ms → 8 s, exponencial |
| **Rendirse con un servicio que no puede funcionar** | `StartLimitBurst` → `failed` | no | no | — | *throttle* | → `maintenance` | **sí (20 caídas seguidas, o 20 arranques en media hora)** |
| **Límite de tiempo al arrancar** | `TimeoutStartSec` 90 s | no | `timeout-up` | sí | `ExitTimeOut` | obligatorio | **sí (90 s, `timeout =`)** |
| Orden por dependencias | sí | no | sí | sí | no | sí | sí (`after =`, topológico) |
| Espera de *readiness* | `Type=notify` | no | *fd* de notificación | no | *socket activation* | no | sondeo de socket/ruta acotado |
| Propagación del fallo de una dependencia | `Requires=` | — | sí | sí | no | sí | **sí (`requires =`)** |
| Límite del tamaño de los logs | journald | `svlogd` | `s6-log` | logrotate | sí | sí | **sí (1 MiB + 1 generación)** |
| Matar lo que quede al parar un servicio | cgroup | grupo de procesos | grupo de procesos | sí | sí | contrato | solo en el `timeout` de un `oneshot` |
| Sobrevive a un fallo propio | — | — | — | — | — | — | **sí (`unwind` + `catch_unwind`)** |
| Apagado ordenado | sí | sí | sí | sí | sí | sí | **no, a propósito** (ver abajo) |

Dos casillas vacías son decisiones tomadas, no huecos:

- **No hay apagado ordenado.** `shutdown()` hace `sync` + `reboot(2)`, la ruta de
  `busybox reboot -f`. Matar labwc y los clientes de GPU antes de la llamada
  colgaba el reinicio en este kernel, y el *quiesce* de los dispositivos
  (GSP-RM/WPR2, NVMe CC.SHN) ya ocurre dentro de `kernel_hal::cpu::reset`.
- **No hay protocolo de *readiness*.** Las puertas `wait_socket =` y
  `wait_path =` cubren los dos casos reales (seatd y los nodos de
  `/dev/input`) sin pedirle nada al servicio.

## Lo que se cerró en esta tanda

1. **Un `oneshot` que no termina ya no cuelga el arranque.** El `waitpid` de un
   `oneshot` no tenía límite: cada espera acotada de este fichero lo tiene, pero
   esa no, así que un solo envoltorio colgado dejaba la máquina muerta — no
   arrancaba ningún servicio posterior, no se llegaba al lazo de supervisión y,
   como las banderas de apagado solo se leen ahí dentro, Ctrl-Alt-Del tampoco
   hacía nada. Ahora hay un límite de 90 s (el de `TimeoutStartSec`), ajustable
   por servicio con `timeout = <segundos>` y desactivable con `timeout = 0`.
   Al pasarse: SIGTERM al **grupo de procesos** (el trabajo de verdad de estos
   envoltorios es un nieto), SIGKILL dos segundos después, y si ni así muere, se
   deja atrás y el arranque sigue. Un `oneshot` que falla, además, ya lo dice:
   antes era completamente silencioso.
2. **Un `respawn` que nunca llega a levantar deja de reintentarse.** El backoff
   abarata el lazo de crash pero no lo termina: un servicio que no puede
   funcionar imprimía su muerte cada 8 s mientras la máquina estuviera
   encendida, y esa consola es el único diagnóstico que tiene la caja. A las 20
   caídas seguidas sin llegar nunca a `HEALTHY_UPTIME` se da por perdido para el
   resto del arranque, con una línea que dice dónde está su log. Mucho más
   paciente que systemd (5 intentos en 10 s) a propósito: con el backoff en 8 s
   son unos dos minutos y medio de intentos, así que nada que simplemente tarde
   en encontrar su dependencia se descarta. Una sola vuelta sana reinicia la
   cuenta.

## Lo que se cerró en la segunda tanda

**El fallo de una dependencia se propaga.** `after =` solo ordenaba; ahora hay
`requires =`, el `Requires=` de systemd. Cuando init da por perdido un servicio
para el resto del arranque (su `exec =` no existe, o se ha pasado del límite de
caídas), todo lo que lo requiere se descarta también, en cadena y diciéndolo una
vez por servicio. Antes, con `seatd` descartado, labwc gastaba sus veinte
intentos pagando la puerta de 10 s en cada uno contra un socket que nadie iba a
crear, y detrás lunarbar y lunarbg hacían lo mismo: tres servicios llenando la
consola con sus muertes en vez del único fallo que importaba.

Dos cosas por las que no es el `Requires=` de systemd tal cual:

- **`requires =` implica `after =`.** En systemd son independientes, y la trampa
  que deja es una unidad que requiere a otra sin estar ordenada detrás, así que
  arranca a su lado. Aquí no se puede escribir.
- **Un requisito que no es un servicio de este arranque se ignora**, igual que
  lo ignora el orden topológico. Una sesión que no trae esa dependencia no
  pierde por eso todo lo que va detrás.

Lo llevan los ocho pares en los que no es una opinión: el servicio espera en
`wait_socket` justamente el socket que crea el otro (labwc → seatd, lunarbar /
lunarbg / xkbmap → labwc, dbus-selftest → dbus) o no tiene sentido sin él
(boot-sound → pulseaudio). Un test de `xtask` fija esa lista y otro comprueba
que ningún `requires =` nombra un servicio que la imagen no escriba, porque un
nombre mal escrito ahí sería silencioso.

## Lo que se cerró en la tercera tanda

**Los logs ya no se comen la RAM.** Todos los `log =` que publica la imagen están
en `/tmp`, que es un **tmpfs**: sus bytes son memoria de la máquina, y nada los
acotaba. El agujero era peor justo donde más duele, porque el servicio que no
consigue dejar de caerse es el que más escribe. Ahora init mira los tamaños una
vez por minuto y, al pasar de 1 MiB, mueve el contenido a `<log>.1` y deja el
fichero a cero: cada servicio cuesta como mucho unos 2 MiB de RAM por mucho que
la máquina lleve encendida, y 1 MiB siguen siendo decenas de miles de líneas.

Tres detalles del cómo:

- **Copiar y truncar, no renombrar** (el `copytruncate` de logrotate). El
  servicio tiene el fichero abierto, así que un `rename` lo dejaría escribiendo
  en el inodo renombrado para siempre y el fichero nuevo vacío. Lo que se pierde
  es una línea escrita entre la copia y el truncado, la misma carrera que
  logrotate lleva veinte años teniendo, y es el precio de no tener que reabrir
  el fd de otro proceso. El `O_APPEND` del hijo es lo que hace que su siguiente
  escritura caiga al principio del fichero nuevo y no a un megabyte de agujero.
- **SIGALRM es el único reloj que tiene PID 1.** El lazo de supervisión se
  bloquea en `waitpid` mientras no haya nada pendiente, así que sin una alarma no
  miraría nunca; el manejador solo levanta la bandera y el lazo rearma. Es una
  alarma de un disparo, no un temporizador de intervalo, para que no siga
  saltando durante el apagado.
- **Por ruta distinta**: dos servicios pueden compartir un `log =`
  (`boot-sound` y `boot-sound-xorg` escriben los dos en `/tmp/boot-sound.log`) y
  rotarlo dos veces en la misma pasada se llevaría la generación recién
  guardada.

Lo que **no** cubre: un fichero que escriba un envoltorio por su cuenta y que no
sea el `log =` de ningún servicio. Init solo conoce la tabla.

## Lo que se cerró en la cuarta tanda

**Un fallo de init ya no es un fallo de la máquina.** El perfil de release era
`panic = "abort"`, así que cualquier `panic` en PID 1 —un índice, un borde
aritmético, un `unwrap` sobre algo que dijo el disco— era al instante
«Attempted to kill init»: la máquina entera parada por un error en una decisión.
Ningún otro init de la tabla tiene que resolver esto, porque todos son procesos
que el kernel puede permitirse perder; este no.

Ahora el perfil es `panic = "unwind"` y los dos sitios que ejecutan código de
decisión van dentro de un `guard`: arrancar un servicio (un fallo cuesta ese
servicio, no el arranque) y el lazo de supervisión (se vuelve a entrar, con una
pausa de 1 s para que un `panic` en la primera sentencia no sea un lazo caliente
llenando la consola). Un gancho de `panic` nombra el fichero y la línea en la
consola antes de desenrollar, porque en una máquina sin más diagnóstico que esa
consola, esa línea es el informe del fallo entero.

Dos detalles: `AssertUnwindSafe` vale aquí porque lo único que cruza el límite
es la tabla de servicios de init, y no hay invariante que un campo a medio
escribir pueda romper —un servicio con `pid` puesto y `started_at` sin poner se
lee como «arrancado ahora mismo», que es lo que habría concluido la siguiente
cosecha—; y lo único que el desenrollado puede envenenar de verdad es el
`Mutex` de `RENDERER_SAID`, que ya trataba un cerrojo envenenado como «dilo».
El desenrollado cuesta unos 17 KiB de binario, un 4%.

## Lo que se cerró en la quinta tanda

**Las esperas acotadas del arranque ya cosechan, y no era cosmético.** Durante
una puerta `wait_socket` —10 s en el caso de labwc— init no cosechaba a nadie, y
el problema no eran los zombis: la vida de un servicio se medía **al
cosecharlo**, así que un hijo que muriera en esa ventana se apuntaba la puerta
entera como tiempo vivido, pasaba de `HEALTHY_UPTIME`, se daba por sano y se
reiniciaba al instante con su backoff a cero. Es exactamente el fallo que tenía
el backoff cuando dormía dentro del lazo de supervisión, en otra ventana.

Ahora las esperas cosechan sin bloquear y **encolan** lo cosechado con el
instante de la muerte; el lazo vacía esa cola al principio de cada vuelta y
contabiliza cada salida con su propio instante. La cola es lo que lo hace
seguro: un `waitpid(-1)` suelto en una espera **se tragaría** la muerte de un
`respawn`, el lazo no vería nunca ese pid, el `pid` del servicio se quedaría
puesto para siempre y nadie lo reiniciaría.

La espera del `oneshot` es la única que no cosecha, a propósito: está vigilando
un hijo concreto y una cosecha de «cualquier hijo» se lo quitaría de debajo del
`waitpid` que lo espera.

La contabilidad salió del lazo a `note_exit`, que ya se puede probar: cuatro
tests, y el que importa fija que una caída de 40 ms durante una puerta de 10 s
sigue siendo una caída.

## Lo que se cerró en la sexta tanda

**Un `respawn` que duerme antes de rendirse ya no es inmortal.** El límite de
caídas de la primera tanda solo se dispara con un servicio que *nunca* pasa de
`HEALTHY_UPTIME`, y eso deja fuera a toda una familia de bucles: un envoltorio
que **duerme** antes de salir. Todos los `eclipse-*` de las imágenes lo hacen;
`eclipse-pulseaudio` es

```sh
command -v pulseaudio >/dev/null 2>&1 || { echo ...; sleep 8; exit 127; }
```

así que cada intento vivía 8 s, se leía como una vuelta sana, ponía la cuenta de
caídas a cero y se reiniciaba al instante con el backoff reseteado. En un
arranque `minimal` (que no trae pulseaudio) eso es, mientras la máquina esté
encendida:

```text
respawn: pulseaudio exited after 8.125682158s (exit 127 ...), restarting
respawn: pulseaudio exited after 8.032575278s (exit 127 ...), restarting
```

Un `sleep` dentro del servicio es exactamente para lo que está el backoff del
supervisor, y de paso desactivaba el backoff **y** el límite de caídas. Dos
cosas lo cierran:

1. **Un 126 o un 127 no es una vuelta sana**, dure lo que dure. Son los dos
   únicos códigos que no son la opinión del programa sino el informe de que no
   llegó a correr: el binario no está, o está y no se puede ejecutar. Ningún
   reintento arregla ninguno de los dos, así que cuentan como caída y el
   servicio acaba dándose por perdido con su línea y su log.
2. **Se cuentan los arranques**, no solo las caídas: más de 20 en media hora y
   se da por perdido, con cualquier código de salida y con cualquier vida. Es la
   red que coge los `sleep 60; exit 1`, que ningún código delata. La ventana es
   larga a propósito —los envoltorios duermen hasta 60 s— porque la de systemd
   (5 arranques en 10 s) no vería ninguno de estos bucles; y un `respawn` sano
   no se reinicia nunca, así que cada arranque es una muerte. Uno que se caiga
   una vez por hora no se descarta jamás.

Se cuenta en `start_service`, que es el único sitio por donde pasan todos los
arranques: un servicio cuyo intento duerme parece sano al salir y
completamente normal al entrar. La negativa **no** se apunta en la ventana, o
echaría de ella un arranque de verdad.

Los `sleep` de los envoltorios se quedan como están: ahora solo marcan el ritmo
de los reintentos, y lo que tenía que cambiar era el supervisor, al que ningún
`sleep` puede engañar.

## Los tres ajustes de arranque, a la vez

`eclipse-kbd --boot`, `eclipse-locale --boot` y `eclipse-tz --boot` son tres
guiones de shell, cada uno con ocho o diez `fork`/`execve` de applets de busybox
dentro (`awk`, `tr`, `dd`, `grep`, `mv`). Corriendo uno detrás de otro eran el
**85 % del arranque entero del init**: 1,13 s de 1,46 s en el arranque de QEMU
que mide `docs/README-boot.md`, contra 150 ms todo lo demás que hace el init
antes del primer servicio. En este núcleo lo caro es el `fork`/`execve`, y eso
son treinta seguidos en el camino crítico con el PID 1 bloqueado en `waitpid`.

Nada los ordena entre sí: cada uno lee su propio fichero de `/etc/eclipse` y la
línea de órdenes, y escribe el suyo. Así que se lanzan los tres y **luego** se
espera a los tres. Siguen terminando todos antes de que arranque el primer
servicio, porque ese servicio tiene que ver ya el idioma y la zona en su
entorno; lo único que cambia es que las tres esperas se solapan.

Lo que lo hace seguro, y sin lo cual esto sería un fallo de distribución de
teclado un arranque más tarde: los tres **sí** escriben un fichero común, el
`environment` de labwc, y tres lecturas-modificaciones-escrituras simultáneas de
un fichero pierden claves (cada una lee el fichero viejo y gana el último `mv`).
Los guiones toman ahora un cerrojo alrededor de exactamente eso — un directorio,
porque `mkdir` o lo crea o falla, atómicamente, en cualquier sistema de ficheros
—, **acotado**: a los tres segundos se lo queda igual y sigue. Un guion de
arranque que espere para siempre a un cerrojo rancio es justo cómo se queda
colgada una máquina entera, y perder una clave en un arranque es el fallo
barato. Los tests de xtask sujetan el cerrojo contra los tres guiones y
comprueban las dos cosas: que esperan, y que ninguno se queda clavado.

**Escotilla**: `init.serial_setup` en la línea de órdenes del núcleo los vuelve
a correr uno detrás de otro, en el mismo orden de antes, sin recompilar nada.

## Cola

Nada pendiente de esta revisión. Lo que queda son decisiones tomadas (el
apagado forzado, la ausencia de protocolo de *readiness*) y lo que no cubre el
techo de logs: un fichero que escriba un envoltorio por su cuenta y que no sea
el `log =` de ningún servicio.
