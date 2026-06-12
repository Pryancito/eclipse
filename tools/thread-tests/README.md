# Tests de multihilo del kernel (clone/futex)

Tests *freestanding* (sin libc) que reproducen la interacción de
`pthread_create` de musl con el kernel y validan las primitivas en las que se
apoya todo programa multihilo (sysbench, etc.):

- **thr.c** — creación de hilos: `clone` con los flags exactos de musl 1.2
  (`0x7d0f00`), TLS por `CLONE_SETTLS`, `CLONE_PARENT_SETTID`, semántica de
  `ctid` (el kernel **no** debe escribir el TID al crear si no hay
  `CLONE_CHILD_SETTID`, y debe limpiar+despertar en la salida por
  `CLONE_CHILD_CLEARTID` — de esto depende `pthread_join`).
- **thr2.c** — estrés de `FUTEX_WAIT`/`FUTEX_WAKE` entre dos hilos (20k
  iteraciones de ping-pong). Detecta *lost wakeups*: si el kernel comprueba el
  valor del futex fuera del lock de la cola de espera, un WAKE puede colarse
  entre la comprobación y el encolado y el hilo duerme para siempre (así se
  colgaba sysbench en «Initializing worker threads...»).

Ambos hacen las syscalls a mano: con la instrucción `syscall` en bare metal y
a través del puntero `rcore_syscall_entry` (parcheado por el loader) en libos.

Véase el `Makefile` para compilar y ejecutar.
