# Tests de CI y diagnóstico del kernel

[English](ci-testing.md) · **Español**

El diagnóstico del kernel va por defecto a nivel INFO. LibOS lo escribe en
`kernel.log`, o en `ZCORE_KERNEL_LOG` si está definida. Su consola de usuario
escribe en stdout. La salida del log va en búfer y se vuelca al salir con
normalidad, con los registros ERROR y en los pánicos de Rust. Pon
`ZCORE_LOG_FLUSH=1` para volcar cada registro cuando diagnostiques fallos
nativos.

Las compilaciones para QEMU usan un canal de depuración propio: x86_64 usa el
puerto 0xe9 (debugcon), y AArch64/RISC-V usan el SYS_WRITE0 de semihosting. El
Makefile activa la feature del kernel y las opciones de QEMU que tocan.
`KERNEL_LOG=/ruta/absoluta` elige el fichero del host; por defecto es
`target/kernel-ARCH.log`. Semihosting solo se activa en compilaciones para
QEMU; no uses esa feature en hardware sin un monitor de semihosting.

`python3 scripts/zircon_core_test.py --arch x86_64 --libos --fast` descubre los
nombres reales de los tests a partir de la imagen y ejecuta los casos
clasificados en lotes acotados. Cada lote tiene ficheros `.guest.log`,
`.kernel.log` y `.host.log` en `target/test-logs/`; `results.json` guarda los
resultados y el motivo de cada skip. Para dar éxito hace falta un resumen
completo y no vacío, que cada caso pedido pase o se salte a sí mismo de forma
explícita, y que el invitado salga bien. Un nombre de test que falta no puede
convertirse en un pase vacío.

Los huecos temporales de implementación están en
`scripts/test_expectations/zircon.json`, con la evidencia de la ejecución de CI
34011543846 y de la reproducción local. Las clasificaciones heredadas
FAILED/TIMEOUT/PARTIAL y los casos nuevos sin clasificar se reportan como
skips. Las regresiones verificadas de channel, Counter, IOVec y estrés de
puertos se promueven de forma explícita. Usa `-t 'Suite.Case'` y
`--include-skipped --timeout 300` para investigar los casos saltados. La misma
política de skips vale para las ejecuciones de CI de un PR y las lanzadas a
mano.

`scripts/linux_libc_test.py` aplica las clasificaciones de libc existentes y
otras regresiones confirmadas sin cambiar el submódulo de tests: las dos
variantes de enlazado de `pthread_cancel` en baremetal; en x86_64 `modfl.exe`,
`log.exe`, `ipc_sem-static.exe` y `pthread_rwlock-ebusy-static.exe`; en
AArch64 `pthread_tsd`, `tls_local_exec-static` y `pthread_rwlock-ebusy-static`;
en AArch64/RISC-V `tls_init`, `pthread_once-deadlock` y `pthread_exit-cancel`;
y en RISC-V `pthread_rwlock-ebusy`. Para estos casos del ciclo de vida de hilos
se saltan las dos variantes de enlazado salvo que se dé un sufijo. La CI sube
los ficheros de diagnóstico del invitado, del kernel y del host aunque falle un
job. Los tests de libc de LibOS también usan INFO y guardan un log de kernel y
de host aparte para cada caso. El runner de libc en QEMU espera una marca de
fin única del invitado y comprueba su código de salida; ni los prompts de la
shell ni los comandos que se reflejan pueden contar como un final con éxito.
También espera al prompt del invitado antes del primer caso (hasta 300 s): el
submódulo le da 5 s al arranque, y bajo TCG la shell tarda unos 30 s en x86_64
y unos 15 s en aarch64, así que todos los casos de los dos jobs agotaban el
tiempo sin salida. Los dos casos de `pthread_atfork-errno-clobber` corren como
`nobody`, porque `RLIMIT_NPROC` no limita a root aquí igual que no lo hace en
Linux, y se espera que pasen.

La dependencia trapframe está fijada a `codex/zcore-fncall-integration`,
basada en el `codex/fncall` de upstream. zCore activa `fncall-preserve-x18`
porque Fuchsia usa x18 para las pilas de shadow-call. Linux localiza el
contexto activo con el ID de hilo del host; Darwin usa su ranura específica de
pthread. La CI nativa de ARM ejecuta los tests de ABI/disposición de trapframe
antes de ejecutar zCore. Los registros SIMD tienen campos con nombre Q0–Q31 y
salida Debug en varias líneas, con los offsets del ensamblador comprobados.

Los tests de IRQ en baremetal necesitan un recurso de arranque que hoy no está
en userboot. El test de estrés de la carrera de registro pasa de 300 segundos
con trazas INFO bajo QEMU, así que sigue activo por defecto en Linux nativo.
Las dos limitaciones están explícitas en el fichero de expectativas y se pueden
sobrescribir.

En AArch64 macOS hoy se compila zCore y se ejecutan los tests de disposición y
concurrencia de trapframe, pero se salta de forma explícita la ejecución del
invitado Fuchsia. Darwin borra x18 al volver de una excepción del host salvo
que el proceso tenga `com.apple.private.custom-x18-abi`. Los runners estándar
de macOS de GitHub rechazan los ejecutables ad hoc con ese entitlement privado
(SIGKILL, CI 34018139132). La imagen de Fuchsia usa x18 para su pila de
shadow-call y se cae durante el arranque de libc sin él (CI 34016355478). El
motivo del skip queda en el log del job y en `results.json`; no cuenta como
una ejecución correcta del invitado. Los tests de trapframe que necesitan x18
también se ignoran en macOS hasta que haya un host con la autorización
adecuada. Linux AArch64 ejecuta la imagen real de Fuchsia y los tests de
preservación de x18.

Las ejecuciones en QEMU desactivan el reinicio automático para que un fallo del
kernel no pueda reintentar un invitado en silencio. Los casos de
process-info/debug con SMP en x86_64 provocan hoy un reinicio al desmontar un
proceso hijo después de que pasan sus aserciones; esos casos se saltan de forma
explícita hasta que haya un arreglo (CI 34018139132 y reproducción local).
