# fp-probe

**Español** · [English](README.en.md)

Sonda de los registros de coma flotante y vectoriales (x87, SSE, AVX) en
Eclipse OS: si un hilo los encuentra como los dejó al volver del núcleo. Un
binario estático de musl, solo para x86_64, que se copia al rootfs y se corre
desde la shell.

## Por qué existe

El núcleo guarda esos registros cuando un hilo entra y, al salir, solo los
recarga si la CPU tiene otra cosa en ellos (`FpOwner`, en `vendor/trapframe`).
Equivocarse ahí no da ningún error: todas las llamadas devuelven lo que deben
y en los registros están los números de otro hilo. Un compilador guarda en
ellos cualquier cosa (un `memcpy` a medias, un contador, un `double`), así que
el síntoma son datos corruptos lejos de la causa.

## Qué comprueba

Cada prueba carga un patrón propio en xmm0-15 (el ymm entero donde hay AVX),
un valor x87 y un modo de redondeo en MXCSR, hace una llamada al sistema desde
el mismo bloque de ensamblador y compara lo que hay después:

- un hilo solo, llamada tras llamada (el núcleo no recarga nada);
- cuatro procesos, y luego cuatro hilos, cediendo el turno y durmiendo (el
  núcleo tiene que recargar los de cada uno, en la CPU que sea);
- `fork`, donde los dos lados vuelven con los registros del padre;
- un manejador de señal que ensucia todos los registros, cada milisegundo.

## Compilar

```sh
make            # x86_64-linux-musl-gcc
make linux      # con el compilador del host, para correrla en Linux
```

`cargo image` la compila y la deja en `/bin/fp-probe` de la imagen x86_64.

## Correr

```sh
fp-probe        # ymm donde hay AVX
fp-probe noavx  # solo xmm
```

Con una CPU y con varias (`-smp 1`, `-smp 2`, `-smp 4`), y con `noavx` en la
línea de órdenes del núcleo para el camino de FXSAVE. Termina con
`fp-probe: all checks passed` o con `fp-probe: FAIL` y código de salida 1.
Todas las pruebas se cumplen en Linux: un FAIL allí es un fallo de la sonda.
