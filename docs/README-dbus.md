# D-Bus en Eclipse OS

Eclipse arranca un **bus de sesión** de verdad. Hasta ahora no había ninguno:
la dirección `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus` estaba fijada
a propósito apuntando a un socket **sin demonio**, para que `connect()` fallara
al instante con `ECONNREFUSED` y libdbus no intentara `autolaunch:` (que hace
fork de `dbus-launch`, abre `$DISPLAY` y cuelga a quien lo espere: así se colgó
gzdoom). Eso evitaba el cuelgue, pero dejaba sin bus a todo lo que necesita uno.

Ahora hay un demonio escuchando en esa misma ruta, así que no hubo que tocar
ninguna variable de entorno: los cuatro sitios que ya exportaban la dirección
(`eclipse-init`, `/etc/profile`, `~/.config/labwc/environment` y el wrapper de
SDL) siguen igual y los clientes encuentran el bus solos.

## Qué arranca, y en qué orden

| Pieza | Dónde |
|---|---|
| Servicio | `/etc/eclipse/services/dbus.service` (`type = respawn`) |
| Wrapper | `/usr/local/bin/eclipse-dbus` |
| Demonio (Alpine) | `dbus-daemon --session --nofork --nopidfile --address=unix:path=/run/user/0/bus` |
| Demonio (Eclipse) | `/bin/eclipse-dbusd --session --address=unix:path=/run/user/0/bus` |

El wrapper prefiere el `dbus-daemon` de Alpine cuando está instalado (el
paquete `dbus` está en `DEFAULT_PACKAGES`, `xtask/src/linux/xorg.rs`) y cae a
`eclipse-dbusd` si no. Ambos aceptan el mismo `argv`, así que la elección es un
`command -v`.

El servicio **no** lleva `desktop =`: lo quieren tanto la sesión labwc como la
de Xorg. `labwc.service` y `xorg.service` llevan `after = dbus`, pero **sin**
`wait_socket`: un bus que no esté no debe costarle nada al arranque, y ningún
servicio de arranque habla con él — los clientes que sí lo hacen (juegos SDL,
aplicaciones GTK) se lanzan minutos después.

El wrapper se encarga además de `/etc/machine-id` y `/var/lib/dbus/machine-id`
(dbus valida que sean 32 dígitos hexadecimales en minúscula y se niega a
arrancar con cualquier otra cosa) y de borrar un socket huérfano de un arranque
anterior, que haría fallar el `bind()` con `EADDRINUSE` sin que hubiera nadie
escuchando.

## `eclipse-dbusd`: el bus propio

`tools/eclipse-dbusd` es un bus de sesión escrito en Rust, estático contra musl,
sin más dependencia que `libc`. Existe por dos razones concretas:

1. Una imagen construida **sin acceso al mirror de Alpine** no tiene
   `dbus-daemon`, y sin bus no arranca nada que use `GtkApplication` ni nada que
   registre un nombre para comprobar si ya hay otra copia corriendo.
2. Es código nuestro, así que se puede **probar contra el kernel de Eclipse en
   QEMU** en vez de solo en hardware. Cada paso del probe es un viaje de ida y
   vuelta por `sendmsg`/`recvmsg`/`poll` sobre un socket unix, y la
   autenticación EXTERNAL se apoya en `SO_PEERCRED`.

### Qué implementa

- Transporte `unix:path=`, saludo SASL (`EXTERNAL` con `SO_PEERCRED`, y
  `ANONYMOUS`) y `NEGOTIATE_UNIX_FD`.
- Nombres únicos (`:1.N`), `Hello`, y toda la maquinaria de nombres conocidos:
  `RequestName`/`ReleaseName` con cola de reemplazo,
  `NameOwnerChanged`/`NameAcquired`/`NameLost`.
- Enrutado unicast por destino con el campo `SENDER` puesto por el bus (un
  cliente no puede falsificarlo), respuestas de error para las llamadas que no
  se pueden entregar, difusión de señales filtrada por reglas `AddMatch`, y
  paso de descriptores (`SCM_RIGHTS`).
- `BecomeMonitor`, así que `dbus-monitor` funciona: en una máquina sin
  depurador es la única forma de ver qué se dice en la sesión.

### Qué NO implementa, a propósito

- **Activación de servicios.** No lee ficheros `.service` ni lanza nada bajo
  demanda; `StartServiceByName` responde `ServiceUnknown` salvo que el nombre ya
  esté en el bus. En Eclipse ningún demonio se activa por demanda: todos son
  servicios de `eclipse-init`.
- **Política (`<allow>`/`<deny>` de `session.conf`).** Eclipse es monousuario
  root: el bus rechaza las conexiones cuyo uid por `SO_PEERCRED` no sea el suyo
  y, más allá de eso, todo está permitido.

Si alguna vez hace falta activación o política, la respuesta es instalar el
paquete `dbus`: el wrapper lo prefiere solo.

## Comprobar que funciona

Desde un terminal de la sesión, o desde el menú del escritorio («Prueba D-Bus
(bus de sesión)»):

```sh
eclipse-dbusd --selftest          # DBUSPROBE: PASS ... / DBUSPROBE: FAIL ...
```

El probe abre dos conexiones y recorre lo que de verdad usa un cliente de
escritorio: nombres únicos distintos, `AddMatch`, `RequestName`,
`NameOwnerChanged`, `GetNameOwner`, una llamada enrutada por nombre conocido con
su respuesta de vuelta, una señal difundida, `Ping`, `ListNames`, y la
liberación del nombre al desconectar. Sale con código 1 y una línea `FAIL` que
dice qué paso falló.

En QEMU, donde no hay escritorio, se puede lanzar en el arranque:

```sh
make -C zCore ... CMDLINE="...:dbus.selftest"
```

`dbus-selftest.service` lleva `cmdline = dbus.selftest`, una clave nueva de
`eclipse-init` que arranca un servicio solo cuando ese token está en la línea de
órdenes del kernel. En un arranque normal el servicio ni se carga.

Con el `dbus-daemon` de Alpine instalado, las herramientas de siempre funcionan
igual:

```sh
dbus-send --print-reply --dest=org.freedesktop.DBus / org.freedesktop.DBus.ListNames
gdbus call --session --dest org.freedesktop.DBus \
  --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.GetId
busctl --user list
dbus-monitor --session
```

Las cuatro se han probado contra `eclipse-dbusd` (libdbus, GDBus, sd-bus y el
monitor), no solo contra `dbus-daemon`.

## Lo que esto desbloquea

- **SDL2.** `SDL_Init()` ejecuta `SDL_DBus_Init()` antes que nada. Ya no se
  colgaba (la dirección fijada lo evitaba), pero ahora además tiene bus, que es
  lo que usa para inhibir el salvapantallas.
- **GTK.** `GtkApplication` sale antes de dibujar sin bus de sesión: eso fue lo
  que mató a waybar en su día.
- **Instancia única.** Todo lo que pregunta «¿ya hay otra copia mía?» lo hace
  con `RequestName`.

Lo que **sigue sin poder correr** es Plasma: `plasmashell`, `kded`, `krunner` y
el agente de polkit son servicios de D-Bus, sí, pero además necesitan Qt, KF6,
polkit y logind, que no están. Ver [README-desktop.md](README-desktop.md).
