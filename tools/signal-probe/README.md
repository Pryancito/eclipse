# signal-probe

Sonda de **integridad** de la entrega de señales de Eclipse OS: si una señal
devuelve el programa a donde estaba, como estaba. Un binario estatico de musl,
sin dependencias, que se copia al rootfs y se corre desde la shell.

## Por que existe

Una señal es el unico sitio donde el kernel coge un programa en marcha, le hace
ejecutar otro trozo de si mismo y tiene que dejarlo exactamente como estaba.
Todas las llamadas que intervienen pueden devolver 0 con la vuelta mal hecha, y
en este kernel ha pasado de maneras que ningun test de syscalls ve:

1. El contexto que interrumpe una señal se guardaba en **una** ranura por hilo,
   asi que una segunda señal dentro de un manejador pisaba la de la primera, y
   el `sigreturn` del manejador de fuera fallaba con `EINVAL`. Un `sigreturn`
   que falla **vuelve**, a lo que haya detras del trampolin de libc. (#1943)
2. Un fallo despues de salir de un manejador con `siglongjmp` mataba el proceso
   aunque tuviera su manejador de SIGSEGV puesto. (#1943)
3. Un manejador que reescribia el `rip` guardado con una direccion no canonica
   tumbaba **el kernel entero**: la CPU falla en la vuelta a modo usuario, en
   el anillo 0. (#1943)
4. Un huerfano conservaba la señal de muerte que habia elegido su padre muerto,
   asi que un hijo clonado con SIGTERM o SIGUSR1 cuyo padre salia se la mandaba
   a init al morir, y eclipse-init reinicia o se apaga con esas. Y la muerte de
   un huerfano se avisaba a quien saliera de buscar desde el padre que lo **creo**,
   no al que lo adopto.
5. Un `SIGSTOP` (o el `SIGTSTP` de Ctrl-Z) a un proceso metido en una syscall
   que espera no lo paraba hasta que la syscall volvia sola: un hijo en
   `pause()` no se paraba **nunca**, y el `waitpid(WUNTRACED)` del padre, el
   de una shell que espera para escribir «Stopped», se quedaba esperando con
   el.
6. Matar un hilo no lo despertaba si dormia en `pause`, `nanosleep`, un `read`
   o un `waitpid`: un `execve` desde un segundo hilo esperaba para siempre a
   que muriera su hermano en `pause()`, y un proceso que salia con un hilo
   dormido se recogia pero nunca se desmontaba, y su memoria no volvia.
7. La imagen que arrancaba un `execve` desde un segundo hilo corria con el tid
   de ese hilo y no con el pid: `gettid() != getpid()` en lo que Linux hace
   primer hilo, y un `tgkill(pid, pid)` no llegaba a nadie.
8. Un padre que esperaba en `vfork` a un hijo que no llegaba a hacer `exec`
   no se podia matar, y un proceso que salia con un hilo en esa espera no se
   desmontaba nunca.
9. Un manejador que corria sobre un hilo esperando un mutex
   `PTHREAD_PRIO_INHERIT` ocupado le daba al mutex un segundo dueño: el
   `FUTEX_LOCK_PI` volvia con `EINTR`, y el `pthread_mutex_lock` de glibc lo
   toma por el cerrojo cogido. Linux lo rehace sea cual sea el manejador.
10. Una espera futex sin timeout volvia con `EINTR` de un manejador con
    `SA_RESTART`, que Linux rehace, asi que el `sem_wait` de glibc devolvia
    -1 sin el semaforo a un programa que instala sus manejadores con
    `signal()`.
11. Un hijo de `vfork` corria en una COPIA de la memoria de su padre, asi que
    lo que escribia antes del `exec` o del `_exit` el padre no lo veia: el
    `posix_spawn` de glibc de un programa que no existe devolvia 0, porque
    se entera del `exec` fallido por una palabra que escribe el hijo.

## Compilar

```sh
make                       # x86_64-linux-musl-gcc -O2 -static
make CC=musl-gcc
make linux                 # el mismo fuente con el cc del host
```

`cargo xtask rootfs` / `image` ya la compila y la copia a `/bin/signal-probe`
dentro de la imagen, igual que el resto de las sondas de `tools/`.

## Correr

```sh
signal-probe [--only SECCION]
```

Salida: **0** si todo se cumple, **1** si algo falla, **2** si la linea de
orden esta mal. Cada seccion corre en un hijo propio con una alarma de 20 s,
asi que una seccion que mata su proceso o se cuelga sale como FAIL y las demas
siguen.

**Correrla tambien en Linux, en la misma maquina.** Todo lo que comprueba es
algo que Linux cumple, asi que un FAIL en Linux es un fallo de la sonda y un
FAIL solo en Eclipse es un fallo de Eclipse.

## Las secciones

| seccion | que comprueba |
| --- | --- |
| `nest` | un manejador interrumpido por otra señal vuelve a donde estaba, con sus registros vectoriales (`xmm8`), y el programa tambien |
| `longjmp` | salir de un manejador con `siglongjmp` no impide que el siguiente fallo llegue a su manejador; tampoco un fallo dentro del manejador de otra señal |
| `fork` | un hijo hecho con `fork` dentro de un manejador vuelve de ese manejador |
| `restart` | con `SA_RESTART`, `read` y `wait4` se reanudan tras el manejador; `nanosleep` y `poll` dan `EINTR` igualmente, como en Linux; `sem_wait` (una espera futex sin timeout) tambien se reanuda; coger un mutex PI ocupado se rehace tras cualquier manejador, con `SA_RESTART` o sin el |
| `badrip` | un manejador que pone un `rip` no canonico en su `ucontext` le cuesta al proceso un SIGSEGV (`SI_KERNEL`) y nada mas. Solo x86_64 |
| `ids` | el kernel rechaza lo que no puede cargar: un puntero de TLS fuera del espacio de usuario (`arch_prctl`, `clone(CLONE_SETTLS)`) y el pid `INT_MIN`, que no tiene negativo, en `waitpid` |
| `orphan` | la muerte de un huerfano avisa a quien lo **adopto**, con SIGCHLD aunque su padre muerto eligiera otra señal en `clone(2)`, y despierta el `waitpid` del adoptante aunque un proceso de en medio se haya hecho subreaper despues de la adopcion. La señal elegida es SIGWINCH, que init descarta, por si el subreaper no se cumpliera |
| `inherit` | lo que pasa por `fork` (manejadores, mascara y pila alternativa; ni señales pendientes ni alarma) y por `execve` (mascara, pendientes, alarma y `SIG_IGN`; los manejadores vuelven a `SIG_DFL` y la pila alternativa se va). La mitad del `execve` la cuenta la propia sonda relanzada con `--exec-report` |
| `wait` | lo que cuentan `waitid` y `waitpid`: una salida con `WNOWAIT` y luego recogida; una muerte por SIGTERM (`CLD_KILLED`); una parada de un hijo en `pause()` y una continuacion, cada una contada una vez; `waitid(P_PGID, 0)`, que es el grupo propio; `SA_NOCLDSTOP`; `exit()` desde un segundo hilo, que acaba con todo el proceso; y `execve()` desde un segundo hilo, cuya imagen nueva toma el pid como tid, conserva la mascara de ese hilo y recibe un `tgkill(pid, pid)` |
| `stop` | una parada llega a un proceso metido en una syscall que espera (`nanosleep`, `read`, `sigsuspend`) y la syscall sigue cuando se le continua: el `read` lee, sin `EINTR`, y el `sigsuspend` sigue esperando su señal y al volver deja la mascara de antes |
| `teardown` | un proceso que sale (`exit_group`) con un segundo hilo dormido en `pause`, `read`, `nanosleep`, `waitpid` o en el `vfork` de un hijo que no hace `exec` ya no tiene ese hilo cuando se le recoge: un mutex robusto compartido que el hilo tenia cogido vuelve con `EOWNERDEAD`. `/proc/meminfo` no sirve para esto ni en Linux, que a veces enseña tarde unos cientos de MiB liberados |
| `spawn` | un hijo de `vfork` corre en la memoria de su padre: lo que escribe ahi lo lee el padre, asi que `posix_spawn` informa de un programa que no existe y de una accion de fichero que falla con `ENOENT`, y veinte `posix_spawn` de `sh -c 'exit 3'` seguidos devuelven cada uno su hijo con su estado |
