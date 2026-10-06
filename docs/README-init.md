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
| **Rendirse con un servicio que no puede funcionar** | `StartLimitBurst` → `failed` | no | no | — | *throttle* | → `maintenance` | **sí (20 caídas seguidas)** |
| **Límite de tiempo al arrancar** | `TimeoutStartSec` 90 s | no | `timeout-up` | sí | `ExitTimeOut` | obligatorio | **sí (90 s, `timeout =`)** |
| Orden por dependencias | sí | no | sí | sí | no | sí | sí (`after =`, topológico) |
| Espera de *readiness* | `Type=notify` | no | *fd* de notificación | no | *socket activation* | no | sondeo de socket/ruta acotado |
| Propagación del fallo de una dependencia | `Requires=` | — | sí | sí | no | sí | **sí (`requires =`)** |
| Límite del tamaño de los logs | journald | `svlogd` | `s6-log` | logrotate | sí | sí | **sí (1 MiB + 1 generación)** |
| Matar lo que quede al parar un servicio | cgroup | grupo de procesos | grupo de procesos | sí | sí | contrato | solo en el `timeout` de un `oneshot` |
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

## Cola, por orden de riesgo

1. **PID 1 puede morir de un `panic!`.** El perfil de release es
   `panic = "abort"`, así que cualquier `panic` en init es un kernel panic
   («Attempted to kill init»). Hay que elegir: quitar los `unwrap`/`expect` de
   las rutas vivas, o compilar con `unwind` y envolver el lazo de supervisión en
   un `catch_unwind` que lo registre y siga.
2. **Durante las esperas acotadas del arranque no se cosecha a nadie.** Los hijos
   que mueran en esos segundos se quedan zombis hasta que se llega al lazo. Es
   cosmético, pero hay que arreglarlo con cuidado: un `waitpid(-1)` suelto ahí
   dentro le robaría al lazo la muerte de un `respawn` y ese servicio no se
   reiniciaría nunca.
