# clock-probe

Sonda de **integridad** de los relojes y los temporizadores de Eclipse OS. Un
binario estatico de musl, sin dependencias, que se copia al rootfs y se corre
desde la shell.

No mide velocidad. Para eso esta `eclipse-bench`. Esta sonda responde a la otra
pregunta, la que ninguna cifra de velocidad contesta: **¿es verdad la hora que
da el kernel?**

## Por que existe

Un reloj tiene el mismo modo de fallo que el sonido: todas las llamadas
devuelven 0 y la respuesta esta mal. En este kernel ya ha pasado tres veces.

1. `timer_now()` escalaba el TSC **absoluto**, sin una base por arranque, asi
   que el uptime era el tiempo desde el ultimo encendido de la maquina (8,2
   dias en la maquina del informe) mientras la hora *avanzaba* bien encima de
   ese desfase. Todos los `clock_gettime` devolvian exito. (#1613)
2. La vDSO publicaba el multiplicador sin esa base: el mismo fallo otra vez,
   pero solo en userspace. Los dos relojes de una misma maquina discrepaban en
   dias, y solo un programa que leyera los dos podia verlo.
3. `timerfd_settime(TFD_TIMER_ABSTIME)` y `timer_settime(TIMER_ABSTIME)`
   pasaban la **fecha** absoluta de quien llamaba al temporizador del kernel
   como una **distancia** monotona: segundos desde 1970 leidos como
   nanosegundos desde el arranque, o sea un temporizador armado medio siglo por
   delante. `timerfd_settime` devolvia 0 y el temporizador no disparaba nunca.

Ninguno de los tres se ve en un valor de retorno, ni en una cifra de
velocidad, ni en una capa que se rompe. Los tres se ven en cuanto se leen dos
relojes el uno contra el otro, que es lo unico que hace esta sonda: enuncia un
invariante, lo mide, e imprime los numeros tanto si pasa como si falla.

## Compilar

```sh
make                       # x86_64-linux-musl-gcc -O2 -static -pthread
make CC=musl-gcc
make linux                 # el mismo fuente con el cc del host
```

`cargo xtask rootfs` / `image` ya la compila y la copia a `/bin/clock-probe`
dentro de la imagen, igual que el resto de las sondas de `tools/`.

## Correr

```sh
clock-probe [--only SECCION] [--quick] [-v]
```

- `--only SECCION` — una sola seccion; se puede repetir.
- `--quick` — menos vueltas, unos 2 s en total en vez de unos 10.
- `-v` — imprime tambien las notas de detalle.

Salida: **0** si todos los invariantes se cumplen, **1** si alguno falla, **2**
si la linea de orden esta mal. No toca el reloj del sistema en ningun momento
(nunca llama a `clock_settime`), asi que es segura en una maquina trabajando.

**Correrla tambien en Linux, en la misma maquina.** Todos los invariantes de
aqui son invariantes que Linux cumple, asi que un FAIL en Linux es un fallo de
la sonda y un FAIL solo en Eclipse es un fallo de Eclipse.

## Las secciones, y el invariante de cada una

| seccion | invariante |
| --- | --- |
| `res` | todo reloj que `clock_getres` admita se puede leer, su resolucion es sana y el `tv_nsec` esta normalizado (< 1e9) |
| `mono` | los relojes monotonos no retroceden en cientos de miles de lecturas seguidas |
| `vdso` | el reloj de userspace y el del kernel son **el mismo reloj**: una lectura por vDSO tomada entre dos syscalls crudos cae entre las dos |
| `offset` | `CLOCK_REALTIME` menos `CLOCK_MONOTONIC` no se mueve; si deriva, los dos relojes corren a ritmos distintos y uno de los dos esta mal |
| `coarse` | los relojes `_COARSE` concuerdan con los finos dentro de su propia resolucion, y nunca adelantan |
| `uptime` | `CLOCK_BOOTTIME` concuerda con `/proc/uptime` y nunca va por detras del monotono |
| `epoch` | la hora de pared es una fecha plausible, y `time()` y `gettimeofday()` coinciden con `clock_gettime` (la vDSO sirve las tres desde una sola pagina de datos, asi que pueden discrepar) |
| `cpu` | los relojes de CPU avanzan cuando el proceso quema CPU, no avanzan mientras duerme, y el del hilo nunca supera al del proceso |
| `affinity` | el reloj monotono no retrocede cuando quien lo lee cambia de CPU |
| `rate` | en una ventana de un segundo, monotono, pared, boottime y `/proc/uptime` avanzan lo mismo (una frecuencia de TSC equivocada se ve aqui y en ningun otro sitio) |
| `sleep` | ningun sueño vuelve antes: `nanosleep`, y `clock_nanosleep` con `TIMER_ABSTIME` en los dos relojes |
| `timerfd` | un timerfd dispara una vez por vencimiento, nunca antes, no pierde expiraciones cuando es periodico, y una fecha absoluta de pared es una fecha y no una distancia |
| `timer` | lo mismo con `timer_create`, mas que el `timer_gettime` nunca devuelva mas de lo armado y cuente hacia atras |
| `itimer` | `setitimer(ITIMER_REAL)` dispara, y no antes de tiempo |
| `timeout` | las llamadas bloqueantes con plazo lo respetan como suelo: `poll`, `ppoll`, `select`, `epoll_wait`, `sem_timedwait`, `pthread_cond_timedwait` |

## Lo que la sonda NO puede afirmar

Un error en la **base** del contador desplaza por igual al monotono, al
boottime, a `/proc/uptime` y al `idle` de `/proc/stat`, porque los cuatro salen
del mismo `timer_now()`: ninguna comparacion entre ellos lo ve, y en esta
maquina no hay un segundo reloj independiente (ni nodo de RTC) contra el que
anclarlos. Lo unico que queda es que alguien sepa cuanto lleva encendida la
maquina. Por eso la seccion `uptime` imprime un **AVISO** —que no falla nunca—
cuando la maquina dice llevar mas de un dia encendida: asi se lee de un golpe,
que es como se reporto el #1613 («un dmesg que empieza en 8,2 dias»).

La mitad de userspace de ese mismo fallo si la ve, y por nanosegundos: es
exactamente lo que mide `vdso`.

## Un fallo, leido

```
  FAIL  CLOCK_MONOTONIC vDSO entre syscalls 20000 de 20000 fuera: hasta 0 ns por detras, 711230999999945 ns por delante
  FAIL  timerfd ABSTIME de pared           no disparo en 3 s: la fecha se armo como una distancia
```

La primera linea es el #1613 en su version de userspace; la segunda es el fallo
del `TFD_TIMER_ABSTIME`. Las dos se obtuvieron inyectando a proposito cada
fallo en la propia sonda, para comprobar que los detecta y, sobre todo, que una
llamada que no vuelve se reporta en tres segundos en vez de colgar la sonda:
cada espera con plazo absoluto corre bajo un hilo guardian que la interrumpe.
