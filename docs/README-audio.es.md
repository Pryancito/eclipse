# Audio: driver HD Audio, HDMI de NVIDIA, PulseAudio y `/dev/dsp`

[English](README-audio.md) · **Español**

El subsistema de audio de Eclipse es un único driver Intel HD Audio (HDA) que
cubre todos los controladores PCI de clase 0403:

- el controlador integrado del PCH (p. ej. `00:1f.3` en placas X299) con su
  códec analógico,
- la función HDA de la GPU NVIDIA que mueve el monitor (`xx:00.1`). Las GPU
  extra de una placa doble (p. ej. una segunda RTX 2060 SUPER sin pantalla)
  se quedan sin enganchar, igual que Linux deja esos pines HDMI sin ELD:
  `aplay -l` muestra el PCH de Intel + un HDMI, no dos tarjetas NVIDIA mudas.
- el `-device intel-hda -device hda-output` de QEMU, que sirve para probar el
  driver en emulación.

## Arquitectura

```
userspace:  pactl / paplay / mpg123 / aplay / SDL / OpenAL
                 │ libpulse  o  ALSA `type pulse`
PulseAudio ── /run/pulse/native  (instancia de sistema, sink mmap=0 en hw:0,0)
                 │
userspace:  wavplay / ffmpeg -f oss
                 │ write(2) + ioctls OSS
/dev/dsp[N] ─ linux-object/src/fs/devfs/dsp.rs   (nodo OSS, uno por controlador)
/dev/snd/*  ─ linux-object/src/fs/devfs/snd.rs   (ALSA, el backend de Pulse)
                 │ AudioScheme::open_stream  → un stream por open(2)
drivers/src/audio/pipeline/host.rs                (búfer host + SRC, por stream)
                 │ lo consume el bucle de relleno, cada 4 ms mientras corre el motor
drivers/src/audio/pipeline/mixer.rs + volume.rs   (suma de todos los streams, ganancia maestra)
                 │
drivers/src/audio/hda.rs                          (controlador + códec + anillo DMA: el DAI)
                 │ verbos CORB/RIRB + DMA del stream (todo por sondeo, sin interrupciones)
Controlador HDA (PCI 04:03) ── códec ── pin ── HDMI/DP o jack analógico
```

Es el pipeline de reproducción de Sound Open Firmware (`host -> src -> mixer
-> volume -> dai`) ejecutado en el kernel en vez de en un DSP: cada cliente
tiene su búfer host, el mezclador tira de todos al ritmo del DAI, y el DAI —
el stream HDA y su anillo — nunca ve a un cliente.

- **Controlador** (`drivers/src/audio/hda.rs`): reset CRST, descubrimiento
  de códecs por STATESTS, anillos CORB/RIRB con respuestas por sondeo, un
  stream de salida sobre un anillo cíclico físicamente contiguo de 256 KiB
  descrito por una BDL. El progreso se deriva del reloj del enlace (WALCLK) y
  se limita con los contadores de posición.
- **El anillo es del DAI y lo rellena el kernel** (como `dai-zephyr.c` de
  Sound Open Firmware). Ningún cliente escribe en él. Un *bucle de relleno*
  (`HdaInner::fill_ring`, que se ejecuta desde cada llamada del front-end y
  desde el temporizador guardián de ALSA cada 4 ms mientras corre el motor)
  mantiene el anillo `FILL_DEPTH` (16 KiB, 85 ms) por delante del punto más
  lejano al que el motor puede haber llegado — la posición de reproducción
  del reloj del enlace o el contador del propio controlador, el que vaya
  delante (`fill_span`) — con la mezcla de todos los streams activos, y nunca
  a menos de `RING_GUARD` de la posición de reproducción por detrás. Pasado el
  punto de relleno se mantiene una banda de ceros, `SILENCE_AHEAD`, para que
  un relleno que llega tarde suene a silencio y no a un anillo de hace una
  vuelta, y un relleno al que el motor adelantó salta hacia delante
  (`late fills` en `/proc/gpusnd`): el PCM de los streams llega tarde, no se
  pierde. Un relleno que no mezcla nada (ningún stream tiene PCM) abre un
  *hueco*: el motor *no* se para, sigue girando sobre ceros, y se para tras
  5 s así (`DAI_IDLE_STOP_US`, el mismo tiempo de inactividad que PulseAudio).
  Nada de lo que hace un cliente — prepare, drop, pause, close, un cambio de
  frecuencia — toca el motor; solo la parada por inactividad. Antes del
  modelo DAI cada underrun paraba el stream y la siguiente escritura lo
  rearrancaba — verbos del códec, detección del pin, empujón al HDMI — y un
  receptor HDMI/DP que ve rearrancar su stream de audio se silencia mientras
  se vuelve a enganchar, cientos de milisegundos en una tele: un hueco de
  10 ms en los datos se oía como un hueco de medio segundo en el sonido. El
  tamaño del anillo ya no es la latencia: la latencia de un cliente es su
  propio búfer (`HOST_BUFFER`, 48 KiB, 256 ms: lo que ofrecía el anillo
  antiguo una vez quitada la guarda, así que nada de lo que ve un cliente
  cambió) más la profundidad de relleno. Lo que el anillo tiene que cubrir es
  la *ventaja* de posición: el `intel-hda` de QEMU lee para el búfer de su
  backend y se le ha visto 106 KB por delante del reloj del enlace, lo que con
  el anillo antiguo de 64 KiB no dejaba ninguna ventana de escritura
  (`free = 0` sin nada audible en cola).
- **Streams** (`drivers/src/audio/pipeline/host.rs`): el componente host de
  SOF. `AudioScheme::open_stream` da a cada `open(2)` de `hw:0,0` o de
  `/dev/dsp` un `HostStream` propio, un `AudioScheme` completo: frecuencia,
  búfer, contadores, pausa, retención del arranque, rewind/forward y reset
  son por stream, y soltar el handle lo saca de la mezcla. La implementación
  del trait del propio dispositivo es un stream más. Así varios clientes
  suenan a la vez a través del kernel — PulseAudio en `hw:0,0` y un `mpg123`
  a pelo en `/dev/dsp` se mezclan en `mix_block` — y una segunda apertura ya
  no es `EBUSY`. La reserva de cliente único con la que los front-ends
  serializaban las aperturas sigue existiendo para un dispositivo cuyo
  `open_stream` responda `None` (un anillo, sin mezclador); el driver HDA
  nunca responde eso.
- **Componentes de procesado** (`drivers/src/audio/pipeline/`): la numérica
  de SOF en Rust, pura y con tests unitarios sin hardware. `volume` es el
  componente de volumen de SOF: ganancia Q8.16 (`1 << 16` = 0 dB),
  multiplicación con redondeo (`(x·g >> 15) + 1 >> 1`, saturada) y una rampa
  lineal de 32 ms recalculada cada 250 µs de audio, así que `amixer set
  Master`, el deslizador del panel y el silencio son fundidos y no escalones
  en la onda. El porcentaje del mezclador es amplitud lineal (la curva cúbica
  de PulseAudio va por encima). `src` es un conversor de frecuencia de
  muestreo FIR polifásico a la manera del SRC de SOF y del conversor sinc de
  libsamplerate: un único filtro paso bajo prototipo con ventana de Kaiser
  (256 fases, 16 coeficientes, ~-90 dB en la banda de rechazo, generado por
  `tools/audio-src-coef/generate.py`) indexado en una fase fraccionaria por
  muestra de salida, con el paso de fase escalado por la razón al reducir la
  frecuencia para que el corte siga al Nyquist más bajo y nada se solape. En
  coma fija y por streaming (guarda la historia de entrada que necesita una
  ventana a caballo, así que un stream troceado se remuestrea igual que un
  búfer entero). Está **conectado a cada stream como sink de frecuencia
  fija**: el enlace siempre se programa a 48 kHz (`LINK_RATE`), `set_params`
  acepta cualquier frecuencia de cliente y un cliente a otra frecuencia se
  remuestrea al entrar en su stream. El stream guarda frames del enlace; solo
  los cuatro números que ven los front-ends (free, queued, delay, buffer) y
  la propia escritura cruzan a frames del cliente, mediante
  `accept_client_frames` / `link_to_client_bytes` / `client_to_link_bytes`.
  `free_bytes` informa exactamente de lo que `write` aceptará, y
  `queued_bytes` se deriva de esa misma cifra (`client_counts`: `buffer -
  accept`), así que el `avail = buffer_size - queued` del propio nodo ALSA es
  igual a lo que `write` toma en cualquier nivel de llenado. Esa identidad es
  lo que mantiene vivo a PulseAudio: su `alsa-sink.c` aborta si una
  escritura para la que se le acaba de decir que había sitio devuelve 0 (el
  assert de EAGAIN de `try_recover`). Tres formas distintas de perderla le
  costaron al demonio un SIGABRT cada una: convertir `queued` por separado
  (de uno a cuatro frames de diferencia cerca de un búfer lleno), acotar el
  búfer del cliente con la capacidad del stream anterior al negociar (el
  búfer guarda un número de frames *del cliente* que depende de la
  frecuencia, 12288 a 48 kHz y 11289 a 44,1 kHz, así que el nodo ALSA pide la
  capacidad *a la frecuencia pedida*, `AudioScheme::buffer_bytes_at`, antes
  del `set_params` que cambia), y, cuando los clientes escribían directamente
  en el anillo DMA, dejar el relleno de silencio del propio driver fuera de
  `queued` mientras `write` estaba acotado por él. Con el anillo del kernel no
  hay relleno ni tercer caso: las dos cuentas salen de una función a partir
  de una única cifra de ocupación, por construcción, y los tests del host
  comprueban la identidad en cada nivel de llenado para cada frecuencia con el
  conversor real. Un cliente a 48 kHz toma un camino byte a byte;
  `/proc/gpusnd` muestra `client=.. Hz src=passthrough|resampling` por
  stream. Un cambio de frecuencia nunca toca el motor: `set_params` es el
  prepare del stream (se tira el PCM en cola y se crea un conversor nuevo), y
  el formato del enlace es fijo. Dejar que el demonio pase los streams al
  sink a su frecuencia nativa (`avoid-resampling` en `daemon.conf`) es
  entonces una línea reversible.
  `mixer` es el componente mezclador de SOF (`mix_n_s16`): suma varios
  streams S16LE en uno, acumulando cada frame en `i32` y recortando una sola
  vez al final, nunca de dos en dos (un recorte de dos en dos pliega un
  stream fuerte sobre uno flojo antes de que se cancelen). No lleva ganancia
  por fuente; el `volume` maestro se aplica a la mezcla, de camino al anillo.
  El bucle de relleno es quien lo llama (`mix_block`: un `pull` por stream a
  un acumulador `i32` compartido; un stream con menos que el bloque aporta
  silencio más allá de lo que tiene).
- **Grafo del códec**: el recorrido de widgets reúne como *camino candidato*
  cada pin capaz de salida con un conversor alcanzable. La elección del
  camino se puntúa (pin digital HDMI/DP > presencia > ELD válido) y — esto es
  clave — **se reevalúa en cada arranque de stream** (`repick_path`), porque
  en las GPU NVIDIA la presencia y el ELD solo aparecen en los pines después
  de que el driver de pantalla empuje el ELD del monitor, mucho después del
  probe PCI de este driver. La reevaluación es pegajosa (`choose_route`): el
  camino actual conserva su sitio en caso de empate y solo se abandona por un
  pin que informe él mismo de presencia. Un `SET_PIN_SENSE` justo después de
  una parada puede leer PD=0 un momento en el propio pin del monitor, y con
  solo la puntuación un rearranque provocado por un underrun se iba al primer
  conector muerto y el siguiente volvía: la misma pista se oía una vez y no
  la siguiente. `/proc/gpusnd` cuenta los cambios de ruta (línea `routing:`)
  con el último.
- **Particularidades del HDMI**: activar el conversor digital,
  `SET_CVT_CHAN_COUNT` (`0x72d`) + ranuras de canal HDMI, el infoframe de
  audio CEA por el búfer DIP del pin (reindexado cada 8 bytes) y los bits de
  configuración PCI de DMA coherente (snoop) de NVIDIA. La puntuación de
  caminos prefiere un pin con presencia/ELD vivos para que un conector de GPU
  sin nada enchufado no le quite la tarjeta 0 al analógico. Al arrancar un
  stream el driver HDA vuelve a activar la transmisión HDMI
  (`kick_hdmi_audio`) y vuelve a detectar el pin (`SET_PIN_SENSE` y luego
  `GET_PIN_SENSE`), porque GOP nunca activa los paquetes de audio.

Al arrancar, `eclipse-boot-sound` reproduce
`/usr/share/eclipse/Eclipse_Awakening.mp3` en cuanto sube el compositor
(`mpg123` por la tarjeta ALSA 0).

## El lado de la pantalla (GPU NVIDIA)

En una GPU el códec HDA solo no basta: el **motor de pantalla** tiene que
transmitir paquetes de audio para el head, y el pin del códec solo informa de
presencia/ELD cuando alguien escribe el ELD. Es el mismo reparto que usa Linux
(`snd_hda_intel` + `nvidia`/`nouveau`).

Eclipse saca la imagen con el modeset de arranque del GOP de UEFI, y el
firmware nunca activa el audio. La GPU de consola — la del monitor — no se
arranca con GSP (SEC2 bloquea el bus), así que el audio HDMI se activa **a la
manera de nouveau**: MMIO por BAR0 sobre el modeset vivo del GOP
(`gf119_sor_hda_eld` / `gf119_sor_hda_hpd` + quitar el silencio GCP de GV100
SF_USER), usando el EDID de UEFI para construir el ELD. Sin GSP y sin `cat
/proc/gpustep14`. Una GPU que ya tiene el RM enganchado sigue usando los
controles RM equivalentes a nvkms (`SET_HDMI_ENABLE`, `SET_ELD_AUDIO_CAPS`,
`SET_AUDIO_ENABLE`, quitar el silencio).

Las tarjetas coinciden con un escritorio Linux típico:

- **la tarjeta 0** es el HDMI cuando el pin del monitor tiene presencia/ELD
  (la regla de PipeWire); si no, el códec analógico del PCH (el orden PCI de
  ALSA).
- **la tarjeta 1** es la otra. Una segunda HDA de NVIDIA sin pantalla no se
  registra.

Busca las líneas `[hdmi-audio]` en dmesg (`GOP ... ELD+PD+GCP unmute` en la
GPU de consola); las líneas `[hda]` muestran los caminos candidatos de cada
códec con su estado vivo de presencia/ELD.

## Diagnosticar el silencio: `/proc/gpusnd`

Que la función HDA de una GPU acepte un stream no prueba nada: el códec vacía
el anillo tanto si el motor de pantalla mete paquetes de audio en el cable
como si no, así que el modo de fallo es *silencio sin ningún error en ninguna
parte*. `cat /proc/gpusnd` vuelca, por tarjeta, lo que el hardware está
haciendo de verdad en este momento:

```sh
cat /proc/gpusnd
```

- **Controlador/stream**: GCAP, STATESTS, la dirección del códec y el
  descriptor del stream de salida: `SD_CTL` (bit RUN y etiqueta del stream),
  `SD_FMT`, `SD_CBL` y `SD_LPIB` muestreado dos veces con 2 ms de
  diferencia, informado como `ADVANCING` o `STALLED`. `STALLED` con RUN
  puesto significa que el motor DMA no está leyendo: un problema del
  controlador o de la BDL, no de la pantalla.
- **Anillo, streams y eventos**: la línea `ring:` dice cuánto por delante del
  cabezal de reproducción está relleno el anillo, cuántos streams hay
  abiertos y, con `gap=`, cuánto lleva el motor girando en silencio; detrás
  va una línea `stream N:` por apertura, con su frecuencia de cliente, si
  remuestrea, lo que tiene, su estado (`started`, `stopped`, `paused`,
  `start held`) y sus propios `underruns` (tirones que encontraron vacío un
  stream arrancado: ese cliente se quedó atrás). La línea `events` cuenta los
  huecos (`drains`: no había ningún stream arrancado; `underruns`: un stream
  arrancado no tenía nada), `idle stops` (un hueco que llegó a 5 s), `stream
  restarts` (un arranque del motor desde parado: la primera escritura tras
  una parada por inactividad) y `late fills` (el motor adelantó al bucle de
  relleno: un hueco de planificación más largo que la profundidad de relleno
  más la banda de silencio). Cada línea `gap:` sitúa uno en su stream.
- **Camino activo**, leído de vuelta *del códec* y no de lo que el driver cree
  que escribió: el id de stream y el formato del conversor, si el conversor
  digital está activo, el estado de energía y, en el pin, el bit OUT_EN, la
  presencia/ELD válido de `GET_PIN_SENSE` y EAPD. Un conversor cuyo id de
  stream se lee como 0 nunca se armó; un pin con `present=0 eld=0` no tiene
  ninguna pantalla viva.
- **Todos los caminos candidatos** con su presencia/ELD vivos, y `<== ACTIVE`
  marcando el que se usa: aquí es donde se ve una mala elección de pin.
- **`[hdmi-audio]`**: una línea por GPU NVIDIA, del último empujón al
  arrancar un stream. La GPU del monitor se activa por el camino GOP/BAR0 (o
  por el RM, si GSP está arriba). Las GPU extra sin pantalla se saltan. Si el
  stream está `ADVANCING` en un pin que informa present+ELD y el monitor
  sigue mudo, este bloque es el sospechoso que queda.

Reproduce algo antes: casi todo el estado de arriba solo existe mientras hay
un stream en marcha:

```sh
speaker-test -c 2 -t sine &
cat /proc/gpusnd
```

## API de usuario: PulseAudio

La sesión ejecuta un demonio PulseAudio de **instancia de sistema**
(`pulseaudio --system`) sobre la tarjeta ALSA 0. Es lo que da a cada cliente
de libpulse y del `default` de ALSA volumen por stream y conversión de formato
en espacio de usuario; el PCM del kernel también mezcla (un stream por
apertura, ver la arquitectura de arriba), así que un cliente que abre
`hw:0,0` o `/dev/dsp` directamente suena junto al demonio en vez de ser
rechazado. (No hay dmix: necesita IPC de SysV y ya no le queda nada que
hacer.) Pulse mezcla en espacio de usuario.

- Socket: `unix:/run/pulse/native` (`PULSE_SERVER` se define en eclipse-init,
  en el wrapper de labwc, en `/etc/profile` y en el `environment` de labwc).
- El `default` de ALSA es el plugin de pulse (`/etc/asound.conf`), así que
  `aplay` / `mpg123` / `SDL_AUDIODRIVER=alsa` también pasan por el demonio. El
  acceso directo al kernel es `aplay -D eclipse_hw` (falla mientras Pulse
  tenga el dispositivo).
- El sink ALSA es `mmap=0 tsched=0`: el PCM de este kernel es solo
  RW-interleaved + `SYNC_PTR`. Los despertares de periodo vienen del
  temporizador de sondeo (~4 ms). Cuando acaba una pista, un guardián del
  kernel sigue leyendo LPIB para que el anillo DMA cíclico no repita el último
  fragmento. `module-suspend-on-idle` **no** se carga a propósito: un sink
  suspendido no se reanudaba cuando se enganchaba un stream nuevo (la
  reanudación corre en el hilo de E/S del sink), así que todo lo que sonaba
  después quedaba mudo. En su lugar el sink se queda IDLE con el PCM abierto.
- **El que remuestrea es el kernel, no el demonio.** `daemon.conf` pone
  `avoid-resampling = yes` con `default-sample-rate = 48000` y sin
  `alternate-sample-rate`. El kernel es un sink de frecuencia fija (ver los
  componentes de procesado de arriba): el enlace HDA siempre va a 48 kHz,
  `hw:0,0` acepta cualquier frecuencia de cliente y la convierte en el stream
  del cliente con `pipeline::src` (~-90 dB), y un cambio de frecuencia es el
  prepare de ese stream y nunca toca el motor, así que nunca obliga a un
  receptor HDMI/DP a volver a engancharse. Por eso `avoid-resampling` pasa un
  stream solo al sink a su frecuencia nativa — 44,1 kHz en casi toda la
  música — en vez de remuestrearlo en espacio de usuario con speex; el demonio
  solo remuestrea cuando suenan a la vez dos streams a frecuencias distintas,
  a `default-sample-rate`. `alternate-sample-rate` se queda sin poner: es la
  forma antigua de seguir una frecuencia, reprogramando el enlace, que es
  justo el silencio por reenganche que esto evita. El remuestreador del propio
  demonio, para el caso de dos streams, es `speex-float-5` (el nivel de alta
  calidad de speex). Para volver a que el demonio lo remuestree todo, basta
  una línea, `avoid-resampling = no`: el lado del kernel pasa entonces a ser
  un paso directo, byte a byte igual que antes. Qué mirar en `/proc/gpusnd`
  con una pista de 44,1 kHz sonando: la línea `stream N:` del demonio dice
  `client=44100 Hz src=resampling`, y cambiar de pista deja `stream restarts`
  donde estaba.
- Un `mpg123 fichero.mp3` a secas puede sonar por `/dev/dsp` en vez de por el
  demonio. mpg123 1.3x no tiene ningún fichero de configuración, así que sin
  `-o` libout123 recorre su lista de drivers integrada y se queda con el
  primer módulo que carga Y abre, y el de OSS está en esa lista. Con el kernel
  mezclando, esa apertura se atiende (un stream propio, mezclado con el del
  demonio) y suena; solo se salta el volumen por stream del demonio. `mpg123
  -o alsa` pasa por `/etc/asound.conf` y el plugin de pulse como antes.
  (Cuando el PCM del kernel era de un solo cliente, `/dev/dsp` respondía
  `EBUSY` mientras el demonio tenía la tarjeta y ese rechazo era lo que
  mandaba a mpg123 al demonio; antes de que existiera la reserva, OSS ganaba
  en la lista, metía un segundo escritor en el anillo de Pulse y sonaba en
  silencio.)
- OpenAL (`ALSOFT_DRIVERS=pulse,alsa`) habla libpulse nativo. Los PI-futex
  están implementados, así que `pa_mutex_new()` ya no aborta.

```sh
pactl info
pactl list sinks short
pactl set-sink-volume @DEFAULT_SINK@ 50%
paplay music.wav          # PCM; el MP3 sigue yendo por mpg123 → ALSA → Pulse
speaker-test -c 2 -t sine # ALSA default → Pulse
```

`module-native-protocol-unix` **tiene que** llevar `auth-cookie-enabled=0`.
Sin eso el módulo carga o crea una cookie en una ruta en la que la cuenta
`pulse` no puede escribir, no se inicializa, y el demonio sigue corriendo
**sin socket**: vivo, dueño de las tarjetas y rechazando a todos los clientes
(`Connection refused` desde el plugin de pulse). xtask solo reescribe las
configuraciones que llevan su marca `# eclipse-generated`, así que un
`system.pa` antiguo sobrevivía a cada recompilación en ese estado; ahora
aparta ese fichero (`system.pa.bak`), siempre incluye el script canónico como
`/etc/pulse/system.pa.eclipse`, y `eclipse-pulseaudio` arranca desde esa
copia (`pulseaudio -n --file=`) cuando a la configuración en su sitio le falta
la clave. `audio-probe` comprueba la línea directamente.

`system.pa` carga `module-native-protocol-unix` **antes** que los sinks ALSA.
`module-alsa-sink` toca el hardware, y una tarjeta cuya carga se atasca deja
al demonio vivo, dueño de ese PCM, sin haber llegado nunca al módulo del
socket: todos los clientes reciben `Connection refused` (el `default` de ALSA
es el plugin de pulse). Con el socket primero, eso cuesta un sink y no el
demonio entero, y `pactl list sinks` sigue nombrando la tarjeta que no subió.
`audio-probe` informa de un demonio vivo sin socket como un fallo propio.

El demonio es un servicio `respawn` de eclipse-init (`pulseaudio.service` →
`/usr/local/bin/eclipse-pulseaudio`). Logs: `/tmp/pulseaudio.log`. Una salida
a los ~150 ms es un aborto al arrancar (`--system` no pudo hacer chown de
`/var/run/pulse` / `/var/lib/pulse`, o `XDG_RUNTIME_DIR` seguía siendo
`/run/user/0` tras pasar al usuario `pulse`); esos directorios son ramfs y el
wrapper quita `XDG_RUNTIME_DIR` antes del exec.

## API de usuario: ALSA (`/dev/snd/*`)

El sonido del sistema lo mezcla PulseAudio; el lado del kernel sigue siendo la
ABI nativa de ALSA: un par `controlC<card>` + `pcmC<card>D0p` por controlador
HDA (`linux-object/src/fs/devfs/snd.rs`), en el mismo orden de tarjetas que
`/dev/dsp<N>`. Implementa lo que necesita el plugin `hw` de alsa-lib (y el
`module-alsa-sink` de Pulse) en modo RW-interleaved: `HW_REFINE`/`HW_PARAMS`
(limitado a S16LE estéreo en el conjunto de frecuencias del HDA),
`SW_PARAMS`, `PREPARE`, `WRITEI_FRAMES`, `DRAIN`/`DROP`, `PAUSE` (para el
DMA, conserva el anillo), `REWIND`/`FORWARD` (silencia el rango que se tira),
`STATUS`, `DELAY` y `SYNC_PTR` (las páginas de estado/control no se pueden
mapear; alsa-lib cae a `SYNC_PTR` solo).

La máquina de estados del stream sigue a `sound/core/pcm_native.c`:

- `PREPARE` arma la retención del arranque del driver
  (`AudioScheme::set_start_hold`): las escrituras se encolan en el anillo y el
  motor arranca cuando hay `start_threshold` frames en cola (el valor por
  defecto de alsa-lib es 1, aplay usa un periodo) o con un `START` explícito
  (PulseAudio pone el umbral en el límite). `START` necesita PREPARED y datos
  (si no, `EBADFD` / `EPIPE`).
- Que el anillo se quede seco en un stream RUNNING es un underrun cuando
  `avail` llega a `stop_threshold` (el tamaño del búfer por defecto):
  `writei`, `DELAY` y `HWSYNC` responden `EPIPE`, `poll()` levanta
  `POLLERR|POLLOUT`, y `PREPARE` lo recupera. Un `stop_threshold` en el límite
  deja el stream corriendo libre (el motor HDA se para solo y la siguiente
  escritura lo rearranca sin cortes), que es también lo que informa un atasco
  con el anillo todavía lleno tras el tiempo de un búfer entero.
- `HW_PARAMS` (solo OPEN/SETUP/PREPARED), `HW_FREE` (SETUP/PREPARED),
  `PREPARE` (no OPEN, RUNNING ni DRAINING), `DROP` (no OPEN) y `PAUSE`
  (pausa RUNNING, reanuda PAUSED) rechazan los demás estados con `EBADFD`,
  como Linux. `SW_PARAMS` valida como `snd_pcm_sw_params` (`avail_min` 0, un
  `tstamp_mode` malo o un `silence_threshold` demasiado grande son `EINVAL`),
  guarda todos los campos para leerlos de vuelta e informa del límite del
  kernel en vez de tomar el del cliente.
- El nodo de control también responde a `TLV_READ` (`ENXIO`, sin escala en
  dB), `HWDEP_NEXT_DEVICE`/`RAWMIDI_NEXT_DEVICE` (ninguno) y `POWER_STATE`
  (D0).

`/etc/asound.conf` (lo escribe xtask) pone `default` en el **plugin de
PulseAudio** cuando `pulseaudio` y el plugin pulse de ALSA
(`libasound2-plugins`) están en la imagen, así que mpg123/`aplay` se
multiplexan por el demonio. El PCM del kernel sigue siendo `eclipse_hw`
(`hw:0,0`) para el sink ALSA del propio Pulse. La conversión de formato sigue
disponible como `aplay -D plug`. Sin Pulse en la imagen, `default` es
`hw:0,0` y el kernel mezcla sus aperturas. La tarjeta 0 es el dispositivo de
reproducción preferido (HDMI/DP con una pantalla viva va por delante de los
jacks analógicos); los demás controladores siguen en el orden del probe PCI:

```sh
aplay -l                      # lista las tarjetas
aplay music.wav               # default = Pulse → hw:0,0 (normalmente HDMI, S16LE estéreo)
aplay -D plug music.wav       # convierte formato/frecuencia en userspace (vía Pulse)
aplay -D eclipse_hw music.wav # el PCM del kernel directamente (ocupado si Pulse está arriba)
speaker-test -c 2 -t sine
pactl set-sink-volume @DEFAULT_SINK@ 50%
amixer set Master 50%         # ctl de Pulse, o el Master del kernel si Pulse no está
amixer set Master mute
```

`libasound2t64`, `libasound2-plugins`, `alsa-utils`, `pulseaudio`,
`pulseaudio-utils`, `libpulse0` y `mpg123` vienen en el conjunto de paquetes
del rootfs (`DESKTOP_UBUNTU` en `xtask/src/linux/apt_rootfs.rs`); lo que falte
se puede añadir en marcha con `apt-get install`. Cada tarjeta expone un
mezclador **Master** sencillo (`amixer set Master 50%`, `amixer set Master
mute`) en el nodo del kernel; con Pulse en marcha, lunarbar y `pactl` ajustan
el volumen del sink.

La tarjeta 0 es el dispositivo de reproducción preferido — HDMI/DP con una
pantalla viva va por delante de los jacks analógicos — así que `aplay` y
`amixer` sin `-c` van a los altavoces del monitor.

### Negociación de hw_params (y cómo probarla sin hardware)

alsa-lib no le da al kernel una configuración terminada. Estrecha un parámetro
cada vez (`snd_pcm_hw_params_choose`), llama a `HW_REFINE` tras cada paso y
espera que el kernel derive los parámetros dependientes: la mayoría de
clientes, `speaker-test` y `aplay` incluidos, solo fijan `period_time` y
`buffer_time` y dejan que salgan los números de frames. Así que el refine
implementa propagación de restricciones al estilo de Linux, iterada hasta un
punto fijo:

```
frame_bits   = sample_bits × channels          (= 32, S16LE estéreo)
period_bytes = period_size × 4
buffer_bytes = buffer_size × 4
buffer_size ≈ period_size × periods            (ver abajo)
period_time  = period_size × 1e6 / rate
buffer_time  = buffer_size × 1e6 / rate
```

Dos propiedades de esa aritmética son estructurales, y las dos fueron bugs
antes:

* **Las direcciones tiempo↔tamaño tienen que ser inversas exactas.** Un tamaño
  de N frames es dueño de la celda de tiempo semiabierta `[N/rate,
  (N+1)/rate)`. Si una dirección redondea un tiempo hacia arriba a un tamaño y
  la otra redondea ese tamaño de vuelta a un tiempo *distinto*, el intervalo
  se vacía y hw_params falla con EINVAL: así es como se rechazaba 0,5 s a
  11,025 kHz (5512,5 frames).
* **`buffer_size = period_size × periods` aquí es aproximado.** El anillo DMA
  es continuo, no está troceado en segmentos de periodo, así que un búfer que
  no es un número entero de periodos suena bien. Exigir el múltiplo exacto
  rechaza peticiones normales: 0,5 s a 44,1 kHz son 22050 frames, mientras que
  cuatro periodos de 125 ms son 22048. El refine lleva un periodo de holgura e
  `install` elige al final la terna exactamente coherente.

`tools/alsa-hwparams-sim/run.sh` ejercita todo esto sin hardware y sin QEMU.
**Extrae** el código de refine/install de `snd.rs` en tiempo de ejecución (así
siempre prueba lo que se publica) y lo conduce con una reproducción fiel de la
negociación de alsa-lib en 23 escenarios: todas las frecuencias admitidas,
baja latencia, búferes demasiado grandes, periodo/periodos explícitos. Un
código de salida 0 significa que todos los escenarios negociaron una
configuración coherente. Ejecútalo después de tocar el refine.

Una configuración rechazada registra el parámetro culpable y el estado
completo de los intervalos (`[snd] hw_params rejected: …`, con un tope de 8
líneas por arranque), así que un EINVAL pelado en espacio de usuario se puede
seguir hasta la restricción que lo causó.

## API de usuario: `/dev/dsp` (OSS)

Un nodo por controlador, en el mismo orden que `/dev/snd`: `/dev/dsp` es la
tarjeta 0 (el HDMI/DP preferido cuando hay una pantalla viva), `/dev/dsp1`,
`/dev/dsp2`, … para el resto, cada uno con un gemelo `/dev/audio<N>` (valores
por defecto de Sun: µ-law, 8 kHz, mono) y un `/dev/mixer<N>`. El nodo sigue a
`snd-pcm-oss` de Linux (`sound/core/oss/pcm_oss.c`), así que un programa
escrito para el `/dev/dsp` de Linux se comporta igual aquí:

- **Formatos** (`SNDCTL_DSP_SETFMT`/`GETFMTS`): µ-law, A-law, U8, S8, S16
  LE/BE, U16 LE/BE, S24 empaquetado / en 32 bits (LE/BE), S32 LE/BE, float. El
  anillo lleva S16LE estéreo; el nodo convierte al entrar y duplica el mono en
  los dos canales. A un formato desconocido se le responde con `AFMT_U8`, como
  en Linux.
- **Frecuencia** (`SPEED`, `SOUND_PCM_READ_RATE`): limitada a 1000..192000 y
  ajustada a la frecuencia más cercana que codifica el formato de stream HDA
  (8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400,
  192000); el ioctl devuelve la frecuencia en vigor. Linux remuestrearía a la
  petición exacta; todos los clientes reales leen de vuelta el valor
  concedido.
- **Fragmentos** (`SETFRAGMENT`, `SUBDIVIDE`, `GETBLKSIZE`, `GETOSPACE`,
  `GETOPTR`, `GETODELAY`): el algoritmo `snd_pcm_oss_period_size` de Linux con
  el anillo como restricción esclava. Por defecto un cliente S16LE estéreo a
  48 kHz recibe 8 fragmentos de 4096 bytes; `SETFRAGMENT` (una vez por
  apertura, como en Linux) elige el tamaño y el número, y una escritura nunca
  encola más allá de `fragments × fragsize`, así que la latencia que pide un
  cliente es la que obtiene. `GETOSPACE.bytes`, `GETODELAY` y `GETOPTR` van en
  bytes del cliente.
- **Disparadores** (`SETTRIGGER`/`GETTRIGGER`, `DSP_CAP_TRIGGER`): quitar
  `PCM_ENABLE_OUTPUT` tira lo que hay en cola y retiene el stream; las
  escrituras llenan entonces el búfer y no suena nada hasta que se vuelve a
  poner el bit (el modismo «llenar y luego arrancar»). La retención es una
  primitiva del driver (`AudioScheme::set_start_hold`) y muere con el fd.
- **Escrituras**: los frames enteros van al dispositivo y un frame parcial al
  final se guarda para la siguiente escritura. Las escrituras bloqueantes
  reintentan contra el anillo (de forma acotada); con `O_NONBLOCK` o tras
  `SNDCTL_DSP_NONBLOCK` una escritura devuelve lo que cupo o `EAGAIN`. Los
  underruns nunca afloran: el driver rearranca el stream en la siguiente
  escritura, igual que la emulación de Linux vuelve a hacer prepare con
  `EPIPE`. `poll(2)` informa de escribible mientras el stream está parado o
  en cuanto cabe un fragmento entero.
- **El resto**: `SYNC` (vacía, y la siguiente escritura vuelve a hacer
  prepare), `POST`, `RESET`, `GETCAPS` (revisión 1, `REALTIME`, `TRIGGER`;
  sin mmap ni dúplex), `OSS_GETVERSION` (3.8.1a),
  `SOUND_PCM_READ_BITS/CHANNELS`, `SETSYNCRO`/`PROFILE` (aceptados),
  `SETDUPLEX` y el par `FILTER` (`EIO`),
  `GETISPACE`/`GETIPTR`/`MAPINBUF`/`MAPOUTBUF` (`EINVAL`). Todo lo demás,
  incluido un `TCGETS` despistado, es `EINVAL` como en Linux. `read(2)` es
  `ENXIO`; una apertura `O_RDONLY` es `EINVAL` (no hay captura).

La única diferencia deliberada: un `/dev/dsp` recién abierto está a 48 kHz
S16LE estéreo en vez de a los 8 kHz U8 mono de Linux, así que `cat music.raw >
/dev/dsp` reproduce un fichero raw moderno. Solo `cat` lo nota; `/dev/audio`
conserva los valores por defecto de µ-law.

`/dev/mixer<N>` sigue a `snd-mixer-oss`: `SOUND_MIXER_READ/WRITE_VOLUME` y
`_PCM` mueven las dos la única ganancia de la tarjeta (`Master`; el HDMI/DP no
tiene volumen analógico, el driver escala el S16LE), un canal escrito a 0 se
silencia, `DEVMASK`/`STEREODEVS` listan esos dos, `RECSRC`/`RECMASK`/`CAPS`
son 0 y `SOUND_MIXER_INFO` nombra la tarjeta. `aumix`, `ossmix` y el control
de volumen OSS de mpv acaban aquí.

`/dev/dsp<N>` y `/dev/snd/pcmC<N>D0p` son dos front-ends sobre la misma
tarjeta, y cada apertura de cualquiera de los dos es un stream propio,
mezclado en el kernel: `wavplay` en `/dev/dsp` suena mientras PulseAudio tiene
`hw:0,0`. (En un dispositivo cuyo driver no responde a `open_stream`, los dos
nodos caen a una única reserva compartida de un solo cliente y el que abre
segundo recibe `EBUSY`, como en Linux con un solo substream detrás de los dos
nodos; el driver HDA nunca toma ese camino.)

## Pruebas

```sh
wavplay --tone                 # seno de 440 Hz, 3 s, /dev/dsp (tarjeta 0)
wavplay --tone 880             # otra frecuencia
wavplay -d /dev/dsp1 --tone    # el siguiente códec (a menudo analógico)
wavplay file.wav               # WAV PCM de 16 bits (el mono se duplica)
aplay -l                       # tarjetas ALSA (necesita /usr/share/alsa/alsa.conf)
aplay music.wav                # default = Pulse → hw:0,0
pactl info                     # el demonio PulseAudio
pactl set-sink-volume @DEFAULT_SINK@ 80%
amixer set Master 80%          # ctl de Pulse, o el Master del kernel si Pulse no está
```

`tools/wavplay` es un binario musl estático que xtask instala en el rootfs.
`aplay`/`amixer`/`mpg123` hablan con `/dev/snd` a través de alsa-lib (y, con
Pulse arriba, a través del plugin de pulse); QEMU arranca el initramfs live,
que tiene que incluir `usr/share/alsa` (el plugin `hw` vive en `alsa.conf`) y
`usr/bin/pulseaudio`. OSS (`wavplay` → `/dev/dsp`) no necesita ese fichero y
se salta Pulse.

Que `HW_REFINE` devuelva `EINVAL` durante `aplay`/`mpg123` es normal:
alsa-lib sondea así combinaciones de formato y periodo no admitidas. Un fallo
real imprime `Unable to set hw params` / `cannot set hw params` en el cliente.
Recompila el kernel después de cambiar `snd.rs`, y el rootfs después de
cambiar `/etc/asound.conf`.

`make qemu` añade Intel HD Audio automáticamente (`-device intel-hda` +
`hda-output`). El backend del host se elige entre lo que admita QEMU:
pipewire, luego PulseAudio, luego ALSA. El QEMU compilado en `/usr/local` que
está en el PATH a menudo no tiene ninguno; en ese caso se usa
`/usr/bin/qemu-system-x86_64` para que puedas oír al invitado. Se puede forzar
con `AUDIODEV=wav` (PCM a `/tmp/eclipse-qemu.wav`) o `AUDIO=off` (códec
presente, host en silencio).

## Límites conocidos

- Solo reproducción (sin captura), solo estéreo, solo S16LE en el PCM del
  kernel. PulseAudio remuestrea los demás formatos en espacio de usuario.
- Varios clientes suenan a la vez, por PulseAudio o directamente: cada
  apertura de `hw:0,0` y `/dev/dsp` es un stream mezclado en el kernel. No hay
  volumen por stream en el kernel (la única ganancia `Master` se aplica a la
  mezcla); el volumen por stream de PulseAudio es cosa del demonio.
- `/dev/snd/timer` muestrea el estado PCM del nodo del registro, que ningún
  cliente de un stream mueve, así que un temporizador ALSA ligado a `hw:0,0`
  no avanza con el stream de un cliente. Nada en la imagen usa uno (`dmix` lo
  haría, y ya no le queda nada que hacer).
- El volumen es escalado del PCM por software (sin programar el amplificador
  analógico), con una rampa de 32 ms de audio; lo que ya está en cola en el
  anillo no se reescala con efecto retroactivo: el nuevo nivel se aplica desde
  el siguiente `write`, así que con el búfer de sink de 250 ms de PulseAudio
  se oye un cuarto de segundo después. El volumen del sink de Pulse se aplica
  a la salida mezclada.
- El PCM de un stream se mezcla en el anillo hasta `FILL_DEPTH` (85 ms) más la
  ventaja de posición por delante del cabezal, así que el puntero de hardware
  de un cliente va ese tanto por delante de lo que se oye; `DELAY`/`GETODELAY`
  informan de la diferencia. Un atasco del bucle de relleno más largo que la
  profundidad más la banda de silencio retrasa los streams en vez de tirar su
  PCM (`late fills` en `/proc/gpusnd`).
- El camino de audio DP usa los mismos controles de ELD y activación, pero no
  se ha ejercitado; el audio DP-MST (entradas de dispositivo > 0) no está
  implementado.
- El quitar silencio de HDMI/DP se reenvía en cada arranque de stream digital
  (GOP nunca activa los paquetes de audio). Un monitor enchufado en caliente
  tras el arranque recibe ELD/PD al empezar la reproducción.
- El camino de audio nunca arranca el GSP de la GPU de consola (un `write(2)`
  a `/dev/snd` no puede colgar la máquina). El HDMI de esa GPU usa en su lugar
  el camino GOP/BAR0; `/proc/gpusnd` muestra `GOP ... ELD+PD+GCP unmute`.
- La tarjeta ALSA 0 prefiere un pin HDMI/DP *vivo* (presencia/ELD). Un pin
  NVIDIA muerto no va por delante del códec analógico del PCH. Una segunda GPU
  sin monitor no se registra como tarjeta de sonido.
