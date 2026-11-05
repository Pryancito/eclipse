# eclipse-bench

[English](README.md) · **Español**

Un benchmark pequeño y sin dependencias para Eclipse OS. Es un único binario
musl estático, así que se copia directamente al rootfs y se ejecuta desde la
shell.

Cada micro-benchmark está *acotado en tiempo* (corre durante un presupuesto
corto y cuenta el trabajo hecho), de modo que la suite entera termina en bastante
menos de un minuto incluso en un pendrive USB lento, y se adapta a máquinas
rápidas (QEMU) y lentas (disco real).

## Por qué cambiaron los números

La versión anterior de esta herramienta daba cifras de CPU, memoria, disco y
`fork` y, leída al pie de la letra, decía que Eclipse estaba a la par de Linux.
Usar el sistema no decía lo mismo, y la herramienta tenía razón en lo que medía:
simplemente no medía lo que decide lo rápido que se siente un sistema.

Tres problemas, ya corregidos:

1. **La mayor parte nunca entraba en el kernel.** Las secciones de CPU y memoria
   son bucles de ALU en espacio de usuario y `memcpy` sobre páginas ya
   falladas. El kernel no aporta nada, así que *no pueden* diferir entre Eclipse
   y Linux en el mismo hardware. Pasarlas no prueba nada; ahora cada una de esas
   líneas lleva la etiqueta `[user]` y las que sí ejercitan el kernel llevan
   `[kernel]`.
2. **Todo corría en solitario.** Cada medida tenía la máquina para ella sola. Es
   justo la condición en la que no se puede pillar a un planificador siendo
   lento: nunca hay una segunda tarea ejecutable detrás de la que esperar. El
   uso real es una shell, un compositor, demonios y tu programa queriendo CPU a
   la vez. La sección `SCHEDULER / IPC` mide ahora con todas las CPUs saturadas a
   propósito.
3. **Faltaban los costes que pagan los programas reales.** Fallos de página,
   `mmap`, copy-on-write tras `fork`, resolución de rutas, cambios de contexto,
   `clock_gettime`, escalado SMP: no aparecía ninguno, y entre todos dominan el
   tiempo de ejecución de casi cualquier carga real.

## Compilar

```sh
make                       # x86_64-linux-musl-gcc -O2 -static -pthread
# o: make CC=musl-gcc
```

Copia el `eclipse-bench` resultante a tu imagen rootfs (por ejemplo `/root`), o
añádelo a la construcción del rootfs igual que se añaden los demás binarios de
`tools/`.

## Ejecutar

```sh
./eclipse-bench [--only SECCIÓN] [--quick] [DIR] [DISK_MB] [MEM_MB]
```

- `--only SECCIÓN`: ejecuta una de `cpu mem syscall vm sched psched net fs heap
  sig futex smp disk proc gfx`. Útil para un antes/después de un solo cambio.
- `--quick`: presupuestos más cortos, unas 3 veces más rápido y con más ruido.
- `--drm RUTA`: dispositivo DRM para la sección `gfx` (por defecto
  `/dev/dri/card0`).
- `DIR`: directorio para las pruebas de disco. **Tiene que estar en el sistema
  de ficheros que quieres medir (la raíz, EclipseFS por defecto), no en un
  tmpfs** como `/tmp` o `/run`, o los números de "disco" medirán solo RAM. Por
  defecto: el directorio actual.
- `DISK_MB`: tamaño del fichero de prueba de disco (por defecto 32).
- `MEM_MB`: tamaño del conjunto de trabajo de memoria (por defecto 32).

Ejemplo en un arranque lento desde USB:

```sh
cd /root            # en el sistema de ficheros raíz, NO en /tmp
./eclipse-bench . 16 16
```

**La comparación que de verdad significa algo** es ejecutar este mismo binario
en Linux en la misma máquina y comparar las dos salidas con diff. Las pistas
`linux: ~N` que aparecen junto a cada línea de kernel son una orientación de
orden de magnitud para un x86_64 moderno, no objetivos: una VM o una CPU emulada
se quedarán muy lejos por motivos que no tienen nada que ver con Eclipse.

## Qué significa cada sección

**CPU** `[user]`: cadenas de operaciones dependientes, así que su ritmo es
~proporcional a la frecuencia efectiva del núcleo. Solo sirve para comprobar la
frecuencia/P-state y como el reloj contra el que la sección `RATIOS` mide los
costes del kernel.

**MEMORY** `[user]`: ancho de banda secuencial y latencia de fallo de caché/DRAM
sobre búferes ya fallados. Es una propiedad del sistema de memoria, no del SO.

**SYSCALL** `[kernel]`: `getpid()` es el suelo: entrar por trap, salir por trap.
Cada fila añade un subsistema encima, así que la *diferencia* respecto a la fila
de `getpid` localiza el coste. Vigila `clock_gettime`: Linux lo sirve desde el
vDSO sin entrar en el kernel, así que una cifra aquí del orden de `getpid`
significa que Eclipse está haciendo un trap real para una de las operaciones más
frecuentes que existen.

**VM / PAGE FAULTS** `[kernel]`: `mmap`/`mprotect`, fallos menores de página a
cero bajo demanda y fallos de copy-on-write tras `fork`. Todo programa los paga
al arrancar y en cada asignación que toca, y ninguno aparece en un benchmark de
`memcpy`.

**SCHEDULER / IPC** `[kernel]`: **la sección que hay que vigilar.** Un viaje de
ida y vuelta por una pipe son dos cambios de contexto, así que mide cuánto espera
cualquier cosa a que le den una CPU. Las filas `sleep 1ms late` se miden dos
veces: con la máquina ociosa y luego con un proceso de cálculo por CPU. La
diferencia entre ambas es la latencia interactiva que notas. Las filas `(worst)`
importan más que las medias: un parón de 20 ms entre cuarenta despertares del
prompt se percibe como tirones y en la media desaparece.

**PREEMPTIVE SCHEDULER INTERNALS** `[kernel]`: la sección anterior dice cuánto
esperó una tarea despertada; esta dice qué mecanismo la hizo esperar.

El planificador de Eclipse no es una runqueue de Linux.
`vendor/PreemptiveScheduler` es un ejecutor asíncrono por CPU, y la política del
código de hilos de `zircon-object` solo decide la *duración* de una porción de
tiempo, nunca qué tarea corre después. Esa forma tiene mecanismos propios, y cada
uno puede fallar por su cuenta mientras todas las filas de `SCHEDULER / IPC`
siguen pareciendo razonables. Una prueba por mecanismo:

- **`yield hand-off`**: dos hilos en una CPU devolviéndose la CPU directamente.
  Sin temporizador, sin descriptor, sin despertar: el ejecutor eligiendo la
  siguiente tarea y cambiando a ella, nada más. Es el suelo bajo todas las demás
  cifras de aquí, así que `pipe RT / yield hand-off` dice qué parte de un viaje
  de ida y vuelta *no* es despacho.
- **el suelo de la porción de tiempo**: un hilo que gira y otro que despierta
  cada 200 µs, anclados a la *misma* CPU. Sin suelo, el que despierta expulsa al
  que gira en cada despertar y `throughput retained` se hunde; con suelo, el
  primer tramo de una porción es de quien esté corriendo. La latencia del que
  despierta se imprime justo debajo porque el suelo *compra* ese rendimiento con
  ella: alta retención junto con baja latencia del que despierta es el único
  resultado bueno sin ambigüedad, y una ejecución que mostrara solo la retención
  estaría anunciando la mitad de un intercambio.
- **el resto de la porción**: un hilo que gira sin más contra otro que gira
  durante casi toda una porción y luego se aparca un momento, ambos en una CPU.
  Un hilo que recibe una porción nueva entera en cada reanudación nunca es
  expulsado y mata de hambre a su vecino; uno que vuelve con el resto que ya
  tenía es expulsado a su hora. `plain spinner fair share` está normalizado para
  que 1.00 sea un reparto igualado, así que no contiene nada del hardware.
- **robo de trabajo**: todos los trabajadores los crea un padre confinado a la
  CPU 0, ninguno anclado, todos de cálculo. El resultado correcto es un
  trabajador por CPU, tanto si el kernel los colocó ahí al crearlos como si otras
  CPUs vieron la cola y se llevaron trabajo; `time to occupy all CPUs` trata
  ambos casos como el éxito que son, y `CPUs occupied` es la fila que detecta que
  el trabajo nunca se reparte.
- **el escaneo de robo en reposo**: un segundo de máquina ociosa a propósito,
  leído de los contadores del propio kernel. Una CPU ociosa sale del halt en cada
  interrupción, y recorrer el runtime de cada vecina para concluir que no hay
  nada que robar cuesta un lock por vecina y por despertar, en líneas de caché que
  son de esas vecinas. `skipped share` es la frecuencia con la que esa pregunta se
  respondió sin tomar ninguno.
- **afinidad**: `sched_setaffinity` es una operación del planificador, no un
  simple almacenamiento: al estrechar una máscara hay que avisar a una CPU de la
  nueva máscara cuando la tarea es ejecutable fuera de ella, y ese aviso recorre
  los runtimes de las otras CPUs.
- **agrupación de despertares entre CPUs**: un hilo que despierta, N durmientes
  confinados a *otra* CPU, despertados en ráfaga. Solo se cronometra la emisión;
  volver a aparcar a los durmientes ocurre fuera del reloj, porque N objetivos
  que comparten una CPU se serializan ahí necesariamente, y cronometrarlo
  reportaría la anchura de la CPU como el coste de un despertar. Eclipse agrupa
  la petición de replanificación por CPU y Linux no agrupa despertares de futex
  distintos, así que `burst / single` enfrenta a los dos kernels más que dar una
  puntuación.
- **trabajo del kernel por operación**: `timer rearms per sleep` debería rondar
  uno, y dice que el temporizador lo está reprogramando algo distinto del sleep
  que lo necesitaba cuando no es así. `task polls` y `weak execs created per RT`
  son la contabilidad del propio ejecutor: una tarea que cede a mitad de un poll
  deja detrás un ejecutor débil con una pila de 32 KiB, y esa rotación es
  invisible desde el espacio de usuario.

Las filas del lado del kernel leen `/proc/perf/kernel`, que Linux no tiene, así
que allí salen `n/a` y las filas de espacio de usuario son toda la comparación.
Todas las filas de espacio de usuario corren en ambos. La sección también repite
la línea `sched mode:` del arranque, para que un informe capturado no se compare
con otro cuyos interruptores de política fueran distintos:
`scripts/qemu-bench.sh -c 'WAKEPREEMPT=0'` y `-c 'TIMERDEADLINE=0'` los apagan
sobre un mismo build, que es la única forma honesta de hacer un A/B (recompilar
entre A y B cambia el binario y su disposición, y la varianza entre ejecuciones
en TCG es lo bastante grande como para ocultar el efecto en cualquier sentido).

**SOCKETS / IPC** `[kernel]`: todo lo que está por encima del socketpair simple
que ya mide la sección `SCHEDULER`. Esa fila es el camino más corto posible por
la pila: sin dirección, sin listener, sin protocolo. Estos son los caminos que
usan los programas reales.

- `socket()+close()` es el suelo de la familia, como `getpid` lo es de la sección
  de syscalls: réstalo de cualquier otra fila de aquí.
- `TCP loopback round trip` es el mismo intercambio de un byte que la fila de la
  pipe, con toda la pila IP dentro, así que la distancia respecto a la fila del
  socketpair es procesamiento de protocolo y no el despertar.
- `TCP connect+accept+close` es lo que paga un servidor por conexión. Un kernel
  puede responder peticiones deprisa y aceptarlas despacio; es un caso habitual y
  una sola fila no puede mostrar ambas cosas.
- `UNIX stream RT (bound)` llega al socket por **nombre** (bind, listen, connect a
  través del sistema de ficheros), que es lo que hace cualquier bus de escritorio
  y cualquier compositor. Frente a la fila del socketpair, la diferencia es el
  nombre y el listener.
- `sendmsg / send` es la fontanería de iovec y mensajes de control, con el mismo
  byte y el mismo socket, así que el cociente es esa fontanería y nada más.
- `SCM_RIGHTS fd pass` es pasar un descriptor por un socket UNIX y recibir un
  byte de vuelta: de eso viven el zygote de un navegador y cualquier proceso
  auxiliar en sandbox. Un `n/a` aquí es un resultado de corrección, no de
  lentitud: el descriptor no llegó.
- `recv on empty (EAGAIN)` es la lectura no bloqueante que hace cualquier bucle de
  eventos por cada notificación de disponibilidad espuria: entrada al kernel más
  una comprobación de cola, sin ningún despertar.
- Las **filas de disponibilidad** son el suelo de un bucle de eventos. Un fd está
  listo y es el *último* del array, que es el peor caso para una implementación
  que recorre el conjunto y el mejor para una que mantiene una lista de listos;
  así que la pendiente `per extra fd` entre las filas de 1 fd y de 64 fd es la
  respuesta, no ninguna de las dos filas por separado. Un bucle de eventos con
  cien conexiones paga esa pendiente en cada despertar. Un `epoll / poll` por
  encima de 1 significa que `epoll` recorre el mismo conjunto que `poll` y no
  aporta nada.

**FILESYSTEM / VFS** `[kernel]`: deliberadamente *no* es la sección `DISK`. Esta
apenas toca datos y mide la capa por encima del dispositivo: resolver una ruta,
abrir un descriptor, responder a `stat`, leer un directorio, servir una página
caliente desde la caché y formatear los ficheros de procfs que leen todas las
herramientas. Todo corre contra ficheros escritos momentos antes, así que los
datos están en caché y el dispositivo queda fuera. Un kernel puede ser rápido
transmitiendo datos y lento en cada una de estas cosas, y en estas es donde pasan
el tiempo una shell, una compilación y `ps`.

- `open+close, 1 component` y `open+close, 9 components` difieren exactamente en
  una cosa, cuántos componentes tuvo que resolver el kernel, así que su
  diferencia es el `path cost per component`. Ninguna fila sola puede mostrarlo.
- `openat(dirfd, name)` es lo que hacen las herramientas de compilación y `find`;
  la distancia respecto a la fila de ruta completa es lo que costaba la
  resolución que se ahorran.
- `open+close (O_PATH)` tiene fila propia en lugar de suponer que vale lo mismo
  que un open normal: `ps` lo usa, y Eclipse lo ha tenido mal dos veces.
- `(lseek+read) / pread` es lo que cuesta mantener la posición del fichero:
  `pread` lleva el desplazamiento, la fila de `read` paga antes un `lseek`
  explícito, y la fila de `lseek` da la otra mitad de la cuenta.
- `mmap+touch 1 MiB file` toca un byte por página en lugar de copiar el fichero,
  así que es el camino de fallo de página y no ancho de banda de memoria. Que
  fallar salga mejor que copiar es una propiedad del kernel, no algo dado.
- `getdents` se cobra **por entrada**, porque es lo que escala con un directorio
  grande, mientras que una cifra por llamada lo esconde. `entries per getdents
  call` es el tamaño del lote: un kernel que devuelve una entrada por syscall
  hace que recorrer un directorio cueste una syscall por fichero, y la fila por
  entrada sola solo diría "lento".
- Las **filas de procfs** son informes que el kernel formatea bajo demanda.
  `ps aux` lee varios por cada proceso de la máquina, por eso uno lento se nota y
  no solo se mide. Que `/proc/self/task listing` dé `n/a` es el directorio que
  antes faltaba del todo, que es lo que tumbaba el zygote de chromium.

**HEAP / ANONYMOUS MEMORY**: cómo obtiene memoria un programa, desde la llamada al
asignador hasta la proyección. La división entre `[user]` y `[kernel]` aquí no la
fija la fila: la decide el umbral de mmap de la libc, y esa es la trampa que esta
sección existe para destapar.

- La familia `malloc+free` de 64 B, 4 KiB y 256 KiB es casi siempre `[user]`: el
  asignador devuelve un bloque que ya tiene y nunca se entra en el kernel.
  **glibc sube su umbral de mmap cada vez que se libera un bloque grande
  proyectado con mmap**, hasta 32 MiB, así que un bucle que reserva y libera
  256 KiB le enseña a mantener ese tamaño en el heap tras la primera iteración,
  que es justo lo que hace el bucle, y por eso esa fila da decenas de
  nanosegundos en lugar de los miles que cuesta un par `mmap`/`munmap`. Su fila
  `mmap calls/op` es la prueba: un valor cercano a 0 es el asignador, uno
  cercano a 1 es el kernel. La fila `malloc+free 64 MiB` está por encima del
  techo del umbral, así que es un par real de proyecciones tanto en glibc como en
  musl. musl no tiene umbral adaptativo, lo que es un motivo más por el que una
  cifra de una libc no dice nada de la otra.
- `malloc churn, 512 live` sustituye uno de 512 bloques vivos de tamaños mezclados
  en un índice disperso. Las filas simples miden el caso más feliz del asignador,
  en el que el bloque que acaba de liberar es el que devuelve; un programa de
  larga duración nunca está en ese caso.
- `mmap+touch+munmap` y su variante `MAP_POPULATE` son el camino de fallo bajo
  demanda frente al anticipado. **Una cifra de `MAP_POPULATE` igual a la de la
  fila simple significa que la bandera se aceptó y se ignoró**, así que los
  fallos se tomaron igualmente página a página, en silencio, porque la llamada
  tuvo éxito.
- `MADV_DONTNEED+refault` es lo que paga un heap que encoge y vuelve a crecer, y
  lo que hacen continuamente todos los recolectores de basura y asignadores por
  arenas.
- Las dos filas `munmap N MiB resident` son el desmontaje que paga un proceso por
  todo su espacio de direcciones al salir. **Un ritmo por MiB que baja a medida
  que crece la proyección es un desmontaje que recorre algo por página en lugar
  de por rango**; ese coste está dentro de la fila `fork + exit` de la sección
  `PROCESS`, donde no se puede separar.

**SIGNALS** `[kernel]`: entrega, fallos y dos veredictos de corrección impresos
como frases y no como números, porque una cifra para un mecanismo que no funciona
es peor que no tener cifra.

- `block+pending+unblock` exige que el manejador **no** haya corrido hasta que se
  levante la máscara. Un kernel que entrega igualmente una señal bloqueada da
  `n/a` aquí en lugar de un número verosímil.
- `SIGSEGV fault+handler+retry` instala un manejador que hace escribible la
  página que falla y deja que el almacenamiento se reintente. Así funcionan un
  JIT, una arena con copy-on-write y cualquier página de guarda de pila, así que
  es un camino, no una patología.
- **`SA_RESTART`** es el veredicto que más importa. `__synccall` de musl, cada
  `read` de la shell y la mayoría del código de bibliotecas suponen que un
  manejador puede interrumpir una syscall bloqueante sin que quien llama vea
  `EINTR`. La fila `interrupted read, over 2x20ms` es el exceso sobre los dos
  periodos de temporizador de 20 ms que la propia prueba programa; el tiempo
  bruto sería de ~40 ms en cualquier kernel y no diría nada. Es **una sola
  muestra** (la prueba no se puede repetir dentro de una misma lectura
  interrumpida), así que arrastra la granularidad del temporizador y oscila un
  factor dos entre ejecuciones.
- **`sigaltstack`** dice si un manejador corrió de verdad en la pila alternativa.
  Si no, un fallo causado por quedarse sin pila no tiene dónde entregarse y el
  proceso muere en lugar de manejarlo.
- `sigsuspend wake, over 1ms` es la latencia de despertar del camino de señales:
  el proceso de verdad se duerme y se despierta, y se resta el periodo de 1 ms
  del temporizador para que lo que queda sea la parte del kernel.

**FUTEX / LOCKS UNDER CONTENTION**: un lock no cuesta nada hasta que hay
contención, y la sección `SCHEDULER` solo mide un viaje de ida y vuelta sin
contención. Esta sección mide el régimen que importa.

- `mutex lock+unlock, alone` es `[user]`: un `pthread_mutex` sin contención son
  dos operaciones atómicas y nunca entra en el kernel. El cociente `contended /
  alone` frente a la fila de dos hilos es lo que cuesta un lock cuando de verdad
  actúa como lock.
- `FUTEX_WAKE, nobody waiting` lo emite cada desbloqueo de un mutex que *podría*
  tener esperando a alguien, así que en un lock casi sin contención es todo el
  coste del kernel.
- Las familias de despertar vienen en dos sabores con 1, 16 y 64 hilos
  aparcados, y responden a preguntas distintas. **`FUTEX_WAKE issue` cronometra
  solo la syscall**: el kernel saca a un hilo de la cola, lo hace ejecutable y
  vuelve. Es la fila que dice si esa elección *recorre* la cola, porque nada en
  ella depende de que la máquina tenga una CPU libre. **`wake+observe` añade que
  el hilo despertado avise de vuelta**, que es lo que nota un programa; pero en
  una máquina con menos CPUs que hilos esperando, la mayor parte es ese hilo
  haciendo cola para una CPU. Un cociente de emisión plano con un cociente de ida
  y vuelta creciente significa que el código de futex es O(1) y lo que crece es
  la espera por una CPU; un cociente de *emisión* creciente es la cola siendo
  recorrida, y eso le toca arreglarlo al kernel.
- El par `condvar signal` / `broadcast` es la estampida que crea cada
  `notify_all`: se despiertan todos y luego compiten por un solo mutex.
- `FUTEX_WAIT 200us timeout` debería dar algo más de 200 us. Mucho más es un
  temporizador que dispara tarde; **mucho menos es una espera que no esperó**, y
  entonces cualquier cola acotada construida encima gira en lugar de dormir.

Cada fila de esta sección que despierta un hilo también tiene que poner de nuevo
en marcha una CPU. Bajo un hipervisor eso es una salida de VM y una IPI, decenas
de microsegundos en una vCPU que estaba en halt, así que las cifras **absolutas**
son de la máquina y solo los cocientes son trasladables. Medido en una microVM de
4 vCPU, `FUTEX_WAKE issue` da ~16 us frente a ~1,5 us en hardware real, y las
filas `wake+observe` oscilan un factor dos entre ejecuciones del mismo binario.
Compara con un control Linux bajo el *mismo* hipervisor o no compares.

**SMP SCALING** `[kernel]`: N hilos ejecutando el mismo bucle de ALU puramente en
espacio de usuario que ejecutó un hilo. El trabajo no tiene componente de kernel,
así que todo lo que no sea escalado lineal es el kernel: colocación, contención
de locks o CPUs que nunca llegaron a arrancar.

**DISK** `[kernel]`: rendimiento en streaming, IOPS y latencia aleatorias, coste
de confirmación de `fsync`, y las líneas `meta` (crear / stat / unlink de muchos
ficheros pequeños), que castigan justo el camino que hace lentos `exec`, la
resolución de rutas y el arranque.

**GRAPHICS / DRM-KMS** `[kernel]`: lo que paga un compositor por fotograma, por el
camino DRM directo: sin libdrm, sin Mesa, sin Wayland, así que el mismo binario
corre en Eclipse y en cualquier Linux sin instalar nada.

Deliberadamente *no* mide una tasa de fotogramas. Bajo QEMU, ambos kernels
manejan la **misma GPU emulada**, así que "lo rápida que es la GPU" no tiene
contenido de kernel. Lo que difiere entre dos kernels sobre una misma GPU virtual
es el coste de las llamadas que hace un compositor en cada fotograma, y eso es lo
que son estas filas:

- `DRM ioctl floor (GET_CAP)` es el equivalente gráfico de la fila `getpid`: un
  ioctl DRM que prácticamente no hace nada. Réstalo de cualquier otra fila para
  separar el trabajo real de esa operación del despacho genérico de ioctl.
- `MODE_GETRESOURCES` / `GETCONNECTOR` / `GETCRTC` son las consultas que hace un
  compositor al arrancar y en cada conexión en caliente.
- `CREATE+DESTROY_DUMB`, `ADDFB2+RMFB`, `MAP_DUMB+mmap+munmap` son el ciclo de vida
  de los búferes: se pagan en cada redimensionado de ventana y en cada
  reconstrucción del swapchain.
- **`mapped fb write`** es la que hay que vigilar. Es un simple bucle de
  almacenamientos, pero el número lo decide *la política de caché que eligió el
  kernel para la proyección*. Write-combining va a velocidad de DRAM; sin caché
  es 10-50 veces más lento y convierte un repintado a pantalla completa de un
  milisegundo en decenas. Nada más de la suite puede detectarlo, y es invisible
  en un benchmark de `memcpy` sobre memoria normal; por eso existe el cociente
  `fb write / memcpy`.
- `page flip -> event` envía un flip y espera su evento de finalización. La
  espera es lo importante: el evento es el fotograma de verdad en pantalla. Una
  cifra cercana al periodo de refresco significa que los fotogramas van
  acompasados; muy por debajo, que no (tearing, y un compositor que gira en
  vacío).
- `vblank interval` / `jitter` es el reloj de refresco tal y como lo entrega el
  kernel. Un intervalo mucho más corto que cualquier periodo de pantalla
  significa que `WAIT_VBLANK` no espera en absoluto: responde con la secuencia
  actual y vuelve. La herramienta lo dice explícitamente en lugar de imprimir lo
  que parecería un número muy rápido: todo cliente que se acompasa con él
  (Present de X, SDL, toolkits antiguos) acaba girando a CPU completa en lugar de
  dormir.

Las filas de flip, cursor y atomic cambian lo que se ve en pantalla, así que solo
se ejecutan si el proceso puede convertirse en maestro DRM. Con un compositor en
marcha, `SET_MASTER` falla y se reportan `n/a` en lugar de pelearse con el
escritorio: para medirlas, detén la sesión o ejecuta desde una consola de texto.

**PROCESS CREATION** `[kernel]`: `fork + exit` es la creación de procesos en
bruto; `fork + exec(self)` añade el reemplazo del espacio de direcciones y la
carga de un ELF estático; `fork + exec(sh -c :)` añade la resolución de rutas y
el enlazador dinámico, es decir, el coste que paga de verdad una shell o un
sistema init por comando.

`tools/drmbench` es la prueba independiente más profunda del mismo camino
(commits atomic, rendimiento del cursor, propiedades de planos) y emite líneas
`clave=valor` para comparar mecánicamente. La sección `gfx` de aquí es la versión
integrada: menos opciones, pero comparte el etiquetado `[user]`/`[kernel]` de la
suite y sus cocientes, de modo que los costes gráficos quedan al lado de los
costes de syscalls y del planificador con los que compiten.

**RATIOS**: cada uno es un coste del kernel dividido por algo medido en la misma
máquina y en la misma ejecución, así que la velocidad del hardware se cancela.
Son los números que hay que citar cuando alguien objeta que estás en una VM.
`wake late loaded/idle` es el titular: cerca de 1 significa que una tarea
despertada obtiene una CPU enseguida incluso con la máquina ocupada; un valor
grande significa que espera a que se agote la porción de tiempo de otro, y el
sistema se sentirá lento por buenos que parezcan los números `[user]`.

## Contadores del lado del kernel

`/proc/perf/kernel` lleva la vista correspondiente del kernel, que incluye:

```
wakeup preempt: N requests (R/s), M honoured (P%)
```

Una *request* (petición) se genera cuando una tarea pasa a ser ejecutable en una
CPU ocupada con otra tarea; se *honra* (honoured) cuando esa CPU recorta en
respuesta la porción de tiempo del hilo que está corriendo. Ese porcentaje es el
gemelo, del lado del kernel, del cociente `wake late loaded/idle` de arriba.

La sección `psched` lee este fichero antes y después de cada prueba e imprime el
*delta* dividido por las operaciones que lo causaron, que es la forma que se
puede comparar entre máquinas. Las líneas que usa son `sched:` (task polls,
cesiones de ejecutores débiles), `sched steal:` (escaneos, sondeos,
afinidad vacía, omitidos), `sched weak:` (ejecutores creados, pico de vivos,
topes blandos alcanzados, desbordamiento del pool de pilas), `sched stack:`
(máximo alcanzado), `timer rearms:` y `wakeup preempt:`. Se localizan por la
etiqueta que abre la línea y luego por posición dentro de ella, así que una línea
renombrada o reordenada hace que esas filas den `n/a` en lugar de mostrar un
número de un campo vecino.

### Contabilidad de syscalls por proceso

Las secciones `net` y `fs` emparejan sus filas con `/proc/self/perf`, la tabla de
syscalls **por proceso** de Eclipse: una fila por cada syscall que ha hecho el
proceso, con el número de llamadas y el tiempo que el kernel pasó dentro. Para
estas secciones es el mejor emparejamiento, por dos motivos.

Es *nuestro* proceso, así que una shell ociosa o un demonio que despierta no
pueden mover los números; la tabla de todo el sistema no puede prometer eso. Y
separa una cifra medida en dos fallos distintos: `calls/op` es cuántas syscalls
hizo de verdad la libc (un valor que no sea el entero esperado significa que la
fila no mide lo que dice su etiqueta: un bucle de reintentos, una lectura corta),
e `in-kernel` es qué parte del tiempo de reloj medido se pasó dentro de ellas. Una
parte baja dentro del kernel con muchos ns/op sitúa el coste en la entrada/salida
o en la replanificación, no en el subsistema. Una única cifra de ns/op no puede
distinguirlos.

El propio `openat`/`read`/`close` del lector cae en las mismas filas que una
prueba de open o read quiere leer después, así que su coste se **mide** una vez
(dos instantáneas seguidas sin trabajo entre ellas) y se resta, en lugar de
suponerlo. Linux no tiene ese fichero, así que allí estas filas dan `n/a`, con la
misma convención que siguen las filas de `/proc/perf/kernel`.

## Comparaciones sugeridas

- **Eclipse frente a Linux, misma máquina**: la única comparación que zanja una
  discusión. Mismo binario, mismo `DIR`, diff de la salida.
- **Eclipse frente a Linux bajo QEMU**: `scripts/qemu-bench.sh` arranca Eclipse y
  `scripts/qemu-linux-bench.sh` arranca un kernel Linux estándar bajo la *misma*
  máquina QEMU, y ambos ejecutan este mismo binario. Para la sección `gfx` el
  control Linux necesita además un driver DRM ligado a la GPU emulada, o dirá
  "no DRM device" y no habrá nada que comparar: pasa `-g` a ese script y meterá
  la pila del driver en el initramfs. Los dos arneses ya dan al invitado la misma
  GPU emulada (la VGA estándar por defecto de QEMU en q35). Bajo TCG (sin KVM)
  las cifras absolutas describen el emulador de QEMU, así que lee la sección
  RATIOS en lugar de los nanosegundos.
- **QEMU frente a hardware real**: una diferencia grande en las líneas `[user]`
  de CPU apunta al escalado de frecuencia; una diferencia sobre todo en `DISK`
  apunta a la E/S.
- **Antes y después de un cambio en el kernel**: captura la salida, recompila,
  vuelve a capturar. `--only sched` suele ser la forma más rápida de ver si un
  cambio en el planificador hizo algo, y `--only psched` la más rápida de ver
  *qué* mecanismo movió.
- **Después de tocar sockets, el VFS o procfs**: `--only net` y `--only fs` son
  los atajos correspondientes. Ambas secciones son pruebas solo de espacio de
  usuario, así que corren sin cambios en el control Linux y toda la salida es
  comparable.
- **Después de tocar el asignador, la entrega de señales o el código de futex**:
  `--only heap`, `--only sig` y `--only futex`. También son solo de espacio de
  usuario, así que el control Linux las ejecuta sin cambios. Para `heap`,
  recuerda que la libc del control decide qué filas son `[user]`, así que compara
  las filas `mmap calls/op` antes de comparar los nanosegundos. Para `futex`,
  compara los cocientes: las cifras absolutas de despertar son una propiedad del
  anfitrión, no del kernel que se prueba.

Pega la salida en algún sitio donde puedas hacer diff; las etiquetas y las
unidades son estables.
