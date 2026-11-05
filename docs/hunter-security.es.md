# hunter — el subsistema de seguridad del kernel de Eclipse OS

[English](hunter-security.md) · **Español**

`hunter` es una pequeña **solución de seguridad** dentro del kernel de Eclipse
OS que combina una **capa de aplicación de políticas al estilo LSM**
(comprobaciones obligatorias en puntos de enganche bien definidos del kernel)
con un **sistema de detección de intrusiones por comportamiento** (IDS). El
kernel llama a un puñado de ganchos; hunter consulta su motor de políticas,
ejecuta heurísticas de anomalías y apunta cada decisión en un registro forense
de eventos que el espacio de usuario puede leer en `/proc/hunter`.

El crate vive en [`hunter/`](../hunter) y es `#![no_std]`. No depende
directamente de `kernel-hal`: el kernel le inyecta un reloj monótono al
arrancar con `hunter::set_time_source`, lo que mantiene el crate abajo en el
grafo de compilación y permite probarlo en el host.

## Ganchos

| Gancho                     | Punto de llamada en el kernel            | Dominio         |
|----------------------------|------------------------------------------|-----------------|
| `check_syscall`            | despacho de `linux-syscall`              | seccomp + IDS   |
| `check_exec_target`        | `sys_execve`                             | política de exec |
| `check_elf_binary`         | lanzamiento en el arranque (`loader`)    | política de exec |
| `check_exec_path`          | intérpretes `PT_INTERP` / `#!`           | política de exec |
| `check_exec_integrity`     | `sys_execve`, intérpretes, lanzamiento en el arranque | integridad de exec |
| `check_exec_av`            | `sys_execve`, intérpretes, lanzamiento en el arranque | antivirus |
| `check_mmap`               | `sys_mmap`                               | memoria W^X     |
| `firewall::check`          | `sys_connect`, `sys_bind`, `sys_sendto`, `sys_sendmsg` | cortafuegos por aplicación |
| `check_mprotect`           | `sys_mprotect`                           | memoria W^X     |
| `check_file_write`         | `write`/`pwrite`/`writev`/`pwritev`, por trozo, ficheros normales | comportamiento (ransomware) |
| `check_file_rename`        | `rename`/`renameat`/`renameat2`          | comportamiento (ransomware) |
| `check_file_unlink`        | `unlink`/`unlinkat` (no `rmdir`)         | comportamiento (ransomware) |
| `check_process_write`      | `process_vm_writev`                      | comportamiento (inyección) |
| `task_fork` / `task_exec`  | `clone` / `execve`                       | linaje para `behavior` |
| `task_exit`                | `sys_exit_group`                         | limpieza de estado |
| `netmon::on_listen`        | `sys_listen`                             | monitor de red  |
| `netmon::observe`          | cada lectura de `/proc/hunter_net`       | monitor de red  |
| `init` / `set_time_source` | arranque de `zCore` (`primary_main`)     | puesta en marcha |

## Dominios y modos de aplicación

Hay varios dominios de aplicación que se ajustan por separado, cada uno con su
propio `Mode`, para que el subsistema pueda desplegarse primero en modo
auditoría e ir endureciéndose dominio a dominio:

- **`Off`** — dominio desactivado (ni comprobaciones ni registro).
- **`Report`** — registra la infracción pero permite la acción (modo
  auditoría / IDS).
- **`Enforce`** — registra la infracción y bloquea la acción.

| Dominio   | Qué gobierna                                      | Por defecto |
|-----------|---------------------------------------------------|-------------|
| `syscall` | Listas blancas de syscalls por proceso (un seccomp ligero) | `Enforce` |
| `wx`      | Memoria escritura-o-ejecución (`mmap` / `mprotect`) | `Report`  |
| `exec`    | Política de rutas de ejecutables (no fiables / escribibles por todos) | `Report` |
| `anomaly` | Bloquear (y no solo registrar) inundaciones y bombas fork detectadas | `Report` |
| `integrity` | SHA-256 de la imagen frente a su resumen fijado; también el [arranque](#integridad-del-arranque-arranque-medido) | `Report` |
| `av`      | Firmas de antivirus encontradas en la imagen      | `Report`    |
| `net`     | Qué programas pueden llegar a la red ([cortafuegos](#cortafuegos-por-aplicación)) | `Report` |
| `behavior` | Ransomware e inyección de procesos, por comportamiento | `Report` |

Los valores por defecto son conservadores a propósito. El dominio de syscalls
está en `Enforce` porque las listas blancas son opcionales por proceso (sin
política registrada ⇒ permisivo), así que no puede romper un proceso que no
pidió filtrado. Los dominios W^X, de rutas de exec y de anomalías van por
defecto en `Report` porque los enlazadores dinámicos y los JIT reales crean
mapeos `W+X` de forma transitoria y el sistema base ejecuta legítimamente
desde muchas rutas distintas: bloquear por defecto podría romper el espacio de
usuario. Los operadores activan `Enforce` por dominio (`policy::set_wx_mode`,
`set_exec_mode`, `set_anomaly_mode`). El plano de control se puede sellar
**solo para endurecer** (`policy::seal_tighten_only`) para que, después del
arranque, ningún dominio pueda relajarse: solo moverse hacia una aplicación más
estricta. Los operadores ajustan todo esto sin tocar código del kernel a
través del [plano de control](#plano-de-control-policyconf-prochunter-hunterctl).

### Endurecimiento (v0.3.0)

Un equipo rojo adversario de varios agentes (ver `docs/hunter-hardening.md`)
guió estas defensas, todas respetando el contrato de valores por defecto
conservadores:

- **W^X es estructural, no por llamada.** El `mmap` respaldado por fichero se
  limita a un *techo* de permisos que preserva W^X (sin ejecución en páginas de
  fichero escribibles, sin escritura en las ejecutables) en vez del `RXW` de
  manta de antes, y un mapa por proceso de *regiones que alguna vez fueron
  escribibles* pilla el rodeo en dos pasos `mmap(W)`→`mprotect(X)`. En modo
  de aplicación, `mprotect` ya no se traga un estrechamiento fallido.
- **Puerta de exec endurecida.** Validación completa de `e_ident`/`e_type`/
  `e_machine` del ELF (se rechazan las imágenes de otra arquitectura); los
  scripts `#!` se reconocen como válidos; la comprobación de integridad lee
  *los mismos bytes del VMO* que se mapean (sin TOCTOU); los enlaces mágicos
  `/proc/*/fd/*` y el recorrido de rutas se tratan como no fiables; el
  enlazador dinámico y el intérprete del shebang se auditan por la misma
  puerta.
- **Forense a prueba de manipulación.** Anillos separados por gravedad para
  que una avalancha de eventos benignos no pueda desalojar las pruebas
  `Warning`/`Critical`; contadores de descartes por gravedad; un sumidero
  duradero opcional manda los eventos de alta gravedad al log del kernel.
- **Ciclo de vida del estado arreglado.** La limpieza se hace desde el
  `Process::terminate` central (todos los caminos de salida, no solo
  `exit_group`); los mapas por pid tienen tope LRU; el reloj monótono está
  sellado y la ventana del IDS tiene un tope por número para que un reloj
  congelado no pueda silenciar la detección.
- **Tablas de syscalls por arquitectura** (x86_64 + asm-generic para
  aarch64/riscv64) y una señal de **tasa de fork de todo el sistema** para
  bombas fork distribuidas.

## Lista de programas de confianza + lista negra (control de aplicaciones)

El dominio `exec` combina una **lista de permitidos que aprende** (una lista
blanca que nunca deniega) con una **lista negra** del operador (la única
denegación dura), más la lista de ubicaciones denegadas que ya existía. Orden
de precedencia en cada `execve` — y para el enlazador dinámico y los
intérpretes `#!`, que pasan por la misma puerta:

1. **Lista negra** — un programa que coincide con `/etc/hunter/blacklist`
   siempre se bloquea (`EACCES`) mientras el dominio `exec` esté activo. Es la
   mitad que deniega.
2. **Aprendizaje (confianza al primer uso)** — cuando está activado, un
   programa *seguro* (un ELF o script válido, no en la lista negra, no desde
   una ubicación escribible por todos como `/tmp`, `/var/tmp`, `/dev/shm` o un
   enlace mágico `/proc/*/fd/*`) se añade solo a la lista de permitidos y **se
   permite sin denegación**. Las rutas que simplemente son relativas (p. ej.
   un gestor de paquetes que ejecuta un binario con una ruta relativa) *no* se
   tratan como inseguras, así que se aprenden en vez de dar aviso. Aquí el
   programa **se permite**, pero solo **se aprende** cuando han pasado también
   todas las demás puertas — la lista negra de la ruta resuelta, el bloqueo de
   soportes, la evaluación de integridad y el antivirus —, así que un programa
   que rechaza alguna de ellas nunca acaba en la lista de confianza (ni en
   `/var/lib/hunter/learned`).
3. **Lista denegada + lista de permitidos** — lo que quede se compara con las
   ubicaciones no fiables y, si la lista de permitidos está activa, con el
   conjunto de confianza. En `Report` esto solo registra; en `Enforce` un
   programa que no es de confianza se bloquea.

La canonicalización va primero, así que un recorrido como `/bin/../opt/evil`
no puede hacerse pasar por un programa de confianza.

### Configuración en `/etc/hunter/`

Al arrancar, el kernel **lee** (nunca escribe) dos ficheros opcionales,
separados por líneas, del sistema de ficheros raíz, y activa el aprendizaje:

| Fichero                   | Efecto                                         |
|---------------------------|------------------------------------------------|
| `/etc/hunter/whitelist`   | programas de confianza que siempre pueden ejecutarse |
| `/etc/hunter/blacklist`   | programas denegados que nunca deben ejecutarse |

Cada línea no vacía y que no empiece por `#` es una entrada; una `/` al final
la convierte en un prefijo de directorio, y si no es la ruta exacta de un
programa. Si faltan los ficheros no pasa nada (las listas se quedan vacías).
Ejemplo de `/etc/hunter/whitelist`:

```
# directorios de confianza
/bin/
/usr/bin/
# un binario extra exacto
/opt/myapp/bin/agent
```

### Persistencia de los programas aprendidos

Las entradas aprendidas viven en la memoria del kernel y se muestran en
`/proc/hunter` en un bloque `learned-programs (N):`. El kernel **no** escribe
en el sistema de ficheros, a propósito.
[`hunterd`](#hunterd-eventos-y-aprendizaje-en-disco) copia ese bloque a
`/var/lib/hunter/learned` cada pocos segundos, y en el siguiente arranque el
kernel vuelve a leer el fichero (después de las listas blanca y negra, antes de
`policy.conf`) en el conjunto **aprendido**, así que el aprendizaje sigue donde
lo dejó y `/proc/hunter` sigue distinguiendo los programas aprendidos de los
configurados.

Cada línea restaurada pasa la misma prueba que pasa un programa para ser
aprendido: una ruta absoluta exacta (un prefijo de directorio se salta), no en
la lista negra, no en una ubicación escribible por todos, no un enlace
`/proc/*/fd/*` y sin caracteres de control. Esa última regla se aplica ahora
también al propio aprendizaje: un programa cuyo nombre tiene un salto de línea
se sigue ejecutando, pero no se aprende, porque `/proc/hunter` lo listaría como
dos líneas y la segunda volvería del disco como un programa que nunca se
ejecutó. El fichero solo crece; para dejar de confiar en un programa aprendido,
haz `hunterctl deny` sobre él (un programa denegado no se restaura).

`hunterctl save-learned` sigue ahí para pasar el conjunto aprendido a
`/etc/hunter/whitelist`, donde un operador puede revisarlo y editarlo.

### Control desde código

```rust
use hunter::{policy, Mode};

policy::set_exec_learning(true);                              // la lista blanca aprende, nunca deniega
policy::add_blacklisted_exec_prefix(String::from("/tmp/"));  // denegar de forma dura una ubicación
policy::add_trusted_exec_path(String::from("/srv/agent"));   // confiar de antemano en un programa
policy::set_exec_mode(Mode::Enforce);                        // denegar también los exec que no son de confianza
```

## Plano de control: `policy.conf`, `/proc/hunter`, `hunterctl`

Todos los mandos de arriba eran antes funciones de Rust que solo podía llamar
código del kernel, y el kernel no llamaba a ninguna de las que endurecen: un
sistema arrancado ejecutaba `wx`, `exec` y `anomaly` en `Report` para siempre,
y el cerrojo de solo endurecer nunca se echaba. El plano de control es un
lenguaje pequeño ([`hunter::control`](../hunter/src/control.rs)) con tres
entradas:

| Entrada                    | Cuándo             | Quién                |
|----------------------------|--------------------|----------------------|
| `/etc/hunter/policy.conf`  | una vez, al arrancar, después de las listas blanca y negra | el kernel |
| escribir en `/proc/hunter` | en cualquier momento | solo root (`euid 0`) |
| `/sbin/hunterctl`          | en cualquier momento | root; una interfaz para la escritura anterior |

### Lenguaje

Una directiva por línea; las líneas en blanco y los comentarios `#` se
ignoran.

| Directiva                              | Efecto                                              |
|----------------------------------------|-----------------------------------------------------|
| `mode <dominio> <off\|report\|enforce>` | `dominio` es `syscall`, `wx`, `exec`, `anomaly`, `integrity`, `av` o `behavior` |
| `learning <on\|off>`                   | confianza al primer uso de exec                     |
| `anomaly-detection <on\|off>`          | contadores de inundación / bomba fork por proceso   |
| `privileged-deny <on\|off>`            | bloquear module/ptrace/bpf/kexec con `anomaly=enforce` |
| `trust <ruta>` / `trust <dir/>`        | añadir a la lista de permitidos de exec             |
| `deny <ruta>` / `deny <dir/>`          | añadir a la lista negra de exec                     |
| `untrusted <dir/>`                     | añadir una ubicación escribible por todos           |
| `hash <sha256> <ruta>`                 | fijar el SHA-256 de un programa (ver abajo)         |
| `require-hash <on\|off>`               | un programa sin resumen fijado es una infracción de integridad |
| `signature <hash\|bytes>:<datos>:<Nombre>` | añadir una firma de antivirus (no se puede quitar) |
| `media-scan <on\|off>`                 | encolar los soportes extraíbles nuevos para escanearlos (por defecto on) |
| `media-lock <on\|off>`                 | nada se ejecuta desde un soporte hasta que se escanee limpio (por defecto off) |
| `media <id> scanning\|failed`          | un agente de escaneo tomó / abandonó un dispositivo |
| `media <id> done <ficheros> <infectados>` | el veredicto de un agente de escaneo sobre un dispositivo |
| `net <allow\|deny> <ruta> [in\|out]`  | una regla del [cortafuegos](#cortafuegos-por-aplicación); sin dirección son las dos |
| `net forget <ruta>` / `net dismiss <ruta>` | quitar las reglas de un programa / sus preguntas abiertas |
| `net timeout <s>` / `net mode <modo>`  | cuánto espera `enforce` una respuesta (1-300 s); igual que `mode net` |
| `canary <ruta>`                        | un fichero señuelo: tocarlo es ransomware           |
| `exempt <ruta>`                        | un programa al que los detectores de `behavior` dejan en paz |
| `release <pid>`                        | levantar la contención por ransomware de un proceso |
| `seal`                                 | echar el cerrojo de un solo sentido, solo para endurecer |

**Todo o nada.** Un script (un `policy.conf` entero, o un `write(2)` a
`/proc/hunter`) se analiza completo antes de aplicar nada; una línea mala lo
rechaza entero. El rechazo es un evento `CRIT` que nombra la línea
(`CONTROL REJECTED: /etc/hunter/policy.conf: line 7: unknown mode
'enforcing'`) y, en una escritura, `EINVAL`. Un fichero de política de más de
1 MiB, que no sea UTF-8 o que vuelva cortado se rechaza igual: media política
es una postura que nadie escribió.

**El cerrojo.** Las directivas se ejecutan en orden, así que `seal` ata a las
líneas que van detrás. Una vez sellado, toda directiva que relaja se rechaza y
se registra (`mode X` a un modo más bajo, `learning on`, `trust`,
`anomaly-detection off`, `privileged-deny off`, `media-scan off`, `media-lock
off`, `exempt`, `release`, un veredicto limpio para un dispositivo ya visto
infectado); las que endurecen se siguen aplicando. Solo un reinicio quita el
sello.

**Quién puede escribir.** `/proc/hunter` tiene modo `0644`, y el manejador de
escritura comprueba el uid *efectivo* del proceso que escribe, no solo el de
la apertura: un descriptor abierto por root y heredado por un hijo sin
privilegios no puede manejar hunter. Cada script aplicado se registra con el
pid de quien lo escribió.

### `hunterctl`

```
hunterctl [status]              modos, contadores, listas (sin el anillo de eventos)
hunterctl events                el informe completo, con el anillo de eventos
hunterctl mode <dominio> <modo>
hunterctl learning|anomaly-detection|privileged-deny <on|off>
hunterctl trust|deny <ruta|dir/>
hunterctl untrusted <dir/>
hunterctl canary <fichero>...   crear ficheros señuelo, armarlos ya y en cada arranque
hunterctl exempt <programa>     los detectores de ransomware/inyección lo dejan en paz
hunterctl release <pid>         levantar la contención por ransomware de un proceso
hunterctl seal
hunterctl apply <fichero>       mandar un script entero (p. ej. un policy.conf nuevo)
hunterctl hash <fichero>...     fijar resúmenes SHA-256 (en vivo, y en /etc/hunter/hashes)
hunterctl require-hash <on|off>
hunterctl verify                sha256sum -c /etc/hunter/hashes
hunterctl boot                  integridad del arranque: veredicto, resúmenes medidos y fijados
hunterctl boot pin [item...]    fijar las medidas de este arranque (en vivo + /etc/hunter/boot)
hunterctl boot verify           volver a calcular ahora el hash del código del kernel (sale con 1 si está manipulado)
hunterctl scan [--quarantine] <ruta>...   escaneo antivirus (sale con 1 si hay infección)
hunterctl quarantine <fichero>...  mover a /var/lib/hunter/quarantine, denegar la ruta
hunterctl quarantined           listar la cuarentena
hunterctl restore <sha256>      devolver a su sitio un fichero en cuarentena
hunterctl update-signatures     descargar las fuentes de /etc/hunter/feeds (las firmadas, verificadas)
hunterctl feeds                 fuentes instaladas, claves de confianza, versiones guardadas
hunterctl feed-rollback <fuente> [<versión>]   volver a una versión guardada, en vivo
hunterctl trust-key <nombre> <hex>   confiar en una clave Ed25519 de fuentes (en vivo, y en /etc/hunter/keys)
hunterctl require-signed-feeds <on|off>
hunterctl scan-media [id|dir]...  escanear soportes extraíbles (por defecto: todos los pendientes)
hunterctl media-scan|media-lock <on|off>
hunterctl lockdown              exec + integrity + av en enforce, media-lock on, learning off, seal
hunterctl report [--pdf|--txt] [--days N] [-o FICHERO]
                                informe de seguridad (abajo), en /var/lib/hunter/reports/
hunterctl save-learned          añadir los programas aprendidos a /etc/hunter/whitelist
hunterctl daemon [segundos]     hunterd (ver abajo); lo ejecuta eclipse-init
hunterctl persist               una pasada de hunterd, ahora
hunterctl history [--alerts|--critical] [--this-boot] [N]
                                los últimos N eventos en disco (por defecto 50, 0 = todos)
```

Una pasada típica de endurecimiento en un sistema en marcha: dejarlo correr con
el aprendizaje activado mientras se ejecuta la carga habitual, `hunterctl
save-learned` y luego `hunterctl lockdown`. A partir de ahí solo se ejecutan
programas permitidos (configurados o aprendidos), y nada puede aflojarlo hasta
reiniciar. `lockdown` deja W^X en paz a propósito: aplicarlo rompe cualquier
JIT (Firefox, cualquier motor de JS).

La imagen incluye `/etc/hunter/policy.conf` como un ejemplo totalmente
comentado ([`tools/hunterctl/policy.conf`](../tools/hunterctl/policy.conf)),
así que no cambia nada hasta que se edita; un test unitario mantiene válidas
todas sus líneas.

### Desde el escritorio: el escudo de lunarbar

La barra inferior ([`tools/lunarbar`](../tools/lunarbar)) lleva un escudo junto
a la píldora del teclado, que se lee de `/proc/hunter` cada tres segundos:

| Escudo | Significado |
|---|---|
| relleno, color de acento | todos los dominios al menos avisan |
| contorno, apagado | algún dominio está en `off` |
| relleno, rojo | llegó un evento crítico que nadie ha mirado todavía |

Al pulsarlo se abre el menú de hunter, que además quita el rojo (abrir el menú
es lo que significa «mirado»). El menú tiene:

- el sello (`sellado` / `sealed`) y los contadores (bloqueados, críticos,
  firmas);
- una fila por dominio con `off / aviso / bloqueo` (`off / report / enforce`
  en inglés), más `learning off / on`. Un clic manda `mode <dominio> <modo>`
  o `learning <on|off>`. Una vez sellado, los segmentos que relajan salen
  apagados y no se pueden pulsar; si aun así se rechaza una relajación (hunter
  acepta la escritura y solo lo dice en su anillo), el menú vuelve a leer los
  modos y lo dice;
- los cuatro avisos y críticos más recientes;
- cuatro botones: **escanear**, **firmas**, **registro** y **bloqueo total**.
  Con `lunarhunter` instalado (abajo), los tres primeros lo abren en la
  pestaña que toca (`lunarhunter --quarantine --scan $HOME`, `--update`,
  `--tab eventos`); sin él ejecutan `hunterctl scan --quarantine $HOME`,
  `hunterctl update-signatures` y `hunterctl events` en un terminal. El
  **bloqueo total** pide un segundo clic para confirmar y luego manda
  exactamente lo mismo que `hunterctl lockdown` (un test unitario compara los
  dos);
- **abrir** / **open** en la cabecera, que abre `lunarhunter`.

La barra no tiene política propia: lee el mismo informe y manda las mismas
directivas que `hunterctl`, por el mismo `/proc/hunter` reservado a root (la
sesión de escritorio corre como root). Un kernel sin hunter no tiene
`/proc/hunter`, y entonces no hay escudo. `LUNARBAR_DUMP=/ruta:WxH
LUNARBAR_DUMP_HUNTER=1 lunarbar` pinta el menú en un fichero sin compositor.

### lunarhunter: la ventana de hunter

`lunarhunter` ([`tools/lunarbar/src/bin/lunarhunter.rs`](../tools/lunarbar/src/bin/lunarhunter.rs))
es el resto de hunter como aplicación de escritorio: una ventana xdg-shell
normal (en la barra de tareas, que se mueve, cambia de tamaño y lleva el marco
de labwc), compilada e instalada con la barra como `/bin/lunarhunter`, y en el
menú de aplicaciones como **Seguridad (hunter)**
(`/usr/local/bin/eclipse-security`, que cae a un terminal con `hunterctl
events`). Pestañas, con las teclas `1`-`8` o Tab:

| Pestaña | Qué hace |
|---|---|
| Resumen | la postura en una línea, ocho contadores, las últimas alertas; acciones rápidas: escanear la carpeta personal, actualizar firmas, bloqueo total, **exportar informe** (el informe de la semana en HTML en `$HOME`) |
| Protección | los siete dominios (`behavior` incluido) con lo que protege cada uno, los seis interruptores (`learning`, `anomaly-detection`, `privileged-deny`, `require-hash`, `media-scan`, `media-lock`); **guardar como política de arranque** escribe la postura en vivo en `/etc/hunter/policy.conf` (el fichero antiguo se guarda como `policy.conf.bak`), **aplicar policy.conf** y **sellar** |
| Eventos | el anillo de eventos entero con hora, pid, gravedad y categoría, filtrado por todos / alertas / críticos |
| Escaneo | preajustes (carpeta personal, `/tmp`, programas, `/`, o `--scan RUTA...`), un contador en vivo con botón de parada, cuarentena por hallazgo o de todos a la vez, o cuarentena automática sobre la marcha |
| Cuarentena | `/var/lib/hunter/quarantine/index`, cada fila restaurable (dos clics) |
| Firmas | lo que tiene el kernel (hashes / patrones de bytes / detecciones), los ficheros de `/etc/hunter/signatures{,.d}`, las fuentes y **actualizar ya** |
| Programas | los recuentos de la lista de permitidos, resúmenes fijados en el kernel frente a en disco, los programas aprendidos, **guardar aprendidos** (`hunterctl save-learned`) y **verificar integridad** (`hunterctl verify`) |
| Programados | los escaneos programados (ver [Antivirus](#antivirus-firmas)): por tarea off / diario / semanal, el día, la hora en pasos de 30 minutos, alertar o poner en cuarentena, la próxima ejecución y **ejecutar ya** (`hunterctl run-scheduled`); luego el historial, del más nuevo al más viejo |
| Red | cada socket TCP/UDP con el proceso que lo tiene (pid, programa, dirección local y remota), los puertos a la escucha inesperados primero y en rojo con un botón **permitir** (`hunterctl expect-port`, dos clics), el interruptor de alertas y los puertos esperados con **quitar** (`hunterctl forget-port`); se vuelve a leer cada dos segundos mientras está abierta. Ver [Monitor de red](#monitor-de-red) |
| Cortafuegos | el [cortafuegos](#cortafuegos-por-aplicación): cada pregunta abierta («¿Permitir que wget se conecte a la red?») con **Permitir** / **Bloquear** / **Ignorar**, las reglas con **Olvidar** y el modo `net`; cada cambio se manda a `/proc/hunter` y las reglas en vivo se escriben en `/etc/hunter/firewall` |

Igual que la barra, no tiene política propia: los ajustes van por
`/proc/hunter`, los escaneos por `/proc/hunter_scan` (los mismos lotes de 256
rutas que escribe `hunterctl scan`, desde un hilo, para que el contador se
mueva), y el trabajo con ficheros (cuarentena, restaurar, descargar fuentes, la
lista blanca, verificar) ejecuta `/sbin/hunterctl` y muestra su salida en la
pestaña. Las acciones de un solo sentido o que mueven ficheros (bloqueo total,
sellar, guardar la política de arranque, poner todo en cuarentena, restaurar)
piden un segundo clic. Con el sello, los controles que relajan no se ofrecen;
sin root, no se ofrece nada que cambie hunter. `lunarhunter --tab NOMBRE
--dump /ruta:WxH` pinta una pestaña en un fichero ARGB8888 crudo sin
compositor (con una muestra integrada si no hay `/proc/hunter`).

### Informes de seguridad

`hunterctl report`, `lunarhunter --export RUTA [--days N]` y el botón
**Exportar informe** escriben el mismo informe
([`tools/lunarbar/src/hunter_export.rs`](../tools/lunarbar/src/hunter_export.rs)),
en el idioma del escritorio, de los últimos 7 días por defecto:

- la postura y los contadores del periodo (eventos por gravedad, críticos,
  detecciones del antivirus, ficheros en cuarentena, firmas, programas
  fijados);
- **recomendaciones**, cada una con el comando de `hunterctl` que actúa sobre
  ella: dominios apagados o solo avisando, firmas viejas o ausentes,
  aprendizaje todavía activado, un anillo de eventos desbordado, sin resúmenes
  fijados o `require-hash off`, sin sello;
- los siete modos y los interruptores; de dónde vienen las alertas (por
  categoría y por fichero/programa); cada alerta con su fecha, pid y texto; la
  cuarentena del periodo; los ficheros de firmas y las fuentes; los programas
  aprendidos; y el registro de eventos entero, plegado.

Tres formatos, según la extensión del fichero: **HTML** (una sola página
autocontenida, sin descargar nada, que se imprime limpia), **PDF** (la versión
de texto paginada en la fuente estándar Courier, así que no necesita ninguna
biblioteca ni fuente incrustada) y **texto**. `-o -` escribe en stdout. Sin
`-o` el fichero es `/var/lib/hunter/reports/hunter-informe-AAAA-MM-DD.<ext>`,
uno por día, así que una ejecución semanal conserva el historial.

Los eventos salen del historial de hunterd (`/var/log/hunter/events.log` y sus
rotaciones, sin las líneas `HUNTERD BOOT` y con las líneas `LOST`) más lo que
tenga el anillo que hunterd aún no haya escrito (por el id de este arranque y
el número de secuencia, para que nada cuente dos veces). Cuando no se puede
leer el historial (sin hunterd, o un lector que no es root: el log es `0640`),
el anillo es todo lo que hay, y empieza en el arranque. En cualquier caso la
sección de eventos cubre el periodo **o desde donde empiezan los eventos, si
es más tarde**, y el informe dice cuál; la cuarentena y las firmas están en
disco y cubren todo el periodo. El informe también avisa cuando el anillo se
desbordó (`dropped=`).

### Notificaciones en el escritorio

lunarbar saca una tarjeta en la esquina, encima de la barra, por cada aviso o
crítico nuevo del anillo de eventos de hunter
([`hunter_toast.rs`](../tools/lunarbar/src/hunter_toast.rs); lo que ofrece cada
evento, en [`hunter_notify.rs`](../tools/lunarbar/src/hunter_notify.rs)):

| Evento | Botones |
|---|---|
| malware (`AV`) con un fichero | **cuarentena** (`hunterctl quarantine FICHERO`), ver, ignorar |
| un programa que rechazó la lista de permitidos (`EXEC`, *not on trusted-program allowlist*), sin sellar | **permitir** (`hunterctl trust RUTA`), ver, ignorar |
| una pregunta del cortafuegos (`NET ASK`) | **permitir** / **bloquear** (`net allow\|deny RUTA out\|in`, y luego las reglas en vivo se guardan en `/etc/hunter/firewall`), ver (la pestaña Cortafuegos); se queda hasta que se responde |
| cualquier otra cosa | ver, ignorar |

**ver** abre el registro de eventos (`lunarhunter --tab eventos`),
**ignorar** deja de mostrar ese evento concreto hasta que la barra se
reinicia, y la cruz solo cierra la tarjeta. La cuarentena y el permitir
responden con una segunda tarjeta que dice si funcionó. Los avisos se van a
los 12 s, los críticos se quedan hasta que se responden, y ninguna tarjeta
caduca mientras el puntero está encima. Se muestran tres como mucho; una
ráfaga mayor espera detrás de una línea **+N más**, y el mismo evento repetido
es una tarjeta con un contador. Lo acumulado en el anillo cuando arranca la
barra no se muestra (para eso están el menú y la ventana). Permitir no se
ofrece para un programa en una ubicación no fiable como `/tmp`, porque hunter
comprueba esas antes que la lista de permitidos y confiar en uno no cambiaría
nada.

**Cualquier otra cosa también puede sacar una**, y así es como las demás
partes de hunter deben contarle o preguntarle algo al usuario: deja un fichero
en `/run/hunter/notify/` (de root, modo 700), o desde una shell:

```sh
hunterctl notify --severity info "Escaneo programado" "0 amenazas en /home"
ans=$(hunterctl notify --severity crit --wait 60 \
        --action allow:permitir:"hunterctl ..." --action deny:denegar \
        "Cortafuegos" "¿Permitir que /usr/bin/curl se conecte a la red?")
# $ans es allow, deny, dismiss o timeout (sale con 2)
```

Cada acción es `ID:ETIQUETA[:COMANDO]`: la barra ejecuta COMANDO con `/bin/sh
-c` cuando se pulsa el botón, y con `--wait` devuelve el ID. Desde Rust,
`lunarbar::hunter_notify::{post, take_answer}` hacen lo mismo. La barra
ejecuta esos comandos como root, así que solo lee un directorio de cola y
ficheros que sean de root (o de su propio usuario) y que nadie más pueda
escribir, y se salta todo lo demás. `LUNARBAR_DUMP=/ruta:WxH
LUNARBAR_DUMP_TOAST=1 lunarbar` pinta una pila de muestra sin compositor.

### hunterd: eventos y aprendizaje en disco

El anillo de eventos de `/proc/hunter` guarda los últimos cientos de eventos
del arranque actual, y el conjunto aprendido vive en la memoria del kernel: un
reinicio perdía los dos. `hunterd` es `hunterctl daemon`, lo arranca
eclipse-init (`/etc/eclipse/services/hunterd.service` →
`/usr/local/bin/eclipse-hunterd`) y se reinicia si muere. Cada 5 segundos lee
`/proc/hunter` y:

- añade a `/var/log/hunter/events.log` cada evento que aún no haya escrito;
- funde los programas aprendidos en `/var/lib/hunter/learned` (ver arriba).

| Fichero | Modo | Qué |
|---|---|---|
| `/var/log/hunter/events.log` (+ `.1` … `.4`) | `0640` | el historial, rotado al pasar de 1 MiB, así que 5 MiB como mucho |
| `/var/lib/hunter/learned` | `0600` | el conjunto aprendido que el kernel restaura al arrancar |
| `/var/lib/hunter/events.cursor` | `0640` | `<boot_id> <siguiente seq>`: dónde empieza la siguiente pasada |

`events.log` va separado por tabuladores, un evento por línea, del más viejo
al más nuevo:

```
time                  boot      seq  uptime   pid  severity category action  description
2026-10-29T18:46:00Z  aaaaaaaa  17   4.500    142  CRIT     EXEC     BLOCKED blacklisted program: /tmp/x
```

`time` está en UTC y se calcula a partir del uptime (así que solo es tan
correcto como lo fuera el reloj), `boot` son los 8 primeros caracteres de
`/proc/sys/kernel/random/boot_id`, y `seq` es el número de secuencia del
propio hunter, que vuelve a empezar en 0 en cada arranque. hunterd añade dos
tipos de líneas propias, todas con categoría `HUNTERD` y seq `-`: `BOOT` abre
cada arranque, y `LOST` nombra los números de secuencia que salieron del
anillo antes de que una pasada pudiera leerlos (una avalancha entre dos
pasadas), para que un hueco nunca sea silencioso.

Un hunterd reiniciado sigue desde el cursor, así que nada se escribe dos
veces. eclipse-init no manda señales a sus servicios antes de `reboot(2)`, así
que como mucho los últimos 5 segundos de eventos antes de un reinicio no
llegan al disco (el sumidero duradero sigue mandando cada evento `Warning` o
superior al log del kernel). Un `TERM` hace que hunterd dé una última pasada y
salga.

`hunterctl history` imprime el fichero para personas (`--alerts` para avisos y
críticos, `--critical`, `--this-boot` y un número); solo lee el disco, así que
funciona tras un reinicio y en un kernel sin hunter. Las herramientas que
quieran el historial (informes, escaneos programados) deben leer
`events.log*` directamente.

## Cortafuegos por aplicación

`hunter::firewall` ([`hunter/src/firewall.rs`](../hunter/src/firewall.rs))
decide qué **programas** pueden llegar a la red. Todos los demás dominios ven
igual a un dropper que llama a casa y a `curl` bajando una página: el mismo
`connect(2)`; este pregunta qué ejecutable lo está haciendo, y guarda la
respuesta como una regla.

**Qué se comprueba.** `connect`, y `sendto` / `sendmsg` con una dirección de
destino, son la dirección de salida (`out`); `bind` a un puerto fijo es la de
entrada (`in`, el programa quiere ser un servidor). Nunca se pregunta por: el
loopback en cualquier dirección, la dirección no especificada como destino, un
bind al puerto 0 (un cliente eligiendo su puerto de origen) y nada que no sea
IP (sockets unix, netlink, packet): el propio escritorio vive en esos. La
mitad del kernel es
[`linux-syscall/src/net/hunter_firewall.rs`](../linux-syscall/src/net/hunter_firewall.rs).
El programa es el ejecutable del proceso tal como lo apuntó `execve` (así que
un script es su intérprete).

**Reglas y preguntas.** Una regla es `allow` o `deny`, para un programa y una
dirección o las dos. Un programa sin regla se convierte en una **pregunta**:
aparece en `/proc/hunter_firewall` y en la pestaña **Cortafuegos** de
lunarhunter, que la responde con `net allow|deny <ruta> <dir>` sobre
`/proc/hunter`. Lo que pasa mientras tanto depende del modo:

| Modo | Sin regla | Regla `deny` |
|---|---|---|
| `off` | permitido, sin preguntar | permitido |
| `report` (por defecto) | permitido, y se pregunta | **bloqueado** (`EACCES`) |
| `enforce` | la llamada **espera** la respuesta (30 s, `net timeout`), luego `EACCES` | **bloqueado** |

Un `deny` bloquea también en `report`: `report` va del criterio del propio
hunter, y una regla que dio la persona no es eso. En `enforce` la llamada que
espera se despierta con `allow`/`deny` en menos de un cuarto de segundo, y con
una señal con `EINTR`; una pregunta que agotó el tiempo sigue abierta, así que
el siguiente intento todavía puede permitirse. La lista de preguntas tiene un
tope de 32 (sale primero la preguntada hace más tiempo), y un programa que
reintenta contra un `deny` se registra a los 1, 2, 4, 8... bloqueos para que
no pueda inundar el anillo.

**`/proc/hunter_firewall`** (solo lectura), un registro por línea, la ruta al
final:

```
mode report
timeout 30
now 640
rule allow both /usr/bin/curl
rule deny out /usr/bin/nc
pending out 3 142 610 636 93.184.216.34:443 /usr/bin/wget
```

`pending <dir> <cuenta> <pid> <primera> <última> <par> <ruta>`, con los
tiempos en segundos desde el arranque.

**`/etc/hunter/firewall`** se carga al arrancar (antes de `policy.conf`, así
que un `seal` ahí sella las reglas), línea a línea: una línea mala pierde una
regla:

```
mode enforce
timeout 30
allow /usr/bin/curl
deny /usr/bin/nc out
```

lunarhunter lo reescribe a partir del estado en vivo después de cada
respuesta.

**El cerrojo.** `net allow` y `net forget` de un programa con un `deny`
relajan la política y se rechazan una vez sellado; `net deny` siempre se
aplica, y `net dismiss` no cambia ninguna regla.

## Integridad de exec (SHA-256)

El dominio `exec` decide *desde dónde* se puede ejecutar un programa y *qué
nombres* son de confianza. No puede distinguir `/usr/bin/ssh` de un sustituto
que alguien con permiso de escritura en `/usr/bin` haya dejado con el mismo
nombre. El dominio `integrity` ([`hunter::integrity`](../hunter/src/integrity.rs))
comprueba justo eso, al estilo de IMA:

- Un programa con un SHA-256 **fijado** solo se ejecuta si la imagen que se va
  a mapear sigue dando ese resumen.
- Con `require-hash on`, un programa **sin** resumen fijado también es una
  infracción, lo que convierte la integridad en una lista de permitidos que
  deniega por defecto.

`Report` (por defecto) apunta una discrepancia como evento `WARN` y deja que
el programa se ejecute; `Enforce` lo bloquea con `EACCES` y un evento `CRIT`.
Sin nada fijado, el dominio no comprueba nada.

**Lo que se resume** es el VMO que mapea el cargador, no una segunda lectura
del fichero, así que un escritor que compita con la comprobación no puede
cambiar los bytes entre el hash y la carga. Eso cubre el programa, su
enlazador dinámico `PT_INTERP`, los intérpretes `#!` y los programas que el
kernel lanza al arrancar.

**Nombres.** Un programa se busca tanto por el nombre con el que se ejecutó
como por la ruta a la que se resuelve a través de enlaces, tras la
canonicalización. Fijar `/bin/busybox` cubre por tanto todos los enlaces de
applets hacia él. Si los dos nombres están fijados, tienen que coincidir los
dos.

**Coste.** Solo se resumen las imágenes fijadas (o todas, con
`require-hash`), y cada imagen una sola vez: el resumen se guarda en caché por
el id de objeto del kernel de la imagen, y la caché de ejecutables del kernel
da la misma imagen a cada exec de un fichero sin cambios.

**Los resúmenes fijados** vienen de `/etc/hunter/hashes`, que se lee al
arrancar en formato `sha256sum` (`<sha256>  <ruta>`), así que `sha256sum
/bin/* > /etc/hunter/hashes` es un fichero válido. Una línea que no se puede
analizar se salta por su nombre; el resto se carga igual. También pueden venir
de directivas `hash`, o de `hunterctl hash`, que fija en vivo y actualiza el
fichero en un solo paso.

Fijar es una relajación: con `require-hash` admite un programa, y un resumen
nuevo admite bytes nuevos bajo un nombre viejo. Así que el cerrojo de solo
endurecer rechaza los resúmenes nuevos, y `require-hash off`, después de
`seal`. Una vez sellado un sistema, una actualización de paquetes que cambie
un binario fijado hace que ese binario deje de ejecutarse hasta reiniciar. Es
el precio que se busca.

### La puerta de `execve` ha vuelto

Al añadir este dominio resultó que `sys_execve` había dejado de llamar a
hunter. La puerta se perdió en «cache executable file images across execve»
(junio de 2026), que sustituyó el bloque que la llamaba. Desde entonces, cada
`execve` en tiempo de ejecución se saltaba la lista negra, la política de
ubicaciones no fiables y la lista de permitidos; solo se seguían comprobando
los lanzamientos del arranque y los intérpretes.
La puerta se restaura como `check_exec_target`, después de la comprobación
previa del ELF/`#!` (así un fichero que no es un programa sigue siendo
`ENOEXEC` para la shell) y antes de desmontar la imagen vieja (así un rechazo
es un `EACCES` normal). Las listas se consultan por el nombre, y la ruta
resuelta también se compara con la lista negra y con las ubicaciones no
fiables. Eso significa que un enlace simbólico desde un nombre con pinta de
fiable hacia `/tmp/evil` ya no oculta dónde está el fichero.

## Integridad del arranque (arranque medido)

La integridad de exec empieza en el primer `execve`. Todo lo de antes — el
kernel, su línea de comandos, el initramfs que lleva `/sbin/init` y
`/etc/hunter/policy.conf` — decide si alguna comprobación posterior significa
algo: una cmdline sin los interruptores de hunter, un `kernel.elf` con la
puerta de exec parcheada o un initramfs cuyo `policy.conf` dice `mode exec off`
arrancan todos un sistema en el que hunter sale en verde.
[`hunter::boot`](../hunter/src/boot.rs) cubre ese tramo, en dos mitades.

**Arranque medido.** rboot calcula el hash (SHA-256) de cada entrada que le
pasa al kernel y le pasa los resúmenes en `BootInfo`
([`rboot::measure`](../rboot/src/measure.rs)). `measure=` en `rboot.conf`
elige qué:

| `measure=` | Qué se mide | Coste |
|------------|-------------|-------|
| `kernel` (por defecto) | el ELF del kernel, `rboot.conf`, la cmdline | se pierde en el tiempo que tarda el firmware en leerlos |
| `all` | eso y el initramfs, resumido mientras se lee | segundos: el initramfs es toda la raíz live |
| `off` | nada | -- |

rboot nunca se niega a arrancar por un resumen: no sabe cuál debería ser, y un
cargador que se parara ante una discrepancia convertiría una actualización del
kernel que nadie volvió a fijar en una máquina que enseña un splash y se para.
El que compara es el kernel.

**El código del kernel.** Justo después de que suba hunter, el kernel calcula
el hash de su propio `.text` y `.rodata` tal como están mapeados (el elemento
`kernel-code`), y los vuelve a calcular cada diez minutos y con `hunterctl
boot verify`. Nada en este kernel escribe en esos rangos tras el arranque, así
que un resumen que cambia es código parcheado en memoria, que es como un
rootkit del kernel engancha una función. Un cambio es pegajoso: código
parcheado y vuelto a dejar como estaba sigue siendo un arranque comprometido.

**Los resúmenes fijados** viven en `/etc/hunter/boot`, con forma de
`sha256sum` y un elemento donde iría la ruta (`<sha256>  kernel`). Elementos:
`kernel`, `initramfs`, `rboot.conf`, `cmdline`, `kernel-code`; cualquier otro
se rechaza como errata. `hunterctl boot pin` escribe ahí las medidas del
arranque en curso y las fija en vivo — confianza al primer uso, así que
ejecútalo en un arranque del que te fíes: justo después de instalar o
actualizar el kernel. Solo se juzgan los elementos fijados:

- medido e igual: `ok`;
- medido y distinto: `MISMATCH`;
- fijado y **no medido**: `UNMEASURED`, también una infracción: `measure=off`
  o un rboot antiguo que nunca medía no pueden ser la forma de esquivar un
  resumen fijado.

El veredicto está en `/proc/hunter_boot` (y como `boot:` en `/proc/hunter`).
El dominio comparte `mode integrity`: `report` apunta cada infracción como un
evento `CRIT` `BOOT`; `enforce` la apunta y **pone hunter en bloqueo total**
— exec, integrity y av en `enforce`, aprendizaje apagado, cerrojo sellado —
porque un arranque no se puede desarrancar, y un sistema que empezó con
entradas que nadie fijó debe mantener todas las guardas arriba y no dejarse
convencer de quitarlas (ni siquiera por el `policy.conf` que traiga un
initramfs manipulado) hasta un reinicio limpio. La evaluación se hace después
de aplicar `policy.conf`, así que el modo es el del operador.

**Límites.** La cadena empieza en rboot, y nada mide a rboot: un rboot
sustituido informa de los resúmenes que quiera. Ese eslabón es cosa del Secure
Boot de UEFI. Los resúmenes fijados viven en el sistema de ficheros raíz, que
en un arranque live *es* el initramfs, así que ahí fija desde una copia de
confianza y no desde el sistema en marcha; en un sistema instalado la raíz es
su propia partición. El instalador reescribe la cmdline (`ROOT=`), así que fija
después de instalar, no desde el medio live.

## Antivirus (firmas)

[`hunter::av`](../hunter/src/av.rs) es un motor de firmas clásico. Una firma
es o el SHA-256 de un fichero malicioso entero conocido
(`hash:<sha256>:<Nombre>`) o un patrón de bytes que se encuentra dentro de uno
(`bytes:<hex>:<Nombre>`). En un patrón de bytes `??` coincide con cualquier
byte; un patrón necesita al menos 4 bytes fijos y tiene que empezar por uno.

**Al acceder.** Cada imagen que el kernel está a punto de ejecutar se escanea
antes de mapearla: `execve`, el enlazador dinámico `PT_INTERP`, los
intérpretes `#!` y los lanzamientos del arranque. El escaneo se hace sobre el
mismo VMO que mapea el cargador, junto a la comprobación de integridad. Una
coincidencia es un `WARN` con `mode av report` (por defecto) y un exec
bloqueado (`EACCES`, `CRIT AV BLOCKED`) con `enforce`. Con la base de datos
vacía no se escanea nada. Los veredictos se guardan en caché por imagen junto
con la generación de la base de datos, así que un binario sin cambios se
escanea una vez y se vuelve a escanear cuando la base de datos crece.

**Bajo demanda.** `/proc/hunter_scan` (solo root): escribe rutas, una por
línea; cada escritura se escanea antes de volver, y luego al leer salen
resultados al estilo de `clamscan` (`ruta: OK`, `ruta: <Nombre> FOUND`, `ruta:
ERROR …`). `hunterctl scan /home /tmp` recorre los árboles y se los da en
lotes. Solo imprime las detecciones y los errores, más un resumen, y sale con
1 si encontró algo. Una detección también se apunta como evento `AV DETECT`.

**Cuarentena.** `hunterctl quarantine <fichero>` (o `scan --quarantine`)
mueve el fichero a `/var/lib/hunter/quarantine/<sha256>` con modo `000`,
apunta su ruta y modo originales en `index` y añade la ruta a la lista negra
de exec, para que una copia nueva que dejen ahí no se ejecute. `hunterctl
restore <sha256>` devuelve el fichero con su modo. La ruta sigue denegada
hasta reiniciar, porque hunter nunca olvida una denegación.

**Base de datos y actualizaciones.** `/etc/hunter/signatures` (local; viene
vacío, porque una firma de ejemplo como el patrón de texto de EICAR coincidía
con todos los ficheros que lo citaban, incluidos los programas y la
documentación que vienen con el sistema) y cada fuente de
`/etc/hunter/signatures.d/` se cargan al arrancar. `hunterctl
update-signatures` descarga cada fuente de `/etc/hunter/feeds` (`<formato>
<nombre> <url> [<clave>]`; formatos `sha256` y `sigs`) con curl o wget a un
fichero oculto de `signatures.d`, y escribe `install <fuente> <fichero>
[<fichero.sig>]` en `/proc/hunter_feed`. El kernel lee él mismo el fichero, lo
comprueba y lo carga entero; solo cuando ha dicho que sí se renombra la fuente
a su sitio y se guarda una copia en `/var/lib/hunter/feeds/<fuente>/` (las
últimas cinco versiones). Una descarga vacía, fallida o rechazada conserva la
fuente anterior. La fuente por defecto es la lista de SHA-256 de malware de las
últimas 48 horas de MalwareBazaar, de abuse.ch. Prográmala con crond, p. ej.
`0 */6 * * * /sbin/hunterctl update-signatures`.

**Fuentes firmadas.** Una firma no es inofensiva por el hecho de que solo
pueda hacer que hunter rechace más: `hash:<sha256 de /bin/sh>` con `mode av
enforce` para el sistema. Una fuente con una `<clave>` en `/etc/hunter/feeds`
tiene que venir con `<url>.sig`, una cabecera de seis líneas firmada con
Ed25519 (RFC 8032):

```
hunter-feed-signature 1
feed <fuente>
serial <versión, un número que solo crece>
key <nombre de la clave>
sha256 <SHA-256 del fichero de la fuente>
ed25519 <firma sobre las cinco líneas de arriba>
```

El kernel (`hunter::feed`) solo la instala si la clave es de confianza, la
firma se verifica, el SHA-256 es el del fichero, el nombre de la fuente es con
el que se instala y el serial no es más antiguo que el instalado (una fuente
vieja reenviada se rechaza). Una fuente instalada firmada nunca se sustituye
por una sin firmar, y `require-signed-feeds on` rechaza del todo las fuentes
sin firmar; MalwareBazaar no firma nada, así que está apagado por defecto. Las
claves vienen de `feed-key <nombre> <hex>` (`/etc/hunter/keys` al arrancar,
leído antes que las fuentes; `hunterctl trust-key` en vivo). Las mismas
comprobaciones se hacen al arrancar en cada `signatures.d/<fuente>` con su
`<fuente>.sig`. El editor firma en otra máquina con
`tools/hunterctl/hunter-feed-sign` (OpenSSL 3): `keygen` crea una clave e
imprime su línea `feed-key`, `sign` escribe el `.sig`, y `from-sha256`
convierte una lista de MalwareBazaar en líneas de hunter, así que un espejo
firmado de ella son tres comandos.

**Vuelta atrás.** hunter recuerda qué firmas trajo cada fuente, así que una
versión nueva quita lo que tenía la vieja y ella no, salvo que otra fuente o
la base de datos local lo siga teniendo. `hunterctl feed-rollback <fuente>
[<versión>]` (`hunterctl feeds` lista las versiones guardadas) reinstala una
versión guardada con `rollback`, la única forma de pasar la comprobación del
serial, y sus firmas desaparecen en vivo, sin reiniciar: el remedio para una
fuente cuya versión más nueva bloquea un programa legítimo. Una vez sellado,
ninguna firma sale de la base de datos: las actualizaciones y las vueltas
atrás siguen añadiendo, y lo que quitarían se queda hasta reiniciar. El sello
también congela las claves y `require-signed-feeds on`.

**Escaneos programados.** `/etc/hunter/schedule` guarda los escaneos que se
ejecutan solos, uno por línea: `<tarea> <cuándo> <HH:MM>
<report|quarantine> <ruta>...`, donde `cuándo` es `daily`, un día de la semana
(`mon` .. `sun`) u `off`, en hora local. La imagen trae dos: `quick`, a diario
a las 12:00, sobre los directorios de programas, `/tmp`, `/root` y `/home`; y
`full`, los domingos a las 13:00, sobre `/` (un escaneo nunca entra en
`/proc`, `/sys`, `/dev`, `/run` ni en la cuarentena). Los dos solo informan
por defecto; `quarantine` además aparta lo que encuentren. El servicio
`hunter-scheduler` (`hunterctl scheduler`, supervisado por eclipse-init)
comprueba una vez por minuto. Una tarea toca en la primera franja después de
aquella en la que empezó por última vez, como anacron, así que una máquina que
estaba apagada a mediodía ejecuta el escaneo del día cuando vuelve, una vez.
Una tarea que se ve por primera vez empieza a contar desde ese momento.

- `hunterctl schedule` lista las tareas con su última y su próxima ejecución.
- `hunterctl schedule quick mon 08:30 quarantine` cambia los campos nombrados
  y deja en paz el resto del fichero, comentarios incluidos.
- `hunterctl run-scheduled full` ejecuta una tarea ya. Se apunta como una
  ejecución programada, con el disparador `manual`.
- `hunterctl scan-history [N]` imprime las últimas ejecuciones de
  `/var/lib/hunter/scans/history`: fecha, tarea, disparador, segundos,
  ficheros, infectados, en cuarentena, estado (`clean`, `infected` o `failed`)
  y el fichero del informe. Se guardan los 30 informes y las 1000 ejecuciones
  más recientes.

Solo se ejecuta un escaneo a la vez (un fichero de pid en
`/var/lib/hunter/scans/lock`). Cada hallazgo es un evento `AV DETECT` como el
de cualquier otro escaneo, así que llega al escudo de lunarbar. Una ejecución
que lanzó el reloj también saca una tarjeta en el escritorio (`hunterctl
notify`): una nota de paso cuando está limpio, un aviso cuando todo lo que
encontró se puso en cuarentena, una tarjeta crítica con un botón de
**cuarentena** cuando quedó algo en su sitio, y un aviso con la ruta del
informe cuando la ejecución no terminó. Una ejecución lanzada a mano no saca
nada. La pestaña **Programados** de lunarhunter muestra las mismas tareas y el
historial, y las cambia con estos comandos.

**Límites.** Los hashes cubren el fichero entero. Los patrones de bytes se
buscan en los primeros 128 MiB de una imagen, y cada patrón es una pasada
anclada aparte, lo que encaja con los cientos de patrones que cuida un
operador y no con una base de datos comercial. Las bibliotecas compartidas que
se cargan después con `mmap(PROT_EXEC)` todavía no se escanean; sus bytes solo
se escanean bajo demanda.

## Soportes extraíbles (memorias USB, tarjetas, discos externos)

[`hunter::media`](../hunter/src/media.rs) escanea cada sistema de ficheros
montado desde un dispositivo de bloques después del arranque: una memoria USB,
una tarjeta SD, un disco externo, una imagen loop. `mount(2)` avisa a hunter de
cada uno (los montajes del arranque desde `/etc/fstab` no pasan por ahí), y
`/proc/hunter` los lista:

```
media: auto-scan=on lock=off devices=1
media-device: id=3 state=pending files=0 infected=0 fstype=vfat source=/dev/sdb1 target=/media/usb
```

**Quién escanea.** El kernel no recorre él mismo el árbol nuevo: `mount(2)` no
volvería hasta haber leído todos los ficheros de una memoria de 64 GB. Lo hace
un agente de espacio de usuario, con los mismos lotes de `/proc/hunter_scan`
que `hunterctl scan`, y responde en el lenguaje de control (`media <id>
scanning`, y luego `media <id> done <ficheros> <infectados>` o `media <id>
failed`):

* **lunarbar** (como root) ve un dispositivo `state=pending` en su siguiente
  lectura de `/proc/hunter`, en pocos segundos, y ejecuta `hunterctl
  scan-media <id>` en segundo plano, un dispositivo cada vez. El menú de su
  escudo tiene una línea para el dispositivo que se está escaneando (o uno
  infectado, o el más nuevo), y la pestaña *Resumen* de lunarhunter lista cada
  dispositivo con su veredicto, los hallazgos de uno infectado y un botón para
  volver a escanearlo.
* **`hunterctl scan-media`** escanea a mano todos los dispositivos pendientes:
  en una consola sin escritorio, o cuando la barra no corre como root. Con un
  id o un punto de montaje vuelve a escanear ese dispositivo.

`scan-media` recorre con `find -xdev` (un disco montado dentro de la memoria es
su propio dispositivo), deja un informe por dispositivo en
`/run/hunter/media/<id>` (las líneas `FOUND` y `ERROR` y un resumen) y sale
con 1 si encontró algo.

**Qué hace una detección.** Un dispositivo infectado es un evento `CRIT MEDIA
INFECTED`, así que el escudo de la barra pasa a alerta. Cada fichero infectado
se marca con `deny` (la lista negra de exec, hasta reiniciar), así que no se
ejecuta ni con `mode av report`. Los ficheros no se mueven a la cuarentena:
están en la memoria de alguien, y `hunterctl quarantine` está a un paso.

**El bloqueo.** `media-lock on` se niega a ejecutar nada desde un dispositivo
cuyo escaneo no haya vuelto limpio: pendiente, en escaneo, fallido o infectado
(`CRIT MEDIA BLOCKED`). Se pregunta por la ubicación real del fichero, como la
lista negra, así que un enlace en otro sitio hacia un programa de la memoria
sigue siendo ese programa. Está apagado por defecto, porque sin agente un
dispositivo se queda pendiente y sus programas no se ejecutarían nunca;
`hunterctl lockdown` lo enciende. `media-scan off` monta los dispositivos
nuevos como `state=skipped`, que el bloqueo deja pasar.

**Límites.** Solo se ejecuta un escaneo a la vez, y `/proc/hunter_scan` guarda
un solo lote de resultados para todos: un `hunterctl scan` manual mientras se
escanea una memoria puede mezclar sus resultados. Un fichero escrito en la
memoria después de su escaneo no se vuelve a escanear hasta que se ejecuta
(al acceder) o se vuelve a escanear el dispositivo.

## Monitor de red

`hunter::netmon` responde a las dos preguntas que `/proc/net/tcp` no puede:
**qué programa tiene qué socket**, y **si algo se ha puesto a escuchar sin que
nadie lo esperara**. Lo primero que hace una puerta trasera en cuanto se
ejecuta es abrir un puerto (`nc -l -p 4444 -e /bin/sh`) o llamar a casa, y los
demás dominios solo vigilan lo que se ejecuta.

El kernel recorre la tabla de descriptores de cada proceso y le da a hunter una
entrada por (proceso, socket inet), como los empareja `netstat -p`. Un
**puerto a la escucha** es un socket TCP en `LISTEN`, o un socket UDP sin
conectar en un puerto por debajo del rango efímero (49152): la consulta DNS de
un cliente no es un servicio, `bind(0.0.0.0:53)` sí. Un puerto a la escucha que
ninguna regla espera es un evento `WARN` de la categoría `NET` (así se enciende
el escudo de lunarbar), una vez por proceso y puerto:

```text
[  412] +  1203.551s pid=733   WARN   NET       WARNING: unexpected listener: tcp 0.0.0.0:4444 by /tmp/.x/nc
```

Los puertos TCP a la escucha se informan desde `sys_listen` en el momento en
que se abren; los UDP, y todo lo demás, en la siguiente lectura de la tabla.
Los que solo escuchan en loopback están exentos por defecto, porque la red no
puede llegar a ellos. El monitor solo vigila: nunca rechaza un `bind`,
`listen` ni `connect` (eso lo decide un cortafuegos, por programa), así que no
puede romper un servicio por equivocarse.

**`/proc/hunter_net`**: al leerlo (cualquiera, como `/proc/net/tcp`), una
instantánea nueva:

```text
hunter netmon
alerts=on ignore-loopback=on sockets=3 listeners=2 unexpected=1 reported=1
expect udp 68
expect udp 546
expect tcp 22 /usr/sbin/sshd
sock 412 tcp LISTEN 0.0.0.0:22 - listen /usr/sbin/sshd
sock 733 tcp LISTEN 0.0.0.0:4444 - ALERT /tmp/.x/nc
sock 612 tcp ESTAB 10.0.2.15:49822 93.184.216.34:443 ok /usr/bin/firefox
```

Al escribirlo (root), un script en el pequeño lenguaje propio de netmon, todo
o nada:

| Directiva | Efecto |
|---|---|
| `expect <tcp\|udp\|any> <puerto\|bajo-alto> [<programa>]` | ese puerto a la escucha se espera; con un programa, solo para ese programa |
| `forget <tcp\|udp\|any> <puerto\|bajo-alto> [<programa>]` | quitar una regla escrita igual |
| `alerts <on\|off>` | los puertos a la escucha inesperados generan eventos o no |
| `ignore-loopback <on\|off>` | los que solo escuchan en loopback están exentos o no |

Una vez sellado, las directivas que silencian alertas (`expect`, `alerts off`,
`ignore-loopback on`) se rechazan y se registran, como toda relajación.

**`/etc/hunter/ports`** lleva las mismas directivas y se lee al arrancar antes
de `policy.conf` (línea a línea: una línea mala pierde una regla, que se nombra
en el log). Los clientes DHCP (`udp 68`, `udp 546`) se esperan sin necesidad
de línea. `hunterctl net` imprime la tabla; `hunterctl expect-port tcp 22
/usr/sbin/sshd` y `hunterctl forget-port ...` cambian las reglas en vivo y el
fichero a la vez.

## Detección de intrusiones (heurísticas)

En el camino caliente de las syscalls se producen dos señales, *después* de la
comprobación de política, así que solo observan llamadas a las que se dejó
ejecutarse:

1. **Vigilancia de syscalls sensibles**: clasificación en tiempo constante de
   cada syscall contra una pequeña tabla de operaciones relevantes para la
   seguridad (carga de módulos del kernel, `ptrace`, `bpf`, cambios de
   credenciales, escapes de espacios de nombres, `kexec`, escrituras entre
   procesos, …). Las coincidencias se apuntan como eventos de auditoría; aquí
   nunca se bloquean salvo que esté echado el cerrojo opcional
   privileged-deny. La tabla se elige por arquitectura (x86_64, y asm-generic
   para aarch64/riscv64).

2. **Anomalías de tasa**: contadores baratos por proceso de ventana deslizante
   que marcan **inundaciones** de syscalls (posible DoS) y **bombas fork**, más
   una señal de tasa de fork de todo el sistema para bombas fork
   *distribuidas*. Cada anomalía se informa como mucho una vez por ventana para
   evitar tormentas en el log; con el modo `Enforce` del dominio `anomaly` la
   syscall culpable se deniega. Se activa o desactiva con
   `heuristics::set_anomaly_detection`.

## Ransomware e inyección de procesos (`behavior`)

Las firmas pillan lo que alguien ya ha visto; el dominio `behavior` pilla lo
que un ataque tiene que *hacer*. Código: `hunter/src/behavior.rs` (dominio,
linaje, contención), `ransomware.rs`, `injection.rs`; el lado del kernel es
`linux-syscall/src/file/hunter_gate.rs` y `task/process_vm.rs`.

### Ransomware

El kernel avisa a hunter de cada escritura, renombrado y borrado de un fichero
normal (se saltan los pseudoficheros de `/proc`, `/sys`, `/dev` y las
tuberías). Cada proceso tiene una ventana de 10 segundos, y se da la alarma
cuando dentro de ella:

| Regla | Salta con |
|------|----------|
| cifrar y sustituir | ≥ 12 ficheros escritos con contenido de aspecto aleatorio **y** ≥ 12 de sus originales borrados (`a.docx` desaparece, se escribe `a.docx.locked` o `a.locked`, en cualquier orden) |
| cifrar en el sitio | ≥ 12 ficheros que ya tenían contenido sobrescritos con bytes de aspecto aleatorio |
| renombrado masivo  | ≥ 12 renombrados seguidos que dan a los ficheros una extensión nueva, añadida o sustituyendo la vieja (`a.docx` → `a.docx.locked`, `b.pdf` → `b.WNCRY`) |
| canario            | cualquier escritura, renombrado o borrado de un señuelo declarado con `canary <ruta>` |

Cada regla necesita dos señales porque una sola es normal: descomprimir un
archivo de fotos escribe muchos ficheros de aspecto aleatorio (los datos
comprimidos parecen aleatorios), y `rm -rf` borra miles, pero ninguno de los
dos destruye lo que leyó. Los nombres también cuentan: un gestor de paquetes
escribe `x.png.dpkg-new`, lo renombra sobre `x.png` y borra `x.png.dpkg-tmp`,
lo que no enlaza ninguna copia con su original, así que una actualización no
dispara nada. Lo que sí se parece exactamente a un ransomware es `gzip` sobre
una docena de ficheros a la vez (`a.log` → `a.log.gz`, original borrado): es la
misma forma, y `exempt /usr/bin/gzip` es la respuesta si es algo habitual.
Sobrescribir en el sitio se distingue de un editor de imágenes que guarda
encima de sus ficheros por el truncado: el editor abre con `O_TRUNC`, así que
el fichero ya está vacío cuando escribe. «De aspecto aleatorio» es el número de
valores de byte distintos en los primeros 512 bytes de una escritura (≈221 en
un texto cifrado, unas pocas docenas en un texto, menos de 200 en código
máquina); las escrituras más cortas no se juzgan.

Con `Enforce`, la operación que cruza la línea se rechaza con `EPERM` y el
proceso queda **contenido**: cada escritura, renombrado o borrado posterior de
un fichero por su parte — o por los hijos que cree después, o tras un
`execve` — también se rechaza, hasta que salga o `release <pid>`. Los
pseudoficheros siguen siendo escribibles.

Los canarios son la señal más fuerte y no cuestan nada: `hunterctl canary
/root/Documents/.~presupuesto.xlsx /home/u/Documentos/.~nominas.ods` crea los
ficheros, los declara y añade líneas `canary` a `/etc/hunter/policy.conf` para
que queden armados en cada arranque.

### Inyección de procesos

`process_vm_writev`, `ptrace(POKETEXT/POKEDATA/POKEUSER/SETREGS/SETFPREGS/
SETREGSET)` y `ptrace(ATTACH/SEIZE)` se juzgan según quién sea el objetivo. Un
proceso que escribe en sí mismo o en uno de sus descendientes es lo que hace
un depurador (`gdb ./prog`, `strace -f make`) y pasa; escribir en cualquier
otro (un hermano, un padre, un extraño) da la alarma, una vez por objetivo, y
con `Enforce` se rechaza con `EPERM`. hunter conoce el linaje gracias a
`task_fork`. `exempt /usr/bin/gdb` deja que `gdb -p <pid>` se enganche a
procesos que no lanzó él. Eclipse todavía no implementa `ptrace` ni
`/proc/<pid>/mem`: un intento de `ptrace` se sigue juzgando por los argumentos
de su syscall, y `hunter::check_process_write` es el gancho al que llamaría un
escritor de `/proc/<pid>/mem`.

`/proc/hunter` muestra el modo en la línea `enforcement:` (`behavior=`) y una
línea `behavior:` con los canarios, los programas exentos, los procesos
contenidos y el número de alarmas. Los eventos se apuntan bajo `RANSOMWARE` e
`INJECTION`.

## `/proc/hunter`

Leer `/proc/hunter` genera un informe en vivo: una cabecera de estado (modos
por dominio, contadores de eventos, políticas de syscalls activas) seguida del
anillo de eventos recientes. Ejemplo:

```
hunter security subsystem v0.4.0
enforcement: syscall=enforce wx=report exec=report anomaly=report
events: total=3 blocked=0 warnings=1 reported=1 critical=0 dropped=0 critical_dropped=0
active syscall policies: 0
heuristics: anomaly-detection=off privileged-deny=off
trusted-program allowlist: inactive (0 entries)

recent events (oldest first):
[     0] +         0.000s pid=0     INFO   SYSTEM    INIT: hunter security subsystem v0.3.0 initialized (...)
[     1] +         4.512s pid=142   NOTICE PRIVILEGE WATCH: sensitive syscall #101 (ptrace)
[     2] +         5.001s pid=142   WARN   ANOMALY   WARNING: possible fork bomb: >200 clone/fork within 1000ms
```

## Configuración desde código

```rust
use hunter::{policy, Mode};

// Limitar un proceso a una lista blanca de syscalls (un seccomp ligero).
policy::register_policy(pid, vec![/* números de syscall permitidos */]);

// Endurecer W^X hasta bloquear de verdad.
policy::set_wx_mode(Mode::Enforce);

// Tratar como fatal la ejecución desde rutas escribibles por todos.
policy::set_exec_mode(Mode::Enforce);
policy::add_untrusted_exec_prefix(String::from("/run/user/"));
```
