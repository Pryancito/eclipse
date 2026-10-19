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
12. Un `execve` de algo que no es un programa mataba al llamante con SIGSEGV
    en vez de volver con un errno: basura, un guion sin `#!` (que la shell
    corre ella misma al recibir `ENOEXEC`), un directorio, un binario de
    otra maquina, un binario dinamico cuyo `PT_INTERP` no esta instalado.
13. `exit(-1)` le llegaba al padre como una muerte por SIGHUP: se guardaba
    el `int` entero, y dentro del kernel un codigo negativo es como se
    escribe una muerte por señal.
14. Un hijo de `fork` y un hilo nuevo nacian con nice 0, en `SCHED_OTHER` y
    libres de correr en cualquier CPU, tuviera lo que tuviera su padre:
    `nice make` solo bajaba la prioridad de `make`. Y `SCHED_RESET_ON_FORK`
    se aceptaba y se tiraba.
15. Un static-pie (lo que construye el `gcc -static` de musl, esta sonda
    incluida) se cargaba en la direccion 0, asi que un puntero nulo leia su
    cabecera ELF en vez de fallar.
16. Un `waitpid` podia cosechar a un padre muerto antes de que sus hijos
    pasaran al adoptante, y el subreaper que lo cosechaba recibia `ECHILD`
    por el huerfano que estaba a punto de heredar, mas o menos una vez de
    cada treinta.
17. Un zombi devolvia su pid antes de que lo cosecharan, asi que un `fork`
    podia recibirlo mientras el zombi seguia esperando, y un `waitpid` del
    hijo nuevo recogia al muerto.
18. Una alarma que un proceso dejaba pendiente al morir saltaba en el proceso
    que heredaba su pid, que moria de ella. La sonda `orphan` moria asi de
    SIGALRM dos o tres vueltas de cada treinta, mucho antes de su propia
    alarma de 20 s.
19. El interprete de un `#!` solo podia ser un enlace simbolico a algo, no un
    enlace a un enlace (el de un `PT_INTERP`, cuatro), asi que un guion cuyo
    interprete pasa por `/etc/alternatives` daba `ELOOP`. Y un interprete que
    nadie puede ejecutar corria igual.
20. El tiempo de sistema de `getrusage` y `times` era el tiempo de reloj de
    una syscall de cada dieciseis, multiplicado por dieciseis: una shell que
    habia esperado medio segundo a un hijo llevaba segundos de CPU. `wait4`
    daba casi nada por un hijo que salia con `exit`, porque contaba solo los
    hilos que ya habian salido, y el reloj de CPU de un hilo dejaba fuera el
    tiempo en el kernel.
21. `RLIMIT_FSIZE` solo frenaba `truncate`, `ftruncate` y `pwrite`: `write`,
    `writev` y `fallocate` pasaban de el, un `pwrite` que lo cruzaba fallaba
    entero en vez de escribir hasta el, un fichero que ya lo pasaba no se
    podia recortar, y ningun `EFBIG` traia su `SIGXFSZ`.
22. `ITIMER_VIRTUAL` e `ITIMER_PROF` contaban tiempo de reloj, asi que
    saltaban en un proceso dormido, y `RLIMIT_CPU` no mandaba ni `SIGXCPU` ni
    `SIGKILL`: un bucle desbocado con `ulimit -t` seguia para siempre.
23. Un temporizador de `timer_create` sobre un reloj de CPU contaba tiempo
    de reloj: el de `CLOCK_PROCESS_CPUTIME_ID` saltaba con el proceso
    dormido, el de `CLOCK_THREAD_CPUTIME_ID` con el trabajo de otro hilo (el
    perfilador de Go arma uno asi en cada hilo), y los relojes de otro hilo
    o de otro proceso (`pthread_getcpuclockid`, `clock_getcpuclockid`) daban
    `EINVAL`.
24. `RLIMIT_AS` se guardaba y no lo miraba nadie: con `ulimit -v`, `mmap`,
    `brk`, `mremap` y `shmat` seguian creciendo el espacio de direcciones, y
    un `malloc` que debia dar `NULL` daba memoria.

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
| `orphan` | la muerte de un huerfano avisa a quien lo **adopto**, con SIGCHLD aunque su padre muerto eligiera otra señal en `clone(2)`, y despierta el `waitpid` del adoptante aunque un proceso de en medio se haya hecho subreaper despues de la adopcion. Y en cuanto `waitpid` ha cosechado al padre muerto, el huerfano ya es del adoptante (se pregunta 128 veces, porque es cuestion de tiempos). La señal elegida es SIGWINCH, que init descarta, por si el subreaper no se cumpliera |
| `inherit` | lo que pasa por `fork` (manejadores, mascara y pila alternativa; ni señales pendientes ni alarma) y por `execve` (mascara, pendientes, alarma y `SIG_IGN`; los manejadores vuelven a `SIG_DFL` y la pila alternativa se va). La mitad del `execve` la cuenta la propia sonda relanzada con `--exec-report` |
| `wait` | lo que cuentan `waitid` y `waitpid`: una salida con `WNOWAIT` y luego recogida; un codigo de salida negativo, que llega como su byte bajo (`exit_group(-1)` es 255 y el `exit(-3)` del ultimo hilo, 253); una muerte por SIGTERM (`CLD_KILLED`); una parada de un hijo en `pause()` y una continuacion, cada una contada una vez; `waitid(P_PGID, 0)`, que es el grupo propio; `SA_NOCLDSTOP`; `exit()` desde un segundo hilo, que acaba con todo el proceso; y `execve()` desde un segundo hilo, cuya imagen nueva toma el pid como tid, conserva la mascara de ese hilo y recibe un `tgkill(pid, pid)`. Y un zombi conserva su pid hasta que lo cosechan: ninguno de 512 `fork` lo recibe; y la alarma de un proceso muerto muere con el, aunque otro reciba su pid (en Linux el pid tarda demasiado en volver para preguntarlo) |
| `stop` | una parada llega a un proceso metido en una syscall que espera (`nanosleep`, `read`, `sigsuspend`) y la syscall sigue cuando se le continua: el `read` lee, sin `EINTR`, y el `sigsuspend` sigue esperando su señal y al volver deja la mascara de antes |
| `teardown` | un proceso que sale (`exit_group`) con un segundo hilo dormido en `pause`, `read`, `nanosleep`, `waitpid` o en el `vfork` de un hijo que no hace `exec` ya no tiene ese hilo cuando se le recoge: un mutex robusto compartido que el hilo tenia cogido vuelve con `EOWNERDEAD`. `/proc/meminfo` no sirve para esto ni en Linux, que a veces enseña tarde unos cientos de MiB liberados |
| `spawn` | un hijo de `vfork` corre en la memoria de su padre: lo que escribe ahi lo lee el padre, asi que `posix_spawn` informa de un programa que no existe y de una accion de fichero que falla con `ENOENT`, y veinte `posix_spawn` de `sh -c 'exit 3'` seguidos devuelven cada uno su hijo con su estado |
| `badexec` | un `execve` de lo que no es un programa vuelve, con el errno de Linux: `EACCES` sin bit de ejecucion, para un directorio o con un `PT_INTERP` que es un directorio; `ENOEXEC` para basura, un guion sin `#!`, un `#!` vacio o que nombra basura, y un ELF de otra maquina; `ENOENT` para un `#!` o un `PT_INTERP` que no existen; `EIO` para un `PT_INTERP` de 10 bytes y `ELIBBAD` para uno de 128 bytes de basura; un `#!` o un `PT_INTERP` que nombran un fichero sin bit de ejecucion dan `EACCES`, y un `#!` que pasa por tres enlaces corre; cinco guiones encadenados corren y seis dan `ELOOP`. Y `sh -c` de un guion sin `#!` lo corre |
| `sched` | lo que un hijo de `fork` y un hilo nuevo heredan del padre: el nice, la mascara de CPUs y la politica. Con `SCHED_RESET_ON_FORK` el padre la conserva y la informa (`sched_getscheduler`, `sched_getattr`) y el hijo nace sin ella: en `SCHED_OTHER` si el padre era de tiempo real, con nice 0 si el suyo era negativo y con el suyo si era positivo. Sin privilegio no se quita, `sched_setattr` no la acepta en la palabra de politica y `sched_getaffinity` quiere un buffer de `long` enteros. La seccion corre como root |
| `image` | el programa esta por encima de la pagina nula y leer por un puntero nulo mata con SIGSEGV, se enlazara como se enlazara (estatico, static-pie, dinamico). Compilada con el musl de `cargo image` la sonda es static-pie, que es el caso que fallaba |
| `cputime` | el tiempo de CPU que cuentan `getrusage`, `times`, `wait4` y los relojes de CPU: nada para un proceso dormido o esperando; los 300 ms de un hijo que trabaja, en `wait4`, en `RUSAGE_CHILDREN` y en `times()`, tambien los de un hilo que sigue trabajando cuando el proceso llama a `exit_group`; y la parte del kernel de un bucle de syscalls en `CLOCK_THREAD_CPUTIME_ID` y `CLOCK_PROCESS_CPUTIME_ID` |
| `fsize` | `RLIMIT_FSIZE` en toda escritura a un fichero: un `write`, `pwrite`, `writev` o `copy_file_range` que cruza el limite escribe hasta el, sin señal; uno que empieza en el limite o pasado da `EFBIG` y un `SIGXFSZ`, y una escritura vacia en el limite da 0. Un `ftruncate` o un `fallocate` que hacen crecer el fichero mas alla dan `EFBIG` y `SIGXFSZ`; recortar un fichero que ya lo pasaba se puede, aunque se quede pasado, y `FALLOC_FL_KEEP_SIZE` no es crecer. Una tuberia no tiene limite, y sin manejador `SIGXFSZ` mata |
| `cputimer` | `ITIMER_VIRTUAL` e `ITIMER_PROF` de 100 ms no se mueven mientras el proceso duerme 300 ms y saltan a los 100 ms de su tiempo de CPU (`ITIMER_VIRTUAL`, de usuario); con intervalo de 20 ms saltan entre 8 y 20 veces en 300 ms de CPU. Con `RLIMIT_CPU` 1/2, un `SIGXCPU` al segundo de CPU (no de sueño), el limite blando pasa a 2 y `SIGKILL` a los 2 s; con 1 y sin manejador, `SIGXCPU` mata |
| `cpuclock` | un temporizador de `timer_create` de 100 ms sobre un reloj de CPU se mueve solo lo que gasta el dueño de ese reloj (lo que dice `getrusage`, con 10 ms de margen: bajo un emulador la primera vuelta cobra decenas de ms de sistema, y la linea da el reparto usuario/sistema) y salta a sus 100 ms con `SI_TIMER` y su valor: el del proceso, que no se mueve mientras duerme 300 ms; el del propio hilo, que no se mueve con 300 ms de trabajo de otro hilo; el de otro hilo (`pthread_getcpuclockid`) y el de un hijo (`clock_getcpuclockid`), cuya señal llega al padre, ninguna mientras el hijo duerme. Con intervalo de 20 ms salta entre 8 y 20 veces en 300 ms de CPU; con `TIMER_ABSTIME` y una lectura ya pasada salta antes de que `timer_settime` vuelva; y un reloj dinamico da `EOPNOTSUPP` |
| `aslimit` | con `RLIMIT_AS` a 4 MiB por encima del `VmSize` de `/proc/self/status` (solo el blando, que vuelve al acabar): 2 MiB de `mmap` caben y 8 no (`ENOMEM`), ni un `malloc` de 16 MiB (`NULL`), ni un `brk` de 8 MiB (devuelve el de antes; uno de 64 KiB si), ni un `mremap` de 2 a 16 MiB, ni un `shmat` de 8 MiB; un `MAP_FIXED` sobre paginas ya mapeadas no suma, y lo que libera `munmap` se puede volver a mapear |
