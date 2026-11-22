# Open POSIX Test Suite en Eclipse OS

Mide cuánto POSIX cumple Eclipse OS con los 1607 tests de
`conformance/interfaces` de la [Open POSIX Test Suite](https://github.com/linux-test-project/ltp/tree/master/testcases/open_posix_testsuite)
(parte de LTP), dentro de QEMU x86_64.

```sh
python3 scripts/open-posix/run.py              # compila, monta la imagen, arranca y compara
python3 scripts/open-posix/run.py --no-build   # con la imagen que ya hay
python3 scripts/open-posix/run.py --only mq_open,sem_post
```

- `build.sh` baja LTP a un commit fijo en `ignored/open-posix/` (no se copia
  al repositorio: es GPL-2.0), compila cada test en estático con el musl de
  `cargo image` y lo deja en `rootfs/x86_64-minimal/libc-test/open-posix`.
- `opts-run.c` corre los tests dentro, cada uno en su grupo de procesos y con
  tope de tiempo, y escribe `R <código> <segundos> <test>` por test.
- `run.py` arranca QEMU, lanza `opts-run`, cuenta PASS por área y compara con
  `expected-x86_64.txt`: sale con 1 si un test que no está ahí deja de pasar,
  y lista los que están ahí y ya pasan, para quitarlos.

Una pasada entera tarda unos 30 minutos con TCG y 3 CPUs, así que no es para
cada PR: es para lanzarla a mano, o cada noche, al tocar señales, hilos,
timers, memoria o IPC.

Medido el 2026-11-22: Eclipse pasa 1316 de 1596 (82,5 %); Linux, con los
mismos binarios, 1412 (88,5 %). Casi toda la diferencia son las colas de
mensajes POSIX, que no existen. Con musl, la planificación POSIX
(`sched_setscheduler` y compañía) es ENOSYS por diseño de musl, y esos tests
fallan igual en Linux.
