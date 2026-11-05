# firefox-probe

[English](README.md) · **Español**

Firefox es una prueba exigente para una personalidad Linux, y lenta de
depurar: son cien megas de navegador que o pintan una ventana o no. Esta
herramienta hace las mismas preguntas que hace Firefox, una a una, y dice qué
respuestas están mal.

Cada comprobación imita un patrón sacado del **propio código de Firefox**, no
de una suposición sobre lo que probablemente hace un navegador, y cada una
nombra el fichero del que sale. Eso importa: la primera versión de esta sonda
probaba un descriptor de solo lectura con `F_DUPFD_CLOEXEC`, que es el camino
de Firefox en *FreeBSD*; en Linux vuelve a abrir `/proc/self/fd/N`, algo
completamente distinto que hay que hacer bien.

## Uso

```sh
make                      # x86_64-linux-musl-gcc, estático
./firefox-probe           # solo el resumen
./firefox-probe -v        # una línea por comprobación
```

Ejecútalo primero en una máquina Linux. Ahí tiene que dar **0 fallos**; un
fallo en Linux significa que la sonda está mal, no el kernel. Después ejecuta
el mismo binario en Eclipse. El código de salida es el número de fallos.

`cargo xtask image` lo mete en el rootfs como `/bin/firefox-probe`.

## Qué cubre, y por qué importa cada parte

| Sección | Qué se rompe si falla |
|---|---|
| **Memoria compartida** | `memfd_create` con `MFD_ALLOW_SEALING`, el sellado de tamaño y la recomprobación con `F_GET_SEALS`. `Platform::IsSafeToMap` de Firefox se niega a mapear *cualquier* segmento compartido cuyos sellos no informen de `F_SEAL_SHRINK`, así que un kernel que acepta `F_ADD_SEALS` y luego dice «sin sellos» pierde todos los búferes de IPC y todos los frames pintados. También `/proc/self/fd/N`, que es como Firefox hace la copia de solo lectura que le pasa a un proceso de contenido. |
| **Paso de fd** | `SCM_RIGHTS` sobre un socketpair `SOCK_CLOEXEC\|SOCK_NONBLOCK`. Es la única forma de que un handle de memoria compartida llegue a un hijo. |
| **Lanzamiento de procesos de contenido** | `fork`, `execve` con un descriptor de canal heredado, `fstat` sobre ese descriptor, `/proc/self/exe`. Una compilación de distribución de Firefox no se puede ejecutar en un solo proceso (ver abajo), así que este camino no es opcional. |
| **Bucle de eventos** | `futex`, `eventfd`, `epoll`, `timerfd`, `pipe2`, `sched_getaffinity`, `getrandom`, `prctl(PR_SET_NAME)`. |
| **Memoria del JIT** | Una reserva `PROT_NONE` grande, cambios W^X con `mprotect` y que `MADV_DONTNEED` devuelva páginas a cero: la corrección de mozjemalloc depende de esto último. |
| **/proc** | `maps`, `statm`, `status`, `cpuinfo`, `meminfo`, `cmdline`, además de `statx` y `RLIMIT_NOFILE`. |

## No hay Firefox de un solo proceso

`/usr/local/bin/eclipse-firefox` pone `MOZ_FORCE_DISABLE_E10S=1`, con la idea
de que un navegador de un solo proceso no ejercita IPC entre procesos. Esa
idea no se sostiene en una compilación de distribución. En
`BrowserTabsRemoteAutostart` (`toolkit/xre/nsAppRunner.cpp`) la variable solo
se respeta cuando `allowDisablingE10s` es verdadero, y en una compilación
`MOZILLA_OFFICIAL` eso exige `xpc::AreNonLocalConnectionsDisabled()`, que es
falso en cualquier navegador que pueda llegar de verdad a la red. La imagen
instala la compilación de Mozilla desde su repositorio apt (el `firefox` de
Ubuntu es un stub de snap), y esa es una compilación `MOZILLA_OFFICIAL`, como
lo eran el `firefox` y el `firefox-esr` de Alpine, así que los procesos de
contenido se lanzan igualmente, y las secciones de IPC, memoria compartida y
lanzamiento de procesos de arriba están todas en el camino crítico.
