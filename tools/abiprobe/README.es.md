# abiprobe — la ABI de Linux en la que se apoya una app Electron antes de imprimir nada

[English](README.md) · **Español**

Un único binario estático que hace, una a una, las llamadas que hace el
proceso principal de Chromium/Electron entre `main()` y su primera línea de
log, y dice PASS o FAIL con el errno de cada una. En un kernel Linux de verdad
sale todo en verde, así que un FAIL aquí es una divergencia nuestra y nada
más.

Existe porque `claude-desktop --no-sandbox` muere en Eclipse con un
`Segmentation fault` pelado y ninguna otra salida: un Electron es una prueba
de estrés muy buena de esta ABI, pero malísima para depurar, porque no informa
de nada. La sonda es la misma superficie con los informes devueltos a su
sitio.

Qué cubre: las reservas de espacio de direcciones con las que arranca V8
(1 TiB, 32 GiB y 4 GiB, cada una alineada a su propio tamaño reservando de más
y recortando, y luego con `mprotect` y tocadas), una región `PROT_NONE` que
crece por el medio, `MAP_FIXED` sobre un mapeo vivo, un fichero mapeado con su
longitud exacta en bytes y leído más allá, un paquete de recursos `MAP_SHARED`
de varios megas leído de punta a punta, las sondas del sandbox
(`PR_SET_NO_NEW_PRIVS`, `seccomp`, `/proc/self/task`), `membarrier`, afinidad e
hilos, `fork` después de crear hilos, `getrandom`, la memoria compartida de
Chromium (`memfd_create` + sellos + un remapeo de solo lectura),
`MAP_SHARED|MAP_ANONYMOUS` a través de `fork`, el `socketpair` + `SCM_RIGHTS` +
`SO_PASSCRED` del zygote, un despertar de `eventfd`/`epoll` por flanco, los
cinco relojes sobre los que se construye `base::TimeTicks`, `/proc/self/exe` y
un manejador de `SIGSEGV` en una pila alternativa.

## Ejecutarlo

Compílalo contra glibc, estático, en el host:

    gcc -O1 -static -o tools/abiprobe/abiprobe tools/abiprobe/abiprobe.c -lpthread
    ./tools/abiprobe/abiprobe          # en Linux tiene que salir todo PASS primero

Luego mételo en el rootfs del invitado y vuelve a empaquetar. **No** está
enganchado a la compilación de la imagen a propósito: es un diagnóstico al que
recurrir, no algo que deba llevar cada imagen.

    cp tools/abiprobe/abiprobe rootfs/x86_64-minimal/bin/
    cargo xtask mk-sfs --image zCore/x86_64-minimal.img \
        --dir rootfs/x86_64-minimal --size 419430400
    make -C zCore build MODE=release LINUX=1 GRAPHIC= LOG=warn VARIANT=minimal
    make -C zCore justrun MODE=release LINUX=1 GRAPHIC= LOG=warn VARIANT=minimal

El `make build` después de `mk-sfs` no es opcional: rboot lee el initramfs de
la ESP, así que una imagen que no se copia ahí es una imagen que no arranca.

Mide el tiempo (`time abiprobe`): una comprobación que pasa despacio es un
hallazgo en sí. La reserva de 1 TiB pasaba en 94 s antes de que existiera
`GenericPageTable::unmapped_span`, y en 0,92 s después.
