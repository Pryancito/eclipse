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
`after`, `wait_socket`, `wait_path`, `desktop`, `cmdline`, `log` y `timeout`.

## Comparación honesta

| Mecanismo | systemd | runit | s6-rc | OpenRC | launchd | SMF | eclipse-init |
|---|---|---|---|---|---|---|---|
| Supervisión y reinicio | sí | sí | sí | no (solo arranque) | sí | sí | sí |
| Backoff del lazo de crash | `RestartSec` | 1 s fijo | 1 s fijo | — | 10 s mínimo | sí | 250 ms → 8 s, exponencial |
| **Rendirse con un servicio que no puede funcionar** | `StartLimitBurst` → `failed` | no | no | — | *throttle* | → `maintenance` | **sí (20 caídas seguidas)** |
| **Límite de tiempo al arrancar** | `TimeoutStartSec` 90 s | no | `timeout-up` | sí | `ExitTimeOut` | obligatorio | **sí (90 s, `timeout =`)** |
| Orden por dependencias | sí | no | sí | sí | no | sí | sí (`after =`, topológico) |
| Espera de *readiness* | `Type=notify` | no | *fd* de notificación | no | *socket activation* | no | sondeo de socket/ruta acotado |
| Propagación del fallo de una dependencia | sí | — | sí | sí | no | sí | **no** |
| Límite del tamaño de los logs | journald | `svlogd` | `s6-log` | logrotate | sí | sí | **no** (tmpfs = RAM) |
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

## Cola, por orden de riesgo

1. **El fallo de una dependencia no se propaga.** Si se da por perdido a
   `seatd`, labwc arranca igual contra un socket que nadie va a crear: se gasta
   sus veinte intentos, con la espera acotada de 10 s en cada uno, y acaba
   también descartado, cuando se sabía desde el primero que no podía funcionar.
   Un `after =` debería poder ser también un `requires`.
2. **Los logs de `/tmp` no tienen techo.** Son tmpfs, o sea RAM: un servicio que
   cae en lazo y escribe en cada vuelta se come la memoria de la máquina. Hace
   falta un tope por fichero (lo que hacen `svlogd`, `s6-log` y journald).
3. **PID 1 puede morir de un `panic!`.** El perfil de release es
   `panic = "abort"`, así que cualquier `panic` en init es un kernel panic
   («Attempted to kill init»). Hay que elegir: quitar los `unwrap`/`expect` de
   las rutas vivas, o compilar con `unwind` y envolver el lazo de supervisión en
   un `catch_unwind` que lo registre y siga.
4. **Durante las esperas acotadas del arranque no se cosecha a nadie.** Los hijos
   que mueran en esos segundos se quedan zombis hasta que se llega al lazo. Es
   cosmético, pero hay que arreglarlo con cuidado: un `waitpid(-1)` suelto ahí
   dentro le robaría al lazo la muerte de un `respawn` y ese servicio no se
   reiniciaría nunca.
